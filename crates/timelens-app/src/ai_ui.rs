use crate::{
    AiConnectionRow, AiHistoryRow, AiState, AppWindow, UiState,
    ai::{AiCommand, AiEvent, AiService},
};
use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, Datelike, Local, NaiveDate, TimeZone, Timelike};
use slint::{ComponentHandle, Model as _, ModelRc, SharedString, Timer, TimerMode, VecModel};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};
use timelens_ai::{
    schedule::{Schedule, ScheduleKind},
    *,
};
use timelens_storage::{
    AiJob, AiJobKind, AiSettings, AiVersion, SnapshotConsent, SnapshotSlot, Storage,
};
use zeroize::Zeroize;

static DATA_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub fn data_generation() -> u64 {
    DATA_GENERATION.load(std::sync::atomic::Ordering::Acquire)
}
pub fn dataset_changed() {
    DATA_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Release);
    crate::snapshot::wake_scheduler();
}

type Store = Arc<Mutex<Storage>>;
type Commands = mpsc::Sender<AiCommand>;

/// How long the preview waits for further changes before it is worked out.
const PREVIEW_DELAY: Duration = Duration::from_millis(250);
/// The most snapshots one summary may carry.
const MAX_IMAGES: usize = 20;
const WEEKDAYS: [&str; 7] = ["周一", "周二", "周三", "周四", "周五", "周六", "周日"];

/// A row of the summary history: a summary version, or a summary job that has
/// not produced one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    Version(i64),
    Job(i64),
}

/// Something that stops a summary from being sent, with what clears it:
/// 1 the connection, 2 the templates, 3 removing the chosen snapshots.
#[derive(Debug)]
struct Blocked {
    message: String,
    fix: i32,
}
impl std::fmt::Display for Blocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for Blocked {}
fn blocked(message: impl Into<String>, fix: i32) -> anyhow::Error {
    Blocked {
        message: message.into(),
        fix,
    }
    .into()
}

struct UiData {
    /// The connection the settings form edits: as saved, plus fetched models.
    profile: ProviderProfile,
    profiles: Vec<ProviderProfile>,
    prompts: Vec<PromptPreset>,
    /// The template the settings edit.
    prompt_id: String,
    /// The connection and template a new summary uses.
    use_profile: Option<String>,
    use_prompt: String,
    /// The range a new summary covers.
    range: Option<(i64, i64)>,
    /// What the preview checked: range, connection revision and template.
    prepared: Option<(i64, i64, String, String)>,
    preview_due: Option<Instant>,
    /// The preview being worked out, by the change count it started at.
    preview_job: Option<(u64, mpsc::Receiver<Result<Preview>>)>,
    /// Counts changes to what a new summary would send.
    preview_seq: u64,
    history: Vec<Target>,
    /// Each version's number within its range.
    numbers: BTreeMap<i64, usize>,
    /// Follow-ups under way, as (job, version).
    followups: Vec<(i64, i64)>,
    /// The summary on screen: a version, with the follow-up under way on it,
    /// or a job.
    version: Option<i64>,
    job: Option<i64>,
    followup: Option<i64>,
    leaf: Option<i64>,
    branches: Vec<Option<i64>>,
    /// Whether the activity behind a version is still recorded.
    source: Option<(i64, bool)>,
    slots: Vec<SnapshotSlot>,
    preview: Option<SnapshotConsent>,
    /// Snapshots added to the next summary, each with its slot time.
    picked: Vec<(SnapshotConsent, i64)>,
    /// The version on screen when a delete or clear confirmation opened.
    confirm_version: Option<i64>,
}
impl Default for UiData {
    fn default() -> Self {
        Self {
            profile: ProviderProfile::preset(0, new_id()),
            profiles: vec![],
            prompts: vec![],
            prompt_id: "default".into(),
            use_profile: None,
            use_prompt: "default".into(),
            range: None,
            prepared: None,
            preview_due: None,
            preview_job: None,
            preview_seq: 0,
            history: vec![],
            numbers: BTreeMap::new(),
            followups: vec![],
            version: None,
            job: None,
            followup: None,
            leaf: None,
            branches: vec![None],
            source: None,
            slots: vec![],
            preview: None,
            picked: vec![],
            confirm_version: None,
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
/// Whether a list already shows these values. Replacing a ComboBox model resets
/// its selection, so lists are only replaced when their content changes.
fn same(current: &ModelRc<SharedString>, values: &[String]) -> bool {
    current.row_count() == values.len()
        && values
            .iter()
            .enumerate()
            .all(|(i, v)| current.row_data(i).is_some_and(|c| c.as_str() == v))
}
fn lock(storage: &Store) -> Result<std::sync::MutexGuard<'_, Storage>> {
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

fn at<Tz: TimeZone>(zone: &Tz, ms: i64) -> Option<DateTime<Tz>> {
    DateTime::from_timestamp_millis(ms).map(|t| t.with_timezone(zone))
}
fn date_text(day: NaiveDate, today: NaiveDate, weekday: bool) -> String {
    let year = if day.year() == today.year() {
        String::new()
    } else {
        format!("{}年", day.year())
    };
    let weekday = if weekday {
        format!(
            " {}",
            WEEKDAYS[day.weekday().num_days_from_monday() as usize]
        )
    } else {
        String::new()
    };
    format!("{year}{}月{}日{weekday}", day.month(), day.day())
}
/// A readable local span such as "10月15日 周四 00:00–14:32" or "10月14日 周三 全天".
/// `brief` drops the weekday and names today and yesterday.
pub(crate) fn range_text<Tz: TimeZone>(
    start: i64,
    end: i64,
    now: i64,
    zone: &Tz,
    brief: bool,
) -> String {
    let (Some(s), Some(e), Some(n)) = (at(zone, start), at(zone, end), at(zone, now)) else {
        return String::new();
    };
    let today = n.date_naive();
    let day = |d: NaiveDate, named: bool| {
        if brief && named && d == today {
            "今天".to_owned()
        } else if brief && named && Some(d) == today.pred_opt() {
            "昨天".to_owned()
        } else {
            date_text(d, today, !brief)
        }
    };
    let midnight = |t: &DateTime<Tz>| t.hour() == 0 && t.minute() == 0 && t.second() == 0;
    let clock = |t: &DateTime<Tz>| format!("{:02}:{:02}", t.hour(), t.minute());
    let first = s.date_naive();
    // An end at midnight closes the day before it.
    let last = if midnight(&e) {
        e.date_naive().pred_opt().unwrap_or(first).max(first)
    } else {
        e.date_naive()
    };
    if midnight(&s) && midnight(&e) {
        return if first == last {
            format!("{} 全天", day(first, true))
        } else {
            format!("{} – {}", day(first, false), day(last, false))
        };
    }
    if first == last {
        let end = if midnight(&e) {
            "24:00".to_owned()
        } else {
            clock(&e)
        };
        format!("{} {}–{end}", day(first, true), clock(&s))
    } else {
        format!(
            "{} {} – {} {}",
            day(first, false),
            clock(&s),
            day(e.date_naive(), false),
            clock(&e)
        )
    }
}
fn duration_text(ms: i64) -> String {
    let minutes = ms.max(0) / 60_000;
    let parts = [
        (minutes / 1440, "天"),
        (minutes % 1440 / 60, "小时"),
        (minutes % 60, "分"),
    ]
    .into_iter()
    .filter(|(n, _)| *n > 0)
    .map(|(n, unit)| format!("{n} {unit}"))
    .collect::<Vec<_>>();
    if parts.is_empty() {
        "共不到 1 分钟".into()
    } else {
        format!("共 {}", parts.join(" "))
    }
}
/// A moment such as "今天 18:05", "明天 22:00" or "10月12日 09:30".
fn when_text(ms: i64, now: i64) -> String {
    let (Some(t), Some(n)) = (at(&Local, ms), at(&Local, now)) else {
        return String::new();
    };
    let (day, today) = (t.date_naive(), n.date_naive());
    let name = if day == today {
        "今天".to_owned()
    } else if Some(day) == today.succ_opt() {
        "明天".to_owned()
    } else if Some(day) == today.pred_opt() {
        "昨天".to_owned()
    } else {
        date_text(day, today, false)
    };
    format!("{name} {:02}:{:02}", t.hour(), t.minute())
}
fn grouped(value: u64) -> String {
    let digits = value.to_string();
    let mut text = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            text.push(',');
        }
        text.push(c);
    }
    text
}
fn excerpt(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.is_empty() {
        return "（没有内容）".into();
    }
    let mut short = line.chars().take(18).collect::<String>();
    if line.chars().count() > 18 {
        short.push('…');
    }
    short
}

/// Keeps the AI page running; on exit, a connection never saved leaves no
/// key behind.
pub struct Installed {
    _timer: Timer,
    data: Rc<RefCell<UiData>>,
}
impl Drop for Installed {
    fn drop(&mut self) {
        if let Ok(d) = self.data.try_borrow() {
            forget_unsaved_credentials(&d);
        }
    }
}

pub fn install(
    w: &AppWindow,
    storage: Store,
    timeline: Rc<RefCell<UiState>>,
    service: AiService,
) -> Result<Installed> {
    let data = Rc::new(RefCell::new(UiData::default()));
    let sender = service.sender;
    w.global::<AiState>()
        .on_markdown(|text| crate::markdown::model(&text));
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
        let timeline = Rc::clone(&timeline);
        w.global::<AiState>().on_opened(move || {
            if let Some(w) = weak.upgrade() {
                let range = {
                    let t = timeline.borrow();
                    (t.range_started_utc_ms, t.range_ended_utc_ms)
                };
                let result =
                    lock(&storage).and_then(|s| opened(&w, &mut state.borrow_mut(), &s, range));
                if let Err(e) = result {
                    w.global::<AiState>().set_status(e.to_string().into());
                }
            }
        });
    }
    {
        let weak = w.as_weak();
        let state = Rc::clone(&data);
        let storage = Arc::clone(&storage);
        w.global::<AiState>().on_regenerate(move || {
            if let Some(w) = weak.upgrade() {
                let result = regenerate(&w, &mut state.borrow_mut(), &storage, &timeline);
                if let Err(e) = result {
                    w.global::<AiState>().set_status(e.to_string().into());
                }
            }
        });
    }

