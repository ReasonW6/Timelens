use crate::{
    AiState, AppWindow, UiState,
    ai::{AiCommand, AiEvent, AiService},
};
use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, Local, NaiveDateTime, TimeZone};
use slint::{ComponentHandle, ModelRc, SharedString, Timer, TimerMode, VecModel};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    rc::Rc,
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};
use timelens_ai::{
    schedule::{Schedule, ScheduleKind},
    *,
};
use timelens_storage::{AiJob, AiSettings, AiVersion, SnapshotConsent, SnapshotSlot, Storage};

static DATA_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub fn data_generation() -> u64 {
    DATA_GENERATION.load(std::sync::atomic::Ordering::Acquire)
}
pub fn dataset_changed() {
    DATA_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Release);
}

struct UiData {
    profile: ProviderProfile,
    profiles: Vec<ProviderProfile>,
    prompts: Vec<PromptPreset>,
    prompt_id: String,
    jobs: Vec<AiJob>,
    versions: Vec<AiVersion>,
    job: Option<i64>,
    version: Option<i64>,
    leaf: Option<i64>,
    branches: Vec<Option<i64>>,
    slots: Vec<SnapshotSlot>,
    preview: Option<SnapshotConsent>,
    consents: Vec<SnapshotConsent>,
    prepared: Option<(i64, i64, String, String)>,
}
impl Default for UiData {
    fn default() -> Self {
        Self {
            profile: ProviderProfile::preset(0, new_id()),
            profiles: vec![],
            prompts: vec![],
            prompt_id: "default".into(),
            jobs: vec![],
            versions: vec![],
            job: None,
            version: None,
            leaf: None,
            branches: vec![None],
            slots: vec![],
            preview: None,
            consents: vec![],
            prepared: None,
        }
    }
}
fn new_id() -> String {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!(
        "p-{}-{}",
        crate::unix_time_ms(),
        SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}
fn strings(values: impl IntoIterator<Item = String>) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(
        values.into_iter().map(Into::into).collect::<Vec<_>>(),
    ))
}
fn lock(storage: &Arc<Mutex<Storage>>) -> Result<std::sync::MutexGuard<'_, Storage>> {
    storage.lock().map_err(|_| anyhow!("存储锁损坏"))
}
fn parse<T: std::str::FromStr>(value: &str, name: &str) -> Result<T> {
    value.trim().parse().map_err(|_| anyhow!("{name}格式无效"))
}
fn optional<T: std::str::FromStr>(value: &str, name: &str) -> Result<Option<T>> {
    if value.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(parse(value, name)?))
    }
}
fn cap(index: i32) -> Capability {
    match index {
        1 => Capability::Supported,
        2 => Capability::Unsupported,
        _ => Capability::Unknown,
    }
}
fn cap_index(c: Capability) -> i32 {
    match c {
        Capability::Supported => 1,
        Capability::Unsupported => 2,
        Capability::Unknown => 0,
    }
}
fn time_text(ms: i64) -> String {
    DateTime::from_timestamp_millis(ms)
        .map(|d| {
            d.with_timezone(&Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_default()
}
fn parse_time(value: &str) -> Result<i64> {
    if let Ok(date) = DateTime::parse_from_rfc3339(value) {
        return Ok(date.timestamp_millis());
    }
    let local = ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M"]
        .into_iter()
        .find_map(|format| NaiveDateTime::parse_from_str(value.trim(), format).ok())
        .ok_or_else(|| anyhow!("请使用 YYYY-MM-DD HH:MM:SS 本地时间"))?;
    Local
        .from_local_datetime(&local)
        .single()
        .map(|d| d.timestamp_millis())
        .ok_or_else(|| anyhow!("此本地时间不存在或有歧义，请用带时区偏移的 RFC3339 时间"))
}
fn range(w: &AppWindow) -> Result<(i64, i64)> {
    let g = w.global::<AiState>();
    let start = parse_time(&g.get_range_start())?;
    let end = parse_time(&g.get_range_end())?;
    if end <= start {
        bail!("结束时间必须晚于开始时间");
    }
    Ok((start, end))
}

pub fn install(
    w: &AppWindow,
    storage: Arc<Mutex<Storage>>,
    timeline: Rc<RefCell<UiState>>,
    service: AiService,
) -> Result<Timer> {
    let data = Rc::new(RefCell::new(UiData::default()));
    let sender = service.sender;
    reset_dataset_ui(w, &mut data.borrow_mut(), &*lock(&storage)?)?;
    macro_rules! bind {
        ($callback:ident,$body:expr) => {{
            let weak = w.as_weak();
            let state = Rc::clone(&data);
            let storage = Arc::clone(&storage);
            let sender = sender.clone();
            w.global::<AiState>().$callback(move || {
                if let Some(w) = weak.upgrade() {
                    let result: Result<()> = $body(&w, &mut state.borrow_mut(), &storage, &sender);
                    if let Err(e) = result {
                        w.global::<AiState>().set_status(e.to_string().into());
                    }
                }
            });
        }};
    }
    macro_rules! bind_index {
        ($callback:ident,$body:expr) => {{
            let weak = w.as_weak();
            let state = Rc::clone(&data);
            let storage = Arc::clone(&storage);
            w.global::<AiState>().$callback(move |index| {
                if let Some(w) = weak.upgrade() {
                    let result: Result<()> = $body(&w, &mut state.borrow_mut(), &storage, index);
                    if let Err(e) = result {
                        w.global::<AiState>().set_status(e.to_string().into());
                    }
                }
            });
        }};
    }
    {
        let weak = w.as_weak();
        let state = Rc::clone(&data);
        let storage = Arc::clone(&storage);
        w.global::<AiState>().on_opened(move || {
            if let Some(w) = weak.upgrade() {
                let g = w.global::<AiState>();
                let t = timeline.borrow();
                g.set_range_start(time_text(t.range_started_utc_ms).into());
                g.set_range_end(time_text(t.range_ended_utc_ms).into());
                g.set_prepared(false);
                let result = lock(&storage).and_then(|s| reload(&w, &mut state.borrow_mut(), &s));
                if let Err(e) = result {
                    g.set_status(e.to_string().into());
                }
            }
        });
    }
    bind_index!(
        on_choose_provider,
        |w: &AppWindow, d: &mut UiData, _s: &Arc<Mutex<Storage>>, i: i32| {
            let p = d
                .profiles
                .get(i as usize)
                .cloned()
                .ok_or_else(|| anyhow!("请选择已保存提供商"))?;
            d.profile = p;
            show_profile(w, &d.profile);
            d.prepared = None;
            w.global::<AiState>().set_prepared(false);
            Ok(())
        }
    );
    bind_index!(
        on_choose_preset,
        |w: &AppWindow, d: &mut UiData, _s: &Arc<Mutex<Storage>>, i: i32| {
            d.profile = ProviderProfile::preset(i as usize, new_id());
            show_profile(w, &d.profile);
            w.global::<AiState>().set_provider_index(-1);
            d.prepared = None;
            Ok(())
        }
    );
    bind_index!(on_choose_model, |w: &AppWindow,
                                  d: &mut UiData,
                                  _s: &Arc<Mutex<Storage>>,
                                  i: i32| {
        let m = d
            .profile
            .models
            .get(i as usize)
            .cloned()
            .ok_or_else(|| anyhow!("请选择模型"))?;
        d.profile.model = m;
        show_model(w, &d.profile.model);
        d.prepared = None;
        w.global::<AiState>().set_prepared(false);
        Ok(())
    });
    bind!(
        on_save_provider,
        |w: &AppWindow, d: &mut UiData, s: &Arc<Mutex<Storage>>, _: &mpsc::Sender<AiCommand>| {
            save_profile(w, d, &*lock(s)?)?;
            w.global::<AiState>()
                .set_status("配置已保存；更改连接或模型后请重新测试".into());
            Ok(())
        }
    );
    bind!(
        on_test_provider,
        |w: &AppWindow,
         d: &mut UiData,
         s: &Arc<Mutex<Storage>>,
         sender: &mpsc::Sender<AiCommand>| {
            save_profile(w, d, &*lock(s)?)?;
            sender.send(AiCommand::Test(d.profile.clone()))?;
            w.global::<AiState>()
                .set_status("合成连接测试已排队…".into());
            Ok(())
        }
    );
    bind!(
        on_fetch_models,
        |w: &AppWindow,
         d: &mut UiData,
         _: &Arc<Mutex<Storage>>,
         sender: &mpsc::Sender<AiCommand>| {
            let empty = w.global::<AiState>().get_model_id().is_empty();
            if empty {
                w.global::<AiState>().set_model_id("model-catalog".into());
            }
            let mut p = read_profile(w, d)?;
            save_credentials(w, &p, d)?;
            p.credential_revision = d.profile.credential_revision;
            d.profile = p;
            sender.send(AiCommand::Models(d.profile.clone()))?;
            if empty {
                w.global::<AiState>().set_model_id("".into());
            }
            w.global::<AiState>().set_status("正在读取模型目录…".into());
            Ok(())
        }
    );
    bind!(
        on_delete_provider,
        |w: &AppWindow, d: &mut UiData, s: &Arc<Mutex<Storage>>, _: &mpsc::Sender<AiCommand>| {
            lock(s)?.delete_ai_profile(&d.profile.id)?;
            credentials::delete(&d.profile.id).map_err(anyhow::Error::msg)?;
            d.profile = ProviderProfile::preset(0, new_id());
            show_profile(w, &d.profile);
            reload(w, d, &*lock(s)?)?;
            w.global::<AiState>()
                .set_status("配置与凭据已删除，引用计划已暂停；历史总结保留".into());
            Ok(())
        }
    );
    bind_index!(
        on_choose_prompt,
        |w: &AppWindow, d: &mut UiData, _: &Arc<Mutex<Storage>>, i: i32| load_prompt(w, d, i)
    );
    bind!(
        on_save_prompt,
        |w: &AppWindow, d: &mut UiData, s: &Arc<Mutex<Storage>>, _: &mpsc::Sender<AiCommand>| {
            save_prompt(w, d, &*lock(s)?, false)?;
            Ok(())
        }
    );
    bind!(
        on_new_prompt,
        |w: &AppWindow, d: &mut UiData, s: &Arc<Mutex<Storage>>, _: &mpsc::Sender<AiCommand>| {
            save_prompt(w, d, &*lock(s)?, true)?;
            Ok(())
        }
    );
    bind!(
        on_reset_prompt,
        |w: &AppWindow, _: &mut UiData, _: &Arc<Mutex<Storage>>, _: &mpsc::Sender<AiCommand>| {
            w.global::<AiState>().set_prompt_text(DEFAULT_PROMPT.into());
            w.global::<AiState>()
                .set_status("已恢复官方正文，点击保存应用到此预设".into());
            Ok(())
        }
    );
    bind!(
        on_delete_prompt,
        |w: &AppWindow, d: &mut UiData, s: &Arc<Mutex<Storage>>, _: &mpsc::Sender<AiCommand>| {
            let s = lock(s)?;
            s.delete_ai_prompt(&d.prompt_id)?;
            reload(w, d, &s)?;
            if !d.prompts.is_empty() {
                load_prompt(w, d, 0)?;
            }
            w.global::<AiState>()
                .set_status("预设已删除，引用计划已暂停".into());
            Ok(())
        }
    );
    bind!(
        on_prepare,
        |w: &AppWindow, d: &mut UiData, s: &Arc<Mutex<Storage>>, _: &mpsc::Sender<AiCommand>| {
            let (start, end) = range(w)?;
            let s = lock(s)?;
            let p = s.ai_profile(&d.profile.id)?;
            if !p.is_tested() {
                bail!("请先保存并测试提供商与模型");
            }
            let prompt = s
                .ai_prompts()?
                .into_iter()
                .find(|p| p.id == d.prompt_id)
                .ok_or_else(|| anyhow!("请先保存提示词"))?;
            let env = s.ai_envelope(start, end)?;
            let chunks = s.ai_request_chunks(&env, &p, &prompt)?;
            let count = if chunks.is_empty() {
                1
            } else {
                chunks.len() + 1
            };
            let missing = env
                .missing
                .iter()
                .take(8)
                .map(|g| {
                    format!(
                        "{}–{} {}",
                        time_text(g.started_utc_ms),
                        time_text(g.ended_utc_ms),
                        missing_label(&g.reason)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            w.global::<AiState>().set_preflight(format!("覆盖率 {:.1}% · {} 个应用 · {} 段缺失\n发送类别：应用名称与聚合时长、窗口数、小时趋势、输入总数、完整自然日高频键及缺失。\n预计 {} 次请求{}；本次 {} 张逐张授权图片。\n{}\n{}",env.covered_ms as f64*100.0/(end-start) as f64,env.applications.len(),env.missing.len(),count,if chunks.is_empty(){""}else{"（超长汇总可能增加合并请求）"},d.consents.len(),if env.available||!d.consents.is_empty(){"有数据即可总结。"}else{"无数据，将保存任务记录且不调用 AI。"},missing).into());
            d.prepared = Some((start, end, p.revision(), serde_json::to_string(&prompt)?));
            w.global::<AiState>().set_prepared(true);
            Ok(())
        }
    );
    bind!(
        on_send_summary,
        |w: &AppWindow,
         d: &mut UiData,
         s: &Arc<Mutex<Storage>>,
         sender: &mpsc::Sender<AiCommand>| {
            let (start, end) = range(w)?;
            let s = lock(s)?;
            let p = s.ai_profile(&d.profile.id)?;
            let prompt = s
                .ai_prompts()?
                .into_iter()
                .find(|p| p.id == d.prompt_id)
                .ok_or_else(|| anyhow!("提示词不存在"))?;
            if d.prepared.as_ref()
                != Some(&(start, end, p.revision(), serde_json::to_string(&prompt)?))
            {
                bail!("范围或配置已改变，请重新预览发送内容");
            }
            let id = s.enqueue_ai_summary(
                &p.id,
                &prompt.id,
                start,
                end,
                &d.consents,
                crate::unix_time_ms(),
            )?;
            d.job = Some(id);
            d.version = None;
            d.consents.clear();
            d.prepared = None;
            w.global::<AiState>().set_prepared(false);
            w.global::<AiState>().set_image_count("本次没有图片".into());
            w.global::<AiState>().set_status("总结已排队".into());
            sender.send(AiCommand::Wake)?;
            reload(w, d, &s)?;
            Ok(())
        }
    );
    bind!(
        on_list_images,
        |w: &AppWindow, d: &mut UiData, s: &Arc<Mutex<Storage>>, _: &mpsc::Sender<AiCommand>| {
            let (start, end) = range(w)?;
            d.slots = lock(s)?
                .list_snapshot_slots(start, end, 1000)?
                .into_iter()
                .filter(|s| s.success)
                .collect();
            w.global::<AiState>()
                .set_image_options(strings(d.slots.iter().map(|s| {
                    format!(
                        "{} · {}×{}",
                        time_text(s.slot_started_utc_ms),
                        s.pixel_width,
                        s.pixel_height
                    )
                })));
            w.global::<AiState>()
                .set_image_index(if d.slots.is_empty() { -1 } else { 0 });
            w.global::<AiState>().set_image_previewed(false);
            d.preview = None;
            Ok(())
        }
    );
    bind!(
        on_preview_image,
        |w: &AppWindow, d: &mut UiData, s: &Arc<Mutex<Storage>>, _: &mpsc::Sender<AiCommand>| {
            let g = w.global::<AiState>();
            let slot = d
                .slots
                .get(g.get_image_index() as usize)
                .ok_or_else(|| anyhow!("请选择图片"))?;
            let s = lock(s)?;
            let image = s.load_snapshot_image(slot.id)?;
            let pixels =
                image::load_from_memory_with_format(&image.webp, image::ImageFormat::WebP)?
                    .to_rgba8();
            let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                pixels.as_raw(),
                pixels.width(),
                pixels.height(),
            );
            g.set_image_preview(slint::Image::from_rgba8(buffer));
            g.set_image_detail(
                format!(
                    "{} · {}×{} · 解密与完整性校验通过",
                    time_text(slot.slot_started_utc_ms),
                    slot.pixel_width,
                    slot.pixel_height
                )
                .into(),
            );
            d.preview = Some(SnapshotConsent {
                slot_id: slot.id,
                pixel_hash: s.snapshot_content_hash(slot.id)?,
            });
            g.set_image_previewed(true);
            Ok(())
        }
    );
    bind!(
        on_authorize_image,
        |w: &AppWindow, d: &mut UiData, _: &Arc<Mutex<Storage>>, _: &mpsc::Sender<AiCommand>| {
            let p = d.preview.take().ok_or_else(|| anyhow!("请先逐张预览"))?;
            if d.consents.len() >= 20 {
                bail!("每次最多授权 20 张图片");
            }
            if !d.consents.iter().any(|c| c.slot_id == p.slot_id) {
                d.consents.push(p);
            }
            w.global::<AiState>().set_image_previewed(false);
            w.global::<AiState>().set_image_count(
                format!("本次已授权 {} 张；授权仅用于一次请求", d.consents.len()).into(),
            );
            d.prepared = None;
            Ok(())
        }
    );
    bind!(
        on_clear_images,
        |w: &AppWindow, d: &mut UiData, _: &Arc<Mutex<Storage>>, _: &mpsc::Sender<AiCommand>| {
            d.consents.clear();
            d.preview = None;
            d.prepared = None;
            w.global::<AiState>().set_image_count("本次没有图片".into());
            w.global::<AiState>().set_prepared(false);
            Ok(())
        }
    );
    bind_index!(on_choose_job, |w: &AppWindow,
                                d: &mut UiData,
                                _: &Arc<Mutex<Storage>>,
                                i: i32| {
        d.job = d.jobs.get(i as usize).map(|j| j.id);
        d.version = None;
        show_current(w, d);
        Ok(())
    });
    bind_index!(
        on_choose_version,
        |w: &AppWindow, d: &mut UiData, s: &Arc<Mutex<Storage>>, i: i32| {
            d.version = d.versions.get(i as usize).map(|v| v.id);
            d.job = None;
            d.leaf = None;
            show_version(w, d, &*lock(s)?)?;
            Ok(())
        }
    );
    bind_index!(
        on_choose_branch,
        |w: &AppWindow, d: &mut UiData, s: &Arc<Mutex<Storage>>, i: i32| {
            d.leaf = *d
                .branches
                .get(i as usize)
                .ok_or_else(|| anyhow!("分支节点无效"))?;
            show_version(w, d, &*lock(s)?)?;
            Ok(())
        }
    );
    bind!(
        on_cancel_job,
        |w: &AppWindow, d: &mut UiData, s: &Arc<Mutex<Storage>>, _: &mpsc::Sender<AiCommand>| {
            let s = lock(s)?;
            let id = d
                .job
                .or_else(|| d.jobs.iter().find(|j| j.state == "running").map(|j| j.id))
                .ok_or_else(|| anyhow!("没有可取消任务"))?;
            s.cancel_ai_job(id, crate::unix_time_ms())?;
            w.global::<AiState>()
                .set_status("已请求取消 AI 工作".into());
            Ok(())
        }
    );
    bind!(
        on_regenerate,
        |w: &AppWindow, d: &mut UiData, s: &Arc<Mutex<Storage>>, _: &mpsc::Sender<AiCommand>| {
            let s = lock(s)?;
            let (start, end) = if let Some(id) = d.version {
                let v = s.ai_version(id)?;
                (
                    v.snapshot.envelope.started_utc_ms,
                    v.snapshot.envelope.ended_utc_ms,
                )
            } else if let Some(id) = d.job {
                let j = s.ai_job(id)?;
                (j.spec.envelope.started_utc_ms, j.spec.envelope.ended_utc_ms)
            } else {
                range(w)?
            };
            w.global::<AiState>()
                .set_range_start(time_text(start).into());
            w.global::<AiState>().set_range_end(time_text(end).into());
            d.prepared = None;
            d.consents.clear();
            w.global::<AiState>()
                .set_image_count("重新生成默认不附图，请重新预览并授权所需图片".into());
            w.global::<AiState>().set_prepared(false);
            w.global::<AiState>().set_page(0);
            w.global::<AiState>()
                .set_status("重新预览并发送后将建立新的成功版本与独立分支".into());
            Ok(())
        }
    );
    bind!(
        on_follow_up,
        |w: &AppWindow,
         d: &mut UiData,
         s: &Arc<Mutex<Storage>>,
         sender: &mpsc::Sender<AiCommand>| {
            let version = d.version.ok_or_else(|| anyhow!("请选择一个成功总结版本"))?;
            let id = lock(s)?.enqueue_ai_followup(
                version,
                d.leaf,
                &w.global::<AiState>().get_question(),
                crate::unix_time_ms(),
            )?;
            d.job = Some(id);
            w.global::<AiState>().set_question("".into());
            sender.send(AiCommand::Wake)?;
            w.global::<AiState>()
                .set_status("追问已排队；本次不重复发送图片".into());
            Ok(())
        }
    );
    bind!(
        on_pin_version,
        |w: &AppWindow, d: &mut UiData, s: &Arc<Mutex<Storage>>, _: &mpsc::Sender<AiCommand>| {
            let id = d.version.ok_or_else(|| anyhow!("请选择版本"))?;
            let s = lock(s)?;
            s.pin_ai_version(id, !s.ai_version(id)?.pinned)?;
            reload(w, d, &s)?;
            show_version(w, d, &s)?;
            Ok(())
        }
    );
    bind!(
        on_delete_version,
        |w: &AppWindow, d: &mut UiData, s: &Arc<Mutex<Storage>>, _: &mpsc::Sender<AiCommand>| {
            let id = d.version.ok_or_else(|| anyhow!("请选择版本"))?;
            let s = lock(s)?;
            s.delete_ai_version(id)?;
            d.version = None;
            d.job = None;
            d.leaf = None;
            reload(w, d, &s)?;
            w.global::<AiState>().set_conversation("版本已删除".into());
            Ok(())
        }
    );
    bind!(
        on_clear_conversation,
        |w: &AppWindow, d: &mut UiData, s: &Arc<Mutex<Storage>>, _: &mpsc::Sender<AiCommand>| {
            let id = d.version.ok_or_else(|| anyhow!("请选择版本"))?;
            let s = lock(s)?;
            s.clear_ai_conversation(id)?;
            d.leaf = None;
            show_version(w, d, &s)?;
            Ok(())
        }
    );
    bind!(
        on_clear_context,
        |w: &AppWindow, d: &mut UiData, s: &Arc<Mutex<Storage>>, _: &mpsc::Sender<AiCommand>| {
            let id = d.version.ok_or_else(|| anyhow!("请选择版本"))?;
            let s = lock(s)?;
            d.leaf = Some(s.mark_ai_context_cleared(id, d.leaf, crate::unix_time_ms())?);
            show_version(w, d, &s)?;
            w.global::<AiState>()
                .set_status("已放置上下文清除标记；之前的消息仍保留在本地".into());
            Ok(())
        }
    );
    bind!(
        on_save_daily,
        |w: &AppWindow,
         d: &mut UiData,
         s: &Arc<Mutex<Storage>>,
         sender: &mpsc::Sender<AiCommand>| {
            save_schedule(w, d, &*lock(s)?, true)?;
            sender.send(AiCommand::Wake)?;
            Ok(())
        }
    );
    bind!(
        on_save_interval,
        |w: &AppWindow,
         d: &mut UiData,
         s: &Arc<Mutex<Storage>>,
         sender: &mpsc::Sender<AiCommand>| {
            save_schedule(w, d, &*lock(s)?, false)?;
            sender.send(AiCommand::Wake)?;
            Ok(())
        }
    );
    bind!(
        on_save_settings,
        |w: &AppWindow, _: &mut UiData, s: &Arc<Mutex<Storage>>, _: &mpsc::Sender<AiCommand>| {
            let g = w.global::<AiState>();
            let settings = AiSettings {
                days: optional(&g.get_retention_days(), "保留天数")?,
                max_bytes: parse::<u64>(&g.get_retention_mib(), "空间上限")?
                    .checked_mul(1024 * 1024)
                    .ok_or_else(|| anyhow!("空间上限过大"))?,
                versions_to_keep: parse(&g.get_version_limit(), "版本数")?,
                compression_enabled: g.get_compression(),
                recent_messages: optional(&g.get_recent_messages(), "消息数")?,
                notify_success: g.get_notify_success(),
                notify_failure: g.get_notify_failure(),
            };
            lock(s)?.set_ai_settings(&settings)?;
            g.set_status("AI 设置已保存".into());
            Ok(())
        }
    );
    let weak = w.as_weak();
    let timer = Timer::default();
    let mut refresh = Instant::now() - Duration::from_secs(2);
    let mut generation = data_generation();
    timer.start(TimerMode::Repeated, Duration::from_millis(200), move || {
        let Some(w) = weak.upgrade() else {
            return;
        };
        let g = w.global::<AiState>();
        if data_generation() != generation
            && let Ok(s) = storage.try_lock()
        {
            match reset_dataset_ui(&w, &mut data.borrow_mut(), &s) {
                Ok(()) => {
                    generation = data_generation();
                    while service.receiver.try_recv().is_ok() {}
                }
                Err(e) => g.set_status(e.to_string().into()),
            }
        }
        let mut changed = false;
        while let Ok(event) = service.receiver.try_recv() {
            match event {
                AiEvent::Status(message) => g.set_status(message.into()),
                AiEvent::Tested(id, success) => {
                    changed = true;
                    if data.borrow().profile.id == id && success {
                        if let Ok(s) = storage.try_lock()
                            && let Ok(p) = s.ai_profile(&id)
                        {
                            data.borrow_mut().profile = p;
                        }
                        g.set_status("连接已验证，可以发送总结".into());
                    }
                }
                AiEvent::Models(id, models) => {
                    let mut d = data.borrow_mut();
                    if d.profile.id == id {
                        d.profile.models = models;
                        g.set_models(strings(d.profile.models.iter().map(model_label)));
                        g.set_status(
                            format!(
                                "已读取 {} 个模型；能力未知时可手动覆盖",
                                d.profile.models.len()
                            )
                            .into(),
                        );
                    }
                }
                AiEvent::Changed => changed = true,
                AiEvent::Finished(id, success) => {
                    changed = true;
                    if let Ok(s) = storage.try_lock() {
                        if let Ok(settings) = s.ai_settings()
                            && ((success && settings.notify_success)
                                || (!success && settings.notify_failure))
                        {
                            crate::tray::notify(id, success);
                        }
                        let mut d = data.borrow_mut();
                        if let Ok(job) = s.ai_job(id) {
                            if success {
                                g.set_status("已完成，回答和用量已加密保存。".into());
                                let version = match job.spec.kind {
                                    timelens_storage::AiJobKind::Summary => s
                                        .ai_versions()
                                        .ok()
                                        .and_then(|v| v.into_iter().find(|v| v.job_id == id))
                                        .map(|v| v.id),
                                    timelens_storage::AiJobKind::FollowUp {
                                        version_id, ..
                                    } => Some(version_id),
                                };
                                if d.job == Some(id) {
                                    d.version = version;
                                    d.job = None;
                                    d.leaf = version
                                        .and_then(|v| s.ai_messages(v).ok())
                                        .and_then(|m| m.last().map(|m| m.id));
                                }
                            } else {
                                g.set_status(format!("任务 {}", job.state).into());
                            }
                        }
                    }
                }
            }
        }
        if g.get_open()
            && (changed || refresh.elapsed() >= Duration::from_secs(1))
            && let Ok(s) = storage.try_lock()
        {
            let mut d = data.borrow_mut();
            if let Err(e) = reload(&w, &mut d, &s) {
                g.set_status(e.to_string().into());
            }
            if d.job.is_some() {
                show_current(&w, &d);
            } else if d.version.is_some() {
                let _ = show_version(&w, &mut d, &s);
            }
            refresh = Instant::now();
        }
    });
    Ok(timer)
}

fn read_profile(w: &AppWindow, d: &UiData) -> Result<ProviderProfile> {
    let g = w.global::<AiState>();
    let mut p = d.profile.clone();
    p.name = g.get_provider_name().to_string();
    p.base_url = g.get_endpoint().trim().into();
    p.protocol = match g.get_protocol_index() {
        1 => Protocol::Anthropic,
        2 => Protocol::Gemini,
        _ => Protocol::OpenAi,
    };
    p.model = Model {
        id: g.get_model_id().trim().into(),
        vision: cap(g.get_vision()),
        reasoning: cap(g.get_reasoning()),
        tools: cap(g.get_tools()),
        context_tokens: parse(&g.get_context_tokens(), "上下文")?,
        supports_temperature: g.get_temperature_supported(),
        supports_reasoning_effort: g.get_effort_supported(),
    };
    p.parameters = Parameters {
        temperature: optional(&g.get_temperature(), "温度")?,
        max_output_tokens: optional(&g.get_max_output(), "最大输出")?,
        reasoning_effort: match g.get_effort() {
            1 => Some(ReasoningEffort::Low),
            2 => Some(ReasoningEffort::Medium),
            3 => Some(ReasoningEffort::High),
            _ => None,
        },
    };
    p.retries = parse(&g.get_retries(), "重试次数")?;
    p.idle_timeout_seconds = optional(&g.get_idle_timeout(), "无响应超时")?;
    p.allow_local_http = g.get_local_http();
    p.azure = g.get_azure();
    p.validate().map_err(anyhow::Error::msg)?;
    Ok(p)
}
fn save_credentials(w: &AppWindow, p: &ProviderProfile, d: &mut UiData) -> Result<()> {
    let g = w.global::<AiState>();
    if g.get_replace_credentials()
        || !g.get_api_key().is_empty()
        || !g.get_secret_headers().is_empty()
        || p.credential_revision == 0
    {
        let mut headers = BTreeMap::new();
        for field in g
            .get_secret_headers()
            .split(';')
            .filter(|v| !v.trim().is_empty())
        {
            let (k, v) = field
                .split_once('=')
                .ok_or_else(|| anyhow!("秘密头格式为 Name=value"))?;
            headers.insert(k.trim().into(), v.trim().into());
        }
        let secrets = credentials::Secrets {
            api_key: g.get_api_key().to_string(),
            headers,
        };
        credentials::save(&p.id, &secrets).map_err(anyhow::Error::msg)?;
        d.profile.credential_revision = p.credential_revision.saturating_add(1);
        g.set_api_key("".into());
        g.set_secret_headers("".into());
        g.set_replace_credentials(false);
    }
    Ok(())
}
fn save_profile(w: &AppWindow, d: &mut UiData, s: &Storage) -> Result<()> {
    let mut p = read_profile(w, d)?;
    save_credentials(w, &p, d)?;
    p.credential_revision = d.profile.credential_revision;
    if let Some(m) = p.models.iter_mut().find(|m| m.id == p.model.id) {
        *m = p.model.clone();
    } else {
        p.models.push(p.model.clone());
    }
    s.save_ai_profile(&p)?;
    d.profile = p;
    d.prepared = None;
    w.global::<AiState>().set_prepared(false);
    reload(w, d, s)
}
fn show_model(w: &AppWindow, m: &Model) {
    let g = w.global::<AiState>();
    g.set_model_id(m.id.clone().into());
    g.set_vision(cap_index(m.vision));
    g.set_reasoning(cap_index(m.reasoning));
    g.set_tools(cap_index(m.tools));
    g.set_context_tokens(m.context_tokens.to_string().into());
    g.set_temperature_supported(m.supports_temperature);
    g.set_effort_supported(m.supports_reasoning_effort);
}
fn model_label(m: &Model) -> String {
    format!(
        "{} {}",
        match m.vision {
            Capability::Supported => "◉",
            Capability::Unsupported => "文本",
            Capability::Unknown => "?",
        },
        m.id
    )
}
fn show_profile(w: &AppWindow, p: &ProviderProfile) {
    let g = w.global::<AiState>();
    g.set_provider_name(p.name.clone().into());
    g.set_endpoint(p.base_url.clone().into());
    g.set_protocol_index(match p.protocol {
        Protocol::OpenAi => 0,
        Protocol::Anthropic => 1,
        Protocol::Gemini => 2,
    });
    show_model(w, &p.model);
    g.set_models(strings(p.models.iter().map(model_label)));
    g.set_model_index(
        p.models
            .iter()
            .position(|m| m.id == p.model.id)
            .map_or(-1, |i| i as i32),
    );
    g.set_retries(p.retries.to_string().into());
    g.set_max_output(
        p.parameters
            .max_output_tokens
            .map_or("4096".into(), |v| v.to_string())
            .into(),
    );
    g.set_temperature(
        p.parameters
            .temperature
            .map_or(String::new(), |v| v.to_string())
            .into(),
    );
    g.set_effort(match p.parameters.reasoning_effort {
        None => 0,
        Some(ReasoningEffort::Low) => 1,
        Some(ReasoningEffort::Medium) => 2,
        Some(ReasoningEffort::High) => 3,
    });
    g.set_idle_timeout(
        p.idle_timeout_seconds
            .map_or(String::new(), |v| v.to_string())
            .into(),
    );
    g.set_local_http(p.allow_local_http);
    g.set_azure(p.azure);
    g.set_api_key("".into());
    g.set_secret_headers("".into());
}
fn reload(w: &AppWindow, d: &mut UiData, s: &Storage) -> Result<()> {
    let g = w.global::<AiState>();
    d.profiles = s.ai_profiles()?;
    d.prompts = s.ai_prompts()?;
    d.jobs = s.ai_jobs(200)?;
    d.versions = s.ai_versions()?;
    if d.job.is_some_and(|id| !d.jobs.iter().any(|j| j.id == id)) {
        d.job = None;
    }
    if d.version
        .is_some_and(|id| !d.versions.iter().any(|v| v.id == id))
    {
        d.version = None;
        d.leaf = None;
        g.set_conversation("".into());
        g.set_exposed_reasoning("".into());
        g.set_source_label("".into());
    }
    if d.version.is_none() && d.job.is_none() {
        d.version = d.versions.first().map(|v| v.id);
    }
    g.set_providers(strings(d.profiles.iter().map(|p| {
        format!(
            "{} · {}",
            p.name,
            if p.is_tested() {
                "已测试"
            } else {
                "待测试"
            }
        )
    })));
    g.set_prompts(strings(d.prompts.iter().map(|p| p.name.clone())));
    g.set_provider_index(
        d.profiles
            .iter()
            .position(|p| p.id == d.profile.id)
            .map_or(-1, |i| i as i32),
    );
    g.set_jobs(strings(d.jobs.iter().map(|j| {
        format!(
            "#{} · {} · {}",
            j.id,
            state_label(&j.state),
            time_text(j.created_utc_ms)
        )
    })));
    g.set_job_index(
        d.jobs
            .iter()
            .position(|j| Some(j.id) == d.job)
            .map_or(-1, |i| i as i32),
    );
    g.set_versions(strings(d.versions.iter().map(|v| {
        format!(
            "{}版本 {} · {}",
            if v.pinned { "★ " } else { "" },
            v.id,
            time_text(v.created_utc_ms)
        )
    })));
    g.set_version_index(
        d.versions
            .iter()
            .position(|v| Some(v.id) == d.version)
            .map_or(-1, |i| i as i32),
    );
    Ok(())
}
fn reset_dataset_ui(w: &AppWindow, d: &mut UiData, s: &Storage) -> Result<()> {
    *d = UiData::default();
    let g = w.global::<AiState>();
    g.set_prepared(false);
    g.set_question("".into());
    g.set_conversation("".into());
    g.set_exposed_reasoning("".into());
    g.set_source_label("".into());
    g.set_token_label("Token 用量尚不可用".into());
    g.set_image_preview(slint::Image::default());
    g.set_image_previewed(false);
    g.set_image_detail("".into());
    g.set_image_count("本次没有图片".into());
    g.set_image_options(strings([]));
    g.set_image_index(-1);
    g.set_branches(strings(["初始总结".into()]));
    g.set_branch_index(0);
    reload(w, d, s)?;
    if let Some(p) = d.profiles.first().cloned() {
        d.profile = p;
        g.set_provider_index(0);
    }
    show_profile(w, &d.profile);
    if !d.prompts.is_empty() {
        load_prompt(w, d, 0)?;
    }
    show_settings(w, s)?;
    if let Some(id) = d.version {
        d.leaf = s.ai_messages(id)?.last().map(|m| m.id);
        show_version(w, d, s)?;
    }
    g.set_status(
        if d.profile.is_tested() {
            "提供商已测试。总结与分支均保存在本机。"
        } else {
            "请在提供商页配置并测试连接。计划默认关闭。"
        }
        .into(),
    );
    Ok(())
}
fn load_prompt(w: &AppWindow, d: &mut UiData, index: i32) -> Result<()> {
    let p = d
        .prompts
        .get(index as usize)
        .ok_or_else(|| anyhow!("请选择提示词预设"))?;
    d.prompt_id = p.id.clone();
    let g = w.global::<AiState>();
    g.set_prompt_name(p.name.clone().into());
    g.set_prompt_text(p.text.clone().into());
    g.set_language(p.language.clone().into());
    g.set_prompt_index(index);
    g.set_prepared(false);
    d.prepared = None;
    Ok(())
}
fn save_prompt(w: &AppWindow, d: &mut UiData, s: &Storage, new: bool) -> Result<()> {
    let g = w.global::<AiState>();
    let id = if new { new_id() } else { d.prompt_id.clone() };
    s.save_ai_prompt(&PromptPreset {
        id: id.clone(),
        name: g.get_prompt_name().to_string(),
        text: g.get_prompt_text().to_string(),
        language: g.get_language().to_string(),
        version: d
            .prompts
            .iter()
            .find(|p| p.id == id)
            .map_or(1, |p| p.version + 1),
    })?;
    d.prompt_id = id;
    reload(w, d, s)?;
    g.set_prompt_index(
        d.prompts
            .iter()
            .position(|p| p.id == d.prompt_id)
            .unwrap_or(0) as i32,
    );
    g.set_status("提示词已保存，仅影响未来任务".into());
    d.prepared = None;
    g.set_prepared(false);
    Ok(())
}
fn show_current(w: &AppWindow, d: &UiData) {
    if let Some(j) = d.jobs.iter().find(|j| Some(j.id) == d.job) {
        let g = w.global::<AiState>();
        g.set_conversation(
            format!(
                "任务 #{} · {}\n\n{}{}",
                j.id,
                state_label(&j.state),
                j.body,
                j.error.as_ref().map_or(String::new(), |e| format!(
                    "\n\n{}{}",
                    e.status.map_or(String::new(), |s| format!("HTTP {s} · ")),
                    e.message
                ))
            )
            .into(),
        );
        g.set_exposed_reasoning(j.reasoning.clone().into());
        g.set_token_label(usage_label(&j.usage).into());
    }
}
fn show_version(w: &AppWindow, d: &mut UiData, s: &Storage) -> Result<()> {
    let Some(id) = d.version else {
        return Ok(());
    };
    let v = s.ai_version(id)?;
    let messages = s.ai_messages(id)?;
    let mut conversation = format!("{}\n\n{}", v.answer, usage_label(&v.usage));
    let mut reasoning = v.reasoning.clone();
    let mut usage = v.usage.clone();
    for m in s.ai_branch(id, d.leaf)? {
        if m.role == "clear_context" {
            conversation.push_str("\n\n[清除上下文标记]");
            continue;
        }
        conversation.push_str(&format!(
            "\n\n{}\n{}",
            if m.role == "user" { "你" } else { "AI" },
            m.body
        ));
        if !m.reasoning.is_empty() {
            reasoning.push_str(&format!("\n\n{}", m.reasoning));
        }
        if m.role == "assistant" {
            conversation.push_str(&format!("\n{}", usage_label(&m.usage)));
        }
        if let Some(error) = m.error {
            conversation.push_str(&format!("\n错误：{}", error.message));
        }
        usage.add(&m.usage);
    }
    let g = w.global::<AiState>();
    g.set_conversation(conversation.into());
    g.set_exposed_reasoning(reasoning.into());
    g.set_token_label(usage_label(&usage).into());
    g.set_pinned(v.pinned);
    g.set_source_label(
        if s.ai_source_available(id)? {
            "原始活动仍可用 · 本次上下文来自保存的过滤快照"
        } else {
            "原始来源已清理或不可用 · 追问使用保存的 AI 上下文快照"
        }
        .into(),
    );
    d.branches = std::iter::once(None)
        .chain(messages.iter().map(|m| Some(m.id)))
        .collect();
    g.set_branches(strings(std::iter::once("初始总结".into()).chain(
        messages.iter().map(|m| {
            format!(
                "#{} {} · {}",
                m.id,
                if m.role == "user" {
                    "用户"
                } else if m.role == "assistant" {
                    "AI"
                } else {
                    "清除标记"
                },
                m.body.chars().take(26).collect::<String>()
            )
        }),
    )));
    g.set_branch_index(d.branches.iter().position(|p| *p == d.leaf).unwrap_or(0) as i32);
    Ok(())
}
fn state_label(s: &str) -> &str {
    match s {
        "queued" => "等待",
        "running" => "运行",
        "completed" => "完成",
        "no_data" => "无数据，未调用 AI",
        "missed" => "已错过",
        "failed" => "失败",
        "canceled" => "已取消",
        _ => s,
    }
}
fn missing_label(s: &str) -> &str {
    match s {
        "unobserved" => "尚未监控",
        "recovery_corruption" => "恢复时无法读取",
        "retention" | "retention_age" => "超过保留期限",
        "disk_pressure" | "retention_size" => "空间清理",
        "global_pause" => "全局暂停",
        "privacy_exclusion" => "隐私排除",
        "collector_restart" => "采集器重启中断",
        "buffer_overflow" => "离线缓冲已满",
        "user_delete" => "主动删除",
        _ => s,
    }
}
fn usage_label(u: &TokenUsage) -> String {
    let n = |v: Option<u64>| v.map_or("—".into(), |v| v.to_string());
    format!(
        "Token · 输入 {} · 输出 {} · 缓存 {} · 推理 {}{}",
        n(u.input),
        n(u.output),
        n(u.cached),
        n(u.reasoning),
        u.cost.as_ref().map_or(String::new(), |c| format!(" · {c}"))
    )
}
fn show_settings(w: &AppWindow, s: &Storage) -> Result<()> {
    let g = w.global::<AiState>();
    let p = s.ai_settings()?;
    g.set_compression(p.compression_enabled);
    g.set_recent_messages(
        p.recent_messages
            .map_or(String::new(), |v| v.to_string())
            .into(),
    );
    g.set_retention_days(p.days.map_or(String::new(), |v| v.to_string()).into());
    g.set_retention_mib((p.max_bytes / 1024 / 1024).to_string().into());
    g.set_version_limit(p.versions_to_keep.to_string().into());
    g.set_notify_success(p.notify_success);
    g.set_notify_failure(p.notify_failure);
    for schedule in s.ai_schedules()? {
        match schedule.kind {
            ScheduleKind::Daily { hour, minute } => {
                g.set_daily(schedule.enabled);
                g.set_daily_time(format!("{hour:02}:{minute:02}").into());
                g.set_daily_retries(
                    schedule
                        .retries
                        .map_or(String::new(), |v| v.to_string())
                        .into(),
                );
            }
            ScheduleKind::Interval { hours, .. } => {
                g.set_interval(schedule.enabled);
                g.set_interval_hours(hours.to_string().into());
                g.set_interval_retries(
                    schedule
                        .retries
                        .map_or(String::new(), |v| v.to_string())
                        .into(),
                );
            }
        }
    }
    Ok(())
}
fn save_schedule(w: &AppWindow, d: &UiData, s: &Storage, daily: bool) -> Result<()> {
    let g = w.global::<AiState>();
    let id = if daily { "daily" } else { "interval" };
    let enabled = if daily {
        g.get_daily()
    } else {
        g.get_interval()
    };
    let kind = if daily {
        let time = g.get_daily_time();
        let (h, m) = time
            .split_once(':')
            .ok_or_else(|| anyhow!("每日时间格式为 HH:MM"))?;
        ScheduleKind::Daily {
            hour: parse(h, "小时")?,
            minute: parse(m, "分钟")?,
        }
    } else {
        ScheduleKind::Interval {
            hours: parse(&g.get_interval_hours(), "间隔小时")?,
            anchor_utc_ms: 0,
        }
    };
    let p = s.ai_profile(&d.profile.id)?;
    let old = s.ai_schedules()?.into_iter().find(|v| v.id == id);
    let mut schedule = Schedule {
        id: id.into(),
        enabled,
        provider_id: p.id,
        model_id: p.model.id,
        prompt_id: d.prompt_id.clone(),
        kind,
        next_due_utc_ms: 0,
        retries: optional(
            &if daily {
                g.get_daily_retries()
            } else {
                g.get_interval_retries()
            },
            "计划重试",
        )?,
        paused_reason: None,
    };
    if enabled {
        let preserve = old.as_ref().is_some_and(|o| {
            o.enabled
                && match (&o.kind, &schedule.kind) {
                    (
                        ScheduleKind::Daily { hour: a, minute: b },
                        ScheduleKind::Daily { hour: c, minute: d },
                    ) => a == c && b == d,
                    (
                        ScheduleKind::Interval { hours: a, .. },
                        ScheduleKind::Interval { hours: b, .. },
                    ) => a == b,
                    _ => false,
                }
        });
        if preserve {
            let old = old.unwrap();
            schedule.next_due_utc_ms = old.next_due_utc_ms;
            schedule.kind = old.kind;
        } else {
            schedule
                .enable(crate::unix_time_ms(), &Local)
                .map_err(anyhow::Error::msg)?;
        }
    }
    s.save_ai_schedule(&schedule)?;
    g.set_schedule_status(
        format!(
            "{}计划{} · 提供商 {} · 模型 {} · 提示词 {}{}",
            if daily { "每日" } else { "间隔" },
            if enabled { "已启用" } else { "已关闭" },
            schedule.provider_id,
            schedule.model_id,
            schedule.prompt_id,
            if enabled {
                format!(" · 下次 {}", time_text(schedule.next_due_utc_ms))
            } else {
                String::new()
            }
        )
        .into(),
    );
    g.set_status("计划已保存".into());
    Ok(())
}
