use anyhow::{Context, Result, anyhow, bail};
use std::{
    io::{BufRead, BufReader, Write},
    os::windows::{io::AsRawHandle, process::CommandExt},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};
use timelens_ai::{
    context::{estimated_tokens, messages_tokens, plan_context},
    *,
};
use timelens_storage::{AiJob, AiJobKind, Storage};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE},
    System::JobObjects::*,
};

pub enum AiCommand {
    Test(ProviderProfile),
    Models(ProviderProfile),
    Wake,
}
pub enum AiEvent {
    Status(String),
    Tested(String, bool),
    Models(String, Vec<Model>),
    Changed,
    Finished(i64, bool),
}
pub struct AiService {
    pub sender: mpsc::Sender<AiCommand>,
    pub receiver: mpsc::Receiver<AiEvent>,
}
static PAUSED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
fn paused() -> bool {
    PAUSED.load(std::sync::atomic::Ordering::SeqCst)
}
struct Active;
impl Drop for Active {
    fn drop(&mut self) {
        ACTIVE.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}
pub struct PauseGuard;
impl PauseGuard {
    pub fn keep_paused(self) {
        std::mem::forget(self);
    }
}
impl Drop for PauseGuard {
    fn drop(&mut self) {
        PAUSED.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}
pub fn pause_and_wait() -> Result<PauseGuard> {
    if PAUSED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        bail!("AI 已在进行数据维护");
    }
    let guard = PauseGuard;
    let start = Instant::now();
    while ACTIVE.load(std::sync::atomic::Ordering::SeqCst) {
        if start.elapsed() > Duration::from_secs(15) {
            bail!("AI 工作进程尚未停止，数据未替换");
        }
        thread::sleep(Duration::from_millis(50));
    }
    Ok(guard)
}

pub fn spawn(storage: Arc<Mutex<Storage>>) -> AiService {
    let (sender, commands) = mpsc::channel();
    let (events, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut schedule_tick = Instant::now() - Duration::from_secs(20);
        let mut generation = crate::ai_ui::data_generation();
        loop {
            if paused() {
                thread::sleep(Duration::from_millis(100));
                continue;
            }
            ACTIVE.store(true, std::sync::atomic::Ordering::SeqCst);
            let _active = Active;
            if paused() {
                continue;
            }
            if generation != crate::ai_ui::data_generation() {
                while commands.try_recv().is_ok() {}
                generation = crate::ai_ui::data_generation();
                schedule_tick = Instant::now() - Duration::from_secs(20);
            }
            match commands.recv_timeout(Duration::from_millis(250)) {
                Ok(AiCommand::Test(mut profile)) => {
                    let id = profile.id.clone();
                    let _ = events.send(AiEvent::Status(
                        "正在发送不含活动数据的合成连接测试…".into(),
                    ));
                    let request = WorkerRequest {
                        version: WORKER_PROTOCOL,
                        profile: profile.clone(),
                        operation: WorkerOperation::Generate {
                            system: "Connection test. Reply briefly.".into(),
                            messages: vec![ChatMessage {
                                role: Role::User,
                                text: "Reply with OK. This is synthetic test data.".into(),
                            }],
                            images: vec![],
                        },
                    };
                    let result = run_worker(&request, || false, |_| Ok(())).and_then(|answer| {
                        if answer.text.trim().is_empty() {
                            bail!("提供商没有返回正文");
                        }
                        profile.tested_revision = Some(profile.revision());
                        let s = lock(&storage)?;
                        let current = s
                            .ai_profile(&id)
                            .context("测试期间配置已删除，请重新选择提供商")?;
                        if current.revision() != profile.revision() {
                            bail!("测试期间配置已改变，请重新测试");
                        }
                        s.save_ai_profile(&profile)?;
                        Ok(())
                    });
                    let _ = events.send(AiEvent::Status(match &result {
                        Ok(()) => "连接测试通过；可手动总结，计划需分别启用".into(),
                        Err(e) => format!("连接测试失败：{e}"),
                    }));
                    let _ = events.send(AiEvent::Tested(id, result.is_ok()));
                }
                Ok(AiCommand::Models(profile)) => {
                    let id = profile.id.clone();
                    let request = WorkerRequest {
                        version: WORKER_PROTOCOL,
                        profile,
                        operation: WorkerOperation::Models,
                    };
                    match run_worker(&request, || false, |_| Ok(())) {
                        Ok(answer) => {
                            let _ = events.send(AiEvent::Models(id, answer.models));
                        }
                        Err(e) => {
                            let _ = events.send(AiEvent::Status(format!(
                                "读取模型列表失败：{e}；仍可手动填写模型 ID"
                            )));
                        }
                    }
                }
                Ok(AiCommand::Wake) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if schedule_tick.elapsed() >= Duration::from_secs(15) {
                let result = crate::snapshot::run_heavy_task(|| {
                    let s = lock(&storage)?;
                    s.enqueue_due_ai_schedules(crate::unix_time_ms())?;
                    s.apply_ai_retention(crate::unix_time_ms())?;
                    Ok(())
                });
                if let Err(e) = result {
                    let _ = events.send(AiEvent::Status(format!("AI 计划检查失败：{e}")));
                }
                schedule_tick = Instant::now();
            }
            let job = match lock(&storage).and_then(|s| Ok(s.claim_ai_job(crate::unix_time_ms())?))
            {
                Ok(job) => job,
                Err(e) => {
                    let _ = events.send(AiEvent::Status(format!("AI 队列读取失败：{e}")));
                    None
                }
            };
            if let Some(job) = job {
                let result = run_job(&storage, &job, &events);
                let mut success = false;
                if let Ok(s) = lock(&storage)
                    && s.ai_job(job.id).is_ok_and(|j| j.state == "running")
                {
                    match result {
                        Ok(()) => match s.finish_ai_job(job.id, crate::unix_time_ms()) {
                            Ok(_) => success = true,
                            Err(e) => {
                                let _ = s.fail_ai_job(
                                    job.id,
                                    &failure(e.to_string()),
                                    crate::unix_time_ms(),
                                );
                            }
                        },
                        Err(error) => {
                            let f = error
                                .downcast_ref::<ProviderError>()
                                .map_or_else(|| failure(error.to_string()), |e| e.0.clone());
                            let _ = s.fail_ai_job(job.id, &f, crate::unix_time_ms());
                        }
                    }
                }
                let _ = events.send(AiEvent::Changed);
                let _ = events.send(AiEvent::Finished(job.id, success));
            }
        }
    });
    AiService { sender, receiver }
}

fn lock(storage: &Arc<Mutex<Storage>>) -> Result<std::sync::MutexGuard<'_, Storage>> {
    storage.lock().map_err(|_| anyhow!("存储锁损坏"))
}
fn failure(message: String) -> AiFailure {
    AiFailure {
        status: None,
        message,
        retry_after_seconds: None,
        retryable: false,
    }
}
#[derive(Debug)]
struct ProviderError(AiFailure);
impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(code) = self.0.status {
            write!(f, "HTTP {code}: ")?;
        }
        f.write_str(&self.0.message)
    }
}
impl std::error::Error for ProviderError {}
#[derive(Default)]
struct Answer {
    text: String,
    reasoning: String,
    usage: TokenUsage,
    models: Vec<Model>,
}