    // ───── Connections ─────
    bind_index!(
        on_choose_provider,
        |w: &AppWindow, d: &mut UiData, _: &Store, i: i32| {
            let p = d
                .profiles
                .get(i as usize)
                .cloned()
                .ok_or_else(|| anyhow!("请选择一个已保存的连接"))?;
            edit_profile(w, d, p, i);
            Ok(())
        }
    );
    bind_index!(
        on_choose_preset,
        |w: &AppWindow, d: &mut UiData, _: &Store, i: i32| {
            w.global::<AiState>().set_preset_index(i);
            edit_profile(w, d, ProviderProfile::preset(i as usize, new_id()), -1);
            Ok(())
        }
    );
    bind_index!(on_choose_model, |w: &AppWindow,
                                  d: &mut UiData,
                                  _: &Store,
                                  i: i32| {
        let m = d
            .profile
            .models
            .get(i as usize)
            .cloned()
            .ok_or_else(|| anyhow!("请选择模型"))?;
        d.profile.model = m;
        show_model(w, &d.profile.model);
        w.global::<AiState>().set_model_index(i);
        Ok(())
    });
    bind!(
        on_save_provider,
        |w: &AppWindow, d: &mut UiData, s: &Store, _: &Commands| {
            if let Err(e) = save_profile(w, d, &*lock(s)?) {
                show_form_error(w, &e);
                return Err(e);
            }
            w.global::<AiState>().set_status("连接已保存".into());
            Ok(())
        }
    );
    bind!(
        on_test_provider,
        |w: &AppWindow, d: &mut UiData, s: &Store, sender: &Commands| {
            if let Err(e) = save_profile(w, d, &*lock(s)?) {
                show_form_error(w, &e);
                return Err(e);
            }
            sender.send(AiCommand::Test(d.profile.clone()))?;
            let g = w.global::<AiState>();
            g.set_test_state(1);
            g.set_test_message("正在测试…".into());
            g.set_status("正在测试连接，只发送一句不含活动数据的测试文本".into());
            Ok(())
        }
    );
    bind!(
        on_fetch_models,
        |w: &AppWindow, d: &mut UiData, s: &Store, sender: &Commands| {
            let result = fetch_models(w, d, &*lock(s)?, sender);
            if let Err(e) = &result {
                w.global::<AiState>()
                    .set_model_note(format!("无法获取：{e}").into());
            }
            result
        }
    );
    bind!(on_clear_key, |w: &AppWindow,
                         d: &mut UiData,
                         s: &Store,
                         _: &Commands| {
        clear_credential(w, d, &*lock(s)?, true)?;
        w.global::<AiState>()
            .set_status("已清除 API 密钥；需要重新测试连接".into());
        Ok(())
    });
    bind!(
        on_clear_headers,
        |w: &AppWindow, d: &mut UiData, s: &Store, _: &Commands| {
            clear_credential(w, d, &*lock(s)?, false)?;
            w.global::<AiState>()
                .set_status("已清除附加请求头；需要重新测试连接".into());
            Ok(())
        }
    );
    bind!(
        on_delete_provider,
        |w: &AppWindow, d: &mut UiData, s: &Store, _: &Commands| {
            let s = lock(s)?;
            let id = d.profile.id.clone();
            s.delete_ai_profile(&id)?;
            credentials::delete(&id).map_err(anyhow::Error::msg)?;
            reload(w, d, &s)?;
            match d.profiles.first().cloned() {
                Some(p) => edit_profile(w, d, p, 0),
                None => {
                    d.profile = ProviderProfile::preset(0, new_id());
                    let g = w.global::<AiState>();
                    g.set_editing(false);
                    g.set_provider_index(-1);
                    show_profile(w, d);
                }
            }
            show_settings(w, d, &s)?;
            w.global::<AiState>()
                .set_status("连接已删除，密钥已从凭据管理器移除；用到它的自动总结已暂停".into());
            Ok(())
        }
    );

    // ───── Templates ─────
    bind_index!(
        on_choose_prompt,
        |w: &AppWindow, d: &mut UiData, _: &Store, i: i32| load_prompt(w, d, i)
    );
    bind!(on_save_prompt, |w: &AppWindow,
                           d: &mut UiData,
                           s: &Store,
                           _: &Commands| {
        save_prompt(w, d, &*lock(s)?, false)
    });
    bind!(on_new_prompt, |w: &AppWindow,
                          d: &mut UiData,
                          s: &Store,
                          _: &Commands| {
        save_prompt(w, d, &*lock(s)?, true)
    });
    bind!(on_reset_prompt, |w: &AppWindow,
                            _: &mut UiData,
                            _: &Store,
                            _: &Commands| {
        let g = w.global::<AiState>();
        g.set_prompt_text(DEFAULT_PROMPT.into());
        g.set_status("已恢复默认正文，点「保存模板」后生效".into());
        Ok(())
    });
    bind!(
        on_delete_prompt,
        |w: &AppWindow, d: &mut UiData, s: &Store, _: &Commands| {
            if d.prompt_id == PromptPreset::default().id {
                bail!("默认模板不能删除");
            }
            let s = lock(s)?;
            s.delete_ai_prompt(&d.prompt_id)?;
            reload(w, d, &s)?;
            load_prompt(w, d, 0)?;
            show_settings(w, d, &s)?;
            w.global::<AiState>()
                .set_status("模板已删除；用到它的自动总结已暂停".into());
            Ok(())
        }
    );

    // ───── A new summary ─────
    bind_index!(on_use_provider, |w: &AppWindow,
                                  d: &mut UiData,
                                  _: &Store,
                                  i: i32| {
        let p = d
            .profiles
            .get(i as usize)
            .ok_or_else(|| anyhow!("请选择 AI 连接"))?;
        d.use_profile = Some(p.id.clone());
        show_use(w, d);
        invalidate(w, d);
        Ok(())
    });
    bind_index!(on_use_prompt, |w: &AppWindow,
                                d: &mut UiData,
                                _: &Store,
                                i: i32| {
        let p = d
            .prompts
            .get(i as usize)
            .ok_or_else(|| anyhow!("请选择总结模板"))?;
        d.use_prompt = p.id.clone();
        show_use(w, d);
        invalidate(w, d);
        Ok(())
    });
    bind!(on_prepare, |w: &AppWindow,
                       d: &mut UiData,
                       _: &Store,
                       _: &Commands| {
        invalidate(w, d);
        d.preview_due = Some(Instant::now());
        Ok(())
    });
    bind!(on_new_summary, |w: &AppWindow,
                           d: &mut UiData,
                           _: &Store,
                           _: &Commands| {
        w.global::<AiState>().set_page(0);
        if d.prepared.is_none() && d.preview_due.is_none() && d.preview_job.is_none() {
            invalidate(w, d);
        }
        Ok(())
    });
    bind!(
        on_send_summary,
        |w: &AppWindow, d: &mut UiData, s: &Store, sender: &Commands| {
            let s = lock(s)?;
            let (start, end) = d.range.ok_or_else(|| anyhow!("请先选择时段"))?;
            let id = d
                .use_profile
                .clone()
                .ok_or_else(|| anyhow!("请先添加 AI 连接"))?;
            let p = s.ai_profile(&id)?;
            let prompt = s
                .ai_prompts()?
                .into_iter()
                .find(|p| p.id == d.use_prompt)
                .ok_or_else(|| anyhow!("总结模板不存在"))?;
            if d.prepared.as_ref()
                != Some(&(start, end, p.revision(), serde_json::to_string(&prompt)?))
            {
                invalidate(w, d);
                bail!("将发送的内容有变化，已重新整理，请确认后再生成");
            }
            let consents = d.picked.iter().map(|(c, _)| c.clone()).collect::<Vec<_>>();
            let job = s.enqueue_ai_summary(
                &p.id,
                &prompt.id,
                start,
                end,
                &consents,
                crate::unix_time_ms(),
            )?;
            d.picked.clear();
            d.preview = None;
            show_picked(w, d);
            invalidate(w, d);
            sender.send(AiCommand::Wake)?;
            d.version = None;
            d.job = Some(job);
            d.followup = None;
            d.leaf = None;
            reload(w, d, &s)?;
            show_reader(w, d, &s)?;
            let g = w.global::<AiState>();
            g.set_page(4);
            g.set_status(
                if g.get_reader_state() == 4 {
                    "这个时段没有记录，已保存一条「无数据」记录，没有调用 AI"
                } else {
                    "已开始生成总结"
                }
                .into(),
            );
            Ok(())
        }
    );

