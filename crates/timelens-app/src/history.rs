//! The home timeline: one continuous history read like a chat, earlier days above
//! and now at the bottom. Earlier days load as the user scrolls up; they are
//! cached because their records rarely change, while today follows the clock.
use std::{
    cell::RefCell,
    collections::BTreeMap,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use chrono::{Datelike, Local, NaiveDate, TimeZone};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use timelens_storage::{Storage, TimelineSnapshot};

use crate::{
    AiState, AppWindow, DayAnchor, HistoryItem, Participant, SegmentRow, ShareSlice, StackApp,
    UiState, app_icon, format_duration, grouped_count, refresh_timeline,
    timeline_view::{self, ActivityEntry, ParticipantRole, Unrecorded},
    try_lock_storage, unix_time_ms,
};
#[path = "history_cache.rs"]
mod paging;

// Row heights; the view lays rows out with these, and the sticky day header
// needs the same numbers to know which day is at the top.
const DAY_HEIGHT: f32 = 60.0;
const PERIOD_HEIGHT: f32 = 34.0;
const ENTRY_HEIGHT: f32 = 58.0;
const PAUSE_HEIGHT: f32 = 32.0;
const END_HEIGHT: f32 = 56.0;

const KIND_DAY: i32 = 0;
const KIND_PERIOD: i32 = 1;
const KIND_ENTRY: i32 = 2;
const KIND_PAUSE: i32 = 3;
const KIND_GAP: i32 = 4;
const KIND_END: i32 = 5;

/// Days fetched per scroll to the end.
const DAYS_PER_LOAD: i32 = 3;
/// Gaps without focus at least this long read as a break between entries.
const BREAK_MS: i64 = 15 * 60_000;
/// Yesterday is rebuilt this often: records delivered late, for example from the
/// collector's offline buffer, may still land there. Older days stay cached until
/// `reset` or `invalidate`.
const PAST_REFRESH: Duration = Duration::from_secs(30);
/// Bumped when every loaded day may read differently, such as after a merge.
static GENERATION: AtomicU64 = AtomicU64::new(0);
/// Other applications a row's card stack shows before "+N".
const STACK_SHOWN: usize = 2;

#[derive(Clone)]
struct DayEntry {
    key: String,
    entry: ActivityEntry,
    participants: Vec<timeline_view::Participant>,
}

struct Day {
    /// Days before today: 0 is today.
    offset: i32,
    date: NaiveDate,
    entries: Vec<Arc<DayEntry>>,
    unrecorded: Vec<Unrecorded>,
    totals: Vec<(String, u64)>,
    focus_ms: u64,
    source: Option<Arc<TimelineSnapshot>>,
}

pub struct HistoryState {
    days: BTreeMap<i32, Arc<Day>>,
    /// The oldest day offset fetched so far.
    loaded: i32,
    horizon: i32,
    query: String,
    selected: Option<String>,
    past_refreshed: Instant,
    generation: u64,
    paging: paging::Paging,
}

impl HistoryState {
    pub fn new() -> Self {
        Self {
            days: BTreeMap::new(),
            loaded: -1,
            horizon: 0,
            query: String::new(),
            selected: None,
            past_refreshed: Instant::now(),
            generation: GENERATION.load(Ordering::Acquire),
            paging: paging::Paging::default(),
        }
    }
}

fn day_bounds(offset: i32) -> Option<(i64, i64)> {
    timeline_view::selected_day_bounds(unix_time_ms(), -offset)
}

fn load_day(storage: &Storage, offset: i32, previous: Option<&Day>) -> anyhow::Result<Option<Day>> {
    let Some((start, end)) = day_bounds(offset) else {
        return Ok(None);
    };
    let end = end.min(unix_time_ms()).max(start + 1);
    let snapshot = if offset == 0 {
        storage.timeline_snapshot_cached(start, end)?
    } else {
        Arc::new(storage.timeline_snapshot(start, end)?)
    };
    let date = Local
        .timestamp_millis_opt(start)
        .single()
        .map(|d| d.date_naive())
        .unwrap_or_default();
    let mut day = build_day(offset, date, &snapshot, previous);
    day.source = (offset == 0).then_some(snapshot);
    Ok(Some(day))
}

fn earliest_change(old: &TimelineSnapshot, new: &TimelineSnapshot) -> i64 {
    if old.range_started_utc_ms != new.range_started_utc_ms
        || new.range_ended_utc_ms < old.range_ended_utc_ms
        || old.monitoring_gaps != new.monitoring_gaps
    {
        return new.range_started_utc_ms;
    }
    let mut earliest = old.range_ended_utc_ms;
    for app in &old.applications {
        let Some(next) = new.applications.iter().find(|a| a.identity == app.identity) else {
            return new.range_started_utc_ms;
        };
        if app.display_name != next.display_name {
            return new.range_started_utc_ms;
        }
        for index in 0..app.segments.len().max(next.segments.len()) {
            match (app.segments.get(index), next.segments.get(index)) {
                (Some(a), Some(b)) if a == b => {}
                (Some(a), Some(b))
                    if a.started_utc_ms == b.started_utc_ms
                        && a.displayed == b.displayed
                        && a.focused == b.focused
                        && a.inferred_tray == b.inferred_tray =>
                {
                    earliest = earliest.min(a.ended_utc_ms.min(b.ended_utc_ms));
                }
                (Some(a), Some(b)) => {
                    earliest = earliest.min(a.started_utc_ms.min(b.started_utc_ms))
                }
                (Some(a), None) | (None, Some(a)) => earliest = earliest.min(a.started_utc_ms),
                _ => {}
            }
        }
    }
    for app in &new.applications {
        if !old.applications.iter().any(|a| a.identity == app.identity) {
            for segment in &app.segments {
                earliest = earliest.min(segment.started_utc_ms);
            }
        }
    }
    earliest
}

fn build_day(
    offset: i32,
    date: NaiveDate,
    snapshot: &TimelineSnapshot,
    previous: Option<&Day>,
) -> Day {
    let boundary = previous
        .and_then(|day| {
            let old = day.source.as_ref()?;
            let changed = earliest_change(old, snapshot);
            // Grouping can merge neighbours repeatedly. Only a gap larger than the
            // existing grouping threshold makes a completed prefix independent.
            day.entries
                .windows(2)
                .filter_map(|pair| {
                    (pair[1].entry.started_utc_ms - pair[0].entry.ended_utc_ms
                        > timeline_view::BRIEF_ABSENCE_MS
                        && pair[1].entry.started_utc_ms <= changed)
                        .then_some(pair[1].entry.started_utc_ms)
                })
                .max()
        })
        .unwrap_or(snapshot.range_started_utc_ms);
    let mut tail = snapshot.clone();
    tail.range_started_utc_ms = boundary;
    let mut entries = previous
        .into_iter()
        .flat_map(|day| &day.entries)
        .filter(|e| boundary > snapshot.range_started_utc_ms && e.entry.ended_utc_ms <= boundary)
        .cloned()
        .collect::<Vec<_>>();
    entries.extend(
        timeline_view::build_activity_rows(&tail)
            .into_iter()
            .map(|entry| {
                Arc::new(DayEntry {
                    key: format!(
                        "{date}:{}:{}",
                        entry.started_utc_ms,
                        entry.identity.as_deref().unwrap_or_default()
                    ),
                    participants: timeline_view::participants(snapshot, &entry),
                    entry,
                })
            }),
    );
    let mut totals = BTreeMap::<String, u64>::new();
    for application in &snapshot.applications {
        *totals.entry(application.identity.clone()).or_default() += application.focused_ms;
    }
    let mut totals = totals
        .into_iter()
        .filter(|(_, focused)| *focused > 0)
        .collect::<Vec<_>>();
    totals.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    Day {
        offset,
        date,
        entries,
        unrecorded: timeline_view::unrecorded_spans(snapshot),
        totals,
        focus_ms: timeline_view::total_focus_ms(snapshot),
        source: (offset == 0).then(|| Arc::new(snapshot.clone())),
    }
}

fn day_title(day: &Day) -> (String, String) {
    let weekdays = [
        "星期一",
        "星期二",
        "星期三",
        "星期四",
        "星期五",
        "星期六",
        "星期日",
    ];
    let weekday = weekdays[day.date.weekday().num_days_from_monday() as usize];
    let date = day.date.format("%-m月%-d日").to_string();
    let this_year = Local::now().year() == day.date.year();
    let long_date = if this_year {
        date.clone()
    } else {
        day.date.format("%Y年%-m月%-d日").to_string()
    };
    match day.offset {
        0 => ("今天".into(), format!("{date} · {weekday}")),
        1 => ("昨天".into(), format!("{date} · {weekday}")),
        _ => (long_date, weekday.into()),
    }
}

fn clock(timestamp: i64) -> String {
    Local
        .timestamp_millis_opt(timestamp)
        .single()
        .map(|t| t.format("%H:%M").to_string())
        .unwrap_or_default()
}

fn period(timestamp: i64) -> &'static str {
    let hour = Local
        .timestamp_millis_opt(timestamp)
        .single()
        .map(|t| chrono::Timelike::hour(&t))
        .unwrap_or(0);
    match hour {
        0..=5 => "凌晨",
        6..=11 => "上午",
        12..=17 => "下午",
        _ => "晚上",
    }
}

