//! Native presentation wiring. Storage remains the source of every displayed fact.
use crate::{
    AppWindow, CollectionState, DataState, UiState, refresh_timeline, timeline_view, ui_model,
    unix_time_ms,
};
use chrono::{Datelike, Local, TimeZone};
use slint::{ComponentHandle, Model};
use std::{
    cell::RefCell,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use timelens_storage::{Storage, TimelineSnapshot};

pub fn today_state() -> UiState {
    state_for_day(unix_time_ms(), 0).unwrap_or_else(|| UiState::recent_hours(24))
}

pub fn filter_apps(window: &AppWindow) {
    let query = window.get_app_search().trim().to_lowercase();
    let rows = window
        .get_apps()
        .iter()
        .filter(|row| row.name.to_lowercase().contains(&query))
        .collect();
    window.set_filtered_apps(ui_model::sync(window.get_filtered_apps(), rows, |a, b| {
        a.identity == b.identity
    }));
}

fn state_for_day(anchor: i64, offset: i32) -> Option<UiState> {
    let (start, end) = timeline_view::selected_day_bounds(anchor, offset)?;
    let now = unix_time_ms();
    if start > now {
        return None;
    }
    let mut state = UiState::recent_hours(24);
    state.horizon_started_utc_ms = start;
    state.horizon_ended_utc_ms = end;
    state.range_started_utc_ms = start;
    state.range_ended_utc_ms = end.min(now).max(start + 1);
    state.calendar_day = Some(start);
    state.follow_now = now < end;
    Some(state)
}

pub fn install(
    window: &AppWindow,
    storage: Arc<Mutex<Storage>>,
    state: Rc<RefCell<UiState>>,
    busy: Arc<AtomicBool>,
) {
    let weak = window.as_weak();
    window.on_app_search_changed(move || {
        if let Some(w) = weak.upgrade() {
            filter_apps(&w);
        }
    });
    {
        // The window's timer gathers the totals for the new scope.
        let state = state.clone();
        window.on_apps_scope_changed(move || {
            state.borrow_mut().apps.requested = None;
        });
    }
    let weak = window.as_weak();
    window.on_navigate(move |page| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        window.set_action_status("".into());
        if page != 2 && window.get_snapshot_selecting() {
            window.invoke_snapshot_check_all(false);
            window.set_snapshot_selecting(false);
        }
        window.set_page(page.clamp(0, 3));
        window.set_snapshot_open(page == 2);
        window.global::<DataState>().set_open(false);
        window.global::<CollectionState>().set_open(false);
        match page {
            2 => window.invoke_snapshot_opened(),
            3 if window.get_stats_tab() == 1 => {
                // The application rules page shares this panel; switch it back.
                window.global::<CollectionState>().set_page(1);
                window.global::<CollectionState>().invoke_action(7);
            }
            _ => {}
        }
    });
    {
        // Leaving a page the timeline pointed at an activity puts its range back.
        let weak = window.as_weak();
        let state = state.clone();
        let storage = storage.clone();
        window.on_range_origin_restore(move || {
            let Some(origin) = state.borrow_mut().origin.take() else {
                return;
            };
            origin.restore(&mut state.borrow_mut());
            if let Some(w) = weak.upgrade() {
                refresh(&w, &storage, &state);
            }
        });
    }
    {
        let weak = window.as_weak();
        let state = state.clone();
        let storage = storage.clone();
        let busy = busy.clone();
        window.on_date_selected(move |date| {
            if busy.load(Ordering::Acquire) {
                return;
            }
            let Some(w) = weak.upgrade() else {
                return;
            };
            let parsed = chrono::NaiveDate::parse_from_str(date.trim(), "%Y-%m-%d")
                .ok()
                .and_then(|d| d.and_hms_opt(12, 0, 0))
                .and_then(|d| Local.from_local_datetime(&d).earliest())
                .and_then(|d| state_for_day(d.timestamp_millis(), 0));
            if let Some(mut next) = parsed {
                next.selected_identity = state.borrow().selected_identity.clone();
                *state.borrow_mut() = next;
                w.set_action_status("".into());
                refresh(&w, &storage, &state);
            } else {
                w.set_action_status("请输入有效日期（YYYY-MM-DD），日期不能晚于今天".into());
            }
        });
    }
    {
        let weak = window.as_weak();
        let state = state.clone();
        let storage = storage.clone();
        let busy = busy.clone();
        window.on_day_selected(move |offset| {
            if busy.load(Ordering::Acquire) {
                return;
            }
            let anchor = {
                let s = state.borrow();
                s.calendar_day
                    .unwrap_or(s.range_ended_utc_ms.saturating_sub(1))
            };
            let Some(mut next) = state_for_day(anchor, offset) else {
                return;
            };
            next.selected_identity = state.borrow().selected_identity.clone();
            *state.borrow_mut() = next;
            if let Some(w) = weak.upgrade() {
                refresh(&w, &storage, &state);
            }
        });
    }
    {
        let weak = window.as_weak();
        let state = state.clone();
        let storage = storage.clone();
        let busy = busy.clone();
        window.on_today_selected(move || {
            if busy.load(Ordering::Acquire) {
                return;
            }
            let mut next = today_state();
            next.selected_identity = state.borrow().selected_identity.clone();
            *state.borrow_mut() = next;
            if let Some(w) = weak.upgrade() {
                refresh(&w, &storage, &state);
            }
        });
    }
    {
        let weak = window.as_weak();
        let state = state.clone();
        let storage = storage.clone();
        let busy = busy.clone();
        window.on_days_ago_selected(move |days| {
            if busy.load(Ordering::Acquire) {
                return;
            }
            let Some(mut next) = state_for_day(unix_time_ms(), -days.max(0)) else {
                return;
            };
            next.selected_identity = state.borrow().selected_identity.clone();
            *state.borrow_mut() = next;
            if let Some(w) = weak.upgrade() {
                refresh(&w, &storage, &state);
            }
        });
    }
    {
        let weak = window.as_weak();
        let state = state.clone();
        let storage = storage.clone();
        let busy = busy.clone();
        window.on_time_range_selected(move |start, end| {
            if busy.load(Ordering::Acquire) {
                return;
            }
            let Some(w) = weak.upgrade() else {
                return;
            };
            let anchor = {
                let s = state.borrow();
                s.calendar_day
                    .unwrap_or(s.range_ended_utc_ms.saturating_sub(1))
            };
            let now = unix_time_ms();
            match local_time_range(anchor, &start, &end, now) {
                Ok((start, end)) => {
                    let mut s = state.borrow_mut();
                    s.origin = None;
                    s.horizon_started_utc_ms = start;
                    s.horizon_ended_utc_ms = end;
                    s.range_started_utc_ms = start;
                    // A range that has not ended yet grows with the clock, like today.
                    s.range_ended_utc_ms = end.min(now);
                    s.follow_now = s.calendar_day.is_some() && end > now;
                    drop(s);
                    w.set_action_status("".into());
                    refresh(&w, &storage, &state);
                }
                Err(message) => w.set_action_status(message.into()),
            }
        });
    }
}