    // ───── Snapshots for one summary ─────
    bind!(on_list_images, |w: &AppWindow,
                           d: &mut UiData,
                           s: &Store,
                           _: &Commands| {
        list_images(w, d, &*lock(s)?)
    });
    bind!(
        on_preview_image,
        |w: &AppWindow, d: &mut UiData, s: &Store, _: &Commands| {
            // Never leave an earlier snapshot's consent on a newly chosen row.
            d.preview = None;
            let g = w.global::<AiState>();
            g.set_image_previewed(false);
            g.set_image_detail("".into());
            let slot = d
                .slots
                .get(g.get_image_index() as usize)
                .ok_or_else(|| anyhow!("请选择快照"))?;
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
                    "{} · {}×{}",
                    when_text(slot.slot_started_utc_ms, crate::unix_time_ms()),
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
        |w: &AppWindow, d: &mut UiData, _: &Store, _: &Commands| {
            let g = w.global::<AiState>();
            let slot = d
                .slots
                .get(g.get_image_index() as usize)
                .ok_or_else(|| anyhow!("请选择快照"))?;
            let consent = d
                .preview
                .clone()
                .filter(|p| p.slot_id == slot.id)
                .ok_or_else(|| anyhow!("请先预览这张快照"))?;
            if !d.picked.iter().any(|(c, _)| c.slot_id == slot.id) {
                if d.picked.len() >= MAX_IMAGES {
                    bail!("每次总结最多加入 {MAX_IMAGES} 张快照");
                }
                d.picked.push((consent, slot.slot_started_utc_ms));
            }
            show_picked(w, d);
            invalidate(w, d);
            Ok(())
        }
    );
    bind!(
        on_unauthorize_image,
        |w: &AppWindow, d: &mut UiData, _: &Store, _: &Commands| {
            let index = w.global::<AiState>().get_image_index();
            let slot = d
                .slots
                .get(index as usize)
                .ok_or_else(|| anyhow!("请选择快照"))?
                .id;
            d.picked.retain(|(c, _)| c.slot_id != slot);
            show_picked(w, d);
            invalidate(w, d);
            Ok(())
        }
    );
    bind!(on_clear_images, |w: &AppWindow,
                            d: &mut UiData,
                            _: &Store,
                            _: &Commands| {
        d.picked.clear();
        show_picked(w, d);
        invalidate(w, d);
        Ok(())
    });

    // ───── Reading a summary ─────
    bind_index!(
        on_choose_history,
        |w: &AppWindow, d: &mut UiData, s: &Store, i: i32| {
            let target = *d
                .history
                .get(i as usize)
                .ok_or_else(|| anyhow!("请选择一条总结记录"))?;
            select(w, d, &*lock(s)?, target)
        }
    );
    bind_index!(on_open_job, |w: &AppWindow,
                              d: &mut UiData,
                              s: &Store,
                              id: i32| {
        let s = lock(s)?;
        let job = s
            .ai_job(i64::from(id))
            .map_err(|_| anyhow!("这条记录已不存在"))?;
        let target = match job.spec.kind {
            AiJobKind::FollowUp { version_id, .. } => Target::Version(version_id),
            AiJobKind::Summary => s
                .ai_versions()?
                .iter()
                .find(|v| v.job_id == job.id)
                .map_or(Target::Job(job.id), |v| Target::Version(v.id)),
        };
        select(w, d, &s, target)?;
        // A failed job may be listed only while it is on screen.
        reload(w, d, &s)
    });
    bind_index!(
        on_choose_branch,
        |w: &AppWindow, d: &mut UiData, s: &Store, i: i32| {
            d.leaf = *d
                .branches
                .get(i as usize)
                .ok_or_else(|| anyhow!("请选择要接着追问的回答"))?;
            show_reader(w, d, &*lock(s)?)
        }
    );
    bind!(on_cancel_job, |w: &AppWindow,
                          d: &mut UiData,
                          s: &Store,
                          _: &Commands| {
        let s = lock(s)?;
        let target = if d.version.is_some() {
            d.followup
        } else {
            d.job
        };
        let job = target
            .and_then(|id| s.ai_job(id).ok())
            .filter(|j| j.state == "queued" || j.state == "running")
            .ok_or_else(|| anyhow!("没有正在进行的生成"))?;
        s.cancel_ai_job(job.id, crate::unix_time_ms())?;
        if d.followup == Some(job.id) {
            d.followup = None;
            // Continue from where the stopped question was asked.
            if let AiJobKind::FollowUp {
                version_id,
                user_message_id,
            } = job.spec.kind
            {
                d.leaf = s
                    .ai_messages(version_id)?
                    .into_iter()
                    .find(|m| m.id == user_message_id)
                    .and_then(|m| m.parent_id);
            }
        }
        reload(w, d, &s)?;
        show_reader(w, d, &s)?;
        w.global::<AiState>().set_status("已停止生成".into());
        Ok(())
    });
    bind!(on_follow_up, |w: &AppWindow,
                         d: &mut UiData,
                         s: &Store,
                         sender: &Commands| {
        let version = d.version.ok_or_else(|| anyhow!("请先选择一份总结"))?;
        if d.followup.is_some() {
            bail!("上一条追问还在回答中");
        }
        let g = w.global::<AiState>();
        let s = lock(s)?;
        let id =
            s.enqueue_ai_followup(version, d.leaf, &g.get_question(), crate::unix_time_ms())?;
        if let AiJobKind::FollowUp {
            user_message_id, ..
        } = s.ai_job(id)?.spec.kind
        {
            d.leaf = Some(user_message_id);
        }
        d.followup = Some(id);
        g.set_question("".into());
        sender.send(AiCommand::Wake)?;
        reload(w, d, &s)?;
        show_reader(w, d, &s)?;
        g.set_status("已发送追问；追问不会再次发送快照".into());
        Ok(())
    });
    bind!(on_pin_version, |w: &AppWindow,
                           d: &mut UiData,
                           s: &Store,
                           _: &Commands| {
        let id = d.version.ok_or_else(|| anyhow!("请先选择一份总结"))?;
        let s = lock(s)?;
        let pinned = !s.ai_version(id)?.pinned;
        s.pin_ai_version(id, pinned)?;
        reload(w, d, &s)?;
        show_reader(w, d, &s)?;
        w.global::<AiState>().set_status(
            if pinned {
                "已设为始终保留，不会被自动清理"
            } else {
                "已取消保留，这一版会按保留设置自动清理"
            }
            .into(),
        );
        Ok(())
    });
    bind!(
        on_delete_version,
        |w: &AppWindow, d: &mut UiData, s: &Store, _: &Commands| {
            let id = confirmed_version(d)?;
            let s = lock(s)?;
            s.delete_ai_version(id)?;
            d.version = None;
            d.followup = None;
            d.leaf = None;
            reload(w, d, &s)?;
            show_reader(w, d, &s)?;
            w.global::<AiState>().set_status("已删除这一版总结".into());
            Ok(())
        }
    );
    bind!(
        on_clear_conversation,
        |w: &AppWindow, d: &mut UiData, s: &Store, _: &Commands| {
            let id = confirmed_version(d)?;
            let s = lock(s)?;
            s.clear_ai_conversation(id)?;
            d.followup = None;
            d.leaf = None;
            reload(w, d, &s)?;
            show_reader(w, d, &s)?;
            w.global::<AiState>()
                .set_status("已清空这一版的追问".into());
            Ok(())
        }
    );
    bind!(
        on_clear_context,
        |w: &AppWindow, d: &mut UiData, s: &Store, _: &Commands| {
            let id = d.version.ok_or_else(|| anyhow!("请先选择一份总结"))?;
            let s = lock(s)?;
            d.leaf = Some(s.mark_ai_context_cleared(id, d.leaf, crate::unix_time_ms())?);
            show_reader(w, d, &s)?;
            w.global::<AiState>()
                .set_status("已开始新话题；之后的追问不再带上之前的问答，记录仍保留".into());
            Ok(())
        }
    );

    bind!(
        on_begin_confirm,
        |_: &AppWindow, d: &mut UiData, _: &Store, _: &Commands| {
            d.confirm_version = d.version;
            Ok(())
        }
    );

    // ───── Automatic summaries, records and notifications ─────
    bind!(on_save_daily, |w: &AppWindow,
                          d: &mut UiData,
                          s: &Store,
                          sender: &Commands| {
        save_schedule_or_revert(w, d, &*lock(s)?, sender, true, true)
    });
    bind!(
        on_save_interval,
        |w: &AppWindow, d: &mut UiData, s: &Store, sender: &Commands| {
            save_schedule_or_revert(w, d, &*lock(s)?, sender, false, true)
        }
    );
    bind!(
        on_save_daily_timing,
        |w: &AppWindow, d: &mut UiData, s: &Store, sender: &Commands| {
            save_schedule_or_revert(w, d, &*lock(s)?, sender, true, false)
        }
    );
    bind!(
        on_save_interval_timing,
        |w: &AppWindow, d: &mut UiData, s: &Store, sender: &Commands| {
            save_schedule_or_revert(w, d, &*lock(s)?, sender, false, false)
        }
    );
    bind!(
        on_save_settings,
        |w: &AppWindow, _: &mut UiData, s: &Store, _: &Commands| {
            let s = lock(s)?;
            let result = read_settings(w).and_then(|p| Ok(s.set_ai_settings(&p)?));
            if result.is_err() {
                show_records(w, &s)?;
            }
            result?;
            w.global::<AiState>().set_status("已保存".into());
            Ok(())
        }
    );
    bind!(
        on_flush_settings,
        |w: &AppWindow, d: &mut UiData, s: &Store, sender: &Commands| {
            flush_settings(w, d, &*lock(s)?, sender)
        }
    );

    let kept = Rc::clone(&data);
    let weak = w.as_weak();
    let timer = Timer::default();
    let mut lists = timelens_observer::PollInterval::new(Duration::from_secs(5));
    let mut settled = timelens_observer::PollInterval::new(Duration::from_secs(30));
    let mut generation = data_generation();
    let mut tested: Vec<(String, Result<(), String>)> = vec![];
    let mut finished: Vec<(i64, bool)> = vec![];
    let mut structural = false;
    let mut changed = false;
    timer.start(TimerMode::Repeated, Duration::from_millis(200), move || {
        let Some(w) = weak.upgrade() else {
            return;
        };
        let g = w.global::<AiState>();
        let mut d = data.borrow_mut();
        if data_generation() != generation
            && let Ok(s) = storage.try_lock()
        {
            match reset_dataset_ui(&w, &mut d, &s) {
                Ok(()) => {
                    generation = data_generation();
                    while service.receiver.try_recv().is_ok() {}
                    tested.clear();
                    finished.clear();
                }
                Err(e) => g.set_status(e.to_string().into()),
            }
        }
        while let Ok(event) = service.receiver.try_recv() {
            match event {
                AiEvent::Status(message) => g.set_status(message.into()),
                AiEvent::Tested(id, result) => {
                    structural = true;
                    tested.push((id, result));
                }
                AiEvent::Models(id, result) => show_models(&w, &mut d, &id, result),
                AiEvent::Changed => changed = true,
                AiEvent::Finished(id, success) => {
                    structural = true;
                    finished.push((id, success));
                }
            }
        }
        // Reading the lists decodes every job, so they are read again when a job
        // starts, ends or is tested, and otherwise at most every few seconds while
        // jobs report progress. The summary on screen follows every change.
        let open = g.get_open();
        let reading = open && g.get_page() == 4;
        let now = Instant::now();
        let reload_due =
            structural || (open && ((changed && lists.is_due(now)) || settled.is_due(now)));
        let reader_due = reading && changed;
        let preview_due = open
            && g.get_page() == 0
            && d.preview_job.is_none()
            && d.preview_due.is_some_and(|due| now >= due);
        if preview_due {
            start_preview(&w, &mut d, &storage);
        }
        poll_preview(&w, &mut d);
        if !(reload_due || reader_due) {
            return;
        }
        let Ok(s) = storage.try_lock() else {
            return;
        };
        for (id, result) in tested.drain(..) {
            show_tested(&w, &mut d, &s, &id, result);
        }
        for (id, success) in finished.drain(..) {
            if let Err(e) = job_finished(&w, &mut d, &s, id, success) {
                g.set_status(e.to_string().into());
            }
        }
        if reload_due {
            if let Err(e) = reload(&w, &mut d, &s) {
                g.set_status(e.to_string().into());
            }
            if w.get_settings_open()
                && w.get_settings_tab() == 4
                && let Err(e) = show_schedule_lines(&w, &d, &s)
            {
                g.set_status(e.to_string().into());
            }
            structural = false;
            lists.mark(now);
            settled.mark(now);
        }
        if reading
            && (reload_due || reader_due)
            && let Err(e) = show_reader(&w, &mut d, &s)
        {
            g.set_status(e.to_string().into());
        }
        if reload_due || reader_due {
            changed = false;
        }
    });
    Ok(Installed {
        _timer: timer,
        data: kept,
    })
}

fn opened(w: &AppWindow, d: &mut UiData, s: &Storage, (start, end): (i64, i64)) -> Result<()> {
    reload(w, d, s)?;
    // The AI thread runs and pauses schedules meanwhile.
    show_schedules(w, d, s, None)?;
    if end > start && d.range != Some((start, end)) {
        set_range(w, d, start, end);
        if w.global::<AiState>().get_page() == 6 {
            list_images(w, d, s)?;
        }
    }
    if d.prepared.is_none() && d.preview_due.is_none() && d.preview_job.is_none() {
        invalidate(w, d);
    }
    show_reader(w, d, s)
}

/// Point a new summary at the range the other pages share.
fn set_range(w: &AppWindow, d: &mut UiData, start: i64, end: i64) {
    d.range = Some((start, end));
    w.global::<AiState>().set_range_label(
        format!(
            "{} · {}",
            range_text(start, end, crate::unix_time_ms(), &Local, false),
            duration_text(end - start)
        )
        .into(),
    );
    // Snapshots stay chosen only while the range still holds them, so a range
    // that grows with the clock keeps them.
    d.picked.retain(|(_, at)| (start..end).contains(at));
    show_picked(w, d);
    invalidate(w, d);
}

/// Prepare a new summary over a summary's range, connection and template. It is
/// never sent from here: the user checks the preview and starts it.
fn regenerate(
    w: &AppWindow,
    d: &mut UiData,
    storage: &Store,
    timeline: &Rc<RefCell<UiState>>,
) -> Result<()> {
    let spec = {
        let s = lock(storage)?;
        if let Some(id) = d.version {
            s.ai_version(id)?.snapshot
        } else if let Some(id) = d.job {
            s.ai_job(id)?.spec
        } else {
            bail!("请先在左侧选择一份总结");
        }
    };
    let (start, end) = (spec.envelope.started_utc_ms, spec.envelope.ended_utc_ms);
    {
        let mut t = timeline.borrow_mut();
        match crate::timeline_view::selected_day_bounds(start, 0)
            .filter(|(_, day_end)| end <= *day_end)
        {
            Some((day_start, day_end)) => {
                t.horizon_started_utc_ms = day_start;
                t.horizon_ended_utc_ms = day_end;
                t.calendar_day = Some(day_start);
            }
            None => {
                t.horizon_started_utc_ms = start;
                t.horizon_ended_utc_ms = end;
                t.calendar_day = None;
            }
        }
        t.range_started_utc_ms = start;
        t.range_ended_utc_ms = end;
        t.follow_now = false;
    }
    if let Err(e) = crate::refresh_timeline(w, storage, timeline) {
        w.set_action_status(format!("刷新失败：{e}").into());
    }
    let s = lock(storage)?;
    reload(w, d, &s)?;
    if d.profiles.iter().any(|p| p.id == spec.provider.id) {
        d.use_profile = Some(spec.provider.id.clone());
    }
    if d.prompts.iter().any(|p| p.id == spec.prompt.id) {
        d.use_prompt = spec.prompt.id.clone();
    }
    show_use(w, d);
    d.picked.clear();
    set_range(w, d, start, end);
    let g = w.global::<AiState>();
    g.set_page(0);
    g.set_status("已按原来的时段、连接和模板准备好；确认将发送的内容后再生成".into());
    Ok(())
}

fn invalidate(w: &AppWindow, d: &mut UiData) {
    d.prepared = None;
    d.preview_seq += 1;
    d.preview_due = Some(Instant::now() + PREVIEW_DELAY);
    let g = w.global::<AiState>();
    g.set_prepared(false);
    g.set_preview_state(0);
}
/// Work out the preview in the background: a long range takes a while to
/// total, and the window stays responsive meanwhile. One runs at a time; the
/// next waits for it.
fn start_preview(w: &AppWindow, d: &mut UiData, storage: &Store) {
    d.preview_due = None;
    let ask = PreviewAsk {
        range: d.range,
        profile: d.use_profile.clone(),
        prompt: d.use_prompt.clone(),
        images: d.picked.len(),
    };
    let (sender, receiver) = mpsc::channel();
    d.preview_job = Some((d.preview_seq, receiver));
    w.global::<AiState>().set_preview_state(0);
    let storage = Arc::clone(storage);
    std::thread::spawn(move || {
        let result = storage
            .lock()
            .map_err(|_| anyhow!("存储锁损坏"))
            .and_then(|s| prepare_preview(&s, &ask));
        let _ = sender.send(result);
    });
}
/// Show a finished preview, unless something changed since it started.
fn poll_preview(w: &AppWindow, d: &mut UiData) {
    let Some((seq, receiver)) = &d.preview_job else {
        return;
    };
    let result = match receiver.try_recv() {
        Ok(result) => result,
        Err(mpsc::TryRecvError::Empty) => return,
        Err(mpsc::TryRecvError::Disconnected) => Err(anyhow!("整理预览时出错")),
    };
    let current = *seq == d.preview_seq;
    d.preview_job = None;
    if !current {
        return;
    }
    let g = w.global::<AiState>();
    match result {
        Ok(p) => {
            g.set_preview_target(p.target.into());
            g.set_preview_facts(p.facts.into());
            g.set_preview_empty(p.empty);
            g.set_preview_missing(p.missing.into());
            g.set_preview_missing_count(p.missing_count);
            d.prepared = Some(p.checked);
            g.set_preview_state(1);
            g.set_prepared(true);
        }
        Err(e) => {
            d.prepared = None;
            g.set_prepared(false);
            g.set_preview_fix(e.downcast_ref::<Blocked>().map_or(0, |b| b.fix));
            g.set_preview_error(e.to_string().into());
            g.set_preview_state(2);
        }
    }
}
/// What a preview is worked out for.
struct PreviewAsk {
    range: Option<(i64, i64)>,
    profile: Option<String>,
    prompt: String,
    images: usize,
}
/// What a new summary would send, as the preview shows it.
struct Preview {
    target: String,
    facts: String,
    empty: bool,
    missing: String,
    missing_count: i32,
    /// What was checked: range, connection revision and template.
    checked: (i64, i64, String, String),
}
/// Work out locally what a new summary would send. Nothing leaves the computer.
fn prepare_preview(s: &Storage, ask: &PreviewAsk) -> Result<Preview> {
    let (start, end) = ask
        .range
        .ok_or_else(|| blocked("请先在右上角选择时段", 0))?;
    let id = ask
        .profile
        .clone()
        .ok_or_else(|| blocked("还没有可用的 AI 连接", 1))?;
    let p = s.ai_profile(&id)?;
    if !p.is_tested() {
        return Err(blocked(
            format!("连接「{}」需要先通过测试才能使用", p.name),
            1,
        ));
    }
    let prompt = s
        .ai_prompts()?
        .into_iter()
        .find(|x| x.id == ask.prompt)
        .ok_or_else(|| blocked("请选择一个总结模板", 2))?;
    if ask.images > 0 && p.model.vision != Capability::Supported {
        return Err(blocked("当前模型不能看图，请先移除已选的快照", 3));
    }
    let envelope = s.ai_envelope(start, end)?;
    let chunks = s
        .ai_request_chunks(&envelope, &p, &prompt)
        .map_err(|e| blocked(e.to_string(), 1))?;
    let empty = !envelope.available && ask.images == 0;
    let share = envelope.covered_ms as f64 * 100.0 / (end - start) as f64;
    let requests = if empty {
        "不会调用 AI".to_owned()
    } else {
        format!(
            "预计 {} 次请求",
            if chunks.is_empty() {
                1
            } else {
                chunks.len() + 1
            }
        )
    };
    let now = crate::unix_time_ms();
    let mut missing = envelope
        .missing
        .iter()
        .take(20)
        .map(|m| {
            format!(
                "{} · {}",
                range_text(m.started_utc_ms, m.ended_utc_ms, now, &Local, true),
                missing_label(&m.reason)
            )
        })
        .collect::<Vec<_>>();
    if envelope.missing.len() > 20 {
        missing.push(format!("另有 {} 段未列出", envelope.missing.len() - 20));
    }
    Ok(Preview {
        target: format!("{} · {}", p.name, p.model.id),
        facts: format!(
            "有记录 {share:.0}% · {} 个应用 · 缺失 {} 段 · {requests} · {} 张快照",
            envelope.applications.len(),
            envelope.missing.len(),
            ask.images
        ),
        empty,
        missing: missing.join("\n"),
        missing_count: envelope.missing.len() as i32,
        checked: (start, end, p.revision(), serde_json::to_string(&prompt)?),
    })
}

fn list_images(w: &AppWindow, d: &mut UiData, s: &Storage) -> Result<()> {
    let (start, end) = d.range.ok_or_else(|| anyhow!("请先选择时段"))?;
    d.slots = s
        .list_snapshot_slots(start, end, 1000)?
        .into_iter()
        .filter(|s| s.success)
        .collect();
    d.preview = None;
    let now = crate::unix_time_ms();
    let g = w.global::<AiState>();
    g.set_image_options(strings(
        d.slots
            .iter()
            .map(|s| when_text(s.slot_started_utc_ms, now)),
    ));
    g.set_image_index(-1);
    g.set_image_previewed(false);
    g.set_image_preview(slint::Image::default());
    g.set_image_detail("".into());
    show_picked(w, d);
    Ok(())
}
fn show_picked(w: &AppWindow, d: &UiData) {
    let g = w.global::<AiState>();
    g.set_image_selected(d.picked.len() as i32);
    g.set_image_checked(ModelRc::new(VecModel::from(
        d.slots
            .iter()
            .map(|slot| d.picked.iter().any(|(c, _)| c.slot_id == slot.id))
            .collect::<Vec<_>>(),
    )));
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
        context_tokens: parse(&g.get_context_tokens(), "上下文窗口")?,
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
/// Read "Name=value; Other=value". A blank field means "keep what is saved".
fn parse_headers(text: &str) -> Result<Option<BTreeMap<String, String>>> {
    if text.trim().is_empty() {
        return Ok(None);
    }
    let mut headers = BTreeMap::new();
    for field in text.split(';').filter(|v| !v.trim().is_empty()) {
        let (k, v) = field
            .split_once('=')
            .ok_or_else(|| anyhow!("附加请求头的格式是 名称=值，多个用分号分隔"))?;
        headers.insert(k.trim().into(), v.trim().into());
    }
    Ok(Some(headers))
}
/// Merge what the form entered into the saved credentials: a blank key or blank
/// headers keep the saved ones, and only an explicit clear empties them. `None`
/// means there is nothing to write.
fn merge_secrets(
    saved: Option<credentials::Secrets>,
    key: &str,
    headers: Option<BTreeMap<String, String>>,
    clear_key: bool,
    clear_headers: bool,
) -> Option<credentials::Secrets> {
    if saved.is_some() && key.is_empty() && headers.is_none() && !clear_key && !clear_headers {
        return None;
    }
    let mut secrets = saved.unwrap_or_default();
    if clear_key || !key.is_empty() {
        secrets.api_key.zeroize();
        if !clear_key {
            secrets.api_key.push_str(key);
        }
    }
    if clear_headers || headers.is_some() {
        for value in secrets.headers.values_mut() {
            value.zeroize();
        }
        secrets.headers = if clear_headers {
            BTreeMap::new()
        } else {
            headers.unwrap_or_default()
        };
    }
    Some(secrets)
}
fn save_credentials(w: &AppWindow, p: &ProviderProfile, d: &mut UiData) -> Result<()> {
    let g = w.global::<AiState>();
    let headers = parse_headers(&g.get_secret_headers())?;
    let key = g.get_api_key();
    let saved = if p.credential_revision == 0 {
        None
    } else {
        credentials::load(&p.id).ok()
    };
    let Some(secrets) = merge_secrets(saved, &key, headers, false, false) else {
        return Ok(());
    };
    credentials::save(&p.id, &secrets).map_err(anyhow::Error::msg)?;
    d.profile.credential_revision = p.credential_revision.saturating_add(1);
    g.set_api_key("".into());
    g.set_secret_headers("".into());
    g.set_has_credentials(!secrets.api_key.is_empty());
    g.set_has_headers(!secrets.headers.is_empty());
    Ok(())
}
/// Clears the saved API key, or the request headers, and nothing else: the
/// rest of the form stays an unsaved draft, and a saved connection only takes
/// the new credential revision, so it needs testing again.
fn clear_credential(w: &AppWindow, d: &mut UiData, s: &Storage, key: bool) -> Result<()> {
    let g = w.global::<AiState>();
    let id = d.profile.id.clone();
    let saved = if d.profile.credential_revision == 0 {
        None
    } else {
        credentials::load(&id).ok()
    };
    if let Some(secrets) = merge_secrets(saved, "", None, key, !key) {
        if secrets.api_key.is_empty() && secrets.headers.is_empty() {
            credentials::delete(&id).map_err(anyhow::Error::msg)?;
        } else {
            credentials::save(&id, &secrets).map_err(anyhow::Error::msg)?;
        }
        g.set_has_credentials(!secrets.api_key.is_empty());
        g.set_has_headers(!secrets.headers.is_empty());
    }
    if key {
        g.set_api_key("".into());
    } else {
        g.set_secret_headers("".into());
    }
    d.profile.credential_revision = d.profile.credential_revision.saturating_add(1);
    if d.profiles.iter().any(|x| x.id == id) {
        let mut stored = s.ai_profile(&id)?;
        stored.credential_revision = d.profile.credential_revision;
        s.save_ai_profile(&stored)?;
        reload(w, d, s)?;
        show_test(w, d);
        if d.use_profile.as_deref() == Some(id.as_str()) {
            invalidate(w, d);
        }
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
    let g = w.global::<AiState>();
    g.set_editing(true);
    if d.use_profile.is_none() || d.use_profile.as_deref() == Some(d.profile.id.as_str()) {
        d.use_profile = Some(d.profile.id.clone());
        invalidate(w, d);
    }
    reload(w, d, s)?;
    g.set_models(strings(d.profile.models.iter().map(model_label)));
    g.set_model_index(
        d.profile
            .models
            .iter()
            .position(|m| m.id == d.profile.model.id)
            .map_or(-1, |i| i as i32),
    );
    show_test(w, d);
    Ok(())
}
fn fetch_models(w: &AppWindow, d: &mut UiData, s: &Storage, sender: &Commands) -> Result<()> {
    let g = w.global::<AiState>();
    // Listing models needs no model; a placeholder passes validation.
    let empty = g.get_model_id().trim().is_empty();
    if empty {
        g.set_model_id("model-catalog".into());
    }
    let result = read_profile(w, d);
    if empty {
        g.set_model_id("".into());
    }
    let mut p = result?;
    let revision = p.credential_revision;
    save_credentials(w, &p, d)?;
    p.credential_revision = d.profile.credential_revision;
    // A saved connection with a new key needs testing again before summaries
    // use it; the rest of the form stays unsaved.
    let resaved = p.credential_revision != revision && d.profiles.iter().any(|x| x.id == p.id);
    if resaved {
        let mut saved = s.ai_profile(&p.id)?;
        saved.credential_revision = p.credential_revision;
        s.save_ai_profile(&saved)?;
        reload(w, d, s)?;
    }
    sender.send(AiCommand::Models(p.clone()))?;
    if empty {
        p.model.id.clear();
    }
    d.profile = p;
    if resaved {
        show_test(w, d);
        if d.use_profile.as_deref() == Some(d.profile.id.as_str()) {
            invalidate(w, d);
        }
    }
    g.set_model_note("正在获取可用模型…".into());
    Ok(())
}
/// A connection never saved leaves no key behind in the Credential Manager;
/// listing its models is what writes one.
fn forget_unsaved_credentials(d: &UiData) {
    if d.profile.credential_revision > 0 && !d.profiles.iter().any(|x| x.id == d.profile.id) {
        let _ = credentials::delete(&d.profile.id);
    }
}
fn edit_profile(w: &AppWindow, d: &mut UiData, p: ProviderProfile, index: i32) {
    forget_unsaved_credentials(d);
    d.profile = p;
    let g = w.global::<AiState>();
    g.set_editing(true);
    g.set_provider_index(index);
    show_profile(w, d);
}
fn show_form_error(w: &AppWindow, e: &anyhow::Error) {
    let g = w.global::<AiState>();
    g.set_test_state(3);
    g.set_test_message(e.to_string().into());
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
    match m.vision {
        Capability::Supported => format!("{}（可看图）", m.id),
        Capability::Unsupported => format!("{}（仅文本）", m.id),
        Capability::Unknown => m.id.clone(),
    }
}
fn model_hint(p: &ProviderProfile) -> &'static str {
    if p.azure {
        return "或手动填写 Azure 部署名称";
    }
    match p.protocol {
        Protocol::OpenAi => "或手动填写模型 ID，例如 gpt-4o-mini",
        Protocol::Anthropic => "或手动填写模型 ID，例如 claude-3-5-haiku-latest",
        Protocol::Gemini => "或手动填写模型 ID，例如 gemini-2.0-flash",
    }
}
/// Whether Credential Manager holds a key and headers for this connection. The
/// values themselves never reach the window.
fn saved_credentials(p: &ProviderProfile) -> (bool, bool) {
    if p.credential_revision == 0 {
        return (false, false);
    }
    credentials::load(&p.id).map_or((false, false), |s| {
        (!s.api_key.is_empty(), !s.headers.is_empty())
    })
}
fn show_profile(w: &AppWindow, d: &UiData) {
    let p = &d.profile;
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
    g.set_model_hint(model_hint(p).into());
    g.set_model_note(
        if p.models.is_empty() {
            "填好密钥后，点这里获取服务提供的模型"
        } else {
            ""
        }
        .into(),
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
    let (key, headers) = saved_credentials(p);
    g.set_has_credentials(key);
    g.set_has_headers(headers);
    show_test(w, d);
}
fn show_test(w: &AppWindow, d: &UiData) {
    let p = &d.profile;
    let saved = d.profiles.iter().any(|x| x.id == p.id);
    let (state, message) = if p.is_tested() {
        (2, "已通过连接测试")
    } else if !saved {
        (0, "")
    } else if p.tested_revision.is_some() {
        (0, "设置已更改，需要重新测试")
    } else {
        (0, "尚未测试")
    };
    let g = w.global::<AiState>();
    g.set_test_state(state);
    g.set_test_message(message.into());
}
fn show_tested(w: &AppWindow, d: &mut UiData, s: &Storage, id: &str, result: Result<(), String>) {
    let g = w.global::<AiState>();
    let current = d.profile.id == id;
    match result {
        Ok(()) => {
            if current && let Ok(p) = s.ai_profile(id) {
                d.profile.tested_revision = p.tested_revision;
            }
            if current {
                g.set_test_state(2);
                g.set_test_message(format!("连接正常（{}）", Local::now().format("%H:%M")).into());
            }
            g.set_status("连接测试通过".into());
        }
        Err(message) => {
            if current {
                g.set_test_state(3);
                g.set_test_message(message.clone().into());
            }
            g.set_status(format!("连接测试失败：{message}").into());
        }
    }
    if d.use_profile.as_deref() == Some(id) {
        invalidate(w, d);
    }
}
fn show_models(w: &AppWindow, d: &mut UiData, id: &str, result: Result<Vec<Model>, String>) {
    if d.profile.id != id {
        return;
    }
    let g = w.global::<AiState>();
    match result {
        Ok(models) => {
            g.set_model_note(
                if models.is_empty() {
                    "服务没有返回模型，请手动填写模型 ID".to_owned()
                } else {
                    format!("找到 {} 个模型，点选一个", models.len())
                }
                .into(),
            );
            d.profile.models = models;
            g.set_models(strings(d.profile.models.iter().map(model_label)));
            let current = g.get_model_id();
            g.set_model_index(
                d.profile
                    .models
                    .iter()
                    .position(|m| m.id == current.as_str())
                    .map_or(-1, |i| i as i32),
            );
        }
        Err(message) => {
            g.set_model_note(format!("获取失败：{message}。也可以手动填写模型 ID").into())
        }
    }
}

fn load_prompt(w: &AppWindow, d: &mut UiData, index: i32) -> Result<()> {
    let p = d
        .prompts
        .get(index as usize)
        .ok_or_else(|| anyhow!("请选择总结模板"))?;
    d.prompt_id = p.id.clone();
    let g = w.global::<AiState>();
    g.set_prompt_name(p.name.clone().into());
    g.set_prompt_text(p.text.clone().into());
    g.set_language(p.language.clone().into());
    g.set_prompt_index(index);
    g.set_prompt_is_default(p.id == PromptPreset::default().id);
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
    let index = d
        .prompts
        .iter()
        .position(|p| p.id == d.prompt_id)
        .unwrap_or(0) as i32;
    load_prompt(w, d, index)?;
    if d.use_prompt == d.prompt_id {
        invalidate(w, d);
    }
    g.set_status(
        if new {
            "已另存为新模板"
        } else {
            "模板已保存，只影响之后的总结"
        }
        .into(),
    );
    Ok(())
}

/// Read the lists again and keep every selection pointing at something that
/// still exists.
fn reload(w: &AppWindow, d: &mut UiData, s: &Storage) -> Result<()> {
    let g = w.global::<AiState>();
    d.profiles = s.ai_profiles()?;
    d.prompts = s.ai_prompts()?;
    let jobs = s.ai_jobs(200)?;
    let versions = s.ai_versions()?;
    g.set_busy(
        jobs.iter()
            .any(|j| j.state == "queued" || j.state == "running"),
    );

    let labels = d
        .profiles
        .iter()
        .map(|p| {
            if p.is_tested() {
                p.name.clone()
            } else {
                format!("{}（需要测试）", p.name)
            }
        })
        .collect::<Vec<_>>();
    if !same(&g.get_providers(), &labels) {
        g.set_providers(strings(labels));
    }
    let rows = d
        .profiles
        .iter()
        .map(|p| AiConnectionRow {
            title: p.name.clone().into(),
            detail: format!(
                "{} · {}",
                if p.model.id.is_empty() {
                    "未选择模型"
                } else {
                    p.model.id.as_str()
                },
                if p.is_tested() {
                    "已连接"
                } else {
                    "需要测试"
                }
            )
            .into(),
            tested: p.is_tested(),
        })
        .collect();
    g.set_connections(crate::ui_model::sync(g.get_connections(), rows, |a, b| {
        a.title == b.title
    }));
    let names = d.prompts.iter().map(|p| p.name.clone()).collect::<Vec<_>>();
    if !same(&g.get_prompts(), &names) {
        g.set_prompts(strings(names));
    }
    g.set_provider_index(
        d.profiles
            .iter()
            .position(|p| p.id == d.profile.id)
            .map_or(-1, |i| i as i32),
    );
    if let Some(i) = d.prompts.iter().position(|p| p.id == d.prompt_id) {
        g.set_prompt_index(i as i32);
    }

    // A new summary keeps its connection and template while they exist.
    if !d
        .use_profile
        .as_ref()
        .is_some_and(|id| d.profiles.iter().any(|p| &p.id == id))
    {
        let fallback = d
            .profiles
            .iter()
            .find(|p| p.is_tested())
            .or(d.profiles.first())
            .map(|p| p.id.clone());
        if fallback != d.use_profile {
            d.use_profile = fallback;
            invalidate(w, d);
        }
    }
    if !d.prompts.iter().any(|p| p.id == d.use_prompt) {
        d.use_prompt = d
            .prompts
            .first()
            .map_or_else(|| PromptPreset::default().id, |p| p.id.clone());
        invalidate(w, d);
    }
    show_use(w, d);

    d.followups = jobs
        .iter()
        .filter(|j| j.state == "queued" || j.state == "running")
        .filter_map(|j| match j.spec.kind {
            AiJobKind::FollowUp { version_id, .. } => Some((j.id, version_id)),
            AiJobKind::Summary => None,
        })
        .collect();
    if d.version
        .is_some_and(|id| !versions.iter().any(|v| v.id == id))
    {
        d.version = None;
        d.followup = None;
        d.leaf = None;
    }
    if d.job.is_some_and(|id| !jobs.iter().any(|j| j.id == id)) {
        d.job = None;
    }
    if let Some(id) = d.version
        && d.followup.is_none()
    {
        d.followup = d
            .followups
            .iter()
            .find(|(_, v)| *v == id)
            .map(|(job, _)| *job);
    }
    d.numbers = version_numbers(&versions);
    let rows = history(
        &versions,
        &jobs,
        &d.numbers,
        d.job,
        crate::unix_time_ms(),
        &Local,
    );
    d.history = rows.iter().map(|(target, _)| *target).collect();
    g.set_history(crate::ui_model::sync(
        g.get_history(),
        rows.into_iter().map(|(_, row)| row).collect(),
        |a, b| a.key == b.key,
    ));
    show_selection(w, d);
    Ok(())
}
fn show_use(w: &AppWindow, d: &UiData) {
    let g = w.global::<AiState>();
    let index = d
        .use_profile
        .as_ref()
        .and_then(|id| d.profiles.iter().position(|p| &p.id == id));
    g.set_use_provider_index(index.map_or(-1, |i| i as i32));
    g.set_use_vision(index.map_or(0, |i| cap_index(d.profiles[i].model.vision)));
    g.set_use_prompt_index(
        d.prompts
            .iter()
            .position(|p| p.id == d.use_prompt)
            .map_or(-1, |i| i as i32),
    );
}
fn show_selection(w: &AppWindow, d: &UiData) {
    let current = d.version.map(Target::Version).or(d.job.map(Target::Job));
    w.global::<AiState>().set_history_index(
        d.history
            .iter()
            .position(|t| Some(*t) == current)
            .map_or(-1, |i| i as i32),
    );
}

/// Number each version within its range, oldest first.
fn version_numbers(versions: &[AiVersion]) -> BTreeMap<i64, usize> {
    let mut ordered = versions.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|v| (v.created_utc_ms, v.id));
    let mut counts = BTreeMap::<&str, usize>::new();
    ordered
        .into_iter()
        .map(|v| {
            let n = counts.entry(v.group_key.as_str()).or_default();
            *n += 1;
            (v.id, *n)
        })
        .collect()
}
fn source_name(group: &str) -> &'static str {
    match group.split(':').next() {
        Some("manual") => "手动",
        Some("daily") => "每日",
        Some("interval") => "间隔",
        _ => "自动",
    }
}
/// The summary history, newest first: every version, and the summary jobs of
/// ranges that have none yet. Jobs under way, and the job on screen, are always
/// listed.
fn history<Tz: TimeZone>(
    versions: &[AiVersion],
    jobs: &[AiJob],
    numbers: &BTreeMap<i64, usize>,
    keep: Option<i64>,
    now: i64,
    zone: &Tz,
) -> Vec<(Target, AiHistoryRow)> {
    let mut rows = versions
        .iter()
        .map(|v| {
            let range = &v.snapshot.envelope;
            (
                (v.created_utc_ms, v.id),
                Target::Version(v.id),
                AiHistoryRow {
                    key: format!("v{}", v.id).into(),
                    title: range_text(range.started_utc_ms, range.ended_utc_ms, now, zone, true)
                        .into(),
                    detail: format!(
                        "{} · 第 {} 版 · {}",
                        source_name(&v.group_key),
                        numbers.get(&v.id).copied().unwrap_or(1),
                        v.snapshot.provider.model.id
                    )
                    .into(),
                    state: 0,
                    pinned: v.pinned,
                },
            )
        })
        .collect::<Vec<_>>();
    let mut listed = versions
        .iter()
        .map(|v| v.group_key.as_str())
        .collect::<BTreeSet<_>>();
    let mut ordered = jobs.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|j| std::cmp::Reverse((j.created_utc_ms, j.id)));
    for j in ordered {
        if !matches!(j.spec.kind, AiJobKind::Summary) || j.state == "completed" {
            continue;
        }
        let under_way = j.state == "queued" || j.state == "running";
        if !under_way && keep != Some(j.id) && listed.contains(j.group_key.as_str()) {
            continue;
        }
        listed.insert(j.group_key.as_str());
        let range = &j.spec.envelope;
        rows.push((
            (j.created_utc_ms, j.id),
            Target::Job(j.id),
            AiHistoryRow {
                key: format!("j{}", j.id).into(),
                title: range_text(range.started_utc_ms, range.ended_utc_ms, now, zone, true).into(),
                detail: format!(
                    "{} · {} · {}",
                    source_name(&j.group_key),
                    state_label(&j.state),
                    j.spec.provider.model.id
                )
                .into(),
                state: match j.state.as_str() {
                    "queued" | "running" => 1,
                    "failed" => 2,
                    _ => 3,
                },
                pinned: false,
            },
        ));
    }
    rows.sort_by_key(|(order, _, _)| std::cmp::Reverse(*order));
    rows.into_iter().map(|(_, t, row)| (t, row)).collect()
}