fn shares(totals: &[(String, u64)]) -> ModelRc<ShareSlice> {
    let sum = totals.iter().map(|(_, ms)| *ms).sum::<u64>().max(1) as f32;
    ModelRc::new(VecModel::from(
        totals
            .iter()
            .map(|(identity, ms)| ShareSlice {
                ratio: *ms as f32 / sum,
                tint: app_icon::tint(identity),
            })
            .collect::<Vec<_>>(),
    ))
}

fn entry_note(participants: &[timeline_view::Participant]) -> String {
    let named = |p: &timeline_view::Participant| app_icon::display_name(&p.identity, &p.name);
    let interleaved = participants
        .iter()
        .filter(|p| p.role == ParticipantRole::Interleaved)
        .collect::<Vec<_>>();
    let same_screen = participants
        .iter()
        .filter(|p| p.role == ParticipantRole::SameScreen)
        .collect::<Vec<_>>();
    let mut parts = Vec::new();
    match same_screen.as_slice() {
        [] => {}
        [one] => parts.push(format!("同时使用 {}", named(one))),
        many => parts.push(format!("同时使用 {} 个应用", many.len())),
    }
    match interleaved.as_slice() {
        [] => {}
        [one] => parts.push(format!("穿插 {}", named(one))),
        many => parts.push(format!("穿插 {} 个应用", many.len())),
    }
    parts.join(" · ")
}