struct WorkerChild {
    child: Child,
    job: HANDLE,
}
impl Drop for WorkerChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        unsafe {
            CloseHandle(self.job);
        }
    }
}
impl WorkerChild {
    fn spawn() -> Result<Self> {
        let directory = std::env::current_exe()?
            .parent()
            .ok_or_else(|| anyhow!("程序目录不可用"))?
            .to_path_buf();
        let path = ["Timelens.AI.exe", "timelens-ai-worker.exe"]
            .into_iter()
            .map(|n| directory.join(n))
            .find(|p| p.is_file())
            .ok_or_else(|| anyhow!("AI 工作进程缺失，请完整安装 Timelens"))?;
        let mut child = Command::new(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(0x08000000)
            .spawn()?;
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if job.is_null()
            || unsafe {
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    std::mem::size_of_val(&limits) as u32,
                )
            } == 0
            || unsafe { AssignProcessToJobObject(job, child.as_raw_handle()) } == 0
        {
            let _ = child.kill();
            let _ = child.wait();
            if !job.is_null() {
                unsafe {
                    CloseHandle(job);
                }
            }
            bail!("无法建立 AI 子进程生命周期隔离");
        }
        Ok(Self { child, job })
    }
}

fn run_worker(
    request: &WorkerRequest,
    canceled: impl Fn() -> bool,
    mut progress: impl FnMut(&WorkerEvent) -> Result<()>,
) -> Result<Answer> {
    if paused() || canceled() {
        bail!("AI 请求已暂停或取消");
    }
    let mut child = WorkerChild::spawn()?;
    let stdout = child.child.stdout.take().context("AI 输出管道缺失")?;
    let (sender, receiver) = mpsc::sync_channel::<Result<WorkerEvent, String>>(64);
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut line = String::new();
            let mut limited = std::io::Read::take(&mut reader, MAX_FRAME_BYTES as u64 + 1);
            let result = limited.read_line(&mut line);
            match result {
                Ok(0) => break,
                Ok(_) => {
                    let event = if line.len() > MAX_FRAME_BYTES {
                        Err("AI 进程返回了过大的消息".into())
                    } else {
                        serde_json::from_str::<WorkerEvent>(&line)
                            .map_err(|_| "AI 进程返回了无效消息".into())
                    };
                    if sender.send(event).is_err() {
                        break;
                    }
                }
                Err(_) => {
                    let _ = sender.send(Err("AI 输出管道中断".into()));
                    break;
                }
            }
        }
    });
    {
        let mut stdin = child.child.stdin.take().context("AI 输入管道缺失")?;
        serde_json::to_writer(&mut stdin, request)?;
        stdin.flush()?;
    }
    let mut answer = Answer::default();
    let mut completed = false;
    let mut provider_error = None;
    loop {
        if paused() || canceled() {
            bail!("AI 任务已取消");
        }
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(Ok(event)) => {
                match &event {
                    WorkerEvent::Text(s) => answer.text.push_str(s),
                    WorkerEvent::Reasoning(s) => answer.reasoning.push_str(s),
                    WorkerEvent::Usage(u) => answer.usage.update(u.clone()),
                    WorkerEvent::Models(m) => answer.models = m.clone(),
                    WorkerEvent::Failure(e) => provider_error = Some(e.clone()),
                    WorkerEvent::Complete => completed = true,
                }
                if answer.text.len() + answer.reasoning.len() > MAX_OUTPUT_BYTES {
                    bail!("AI 输出超过 16 MiB");
                }
                progress(&event)?;
            }
            Ok(Err(error)) => bail!("{error}"),
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if child.child.try_wait()?.is_some() {
                    continue;
                }
            }
        }
    }
    if let Some(error) = provider_error {
        return Err(ProviderError(error).into());
    }
    if !completed {
        bail!("AI 工作进程提前退出，未收到完成标记");
    }
    Ok(answer)
}