fn local_time_range(
    anchor: i64,
    start: &str,
    end: &str,
    now: i64,
) -> Result<(i64, i64), &'static str> {
    let day = Local
        .timestamp_millis_opt(anchor)
        .single()
        .ok_or("所选日期不可用")?
        .date_naive();
    let parse = |value: &str, end_boundary: bool| {
        let local = if value.trim() == "24:00" && end_boundary {
            day.succ_opt()?.and_hms_opt(0, 0, 0)?
        } else {
            day.and_time(chrono::NaiveTime::parse_from_str(value.trim(), "%H:%M").ok()?)
        };
        Local
            .from_local_datetime(&local)
            .earliest()
            .map(|date| date.timestamp_millis())
    };
    let start = parse(start, false).ok_or("请输入有效的开始时间，格式为 HH:MM")?;
    let end = parse(end, true).ok_or("请输入有效的结束时间，格式为 HH:MM")?;
    if end <= start {
        return Err("结束时间需要晚于开始时间");
    }
    if start >= now {
        return Err("所选时段还没有开始，请选择当前时刻之前的开始时间");
    }
    Ok((start, end))
}

fn refresh(window: &AppWindow, storage: &Arc<Mutex<Storage>>, state: &Rc<RefCell<UiState>>) {
    if let Err(error) = refresh_timeline(window, storage, state) {
        window.set_action_status(format!("刷新失败：{error}").into());
    }
}

fn local_label(timestamp: i64, format: &str) -> String {
    Local
        .timestamp_millis_opt(timestamp)
        .single()
        .map(|v| v.format(format).to_string())
        .unwrap_or_else(|| "时间不可用".to_owned())
}