fn matches_query(entry: &DayEntry, query: &str) -> bool {
    query.is_empty()
        || entry.participants.iter().any(|p| {
            app_icon::display_name(&p.identity, &p.name)
                .to_lowercase()
                .contains(query)
        })
}

fn unrecorded_title(span: &Unrecorded) -> String {
    let why = if span.count > 1 {
        format!("记录中断 {} 次", span.count)
    } else {
        match span.reason.as_str() {
            "collector_restart" => "采集器未运行",
            "buffer_overflow" => "记录过多，部分丢失",
            "clock_discontinuity" => "系统时间跳变",
            _ => "原因未知",
        }
        .to_owned()
    };
    format!("未记录 {} · {why}", format_duration(span.missing_ms))
}

/// A history row before it becomes a view item.
enum Row<'a> {
    Entry(&'a DayEntry),
    Unrecorded(&'a Unrecorded),
}

impl Row<'_> {
    fn started(&self) -> i64 {
        match self {
            Row::Entry(item) => item.entry.started_utc_ms,
            Row::Unrecorded(span) => span.started_utc_ms,
        }
    }
}

/// Flatten loaded days into rows, earliest first, with the offsets of each day.
#[cfg(test)]
fn flatten(state: &HistoryState) -> (Vec<HistoryItem>, Vec<DayAnchor>) {
    flatten_days(
        state.days.values().rev().map(Arc::as_ref),
        &state.query,
        state.loaded,
        state.horizon,
        Some((f32::MIN, f32::MAX)),
    )
}