fn job_request(
    storage: &Arc<Mutex<Storage>>,
    job: &AiJob,
    system: String,
    messages: Vec<ChatMessage>,
    images: Vec<ImageInput>,
    accumulated: &mut TokenUsage,
    events: &mpsc::Sender<AiEvent>,
) -> Result<Answer> {
    let request = WorkerRequest {
        version: WORKER_PROTOCOL,
        profile: job.spec.provider.clone(),
        operation: WorkerOperation::Generate {
            system,
            messages,
            images,
        },
    };
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut usage = TokenUsage::default();
    let mut last = Instant::now() - Duration::from_secs(1);
    let result = run_worker(
        &request,
        || {
            lock(storage).map_or(true, |s| {
                s.ai_job(job.id).map_or(true, |j| j.state != "running")
            })
        },
        |event| {
            match event {
                WorkerEvent::Text(s) => text.push_str(s),
                WorkerEvent::Reasoning(s) => reasoning.push_str(s),
                WorkerEvent::Usage(u) => usage.update(u.clone()),
                _ => {}
            }
            if last.elapsed() >= Duration::from_millis(250) {
                let mut total = accumulated.clone();
                total.add(&usage);
                lock(storage)?.persist_ai_progress(
                    job.id,
                    &text,
                    &reasoning,
                    &total,
                    crate::unix_time_ms(),
                )?;
                let _ = events.send(AiEvent::Changed);
                last = Instant::now();
            }
            Ok(())
        },
    );
    accumulated.add(&usage);
    lock(storage)?.persist_ai_progress(
        job.id,
        &text,
        &reasoning,
        accumulated,
        crate::unix_time_ms(),
    )?;
    let _ = events.send(AiEvent::Changed);
    result
}