/// A local date as the number yyyymmdd.
fn ymd(timestamp: i64) -> i32 {
    Local
        .timestamp_millis_opt(timestamp)
        .single()
        .map_or(0, |d| {
            d.year() * 10_000 + d.month() as i32 * 100 + d.day() as i32
        })
}

/// Where the shared range ends as the user chose it: a range that runs with the
/// clock ends where it was planned to, any other where it was set.
pub(crate) fn chosen_end(state: &UiState) -> i64 {
    if state.follow_now {
        state.horizon_ended_utc_ms
    } else {
        state.range_ended_utc_ms
    }
}

/// Whether the shared range is the whole of its calendar day.
fn whole_day(state: &UiState) -> bool {
    state.calendar_day.is_some_and(|day| {
        state.range_started_utc_ms == day
            && timeline_view::selected_day_bounds(day, 0).map(|(_, end)| end)
                == Some(chosen_end(state))
    })
}

/// The shared range in words: a day by its date, part of a day with its hours,
/// or a recent stretch by its length.
fn range_title(state: &UiState, now: i64) -> String {
    let Some(day) = state.calendar_day else {
        if !state.follow_now {
            // A fixed stretch over several days, such as a summary's.
            return crate::timeline_view::range_text(
                state.range_started_utc_ms,
                state.range_ended_utc_ms,
                now,
                &Local,
                true,
            );
        }
        let hours = state
            .horizon_ended_utc_ms
            .saturating_sub(state.horizon_started_utc_ms)
            / 3_600_000;
        return if hours >= 48 && hours % 24 == 0 {
            format!("最近 {} 天", hours / 24)
        } else {
            format!("最近 {hours} 小时")
        };
    };
    let date = |timestamp: i64| {
        Local
            .timestamp_millis_opt(timestamp)
            .single()
            .map(|d| d.date_naive())
    };
    let (Some(date), Some(today)) = (date(day), date(now)) else {
        return "时间不可用".to_owned();
    };
    let name = if date == today {
        "今天 · "
    } else if today.pred_opt() == Some(date) {
        "昨天 · "
    } else {
        ""
    };
    let weekday = ["周一", "周二", "周三", "周四", "周五", "周六", "周日"]
        [date.weekday().num_days_from_monday() as usize];
    let format = if date.year() == today.year() {
        "%-m月%-d日"
    } else {
        "%Y年%-m月%-d日"
    };
    let label = format!("{name}{} {weekday}", date.format(format));
    if whole_day(state) {
        return label;
    }
    let end_ms = chosen_end(state);
    let end = if timeline_view::selected_day_bounds(day, 0).map(|(_, end)| end) == Some(end_ms) {
        "24:00".to_owned()
    } else {
        local_label(end_ms, "%H:%M")
    };
    format!(
        "{label} · {}–{end}",
        local_label(state.range_started_utc_ms, "%H:%M")
    )
}

pub fn render(window: &AppWindow, state: &Rc<RefCell<UiState>>, snapshot: &TimelineSnapshot) {
    let state = state.borrow();
    let anchor = state
        .calendar_day
        .unwrap_or(snapshot.range_ended_utc_ms.saturating_sub(1));
    let now = unix_time_ms();
    let now_date = Local::now().date_naive();
    window.set_calendar_date(local_label(anchor, "%Y-%m-%d").into());
    window.set_range_title(range_title(&state, now).into());
    window.set_range_origin(
        state
            .origin
            .as_ref()
            .map_or_else(Default::default, |origin| origin.label.as_str().into()),
    );
    window.set_today_ymd(ymd(now));
    window.set_selected_ymd(state.calendar_day.map_or(0, ymd));
    window.set_can_next_day(
        Local
            .timestamp_millis_opt(anchor)
            .single()
            .is_some_and(|d| d.date_naive() < now_date),
    );
    // Only the whole of today counts; part of it leaves "today" to choose.
    window.set_is_today(
        whole_day(&state)
            && Local
                .timestamp_millis_opt(anchor)
                .single()
                .is_some_and(|d| d.date_naive() == now_date),
    );
}