fn flatten_days<'a>(
    days: impl Iterator<Item = &'a Day>,
    query: &str,
    loaded: i32,
    horizon: i32,
    render: Option<(f32, f32)>,
) -> (Vec<HistoryItem>, Vec<DayAnchor>) {
    let query = query.trim().to_lowercase();
    let mut items = Vec::<HistoryItem>::new();
    let mut anchors = Vec::<DayAnchor>::new();
    let blank = || HistoryItem {
        kind: 0,
        key: SharedString::new(),
        height: 0.0,
        title: SharedString::new(),
        subtitle: SharedString::new(),
        time: SharedString::new(),
        duration: SharedString::new(),
        icon: Default::default(),
        tint: Default::default(),
        stack: ModelRc::default(),
        more: 0,
        shares: ModelRc::default(),
        rail_top: false,
        rail_bottom: false,
    };
    // More history waits above; the row loads it when clicked.
    let can_load = query.is_empty() && loaded >= 0 && loaded < horizon;
    items.push(HistoryItem {
        kind: KIND_END,
        key: "end".into(),
        height: END_HEIGHT,
        title: if !query.is_empty() {
            "搜索只覆盖最近一个月的记录"
        } else if can_load {
            "载入更早的记录"
        } else {
            "已显示保留期内的全部记录"
        }
        .into(),
        more: i32::from(can_load),
        ..blank()
    });
    let mut top = END_HEIGHT;
    for day in days {
        let entries = day
            .entries
            .iter()
            .filter(|e| matches_query(e, &query))
            .map(Arc::as_ref)
            .collect::<Vec<_>>();
        if entries.is_empty() && (day.offset != 0 || !query.is_empty()) {
            continue;
        }
        let mut rows = entries.iter().copied().map(Row::Entry).collect::<Vec<_>>();
        // Only interruptions between the day's activities show; before the first
        // or after the last the computer was most likely off.
        if query.is_empty()
            && let (Some(first), Some(last)) = (entries.first(), entries.last())
        {
            rows.extend(
                day.unrecorded
                    .iter()
                    .filter(|span| {
                        span.started_utc_ms >= first.entry.ended_utc_ms
                            && span.ended_utc_ms <= last.entry.started_utc_ms
                    })
                    .map(Row::Unrecorded),
            );
        }
        rows.sort_by_key(Row::started);
        let (title, subtitle) = day_title(day);
        let apps = format!("{} 个应用", day.totals.len());
        let duration = format!("聚焦 {}", format_duration(day.focus_ms));
        let day_top = top;
        items.push(HistoryItem {
            kind: KIND_DAY,
            key: format!("day:{}", day.date).into(),
            height: DAY_HEIGHT,
            title: title.clone().into(),
            subtitle: subtitle.clone().into(),
            time: apps.clone().into(),
            duration: duration.clone().into(),
            shares: if render.is_some_and(|(from, to)| top + DAY_HEIGHT >= from && top <= to) {
                shares(&day.totals)
            } else {
                ModelRc::default()
            },
            ..blank()
        });
        top += DAY_HEIGHT;
        if rows.is_empty() {
            items.push(HistoryItem {
                kind: KIND_PAUSE,
                key: format!("empty:{}", day.date).into(),
                height: PAUSE_HEIGHT,
                title: "今天还没有记录。使用电脑时，应用活动会按时间出现在这里。".into(),
                ..blank()
            });
            top += PAUSE_HEIGHT;
        }
        let mut current_period = "";
        // The end of the previous entry, unless an interruption already explains
        // the time since.
        let mut previous_end: Option<i64> = None;
        for row in rows {
            let label = period(row.started());
            if label != current_period {
                current_period = label;
                items.push(HistoryItem {
                    kind: KIND_PERIOD,
                    key: format!("period:{}:{label}", day.date).into(),
                    height: PERIOD_HEIGHT,
                    title: label.into(),
                    ..blank()
                });
                top += PERIOD_HEIGHT;
                previous_end = None;
            }
            match row {
                Row::Unrecorded(span) => {
                    items.push(HistoryItem {
                        kind: KIND_GAP,
                        key: format!("gap:{}", span.started_utc_ms).into(),
                        height: PAUSE_HEIGHT,
                        time: clock(span.started_utc_ms).into(),
                        title: unrecorded_title(span).into(),
                        ..blank()
                    });
                    top += PAUSE_HEIGHT;
                    previous_end = None;
                }
                Row::Entry(item) => {
                    let entry = &item.entry;
                    if query.is_empty()
                        && let Some(previous_end) = previous_end
                        && entry.started_utc_ms - previous_end >= BREAK_MS
                    {
                        items.push(HistoryItem {
                            kind: KIND_PAUSE,
                            key: format!("pause:{}:{previous_end}", day.date).into(),
                            height: PAUSE_HEIGHT,
                            title: format!(
                                "休息 {}",
                                format_duration(entry.started_utc_ms.abs_diff(previous_end))
                            )
                            .into(),
                            ..blank()
                        });
                        top += PAUSE_HEIGHT;
                    }
                    let identity = entry.identity.as_deref().unwrap_or_default();
                    let visible =
                        render.is_some_and(|(from, to)| top + ENTRY_HEIGHT >= from && top <= to);
                    let others = item
                        .participants
                        .iter()
                        .filter(|p| p.role != ParticipantRole::Main)
                        .collect::<Vec<_>>();
                    items.push(HistoryItem {
                        kind: KIND_ENTRY,
                        key: item.key.clone().into(),
                        height: ENTRY_HEIGHT,
                        title: if visible {
                            app_icon::display_name(identity, &entry.name).into()
                        } else {
                            SharedString::new()
                        },
                        subtitle: if visible {
                            entry_note(&item.participants).into()
                        } else {
                            SharedString::new()
                        },
                        time: clock(entry.started_utc_ms).into(),
                        duration: format_duration(entry.duration_ms).into(),
                        icon: if visible {
                            app_icon::for_identity(identity)
                        } else {
                            Default::default()
                        },
                        tint: if visible {
                            app_icon::tint(identity)
                        } else {
                            Default::default()
                        },
                        stack: if visible {
                            ModelRc::new(VecModel::from(
                                others
                                    .iter()
                                    .take(STACK_SHOWN)
                                    .map(|p| StackApp {
                                        name: app_icon::display_name(&p.identity, &p.name).into(),
                                        icon: app_icon::for_identity(&p.identity),
                                        tint: app_icon::tint(&p.identity),
                                    })
                                    .collect::<Vec<_>>(),
                            ))
                        } else {
                            ModelRc::default()
                        },
                        more: others.len().saturating_sub(STACK_SHOWN) as i32,
                        ..blank()
                    });
                    top += ENTRY_HEIGHT;
                    previous_end = Some(entry.ended_utc_ms);
                }
            }
        }
        anchors.push(DayAnchor {
            title: title.into(),
            subtitle: subtitle.into(),
            duration: duration.into(),
            apps: apps.into(),
            shares: if render.is_some_and(|(from, to)| top >= from && day_top <= to) {
                shares(&day.totals)
            } else {
                ModelRc::default()
            },
            top: day_top,
            bottom: top,
        });
    }
    // Connect the rail between neighbouring rows of the same stretch.
    for index in 0..items.len() {
        let joins = |item: &HistoryItem| matches!(item.kind, KIND_ENTRY | KIND_PAUSE | KIND_GAP);
        let rail_top = index > 0 && joins(&items[index - 1]) && joins(&items[index]);
        let rail_bottom =
            index + 1 < items.len() && joins(&items[index + 1]) && joins(&items[index]);
        items[index].rail_top = rail_top;
        items[index].rail_bottom = rail_bottom;
    }
    (items, anchors)
}