fn select(w: &AppWindow, d: &mut UiData, s: &Storage, target: Target) -> Result<()> {
    match target {
        Target::Version(id) => {
            d.version = Some(id);
            d.job = None;
            d.followup = d
                .followups
                .iter()
                .find(|(_, v)| *v == id)
                .map(|(job, _)| *job);
            // Show the latest answer, follow-ups included.
            d.leaf = s.ai_messages(id)?.last().map(|m| m.id);
        }
        Target::Job(id) => {
            d.version = None;
            d.job = Some(id);
            d.followup = None;
            d.leaf = None;
        }
    }
    show_selection(w, d);
    w.global::<AiState>().set_page(4);
    show_reader(w, d, s)
}
fn show_reader(w: &AppWindow, d: &mut UiData, s: &Storage) -> Result<()> {
    if let Some(id) = d.version {
        return show_version(w, d, s, id);
    }
    if let Some(id) = d.job {
        return show_job(w, d, s, id);
    }
    clear_reader(w);
    let g = w.global::<AiState>();
    if g.get_page() == 4 {
        match d.history.first().copied() {
            Some(target) => return select(w, d, s, target),
            None => g.set_page(0),
        }
    }
    Ok(())
}
fn clear_reader(w: &AppWindow) {
    let g = w.global::<AiState>();
    g.set_reader_title("".into());
    g.set_reader_meta("".into());
    g.set_reader_state(0);
    g.set_reader_error("".into());
    g.set_reader_busy("".into());
    g.set_conversation("".into());
    g.set_exposed_reasoning("".into());
    g.set_token_label("".into());
    g.set_source_label("".into());
    g.set_pinned(false);
    g.set_has_follow_ups(false);
    g.set_branches(strings([]));
    g.set_branch_index(0);
    g.set_branch_label("最新回答".into());
}
fn show_version(w: &AppWindow, d: &mut UiData, s: &Storage, id: i64) -> Result<()> {
    let v = s.ai_version(id)?;
    let messages = s.ai_messages(id)?;
    if d.leaf
        .is_some_and(|leaf| !messages.iter().any(|m| m.id == leaf))
    {
        d.leaf = messages.last().map(|m| m.id);
    }
    let mut text = v.answer.clone();
    let mut reasoning = v.reasoning.clone();
    let mut usage = v.usage.clone();
    let mut add_reasoning = |more: &str| {
        if !more.is_empty() {
            if !reasoning.is_empty() {
                reasoning.push_str("\n\n");
            }
            reasoning.push_str(more);
        }
    };
    for m in s.ai_branch(id, d.leaf)? {
        match m.role.as_str() {
            "clear_context" => text.push_str("\n\n—— 新话题：之后的追问不带上面的问答 ——"),
            "user" => {
                text.push_str("\n\n————————\n你：\n");
                text.push_str(&m.body);
            }
            _ => {
                text.push_str("\n\nAI：\n");
                text.push_str(&m.body);
                if let Some(e) = &m.error {
                    text.push_str(&format!("\n（回答没有完成：{}）", failure_text(Some(e))));
                }
            }
        }
        add_reasoning(&m.reasoning);
        usage.add(&m.usage);
    }
    let mut busy = "";
    if let Some(job) = d.followup {
        match s.ai_job(job) {
            Ok(job) if job.state == "queued" || job.state == "running" => {
                let running = job.state == "running";
                text.push_str(if running {
                    "\n\nAI（回答中…）：\n"
                } else {
                    "\n\nAI（排队中…）：\n"
                });
                text.push_str(&job.body);
                add_reasoning(&job.reasoning);
                busy = if running { "回答中…" } else { "排队中" };
            }
            _ => d.followup = None,
        }
    }
    let available = match d.source {
        Some((version, available)) if version == id => available,
        _ => {
            let available = s.ai_source_available(id)?;
            d.source = Some((id, available));
            available
        }
    };

    let mut options = vec!["最初的总结".to_owned()];
    d.branches = vec![None];
    for m in messages.iter().filter(|m| m.role == "assistant") {
        options.push(format!(
            "第 {} 条回答：{}",
            d.branches.len(),
            excerpt(&m.body)
        ));
        d.branches.push(Some(m.id));
    }
    let latest = messages.last().map(|m| m.id);
    let label = if d.leaf == latest {
        "最新回答".to_owned()
    } else if d.leaf.is_none() {
        "最初的总结".to_owned()
    } else if let Some(i) = d.branches.iter().position(|b| *b == d.leaf) {
        format!("第 {i} 条回答")
    } else if messages
        .iter()
        .any(|m| Some(m.id) == d.leaf && m.role == "clear_context")
    {
        "新话题".to_owned()
    } else {
        "你的提问".to_owned()
    };

    let now = crate::unix_time_ms();
    let g = w.global::<AiState>();
    let range = &v.snapshot.envelope;
    g.set_reader_title(
        range_text(range.started_utc_ms, range.ended_utc_ms, now, &Local, false).into(),
    );
    g.set_reader_meta(
        format!(
            "{} · 第 {} 版 · {} / {} · 模板「{}」· {} 生成",
            source_name(&v.group_key),
            d.numbers.get(&id).copied().unwrap_or(1),
            v.snapshot.provider.name,
            v.snapshot.provider.model.id,
            v.snapshot.prompt.name,
            when_text(v.created_utc_ms, now)
        )
        .into(),
    );
    g.set_reader_state(0);
    g.set_reader_error("".into());
    g.set_reader_busy(busy.into());
    g.set_pinned(v.pinned);
    g.set_has_follow_ups(!messages.is_empty());
    g.set_conversation(text.into());
    g.set_exposed_reasoning(reasoning.into());
    g.set_token_label(usage_label(&usage).into());
    g.set_source_label(
        if available {
            ""
        } else {
            "原始记录已清理，追问基于当时发送的数据"
        }
        .into(),
    );
    if !same(&g.get_branches(), &options) {
        g.set_branches(strings(options));
    }
    g.set_branch_index(
        d.branches
            .iter()
            .position(|b| *b == d.leaf)
            .map_or(-1, |i| i as i32),
    );
    g.set_branch_label(label.into());
    Ok(())
}
fn show_job(w: &AppWindow, d: &mut UiData, s: &Storage, id: i64) -> Result<()> {
    let job = s.ai_job(id)?;
    if job.state == "completed"
        && let Some(v) = s.ai_versions()?.into_iter().find(|v| v.job_id == id)
    {
        d.job = None;
        d.version = Some(v.id);
        d.leaf = s.ai_messages(v.id)?.last().map(|m| m.id);
        show_selection(w, d);
        return show_version(w, d, s, v.id);
    }
    let state = match job.state.as_str() {
        "queued" => 1,
        "running" => 2,
        "failed" => 3,
        "no_data" => 4,
        "missed" => 5,
        _ => 6,
    };
    let now = crate::unix_time_ms();
    let g = w.global::<AiState>();
    let range = &job.spec.envelope;
    g.set_reader_title(
        range_text(range.started_utc_ms, range.ended_utc_ms, now, &Local, false).into(),
    );
    g.set_reader_meta(
        format!(
            "{} · {} / {} · 模板「{}」· {} 创建",
            source_name(&job.group_key),
            job.spec.provider.name,
            job.spec.provider.model.id,
            job.spec.prompt.name,
            when_text(job.created_utc_ms, now)
        )
        .into(),
    );
    g.set_reader_state(state);
    // A queued job with an error is waiting to retry; say why.
    g.set_reader_error(
        if state == 3 || (state == 1 && job.error.is_some()) {
            failure_text(job.error.as_ref())
        } else {
            String::new()
        }
        .into(),
    );
    g.set_reader_busy(
        match state {
            1 if job.error.is_some() => "等待重试",
            1 => "排队中",
            2 => "生成中…",
            _ => "",
        }
        .into(),
    );
    g.set_pinned(false);
    g.set_has_follow_ups(false);
    g.set_conversation(job.body.into());
    g.set_exposed_reasoning(job.reasoning.into());
    g.set_token_label(usage_label(&job.usage).into());
    g.set_source_label("".into());
    Ok(())
}
fn job_finished(w: &AppWindow, d: &mut UiData, s: &Storage, id: i64, success: bool) -> Result<()> {
    let job = s.ai_job(id)?;
    let settings = s.ai_settings()?;
    if (success && settings.notify_success) || (job.state == "failed" && settings.notify_failure) {
        crate::tray::notify(id, success);
    }
    let follow_up = matches!(job.spec.kind, AiJobKind::FollowUp { .. });
    let what = if follow_up { "追问" } else { "总结" };
    w.global::<AiState>().set_status(
        match job.state.as_str() {
            "completed" if follow_up => "AI 已回答追问".to_owned(),
            "completed" => "总结已生成".to_owned(),
            "queued" => format!(
                "{what}这次没有成功（{}），稍后自动重试",
                failure_text(job.error.as_ref())
            ),
            "failed" => format!("{what}失败：{}", failure_text(job.error.as_ref())),
            other => format!("{what}{}", state_label(other)),
        }
        .into(),
    );
    if d.followup == Some(id) && job.state != "queued" && job.state != "running" {
        d.followup = None;
        if let AiJobKind::FollowUp { version_id, .. } = job.spec.kind
            && d.version == Some(version_id)
        {
            d.leaf = s.ai_messages(version_id)?.last().map(|m| m.id);
        }
    }
    if d.job == Some(id)
        && success
        && let Some(v) = s.ai_versions()?.into_iter().find(|v| v.job_id == id)
    {
        d.job = None;
        d.version = Some(v.id);
        d.leaf = None;
    }
    Ok(())
}
fn state_label(s: &str) -> &str {
    match s {
        "queued" => "排队中",
        "running" => "生成中",
        "completed" => "已完成",
        "no_data" => "无数据",
        "missed" => "已跳过",
        "failed" => "失败",
        "canceled" => "已取消",
        _ => s,
    }
}
fn failure_text(e: Option<&AiFailure>) -> String {
    e.map_or_else(
        || "原因未知".to_owned(),
        |e| {
            format!(
                "{}{}",
                e.status.map_or(String::new(), |s| format!("HTTP {s} · ")),
                e.message
            )
        },
    )
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
/// The usage the service reported, such as "用量：输入 3,210 · 输出 812 tokens".
fn usage_label(u: &TokenUsage) -> String {
    let parts = [
        ("输入", u.input),
        ("输出", u.output),
        ("缓存", u.cached),
        ("思考", u.reasoning),
    ]
    .into_iter()
    .filter_map(|(name, value)| value.map(|v| format!("{name} {}", grouped(v))))
    .collect::<Vec<_>>();
    let text = if parts.is_empty() {
        String::new()
    } else {
        format!("用量：{} tokens", parts.join(" · "))
    };
    match (u.cost.as_deref(), text.is_empty()) {
        (Some(cost), true) => format!("费用 {cost}"),
        (Some(cost), false) => format!("{text} · 费用 {cost}"),
        (None, _) => text,
    }
}

/// The version a delete or clear confirmation was opened on, if the reader still
/// shows it.
fn confirmed_version(d: &mut UiData) -> Result<i64> {
    let id = d
        .confirm_version
        .take()
        .ok_or_else(|| anyhow!("请先选择一份总结"))?;
    if d.version != Some(id) {
        bail!("显示的总结已经切换，没有执行；请重新操作");
    }
    Ok(id)
}

fn reset_dataset_ui(w: &AppWindow, d: &mut UiData, s: &Storage) -> Result<()> {
    let range = d.range;
    forget_unsaved_credentials(d);
    *d = UiData::default();
    let g = w.global::<AiState>();
    g.set_page(0);
    g.set_question("".into());
    clear_reader(w);
    g.set_history_index(-1);
    g.set_image_options(strings([]));
    g.set_image_checked(ModelRc::default());
    g.set_image_index(-1);
    g.set_image_selected(0);
    g.set_image_preview(slint::Image::default());
    g.set_image_previewed(false);
    g.set_image_detail("".into());
    g.set_preview_target("".into());
    g.set_preview_facts("".into());
    g.set_preview_missing("".into());
    g.set_preview_missing_count(0);
    g.set_preview_empty(false);
    g.set_preview_error("".into());
    g.set_preview_fix(0);
    g.set_range_label("".into());
    g.set_test_state(0);
    g.set_test_message("".into());
    g.set_confirm_kind(0);
    reload(w, d, s)?;
    let current = d
        .use_profile
        .as_ref()
        .and_then(|id| d.profiles.iter().position(|p| &p.id == id));
    match current {
        Some(i) => {
            let p = d.profiles[i].clone();
            edit_profile(w, d, p, i as i32);
        }
        None => {
            g.set_editing(false);
            g.set_provider_index(-1);
            g.set_preset_index(-1);
            show_profile(w, d);
        }
    }
    if !d.prompts.is_empty() {
        load_prompt(w, d, 0)?;
    }
    show_settings(w, d, s)?;
    match range {
        Some((start, end)) => set_range(w, d, start, end),
        None => invalidate(w, d),
    }
    g.set_status(
        if d.profiles.is_empty() {
            "先在设置中添加一个 AI 连接。总结和对话只保存在本机。"
        } else if d.profiles.iter().any(ProviderProfile::is_tested) {
            "总结和对话只保存在本机。"
        } else {
            "AI 连接需要先通过测试才能生成总结。"
        }
        .into(),
    );
    Ok(())
}

/// The records and notification settings as the fields show them.
fn read_settings(w: &AppWindow) -> Result<AiSettings> {
    let g = w.global::<AiState>();
    Ok(AiSettings {
        days: optional(&g.get_retention_days(), "保留天数")?,
        max_bytes: parse::<u64>(&g.get_retention_mib(), "空间上限")?
            .checked_mul(1024 * 1024)
            .ok_or_else(|| anyhow!("空间上限过大"))?,
        versions_to_keep: parse(&g.get_version_limit(), "版本数")?,
        compression_enabled: g.get_compression(),
        recent_messages: optional(&g.get_recent_messages(), "消息数")?,
        notify_success: g.get_notify_success(),
        notify_failure: g.get_notify_failure(),
    })
}
fn show_settings(w: &AppWindow, d: &UiData, s: &Storage) -> Result<()> {
    show_records(w, s)?;
    show_schedules(w, d, s, None)
}
fn show_records(w: &AppWindow, s: &Storage) -> Result<()> {
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
    Ok(())
}
/// Show the schedules as saved; `only` limits this to the daily (`true`) or the
/// interval (`false`) one, leaving what the other's fields hold.
fn show_schedules(w: &AppWindow, d: &UiData, s: &Storage, only: Option<bool>) -> Result<()> {
    let g = w.global::<AiState>();
    let (daily, interval) = (only != Some(false), only != Some(true));
    // Until a schedule is saved, offer the choices of a new summary.
    let provider = g.get_use_provider_index();
    let prompt = g.get_use_prompt_index().max(0);
    if daily {
        g.set_daily(false);
        g.set_daily_provider_index(provider);
        g.set_daily_prompt_index(prompt);
        g.set_daily_line("已关闭".into());
    }
    if interval {
        g.set_interval(false);
        g.set_interval_provider_index(provider);
        g.set_interval_prompt_index(prompt);
        g.set_interval_line("已关闭".into());
    }
    let now = crate::unix_time_ms();
    for schedule in s.ai_schedules()? {
        let provider = d
            .profiles
            .iter()
            .position(|p| p.id == schedule.provider_id)
            .map_or(-1, |i| i as i32);
        let prompt = d
            .prompts
            .iter()
            .position(|p| p.id == schedule.prompt_id)
            .map_or(-1, |i| i as i32);
        let line = schedule_line(&schedule, d, now);
        let retries = schedule
            .retries
            .map_or(String::new(), |v| v.to_string())
            .into();
        match schedule.kind {
            ScheduleKind::Daily { hour, minute } if daily => {
                g.set_daily(schedule.enabled);
                g.set_daily_time(format!("{hour:02}:{minute:02}").into());
                g.set_daily_retries(retries);
                g.set_daily_provider_index(provider);
                g.set_daily_prompt_index(prompt);
                g.set_daily_line(line.into());
            }
            ScheduleKind::Interval { hours, .. } if interval => {
                g.set_interval(schedule.enabled);
                g.set_interval_hours(hours.to_string().into());
                g.set_interval_retries(retries);
                g.set_interval_provider_index(provider);
                g.set_interval_prompt_index(prompt);
                g.set_interval_line(line.into());
            }
            _ => {}
        }
    }
    if daily {
        g.set_daily_saved_provider_index(g.get_daily_provider_index());
        g.set_daily_saved_prompt_index(g.get_daily_prompt_index());
        g.set_daily_dirty(false);
    }
    if interval {
        g.set_interval_saved_provider_index(g.get_interval_provider_index());
        g.set_interval_saved_prompt_index(g.get_interval_prompt_index());
        g.set_interval_dirty(false);
    }
    Ok(())
}
/// Refresh only the switches and status lines, leaving fields being edited.
fn show_schedule_lines(w: &AppWindow, d: &UiData, s: &Storage) -> Result<()> {
    let g = w.global::<AiState>();
    let now = crate::unix_time_ms();
    for schedule in s.ai_schedules()? {
        let line = schedule_line(&schedule, d, now).into();
        match schedule.kind {
            ScheduleKind::Daily { .. } => {
                g.set_daily(schedule.enabled);
                g.set_daily_line(line);
            }
            ScheduleKind::Interval { .. } => {
                g.set_interval(schedule.enabled);
                g.set_interval_line(line);
            }
        }
    }
    Ok(())
}
/// Whether a schedule's timing fields still say what is saved.
fn timing_saved(w: &AppWindow, saved: &Schedule) -> bool {
    let g = w.global::<AiState>();
    let (timing, retries) = match saved.kind {
        ScheduleKind::Daily { .. } => (g.get_daily_time(), g.get_daily_retries()),
        ScheduleKind::Interval { .. } => (g.get_interval_hours(), g.get_interval_retries()),
    };
    let same_timing = match saved.kind {
        ScheduleKind::Daily { hour, minute } => {
            timing
                .split_once(':')
                .and_then(|(h, m)| Some((h.trim().parse().ok()?, m.trim().parse().ok()?)))
                == Some((hour, minute))
        }
        ScheduleKind::Interval { hours, .. } => timing.trim().parse().ok() == Some(hours),
    };
    same_timing && optional::<u8>(&retries, "").ok() == Some(saved.retries)
}
/// Save what the settings fields hold and nobody saved yet. A field that does
/// not parse goes back to what is saved, and the status says why.
fn flush_settings(w: &AppWindow, d: &UiData, s: &Storage, sender: &Commands) -> Result<()> {
    let g = w.global::<AiState>();
    match read_settings(w) {
        Ok(settings) if settings != s.ai_settings()? => s.set_ai_settings(&settings)?,
        Ok(_) => {}
        Err(e) => {
            show_records(w, s)?;
            g.set_status(format!("{e}，已恢复为保存的值").into());
        }
    }
    for saved in s.ai_schedules()? {
        if timing_saved(w, &saved) {
            continue;
        }
        let daily = matches!(saved.kind, ScheduleKind::Daily { .. });
        if let Err(e) = save_schedule_or_revert(w, d, s, sender, daily, false) {
            g.set_status(e.to_string().into());
        }
    }
    Ok(())
}
/// Save a schedule; when that fails, its fields go back to what is saved and
/// the status says why. `choice` saves the connection and template the
/// pickers show, otherwise the saved ones stay.
fn save_schedule_or_revert(
    w: &AppWindow,
    d: &UiData,
    s: &Storage,
    sender: &Commands,
    daily: bool,
    choice: bool,
) -> Result<()> {
    let shown = pickers(w, daily);
    if let Err(e) = save_schedule(w, d, s, daily, choice) {
        show_schedules(w, d, s, Some(daily))?;
        // A pick that did not save stays on screen, still marked unsaved.
        set_pickers(w, daily, shown);
        return Err(e);
    }
    if !choice {
        // A picker changed but not saved stays as the user left it.
        set_pickers(w, daily, shown);
    }
    sender.send(AiCommand::Wake)?;
    Ok(())
}
/// A schedule's connection and template pickers, and whether a pick is unsaved.
fn pickers(w: &AppWindow, daily: bool) -> (i32, i32, bool) {
    let g = w.global::<AiState>();
    if daily {
        (
            g.get_daily_provider_index(),
            g.get_daily_prompt_index(),
            g.get_daily_dirty(),
        )
    } else {
        (
            g.get_interval_provider_index(),
            g.get_interval_prompt_index(),
            g.get_interval_dirty(),
        )
    }
}
fn set_pickers(w: &AppWindow, daily: bool, (provider, prompt, dirty): (i32, i32, bool)) {
    let g = w.global::<AiState>();
    if daily {
        g.set_daily_provider_index(provider);
        g.set_daily_prompt_index(prompt);
        g.set_daily_dirty(dirty);
    } else {
        g.set_interval_provider_index(provider);
        g.set_interval_prompt_index(prompt);
        g.set_interval_dirty(dirty);
    }
}
fn schedule_line(schedule: &Schedule, d: &UiData, now: i64) -> String {
    if let Some(reason) = &schedule.paused_reason {
        let reason = match reason.as_str() {
            "提供商已删除" | "关联配置已删除，请重新选择" => {
                "所用的连接或模板已删除，请重新选择"
            }
            "配置、模型或提示词需要重新测试与选择" => {
                "连接或模板已更改，请重新选择并测试"
            }
            other => other,
        };
        return format!("已暂停：{reason}");
    }
    if !schedule.enabled {
        return "已关闭".into();
    }
    let provider = d
        .profiles
        .iter()
        .find(|p| p.id == schedule.provider_id)
        .map_or("（连接已删除）", |p| p.name.as_str());
    let prompt = d
        .prompts
        .iter()
        .find(|p| p.id == schedule.prompt_id)
        .map_or("（模板已删除）", |p| p.name.as_str());
    format!(
        "下次：{} · {provider} / {} / {prompt}",
        when_text(schedule.next_due_utc_ms, now),
        schedule.model_id
    )
}
fn save_schedule(w: &AppWindow, d: &UiData, s: &Storage, daily: bool, choice: bool) -> Result<()> {
    let g = w.global::<AiState>();
    let id = if daily { "daily" } else { "interval" };
    let (enabled, provider, prompt, retries) = if daily {
        (
            g.get_daily(),
            g.get_daily_provider_index(),
            g.get_daily_prompt_index(),
            g.get_daily_retries(),
        )
    } else {
        (
            g.get_interval(),
            g.get_interval_provider_index(),
            g.get_interval_prompt_index(),
            g.get_interval_retries(),
        )
    };
    let kind = if daily {
        let time = g.get_daily_time();
        let (h, m) = time
            .split_once(':')
            .ok_or_else(|| anyhow!("每天执行时间的格式是 HH:MM"))?;
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
    let old = s.ai_schedules()?.into_iter().find(|v| v.id == id);
    let (profile, template) = match old.as_ref().filter(|_| !choice) {
        Some(o) => (
            d.profiles.iter().find(|p| p.id == o.provider_id),
            d.prompts.iter().find(|p| p.id == o.prompt_id),
        ),
        None => (
            d.profiles.get(provider as usize),
            d.prompts.get(prompt as usize),
        ),
    };
    if enabled {
        let p = profile.ok_or_else(|| anyhow!("请先选择自动总结使用的 AI 连接"))?;
        if !p.is_tested() {
            bail!("连接「{}」需要先通过测试", p.name);
        }
        if template.is_none() {
            bail!("请选择自动总结使用的总结模板");
        }
    }
    let (provider_id, model_id) = profile.map_or_else(
        || {
            old.as_ref().map_or_else(Default::default, |o| {
                (o.provider_id.clone(), o.model_id.clone())
            })
        },
        |p| (p.id.clone(), p.model.id.clone()),
    );
    let prompt_id = template.map_or_else(
        || {
            old.as_ref()
                .map_or_else(|| PromptPreset::default().id, |o| o.prompt_id.clone())
        },
        |p| p.id.clone(),
    );
    let mut schedule = Schedule {
        id: id.into(),
        enabled,
        provider_id,
        model_id,
        prompt_id,
        kind,
        next_due_utc_ms: 0,
        retries: optional(&retries, "重试次数")?,
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
    show_schedules(w, d, s, Some(daily))?;
    let name = if daily {
        "每日自动总结"
    } else {
        "间隔自动总结"
    };
    g.set_status(
        if let Some(p) = profile.filter(|_| enabled) {
            format!(
                "{name}已开启，使用「{}」，下次：{}",
                p.name,
                when_text(schedule.next_due_utc_ms, crate::unix_time_ms())
            )
        } else {
            format!("{name}已关闭")
        }
        .into(),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;

    fn zone() -> FixedOffset {
        FixedOffset::east_opt(8 * 3600).unwrap()
    }
    fn ms(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
        zone()
            .with_ymd_and_hms(y, mo, d, h, mi, 0)
            .unwrap()
            .timestamp_millis()
    }

    #[test]
    fn ranges_read_as_local_days_and_times() {
        let now = ms(2026, 10, 15, 14, 32);
        let z = zone();
        let today = (ms(2026, 10, 15, 0, 0), now);
        assert_eq!(
            range_text(today.0, today.1, now, &z, false),
            "10月15日 周四 00:00–14:32"
        );
        assert_eq!(
            range_text(today.0, today.1, now, &z, true),
            "今天 00:00–14:32"
        );
        let yesterday = (ms(2026, 10, 14, 0, 0), ms(2026, 10, 15, 0, 0));
        assert_eq!(
            range_text(yesterday.0, yesterday.1, now, &z, false),
            "10月14日 周三 全天"
        );
        assert_eq!(
            range_text(yesterday.0, yesterday.1, now, &z, true),
            "昨天 全天"
        );
        assert_eq!(
            range_text(
                ms(2026, 10, 14, 20, 0),
                ms(2026, 10, 15, 0, 0),
                now,
                &z,
                true
            ),
            "昨天 20:00–24:00"
        );
        assert_eq!(
            range_text(
                ms(2026, 10, 13, 22, 0),
                ms(2026, 10, 14, 1, 30),
                now,
                &z,
                true
            ),
            "10月13日 22:00 – 10月14日 01:30"
        );
        assert_eq!(
            range_text(
                ms(2026, 10, 5, 0, 0),
                ms(2026, 10, 12, 0, 0),
                now,
                &z,
                false
            ),
            "10月5日 周一 – 10月11日 周日"
        );
        assert_eq!(
            range_text(ms(2025, 12, 31, 0, 0), ms(2026, 1, 1, 0, 0), now, &z, true),
            "2025年12月31日 全天"
        );
    }

    #[test]
    fn durations_read_in_days_hours_and_minutes() {
        assert_eq!(
            duration_text(14 * 3_600_000 + 32 * 60_000),
            "共 14 小时 32 分"
        );
        assert_eq!(duration_text(3_600_000), "共 1 小时");
        assert_eq!(duration_text(7 * 86_400_000), "共 7 天");
        assert_eq!(duration_text(25 * 60_000), "共 25 分");
        assert_eq!(duration_text(30_000), "共不到 1 分钟");
    }

    #[test]
    fn usage_lists_only_reported_counts() {
        let usage = TokenUsage {
            input: Some(3210),
            output: Some(812),
            ..TokenUsage::default()
        };
        assert_eq!(usage_label(&usage), "用量：输入 3,210 · 输出 812 tokens");
        assert_eq!(usage_label(&TokenUsage::default()), "");
    }

    fn saved() -> credentials::Secrets {
        credentials::Secrets {
            api_key: "old-key".into(),
            headers: BTreeMap::from([("X-Tenant".into(), "a".into())]),
        }
    }

    #[test]
    fn a_new_key_keeps_the_saved_headers() {
        let merged = merge_secrets(Some(saved()), "new-key", None, false, false).unwrap();
        assert_eq!(merged.api_key, "new-key");
        assert_eq!(
            merged.headers.get("X-Tenant").map(String::as_str),
            Some("a")
        );
    }

    #[test]
    fn new_headers_keep_the_saved_key() {
        let headers = parse_headers("X-Org = b; X-Team=c").unwrap();
        let merged = merge_secrets(Some(saved()), "", headers, false, false).unwrap();
        assert_eq!(merged.api_key, "old-key");
        assert_eq!(merged.headers.len(), 2);
        assert_eq!(merged.headers.get("X-Org").map(String::as_str), Some("b"));
    }

    #[test]
    fn blank_fields_change_nothing_and_clears_are_explicit() {
        assert!(merge_secrets(Some(saved()), "", None, false, false).is_none());
        let cleared = merge_secrets(Some(saved()), "", None, true, false).unwrap();
        assert!(cleared.api_key.is_empty());
        assert_eq!(cleared.headers.len(), 1);
        let cleared = merge_secrets(Some(saved()), "", None, false, true).unwrap();
        assert_eq!(cleared.api_key, "old-key");
        assert!(cleared.headers.is_empty());
        // A clear wins over what was typed beside it.
        let cleared = merge_secrets(
            Some(saved()),
            "typed",
            parse_headers("X-Org=b").unwrap(),
            true,
            true,
        )
        .unwrap();
        assert!(cleared.api_key.is_empty() && cleared.headers.is_empty());
        // A first save writes even an empty entry, for services without keys.
        let first = merge_secrets(None, "", None, false, false).unwrap();
        assert!(first.api_key.is_empty() && first.headers.is_empty());
        assert!(parse_headers("no-equals-sign").is_err());
    }

    fn spec(start: i64, end: i64, kind: AiJobKind) -> timelens_storage::AiJobSpec {
        timelens_storage::AiJobSpec {
            kind,
            provider: ProviderProfile::preset(0, "p".into()),
            prompt: PromptPreset::default(),
            effective_prompt: String::new(),
            envelope: AiEnvelope {
                started_utc_ms: start,
                ended_utc_ms: end,
                ..AiEnvelope::default()
            },
            chunks: vec![],
            retry_limit: 0,
        }
    }
    fn version(id: i64, group: &str, created: i64) -> AiVersion {
        AiVersion {
            id,
            job_id: id * 100,
            group_key: group.into(),
            created_utc_ms: created,
            pinned: false,
            answer: String::new(),
            reasoning: String::new(),
            usage: TokenUsage::default(),
            snapshot: spec(1, 2, AiJobKind::Summary),
        }
    }
    fn job(id: i64, group: &str, state: &str, created: i64, kind: AiJobKind) -> AiJob {
        AiJob {
            id,
            state: state.into(),
            group_key: group.into(),
            created_utc_ms: created,
            attempts: 0,
            spec: spec(1, 2, kind),
            body: String::new(),
            reasoning: String::new(),
            error: None,
            usage: TokenUsage::default(),
        }
    }

    #[test]
    fn history_lists_versions_and_unfinished_summaries() {
        let versions = [
            version(1, "manual:1:2", 10),
            version(2, "manual:1:2", 30),
            version(3, "daily:5:6", 20),
        ];
        let follow_up = AiJobKind::FollowUp {
            version_id: 1,
            user_message_id: 9,
        };
        let jobs = [
            job(10, "manual:1:2", "failed", 40, AiJobKind::Summary),
            job(11, "manual:3:4", "failed", 50, AiJobKind::Summary),
            job(12, "manual:3:4", "failed", 45, AiJobKind::Summary),
            job(13, "manual:1:2", "running", 60, AiJobKind::Summary),
            job(14, "manual:7:8", "completed", 70, AiJobKind::Summary),
            job(15, "manual:1:2", "queued", 80, follow_up),
        ];
        let numbers = version_numbers(&versions);
        assert_eq!(numbers.get(&2), Some(&2));
        assert_eq!(numbers.get(&3), Some(&1));
        let rows = history(&versions, &jobs, &numbers, None, 100, &zone());
        let targets = rows.iter().map(|(t, _)| *t).collect::<Vec<_>>();
        assert_eq!(
            targets,
            [
                Target::Job(13),
                Target::Job(11),
                Target::Version(2),
                Target::Version(3),
                Target::Version(1),
            ]
        );
        assert_eq!(rows[0].1.state, 1);
        assert_eq!(rows[1].1.state, 2);
        assert!(rows[2].1.detail.contains("第 2 版"));
        // The job on screen stays listed even after its range has a version.
        let kept = history(&versions, &jobs, &numbers, Some(10), 100, &zone());
        assert!(kept.iter().any(|(t, _)| *t == Target::Job(10)));
    }
}