/// What the application totals cover: all records since the first, or the
/// chosen range.
pub fn apps_range_label(snapshot: &TimelineSnapshot, range_start: i64, all: bool) -> String {
    if !all {
        let same_day = local_label(range_start, "%Y-%m-%d")
            == local_label(snapshot.range_ended_utc_ms.saturating_sub(1), "%Y-%m-%d");
        let format = if same_day {
            "%H:%M"
        } else {
            "%-m月%-d日 %H:%M"
        };
        format!(
            "{}{}–{}",
            if same_day {
                local_label(range_start, "%-m月%-d日 ")
            } else {
                String::new()
            },
            local_label(range_start, format),
            match local_label(snapshot.range_ended_utc_ms, format) {
                end if same_day && end == "00:00" => "24:00".to_owned(),
                end => end,
            }
        )
    } else if snapshot.applications.is_empty() {
        "全部记录".to_owned()
    } else {
        format!(
            "全部记录 · {}起",
            local_label(range_start, "%Y年%-m月%-d日")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_all_time_snapshot_keeps_the_first_activity_date_in_its_label() {
        let start = Local
            .with_ymd_and_hms(2026, 9, 5, 12, 0, 0)
            .earliest()
            .unwrap()
            .timestamp_millis();
        let snapshot = TimelineSnapshot {
            range_started_utc_ms: 1,
            range_ended_utc_ms: start + 60_000,
            applications: vec![timelens_storage::TimelineApplication {
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
                windows: Default::default(),
                segments: Default::default(),
            }],
            keyboard_count: 0,
            left_click_count: 0,
            middle_click_count: 0,
            right_click_count: 0,
            monitoring_gaps: Vec::new(),
        };
        assert_eq!(
            apps_range_label(&snapshot, start, true),
            "全部记录 · 2026年9月5日起"
        );
        assert_eq!(
            apps_range_label(&snapshot, start, false),
            "9月5日 12:00–12:01"
        );
        assert_eq!(snapshot.range_started_utc_ms, 1);
    }

    #[test]
    fn exact_local_range_and_invalid_or_future_ranges() {
        let anchor = Local
            .with_ymd_and_hms(2026, 9, 5, 12, 0, 0)
            .earliest()
            .unwrap()
            .timestamp_millis();
        let now = Local
            .with_ymd_and_hms(2026, 9, 6, 0, 0, 0)
            .earliest()
            .unwrap()
            .timestamp_millis();
        let (start, end) = local_time_range(anchor, "09:00", "18:00", now).unwrap();
        assert_eq!(end - start, 9 * 3_600_000);
        assert_eq!(local_label(start, "%H:%M"), "09:00");
        assert_eq!(local_label(end, "%H:%M"), "18:00");
        assert!(local_time_range(anchor, "18:00", "09:00", now).is_err());
        assert!(local_time_range(anchor, "oops", "18:00", now).is_err());
        // An unfinished range is allowed; one that has not started is not.
        assert!(local_time_range(anchor, "09:00", "18:00", anchor).is_ok());
        assert!(local_time_range(anchor, "13:00", "18:00", anchor).is_err());
        assert_eq!(
            local_time_range(anchor, "09:00", "24:00", now).unwrap().1,
            now
        );
    }

    #[test]
    fn range_titles_name_days_their_hours_and_recent_stretches() {
        let at = |day: u32, hour: u32| {
            Local
                .with_ymd_and_hms(2026, 9, day, hour, 0, 0)
                .earliest()
                .unwrap()
                .timestamp_millis()
        };
        let now = at(10, 9);
        let mut state = state_for_day(now, 0).unwrap();
        assert_eq!(ymd(now), 20260910);
        assert_eq!(range_title(&state, now), "今天 · 9月10日 周四");
        state = state_for_day(now, -1).unwrap();
        assert_eq!(range_title(&state, now), "昨天 · 9月9日 周三");
        let (start, end) = local_time_range(at(5, 12), "09:00", "24:00", now).unwrap();
        state = state_for_day(at(5, 12), 0).unwrap();
        state.range_started_utc_ms = start;
        state.horizon_ended_utc_ms = end;
        assert_eq!(range_title(&state, now), "9月5日 周六 · 09:00–24:00");
        // An entry picked on the timeline, or a summary's span, within the day.
        state.range_started_utc_ms = at(5, 9);
        state.range_ended_utc_ms = at(5, 10);
        state.follow_now = false;
        assert_eq!(range_title(&state, now), "9月5日 周六 · 09:00–10:00");
        assert!(!whole_day(&state));
        state = state_for_day(now, 0).unwrap();
        assert!(whole_day(&state));
        state.range_started_utc_ms = at(10, 8);
        assert!(!whole_day(&state));
        assert_eq!(range_title(&UiState::recent_hours(168), now), "最近 7 天");
        assert_eq!(range_title(&UiState::recent_hours(24), now), "最近 24 小时");
    }
}