/// Bring the shown rows in line with `items`, touching only what changed: rows
/// usually arrive at the bottom (today) or the top (earlier days).
fn patch(model: &VecModel<HistoryItem>, items: Vec<HistoryItem>) {
    let old = model.iter().map(|item| item.key).collect::<Vec<_>>();
    let prefix = old
        .iter()
        .zip(&items)
        .take_while(|(old, new)| **old == new.key)
        .count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(items[prefix..].iter().rev())
        .take_while(|(old, new)| **old == new.key)
        .count();
    for _ in prefix..old.len() - suffix {
        model.remove(prefix);
    }
    let inserted = prefix..items.len() - suffix;
    for (index, item) in items.into_iter().enumerate() {
        if inserted.contains(&index) {
            model.insert(index, item);
        } else if model.row_data(index).as_ref() != Some(&item) {
            model.set_row_data(index, item);
        }
    }
}

#[cfg(test)]
fn row_tops(items: impl Iterator<Item = HistoryItem>) -> Vec<(SharedString, f32, f32)> {
    let mut top = 0.0;
    items
        .map(|item| {
            let row = (item.key, top, item.height);
            top += item.height;
            row
        })
        .collect()
}

fn reach(storage: &Storage) -> Option<i32> {
    let policy = storage.retention_policy().ok()?;
    let oldest = match storage.earliest_activity_utc_ms().ok()? {
        Some(timestamp) => {
            let date = Local.timestamp_millis_opt(timestamp).single()?.date_naive();
            (Local::now().date_naive() - date)
                .num_days()
                .clamp(0, i32::MAX as i64) as i32
        }
        None => 0,
    };
    Some(policy.days.map_or(oldest, |days| oldest.min(days as i32)))
}

pub fn refresh(
    window: &AppWindow,
    storage: &Arc<Mutex<Storage>>,
    state: &Rc<RefCell<HistoryState>>,
) -> anyhow::Result<()> {
    paging::refresh(window, storage, state)
}

/// Every loaded day reads differently, for example after merging applications;
/// loaded days are rebuilt on the next refresh.
pub fn invalidate() {
    GENERATION.fetch_add(1, Ordering::Release);
}

/// Changes whenever `invalidate` is called.
pub fn generation() -> u64 {
    GENERATION.load(Ordering::Acquire)
}

/// Every loaded day changes, for example after clearing or deleting history.
pub fn reset(state: &Rc<RefCell<HistoryState>>) {
    let mut state = state.borrow_mut();
    state.days.clear();
    paging::reset(&mut state);
    state.loaded = -1;
    state.selected = None;
}