fn run_job(
    storage: &Arc<Mutex<Storage>>,
    job: &AiJob,
    events: &mpsc::Sender<AiEvent>,
) -> Result<()> {
    let mut usage = job.usage.clone();
    match job.spec.kind {
        AiJobKind::Summary => {
            let mut data = if job.spec.chunks.is_empty() {
                serde_json::to_string(&job.spec.envelope)?
            } else {
                let mut summaries = vec![];
                for (index, chunk) in job.spec.chunks.iter().enumerate() {
                    let _ = events.send(AiEvent::Status(format!(
                        "正在总结自然日分块 {}/{}",
                        index + 1,
                        job.spec.chunks.len()
                    )));
                    let answer = job_request(
                        storage,
                        job,
                        format!(
                            "{}\n这是长范围的一个数据分块，请保留全部应用和缺失说明。同一天的多个分块可能重复总计，后续汇总不可重复累计。",
                            job.spec.effective_prompt
                        ),
                        vec![ChatMessage {
                            role: Role::User,
                            text: serde_json::to_string(chunk)?,
                        }],
                        vec![],
                        &mut usage,
                        events,
                    )?;
                    summaries.push(answer.text);
                }
                summaries.join("\n\n---\n\n")
            };
            let budget = (job.spec.provider.model.context_tokens as usize)
                .saturating_sub(
                    job.spec
                        .provider
                        .parameters
                        .max_output_tokens
                        .unwrap_or(4096)
                        .min(job.spec.provider.model.context_tokens / 2)
                        as usize,
                )
                .saturating_sub(estimated_tokens(&job.spec.effective_prompt));
            // Hierarchical reduction retains every chunk even for very long ranges.
            for level in 0..12 {
                if estimated_tokens(&data) < budget * 8 / 10 {
                    break;
                }
                let _ = events.send(AiEvent::Status(format!(
                    "正在合并长范围分块，第 {} 层",
                    level + 1
                )));
                let pieces = split_text(&data, budget / 2);
                let mut reduced = vec![];
                for piece in pieces {
                    let answer=job_request(storage,job,"压缩以下分块总结；保留所有应用、范围、数字和缺失，不叠加重叠时间或重复总计。".into(),vec![ChatMessage{role:Role::User,text:piece}],vec![],&mut usage,events)?;
                    reduced.push(answer.text);
                }
                let next = reduced.join("\n\n");
                if next.len() >= data.len() {
                    bail!("分块汇总未能缩小上下文；部分结果已保留，请选择更大上下文模型");
                }
                data = next;
            }
            if estimated_tokens(&data) > budget {
                bail!("汇总仍超出上下文，未静默丢弃数据");
            }
            let images = crate::snapshot::run_heavy_task(|| {
                Ok(lock(storage)?.consume_ai_images(job.id, crate::unix_time_ms())?)
            })?;
            let _ = events.send(AiEvent::Status("正在生成最终总结…".into()));
            job_request(
                storage,
                job,
                job.spec.effective_prompt.clone(),
                vec![ChatMessage {
                    role: Role::User,
                    text: format!("请依据以下数据生成总结。分块若有重复聚合不能重复累计。\n{data}"),
                }],
                images,
                &mut usage,
                events,
            )?;
        }
        AiJobKind::FollowUp {
            version_id,
            user_message_id,
        } => {
            let (version, branch, settings, compression) = {
                let s = lock(storage)?;
                let version = s.ai_version(version_id)?;
                let branch = s.ai_branch(version_id, Some(user_message_id))?;
                let c = s.ai_compression(version_id, &branch)?;
                (version, branch, s.ai_settings()?, c)
            };
            let cleared = branch.iter().rposition(|m| m.role == "clear_context");
            let mut messages = vec![];
            let mut indexes = vec![];
            if cleared.is_none() {
                messages.push(ChatMessage {
                    role: Role::User,
                    text: "请总结当前数据。".into(),
                });
                indexes.push(None);
                messages.push(ChatMessage {
                    role: Role::Assistant,
                    text: version.answer.clone(),
                });
                indexes.push(None);
            }
            for m in branch
                .iter()
                .skip(cleared.map_or(0, |i| i + 1))
                .filter(|m| m.completed && (m.role == "user" || m.role == "assistant"))
            {
                messages.push(ChatMessage {
                    role: if m.role == "user" {
                        Role::User
                    } else {
                        Role::Assistant
                    },
                    text: m.body.clone(),
                });
                indexes.push(Some(m.id));
            }
            let mut data = serde_json::to_string(&job.spec.envelope)?;
            let context_window = job.spec.provider.model.context_tokens;
            if estimated_tokens(&data) > context_window as usize / 2 {
                data = format!(
                    "原始结构化快照仍在本地。此范围超过当前上下文，本次使用已保存的初始总结作为压缩数据上下文：\n{}",
                    version.answer
                );
                let _ = events.send(AiEvent::Status(
                    "范围较长，本次追问使用初始总结作为压缩上下文；完整数据包仍保留在本地".into(),
                ));
            }
            let mut system = format!(
                "{}\n本地保存的活动上下文：\n{}",
                job.spec.effective_prompt, data
            );
            if let Some((through, summary)) = compression
                && let Some(index) = indexes.iter().position(|id| *id == Some(through))
                && cleared.is_none()
            {
                system.push_str(&format!("\n早期对话压缩摘要：\n{summary}"));
                messages.drain(..=index);
                indexes.drain(..=index);
            }
            let reserved = estimated_tokens(&system)
                + job
                    .spec
                    .provider
                    .parameters
                    .max_output_tokens
                    .unwrap_or(4096) as usize;
            let plan = plan_context(
                &messages,
                context_window,
                reserved,
                settings.recent_messages,
                settings.compression_enabled,
            );
            let mut recent = plan.recent;
            if plan.needs_compression {
                let _ = events.send(AiEvent::Status(
                    "正在压缩当前分支的早期对话；原始消息继续保留…".into(),
                ));
                let compression_request = WorkerRequest {
                    version: WORKER_PROTOCOL,
                    profile: job.spec.provider.clone(),
                    operation: WorkerOperation::Generate {
                        system: "总结以下较早对话，保留事实、用户问题与已作结论。不要添加新事实。"
                            .into(),
                        messages: vec![ChatMessage {
                            role: Role::User,
                            text: serde_json::to_string(&plan.older)?,
                        }],
                        images: vec![],
                    },
                };
                match run_worker(
                    &compression_request,
                    || {
                        lock(storage).map_or(true, |s| {
                            s.ai_job(job.id).map_or(true, |j| j.state != "running")
                        })
                    },
                    |_| Ok(()),
                ) {
                    Ok(answer) => {
                        usage.add(&answer.usage);
                        system.push_str(&format!("\n早期对话摘要：\n{}", answer.text));
                        if let Some(through) = indexes
                            .get(plan.older.len().saturating_sub(1))
                            .copied()
                            .flatten()
                        {
                            lock(storage)?.save_ai_compression(
                                version_id,
                                through,
                                &answer.text,
                                &answer.usage,
                                crate::unix_time_ms(),
                            )?;
                        }
                    }
                    Err(_) => {
                        let _ = events.send(AiEvent::Status(
                            "压缩失败；本次仅发送可容纳的最近完整用户对话，较早内容未发送".into(),
                        ));
                        recent = plan_context(
                            &messages,
                            context_window,
                            reserved,
                            settings.recent_messages,
                            false,
                        )
                        .recent;
                    }
                }
            } else if plan.omitted {
                let _ = events.send(AiEvent::Status(
                    "本次按最近消息设置发送，较早消息未发送但仍在本地保留".into(),
                ));
            }
            if messages_tokens(&recent) + estimated_tokens(&system)
                > context_window as usize
                    - job
                        .spec
                        .provider
                        .parameters
                        .max_output_tokens
                        .unwrap_or(4096)
                        .min(context_window / 2) as usize
            {
                bail!("当前问题或数据超过模型上下文，请缩短问题或选择更大上下文模型");
            }
            job_request(storage, job, system, recent, vec![], &mut usage, events)?;
        }
    }
    Ok(())
}

fn split_text(text: &str, budget: usize) -> Vec<String> {
    let max_chars = (budget / 2).max(128);
    let mut pieces = vec![];
    let mut current = String::new();
    let mut count = 0;
    for c in text.chars() {
        current.push(c);
        count += 1;
        if count >= max_chars {
            pieces.push(std::mem::take(&mut current));
            count = 0;
        }
    }
    if !current.is_empty() {
        pieces.push(current);
    }
    pieces
}
