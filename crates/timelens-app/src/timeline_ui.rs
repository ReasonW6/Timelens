//! Native presentation wiring. Storage remains the source of every displayed fact.
use crate::{
    ActivityRow, AiState, AppWindow, CollectionState, DataState, DayRow, SegmentRow, UiState,
    app_icon, format_duration, refresh_timeline, timeline_view, ui_model, unix_time_ms,
};
use chrono::{Datelike, Local, TimeZone};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
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
    let weak = window.as_weak();
    window.on_navigate(move |page| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        window.set_action_status("".into());
        window.set_page(page.clamp(0, 5));
        window.set_snapshot_open(page == 2);
        window.set_report_open(page == 3);
        window
            .global::<AiState>()
            .set_open(page == 4 || (page == 5 && window.get_settings_tab() == 2));
        window.global::<DataState>().set_open(false);
        window.global::<CollectionState>().set_open(false);
        match page {
            2 => window.invoke_snapshot_opened(),
            3 if window.get_stats_tab() == 1 => {
                window
                    .global::<CollectionState>()
                    .set_date(window.get_calendar_date());
                window.global::<CollectionState>().invoke_action(7);
            }
            4 => window.global::<AiState>().invoke_opened(),
            _ => {}
        }
    });
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
            *state.borrow_mut() = today_state();
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
            match local_time_range(anchor, &start, &end, unix_time_ms()) {
                Ok((start, end)) => {
                    let mut s = state.borrow_mut();
                    s.horizon_started_utc_ms = start;
                    s.horizon_ended_utc_ms = end;
                    s.range_started_utc_ms = start;
                    s.range_ended_utc_ms = end;
                    s.follow_now = false;
                    s.selected_activity = None;
                    drop(s);
                    w.set_action_status("".into());
                    refresh(&w, &storage, &state);
                }
                Err(message) => w.set_action_status(message.into()),
            }
        });
    }
    {
        let weak = window.as_weak();
        window.on_activity_selected(move |index| {
            if busy.load(Ordering::Acquire) || index < 0 {
                return;
            }
            let mut s = state.borrow_mut();
            let Some(entry) = s.activity_rows.get(index as usize).cloned() else {
                return;
            };
            let Some(identity) = entry.identity else {
                return;
            };
            s.selected_identity = Some(identity.clone());
            s.selected_activity = Some((identity, entry.started_utc_ms));
            drop(s);
            if let Some(w) = weak.upgrade() {
                refresh(&w, &storage, &state);
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
    if end > now {
        return Err("所选时段尚未结束，请将结束时间设为当前时刻之前");
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

pub fn render(window: &AppWindow, state: &Rc<RefCell<UiState>>, snapshot: &TimelineSnapshot) {
    let mut state = state.borrow_mut();
    let anchor = state
        .calendar_day
        .unwrap_or(snapshot.range_ended_utc_ms.saturating_sub(1));
    let now_date = Local::now().date_naive();
    window.set_previous_year(
        Local
            .timestamp_millis_opt(anchor)
            .single()
            .filter(|date| date.year() != now_date.year())
            .map(|date| date.year().to_string())
            .unwrap_or_default()
            .into(),
    );
    window.set_date_title(local_label(anchor, "%-m月%-d日").into());
    window.set_calendar_date(local_label(anchor, "%Y-%m-%d").into());
    let weekdays = [
        "星期一",
        "星期二",
        "星期三",
        "星期四",
        "星期五",
        "星期六",
        "星期日",
    ];
    window.set_weekday_title(
        Local
            .timestamp_millis_opt(anchor)
            .single()
            .map(|d| weekdays[d.weekday().num_days_from_monday() as usize])
            .unwrap_or("")
            .into(),
    );
    window.set_can_next_day(
        Local
            .timestamp_millis_opt(anchor)
            .single()
            .is_some_and(|d| d.date_naive() < now_date),
    );
    let days = timeline_view::week_days(anchor)
        .into_iter()
        .map(|day| {
            let enabled = chrono::NaiveDate::parse_from_str(&day.date, "%Y-%m-%d")
                .is_ok_and(|d| d <= now_date);
            DayRow {
                date: chrono::NaiveDate::parse_from_str(&day.date, "%Y-%m-%d")
                    .map(|d| d.format("%-m/%-d").to_string())
                    .unwrap_or(day.date)
                    .into(),
                weekday: day.weekday.trim_start_matches('周').into(),
                offset: day.offset,
                selected: day.selected,
                today: day.is_today,
                enabled,
            }
        })
        .collect::<Vec<_>>();
    window.set_week(ui_model::sync(window.get_week(), days, |a, b| {
        a.date == b.date
    }));
    let same_day = local_label(snapshot.range_started_utc_ms, "%Y-%m-%d")
        == local_label(snapshot.range_ended_utc_ms.saturating_sub(1), "%Y-%m-%d");
    let time_format = if same_day { "%H:%M" } else { "%-m/%-d %H:%M" };
    window.set_multiple_days(!same_day);
    window.set_range_time(
        format!(
            "{}–{}",
            local_label(snapshot.range_started_utc_ms, time_format),
            if same_day && local_label(snapshot.range_ended_utc_ms, "%H:%M") == "00:00" {
                "24:00".to_owned()
            } else {
                local_label(snapshot.range_ended_utc_ms, time_format)
            }
        )
        .into(),
    );
    window.set_focus_total(format_duration(timeline_view::total_focus_ms(snapshot)).into());
    window.set_has_gaps(!snapshot.monitoring_gaps.is_empty());
    let gap_count = snapshot
        .monitoring_gaps
        .iter()
        .map(|gap| (gap.started_utc_ms, gap.ended_utc_ms, &gap.reason))
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    window.set_coverage_value(
        if snapshot.applications.is_empty() && snapshot.monitoring_gaps.is_empty() {
            "当前时段尚无活动记录".into()
        } else if snapshot.monitoring_gaps.is_empty() {
            "未发现已知记录缺口".into()
        } else {
            format!("{gap_count} 段明确记录缺口").into()
        },
    );

    let entries = timeline_view::build_activity_rows(snapshot);
    let selected_activity = state.selected_activity.as_ref().and_then(|(id, start)| {
        entries
            .iter()
            .position(|e| e.identity.as_ref() == Some(id) && e.started_utc_ms == *start)
    });
    let mut previous_group = String::new();
    let rows = entries
        .iter()
        .map(|entry| {
            let group = if entry.group == previous_group {
                String::new()
            } else {
                previous_group = entry.group.clone();
                if same_day {
                    entry
                        .group
                        .rsplit(" · ")
                        .next()
                        .unwrap_or(&entry.group)
                        .to_owned()
                } else {
                    entry.group.clone()
                }
            };
            ActivityRow {
                identity: format!(
                    "{}:{}:{}",
                    entry.identity.as_deref().unwrap_or("gap"),
                    entry.started_utc_ms,
                    if entry.gap { &entry.detail } else { "" }
                )
                .into(),
                name: entry
                    .identity
                    .as_deref()
                    .map(|id| app_icon::display_name(id, &entry.name))
                    .unwrap_or_else(|| entry.name.clone())
                    .into(),
                time: local_label(entry.started_utc_ms, "%H:%M").into(),
                interval: format!(
                    "{}–{}",
                    local_label(entry.started_utc_ms, "%H:%M"),
                    local_label(entry.ended_utc_ms, "%H:%M")
                )
                .into(),
                duration: format_duration(entry.duration_ms).into(),
                group: group.into(),
                gap: entry.gap,
                detail: entry.detail.clone().into(),
                icon: entry
                    .identity
                    .as_deref()
                    .map(app_icon::for_identity)
                    .unwrap_or_default(),
            }
        })
        .collect::<Vec<_>>();
    window.set_activity(ui_model::sync(window.get_activity(), rows, |a, b| {
        a.identity == b.identity
    }));
    window.set_activity_selected_index(selected_activity.map_or(-1, |i| i as i32));
    window.set_selected_is_activity(selected_activity.is_some());
    if let Some(index) = selected_activity {
        let entry = &entries[index];
        window.set_selected_start(local_label(entry.started_utc_ms, "%H:%M").into());
        window.set_selected_end(local_label(entry.ended_utc_ms, "%H:%M").into());
        window.set_selected_duration(format_duration(entry.duration_ms).into());
        window.set_selected_interval(
            format!(
                "{} · {}–{}",
                local_label(entry.started_utc_ms, "%-m月%-d日"),
                local_label(entry.started_utc_ms, "%H:%M"),
                local_label(entry.ended_utc_ms, "%H:%M")
            )
            .into(),
        );
    } else {
        state.selected_activity = None;
    }
    if let Some(app) = state.selected_identity.as_ref().and_then(|identity| {
        snapshot
            .applications
            .iter()
            .find(|app| &app.identity == identity)
    }) {
        window.set_selected_icon(app_icon::for_identity(&app.identity));
        window
            .set_keyboard_value(format!("{} 次", crate::grouped_count(app.keyboard_count)).into());
        window.set_mouse_value(
            format!(
                "{} 次",
                crate::grouped_count(
                    app.left_click_count
                        .saturating_add(app.middle_click_count)
                        .saturating_add(app.right_click_count)
                )
            )
            .into(),
        );
    } else {
        window.set_selected_icon(slint::Image::default());
        window.set_keyboard_value("0 次".into());
        window.set_mouse_value("0 次".into());
    }
    state.activity_rows = entries;
}

pub fn render_overview(window: &AppWindow, state: &UiState, snapshot: &TimelineSnapshot) {
    let start = state.horizon_started_utc_ms;
    let end = state.horizon_ended_utc_ms;
    let span = end.saturating_sub(start).max(1) as f64;
    let normalize = |a: i64, b: i64, kind: i32| -> Option<SegmentRow> {
        let a = a.max(start);
        let b = b.min(end);
        (b > a).then(|| SegmentRow {
            offset: ((a - start) as f64 / span) as f32,
            width: ((b - a) as f64 / span) as f32,
            kind,
        })
    };
    let mut rows = Vec::new();
    for (index, app) in snapshot.applications.iter().enumerate() {
        rows.extend(
            app.segments
                .iter()
                .filter(|s| s.focused)
                .filter_map(|s| normalize(s.started_utc_ms, s.ended_utc_ms, (index % 4) as i32)),
        );
    }
    rows.extend(
        snapshot
            .monitoring_gaps
            .iter()
            .filter(|g| g.data_class == "activity")
            .filter_map(|g| normalize(g.started_utc_ms, g.ended_utc_ms, -1)),
    );
    window.set_overview_segments(ModelRc::new(VecModel::from(rows)));
    let axis_format = if end - start > 86_400_000 {
        "%-m/%-d %H:%M"
    } else {
        "%H:%M"
    };
    window.set_axis_start(local_label(start, axis_format).into());
    window.set_axis_middle(local_label(start + (end - start) / 2, axis_format).into());
    window.set_axis_end(
        if state.calendar_day.is_some_and(|day| {
            timeline_view::selected_day_bounds(day, 0).is_some_and(|(_, day_end)| day_end == end)
        }) {
            "24:00".into()
        } else {
            local_label(end, axis_format).into()
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert!(local_time_range(anchor, "09:00", "18:00", anchor).is_err());
        assert_eq!(
            local_time_range(anchor, "09:00", "24:00", now).unwrap().1,
            now
        );
    }
}