fn find<'a>(state: &'a HistoryState, key: &str) -> Option<&'a DayEntry> {
    state
        .days
        .values()
        .flat_map(|day| &day.entries)
        .find(|entry| entry.key == key)
        .map(Arc::as_ref)
}

fn show_detail(
    window: &AppWindow,
    storage: &Arc<Mutex<Storage>>,
    entry: &DayEntry,
) -> anyhow::Result<()> {
    let (start, end) = (entry.entry.started_utc_ms, entry.entry.ended_utc_ms);
    let span = {
        let Some(storage) = try_lock_storage(storage)? else {
            return Ok(());
        };
        storage.timeline_snapshot(start, end.max(start + 1))?
    };
    let identity = entry.entry.identity.as_deref().unwrap_or_default();
    let length = end.saturating_sub(start).max(1) as f64;
    let participants = entry
        .participants
        .iter()
        .map(|participant| {
            let application = span
                .applications
                .iter()
                .find(|application| application.identity == participant.identity);
            let (focused, displayed, background) = application.map_or((0, 0, 0), |a| {
                (
                    a.focused_ms,
                    a.displayed_ms.saturating_sub(a.focused_ms),
                    a.background_ms,
                )
            });
            let tint = app_icon::tint(&participant.identity);
            let segments = application
                .map(|a| {
                    a.segments
                        .iter()
                        .map(|segment| SegmentRow {
                            offset: ((segment.started_utc_ms - start) as f64 / length)
                                .clamp(0.0, 1.0) as f32,
                            width: ((segment.ended_utc_ms - segment.started_utc_ms) as f64 / length)
                                .clamp(0.0, 1.0) as f32,
                            kind: if segment.focused {
                                2
                            } else if segment.displayed {
                                1
                            } else {
                                0
                            },
                            tint,
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            Participant {
                name: app_icon::display_name(&participant.identity, &participant.name).into(),
                icon: app_icon::for_identity(&participant.identity),
                tint,
                role: match participant.role {
                    ParticipantRole::Main => "主应用",
                    ParticipantRole::Interleaved => "穿插",
                    ParticipantRole::SameScreen => "同屏",
                }
                .into(),
                focused: format_duration(focused).into(),
                visible: format_duration(displayed).into(),
                background: format_duration(background).into(),
                segments: ModelRc::new(VecModel::from(segments)),
            }
        })
        .collect::<Vec<_>>();
    // The row's own application leads the pane; the list holds the others.
    let (main, others): (Vec<_>, Vec<_>) = entry
        .participants
        .iter()
        .zip(participants)
        .partition(|(participant, _)| participant.role == ParticipantRole::Main);
    let others = others.into_iter().map(|(_, row)| row).collect::<Vec<_>>();
    let date = Local
        .timestamp_millis_opt(start)
        .single()
        .map(|d| d.format("%-m月%-d日").to_string())
        .unwrap_or_default();
    window.set_detail_name(app_icon::display_name(identity, &entry.entry.name).into());
    window.set_detail_icon(app_icon::for_identity(identity));
    window.set_detail_tint(app_icon::tint(identity));
    window.set_detail_when(format!("{date} · {}–{}", clock(start), clock(end)).into());
    window.set_detail_duration(format_duration(entry.entry.duration_ms).into());
    window.set_detail_note(
        if others.is_empty() {
            "一直保持聚焦，没有切换到其他应用".to_owned()
        } else {
            format!("期间还用了 {} 个应用", others.len())
        }
        .into(),
    );
    window.set_detail_main(
        main.into_iter()
            .next()
            .map(|(_, row)| row)
            .unwrap_or_default(),
    );
    window.set_detail_participants(ModelRc::new(VecModel::from(others)));
    window.set_detail_keyboard(grouped_count(span.keyboard_count).into());
    window.set_detail_mouse(
        grouped_count(
            span.left_click_count
                .saturating_add(span.middle_click_count)
                .saturating_add(span.right_click_count),
        )
        .into(),
    );
    window.set_detail_open(true);
    Ok(())
}

/// Point the other pages at the selected entry's span.
fn focus_range(
    window: &AppWindow,
    storage: &Arc<Mutex<Storage>>,
    ui_state: &Rc<RefCell<UiState>>,
    entry: &DayEntry,
) {
    let (start, end) = (entry.entry.started_utc_ms, entry.entry.ended_utc_ms);
    if let Some((day_start, day_end)) = timeline_view::selected_day_bounds(start, 0) {
        let mut state = ui_state.borrow_mut();
        state.horizon_started_utc_ms = day_start;
        state.horizon_ended_utc_ms = day_end;
        state.calendar_day = Some(day_start);
        state.range_started_utc_ms = start;
        state.range_ended_utc_ms = end.max(start + 1);
        state.follow_now = false;
        state.selected_identity = entry.entry.identity.clone();
    }
    if let Err(error) = refresh_timeline(window, storage, ui_state) {
        window.set_action_status(format!("刷新失败：{error}").into());
    }
}

pub fn install(
    window: &AppWindow,
    storage: Arc<Mutex<Storage>>,
    ui_state: Rc<RefCell<UiState>>,
    state: Rc<RefCell<HistoryState>>,
) {
    {
        let weak = window.as_weak();
        let storage = Arc::clone(&storage);
        let state = Rc::clone(&state);
        window.on_history_viewport_changed(move || {
            if let Some(window) = weak.upgrade() {
                paging::viewport(&window, &storage, &state);
            }
        });
    }
    {
        let weak = window.as_weak();
        let storage = Arc::clone(&storage);
        let state = Rc::clone(&state);
        window.on_history_data_ready(move || {
            if let Some(window) = weak.upgrade()
                && let Err(error) = paging::refresh(&window, &storage, &state)
            {
                window.set_action_status(format!("读取历史失败：{error}").into());
            }
        });
    }
    {
        let weak = window.as_weak();
        let storage = Arc::clone(&storage);
        let state = Rc::clone(&state);
        window.on_history_load_more(move || {
            if let Some(window) = weak.upgrade() {
                paging::more(&window, &storage, &state);
            }
        });
    }
    {
        let weak = window.as_weak();
        let storage = Arc::clone(&storage);
        let state = Rc::clone(&state);
        window.on_history_search(move |query| {
            if let Some(window) = weak.upgrade() {
                paging::search(&window, &storage, &state, &query);
            }
        });
    }
    {
        let weak = window.as_weak();
        let storage = Arc::clone(&storage);
        let state = Rc::clone(&state);
        window.on_history_select(move |key| {
            let Some(window) = weak.upgrade() else {
                return;
            };
            let mut s = state.borrow_mut();
            let Some(entry) = find(&s, &key) else {
                return;
            };
            if let Err(error) = show_detail(&window, &storage, entry) {
                window.set_action_status(format!("读取详情失败：{error}").into());
                return;
            }
            s.selected = Some(key.to_string());
            window.set_history_selected(key);
        });
    }
    {
        let weak = window.as_weak();
        let state = Rc::clone(&state);
        window.on_history_close(move || {
            state.borrow_mut().selected = None;
            if let Some(window) = weak.upgrade() {
                window.set_detail_open(false);
                window.set_history_selected(SharedString::new());
            }
        });
    }
    {
        let weak = window.as_weak();
        let storage = Arc::clone(&storage);
        let state = Rc::clone(&state);
        window.on_history_jump(move |date| {
            let Some(window) = weak.upgrade() else {
                return;
            };
            let Ok(date) = NaiveDate::parse_from_str(date.trim(), "%Y-%m-%d") else {
                window.set_action_status("请输入有效日期，格式为 YYYY-MM-DD".into());
                return;
            };
            let offset = (Local::now().date_naive() - date).num_days();
            if offset < 0 {
                window.set_action_status("日期不能晚于今天".into());
                return;
            }
            let s = state.borrow();
            if offset as i32 > s.horizon {
                window.set_action_status("这一天早于最早的记录".into());
                return;
            }
            drop(s);
            paging::jump(&window, &storage, &state, offset as i32);
        });
    }
    // Actions from the detail pane act on the selected entry's span.
    {
        let weak = window.as_weak();
        let storage = Arc::clone(&storage);
        let state = Rc::clone(&state);
        let ui_state = Rc::clone(&ui_state);
        window.on_detail_action(move |action| {
            let Some(window) = weak.upgrade() else {
                return;
            };
            let s = state.borrow();
            let Some(entry) = s.selected.as_deref().and_then(|key| find(&s, key)) else {
                return;
            };
            if action == 2 {
                // The rules page follows the selected application.
                ui_state.borrow_mut().selected_identity = entry.entry.identity.clone();
                return;
            }
            focus_range(&window, &storage, &ui_state, entry);
            match action {
                0 => window.invoke_navigate(2),
                1 => {
                    window.global::<AiState>().set_page(0);
                    window.invoke_navigate(4);
                }
                _ => {}
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use timelens_storage::{TimelineApplication, TimelineSegment};

    pub(super) fn history_fixture(end: i64) -> TimelineSnapshot {
        let start = 1_700_000_000_000;
        let spans = [(start, start + 300_000), (start + 900_000, end)];
        TimelineSnapshot {
            range_started_utc_ms: start,
            range_ended_utc_ms: end,
            applications: vec![TimelineApplication {
                identity: "fixture".into(),
                display_name: "Fixture".into(),
                opened_ms: 0,
                displayed_ms: 0,
                focused_ms: 0,
                background_ms: 0,
                window_count: 0,
                keyboard_count: 0,
                left_click_count: 0,
                middle_click_count: 0,
                right_click_count: 0,
                windows: vec![].into(),
                segments: spans
                    .into_iter()
                    .map(|(began, ended)| TimelineSegment {
                        started_utc_ms: began,
                        ended_utc_ms: ended,
                        displayed: true,
                        focused: true,
                        inferred_tray: false,
                    })
                    .collect(),
            }],
            keyboard_count: 0,
            left_click_count: 0,
            middle_click_count: 0,
            right_click_count: 0,
            monitoring_gaps: vec![],
        }
    }

    #[test]
    fn completed_history_prefix_and_live_tail_match_a_full_rebuild() {
        let first = history_fixture(1_700_001_500_000);
        let date = Local
            .timestamp_millis_opt(first.range_started_utc_ms)
            .single()
            .unwrap()
            .date_naive();
        let day = build_day(0, date, &first, None);
        let next = history_fixture(first.range_ended_utc_ms + 60_000);
        assert_eq!(earliest_change(&first, &next), first.range_ended_utc_ms);
        let cached = build_day(0, date, &next, Some(&day));
        let full = build_day(0, date, &next, None);
        assert_eq!(
            cached
                .entries
                .iter()
                .map(|e| (&e.key, &e.entry, &e.participants))
                .collect::<Vec<_>>(),
            full.entries
                .iter()
                .map(|e| (&e.key, &e.entry, &e.participants))
                .collect::<Vec<_>>()
        );
        assert_eq!(cached.focus_ms, full.focus_ms);
    }

    #[test]
    fn late_changes_and_renames_rebuild_the_affected_history_prefix() {
        let first = history_fixture(1_700_001_500_000);
        let date = Local
            .timestamp_millis_opt(first.range_started_utc_ms)
            .single()
            .unwrap()
            .date_naive();
        let day = build_day(0, date, &first, None);
        let mut next = history_fixture(first.range_ended_utc_ms + 60_000);
        next.applications[0].segments[0].focused = false;
        next.applications[0].display_name = "Updated".into();
        assert_eq!(earliest_change(&first, &next), first.range_started_utc_ms);
        let cached = build_day(0, date, &next, Some(&day));
        let full = build_day(0, date, &next, None);
        assert_eq!(
            cached.entries.iter().map(|e| &e.entry).collect::<Vec<_>>(),
            full.entries.iter().map(|e| &e.entry).collect::<Vec<_>>()
        );
    }

    fn row(key: &str, title: &str) -> HistoryItem {
        HistoryItem {
            key: key.into(),
            title: title.into(),
            height: 10.0,
            ..Default::default()
        }
    }

    fn keys(model: &VecModel<HistoryItem>) -> Vec<String> {
        model.iter().map(|item| item.key.to_string()).collect()
    }

    #[test]
    fn patch_inserts_earlier_days_above_and_new_rows_below_in_place() {
        let model = VecModel::from(vec![row("end", ""), row("a", "A"), row("b", "B")]);
        // An earlier day arrives above, today's last row changes and grows.
        patch(
            &model,
            vec![
                row("end", ""),
                row("x", "X"),
                row("y", "Y"),
                row("a", "A"),
                row("b", "B, longer"),
                row("c", "C"),
            ],
        );
        assert_eq!(keys(&model), ["end", "x", "y", "a", "b", "c"]);
        assert_eq!(model.row_data(4).unwrap().title, "B, longer");
        // The row that was second now starts two rows lower.
        let tops = row_tops(model.iter());
        assert_eq!((tops[3].0.as_str(), tops[3].1), ("a", 30.0));

        patch(&model, vec![row("end", ""), row("c", "C")]);
        assert_eq!(keys(&model), ["end", "c"]);
        patch(&model, Vec::new());
        assert_eq!(model.row_count(), 0);
    }
}
