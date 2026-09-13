//! Calendar and activity projections shared by the native timeline views.
//!
//! Intervals are half-open UTC ranges. Calendar navigation and row grouping use
//! the computer's local time zone; neither requires a UI model or stored titles.

use std::collections::BTreeMap;

use chrono::{
    Datelike, Days, Duration, Local, NaiveDate, NaiveDateTime, Offset, TimeZone, Timelike,
};
use timelens_storage::{TimelineGap, TimelineSnapshot};

type Interval = (i64, i64);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivityEntry {
    pub identity: Option<String>,
    pub name: String,
    pub started_utc_ms: i64,
    pub ended_utc_ms: i64,
    pub duration_ms: u64,
    pub group: String,
    pub gap: bool,
    pub detail: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DayEntry {
    /// Full local calendar date, in YYYY-MM-DD form.
    pub date: String,
    pub weekday: String,
    /// Calendar-day difference from the date containing the supplied anchor.
    pub offset: i32,
    pub is_today: bool,
    pub selected: bool,
}

struct FocusGroup<'a> {
    name: &'a str,
    intervals: Vec<Interval>,
}

/// Build chronological focus records and explicit monitoring-gap records.
///
/// Each application's touching or overlapping focus intervals are united before
/// splitting only at local day boundaries. Each row's morning/afternoon group
/// follows its starting time. Distinct identities remain distinct, including
/// when their display names or observed intervals coincide.
/// Gaps retain their data class and reason; an input gap does not erase an
/// independently observed activity interval.
pub fn build_activity_rows(snapshot: &TimelineSnapshot) -> Vec<ActivityEntry> {
    build_activity_rows_in(snapshot, &Local)
}

fn build_activity_rows_in<Tz: TimeZone>(
    snapshot: &TimelineSnapshot,
    timezone: &Tz,
) -> Vec<ActivityEntry> {
    let mut applications = BTreeMap::<&str, FocusGroup<'_>>::new();
    for application in &snapshot.applications {
        let group = applications
            .entry(&application.identity)
            .or_insert_with(|| FocusGroup {
                name: &application.display_name,
                intervals: Vec::new(),
            });
        if group.name.trim().is_empty() {
            group.name = &application.display_name;
        }
        group.intervals.extend(
            application
                .segments
                .iter()
                .filter(|segment| segment.focused)
                .filter_map(|segment| {
                    clipped_interval(snapshot, segment.started_utc_ms, segment.ended_utc_ms)
                }),
        );
    }

    let mut rows = Vec::new();
    for (identity, application) in applications {
        let name = if application.name.trim().is_empty() {
            "未命名应用"
        } else {
            application.name
        };
        for (started, ended) in union_intervals(application.intervals) {
            for (started, ended, group) in grouped_intervals(timezone, started, ended) {
                rows.push(ActivityEntry {
                    identity: Some(identity.to_owned()),
                    name: name.to_owned(),
                    started_utc_ms: started,
                    ended_utc_ms: ended,
                    duration_ms: ended.abs_diff(started),
                    group,
                    gap: false,
                    detail: "应用聚焦记录".to_owned(),
                });
            }
        }
    }

    let mut gap_groups = BTreeMap::<(i64, i64, &str), Vec<&TimelineGap>>::new();
    for gap in &snapshot.monitoring_gaps {
        let Some((started, ended)) =
            clipped_interval(snapshot, gap.started_utc_ms, gap.ended_utc_ms)
        else {
            continue;
        };
        gap_groups
            .entry((started, ended, &gap.reason))
            .or_default()
            .push(gap);
    }
    for ((started, ended, _), gaps) in gap_groups {
        let (mut name, mut detail) = gap_labels(gaps[0]);
        if gaps.iter().any(|g| g.data_class != gaps[0].data_class) {
            name = "记录缺口".to_owned();
            let classes = gaps
                .iter()
                .map(|g| gap_labels(g).1.split(" · ").next().unwrap_or("").to_owned())
                .collect::<std::collections::BTreeSet<_>>();
            let reason = detail.split(" · ").nth(1).unwrap_or("").to_owned();
            detail = format!(
                "{} · {}",
                classes.into_iter().collect::<Vec<_>>().join("、"),
                reason
            );
        }
        for (started, ended, group) in grouped_intervals(timezone, started, ended) {
            rows.push(ActivityEntry {
                identity: None,
                name: name.clone(),
                started_utc_ms: started,
                ended_utc_ms: ended,
                duration_ms: ended.abs_diff(started),
                group,
                gap: true,
                detail: detail.clone(),
            });
        }
    }

    rows.sort_by(|left, right| {
        left.started_utc_ms
            .cmp(&right.started_utc_ms)
            .then_with(|| right.gap.cmp(&left.gap))
            .then_with(|| left.ended_utc_ms.cmp(&right.ended_utc_ms))
            .then_with(|| left.identity.cmp(&right.identity))
            .then_with(|| left.detail.cmp(&right.detail))
    });
    rows
}

/// Total observed focus time, counting overlapping applications only once.
/// Stored per-application summary counters are deliberately not summed.
pub fn total_focus_ms(snapshot: &TimelineSnapshot) -> u64 {
    let intervals = snapshot
        .applications
        .iter()
        .flat_map(|application| &application.segments)
        .filter(|segment| segment.focused)
        .filter_map(|segment| {
            clipped_interval(snapshot, segment.started_utc_ms, segment.ended_utc_ms)
        })
        .collect();
    union_intervals(intervals)
        .into_iter()
        .fold(0_u64, |total, (started, ended)| {
            total.saturating_add(ended.abs_diff(started))
        })
}

/// Return [start, end) for a local calendar day relative to the anchor's date.
///
/// Boundaries are resolved separately, so a DST day may contain 23 or 25 hours.
/// Repeated midnight uses its first occurrence; a skipped midnight advances to
/// the first valid instant. A wholly skipped date or an out-of-range input
/// returns None instead of silently selecting a different date.
pub fn selected_day_bounds(anchor: i64, day_delta: i32) -> Option<(i64, i64)> {
    selected_day_bounds_in(&Local, anchor, day_delta)
}

fn selected_day_bounds_in<Tz: TimeZone>(
    timezone: &Tz,
    anchor: i64,
    day_delta: i32,
) -> Option<Interval> {
    let anchor_date = local_datetime_at(timezone, anchor)?.date();
    let days = Days::new(u64::from(day_delta.unsigned_abs()));
    let date = if day_delta >= 0 {
        anchor_date.checked_add_days(days)?
    } else {
        anchor_date.checked_sub_days(days)?
    };
    let next_date = date.succ_opt()?;
    let started = resolve_local_boundary(timezone, date.and_hms_opt(0, 0, 0)?)?;
    let ended = resolve_local_boundary(timezone, next_date.and_hms_opt(0, 0, 0)?)?;
    (started < ended && local_datetime_at(timezone, started)?.date() == date)
        .then_some((started, ended))
}

/// Seven real local dates, Monday through Sunday, containing the anchor date.
/// `selected` refers to the anchor; `is_today` refers to the current local date.
/// An unrepresentable anchor or incomplete week returns an empty vector.
pub fn week_days(anchor: i64) -> Vec<DayEntry> {
    let Some(selected) = local_datetime_at(&Local, anchor).map(|value| value.date()) else {
        return Vec::new();
    };
    week_days_for_date(selected, Local::now().date_naive())
}

fn week_days_for_date(selected: NaiveDate, today: NaiveDate) -> Vec<DayEntry> {
    let selected_index = selected.weekday().num_days_from_monday();
    let Some(monday) = selected.checked_sub_days(Days::new(u64::from(selected_index))) else {
        return Vec::new();
    };
    if monday.checked_add_days(Days::new(6)).is_none() {
        return Vec::new();
    }
    let weekdays = ["周一", "周二", "周三", "周四", "周五", "周六", "周日"];
    (0..7)
        .filter_map(|index| {
            let date = monday.checked_add_days(Days::new(index))?;
            Some(DayEntry {
                date: date.format("%Y-%m-%d").to_string(),
                weekday: weekdays[index as usize].to_owned(),
                offset: index as i32 - selected_index as i32,
                is_today: date == today,
                selected: date == selected,
            })
        })
        .collect()
}

fn clipped_interval(snapshot: &TimelineSnapshot, started: i64, ended: i64) -> Option<Interval> {
    let started = started.max(snapshot.range_started_utc_ms);
    let ended = ended.min(snapshot.range_ended_utc_ms);
    (started < ended).then_some((started, ended))
}

fn union_intervals(mut intervals: Vec<Interval>) -> Vec<Interval> {
    intervals.retain(|(started, ended)| started < ended);
    intervals.sort_unstable();
    let mut merged = Vec::<Interval>::new();
    for (started, ended) in intervals {
        match merged.last_mut() {
            Some((_, previous_end)) if started <= *previous_end => {
                *previous_end = (*previous_end).max(ended);
            }
            _ => merged.push((started, ended)),
        }
    }
    merged
}

fn grouped_intervals<Tz: TimeZone>(
    timezone: &Tz,
    started: i64,
    ended: i64,
) -> Vec<(i64, i64, String)> {
    let mut groups = Vec::new();
    let mut cursor = started;
    while cursor < ended {
        let Some(local) = local_datetime_at(timezone, cursor) else {
            // Retain the observed range even if Chrono cannot display its date.
            groups.push((cursor, ended, "日期未知".to_owned()));
            break;
        };
        let morning = local.hour() < 12;
        let boundary = local
            .date()
            .succ_opt()
            .and_then(|date| date.and_hms_opt(0, 0, 0));
        let stop = boundary
            .and_then(|boundary| resolve_local_boundary(timezone, boundary))
            .filter(|boundary| *boundary > cursor)
            .unwrap_or(ended)
            .min(ended);
        let period = if morning { "上午" } else { "下午" };
        groups.push((
            cursor,
            stop,
            format!("{} · {period}", local.date().format("%Y-%m-%d")),
        ));
        cursor = stop;
    }
    groups
}

fn local_datetime_at<Tz: TimeZone>(timezone: &Tz, timestamp: i64) -> Option<NaiveDateTime> {
    let datetime = timezone.timestamp_millis_opt(timestamp).single()?;
    datetime
        .naive_utc()
        .checked_add_offset(datetime.offset().fix())
}

fn resolve_local_boundary<Tz: TimeZone>(timezone: &Tz, local: NaiveDateTime) -> Option<i64> {
    if let Some(datetime) = timezone.from_local_datetime(&local).earliest() {
        return Some(datetime.timestamp_millis());
    }

    // Midnight can be skipped by a time-zone transition. Locate the first valid
    // minute, then the exact second at which that gap ends. Chrono offsets have
    // whole-second precision. The 48-hour bound also covers a skipped date.
    for minutes in 1..=48 * 60 {
        let probe = local.checked_add_signed(Duration::minutes(minutes))?;
        if timezone.from_local_datetime(&probe).earliest().is_none() {
            continue;
        }
        let mut lower = (minutes - 1) * 60;
        let mut upper = minutes * 60;
        while lower + 1 < upper {
            let middle = lower + (upper - lower) / 2;
            let candidate = local.checked_add_signed(Duration::seconds(middle))?;
            if timezone
                .from_local_datetime(&candidate)
                .earliest()
                .is_some()
            {
                upper = middle;
            } else {
                lower = middle;
            }
        }
        let first = local.checked_add_signed(Duration::seconds(upper))?;
        return timezone
            .from_local_datetime(&first)
            .earliest()
            .map(|datetime| datetime.timestamp_millis());
    }
    None
}

fn gap_labels(gap: &TimelineGap) -> (String, String) {
    let (name, data_class) = match gap.data_class.as_str() {
        "activity" => ("活动记录缺口", "应用活动".to_owned()),
        "input" => ("输入记录缺口", "键盘与鼠标计数".to_owned()),
        other => ("记录缺口", format!("未知记录类型（{other}）")),
    };
    let reason = match gap.reason.as_str() {
        "collector_restart" => "采集器重启".to_owned(),
        "buffer_overflow" => "采集缓冲区溢出".to_owned(),
        "input_overflow" => "输入缓冲区溢出".to_owned(),
        "retention_time" => "已按保留时间清理".to_owned(),
        "retention_space" => "已按空间上限清理".to_owned(),
        "user_deleted" => "用户已删除".to_owned(),
        other => format!("未知原因（{other}）"),
    };
    (name.to_owned(), format!("{data_class} · {reason}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, LocalResult};
    use timelens_storage::{TimelineApplication, TimelineSegment};

    const MINUTE: i64 = 60_000;

    fn zone() -> FixedOffset {
        FixedOffset::east_opt(8 * 3_600).unwrap()
    }

    fn timestamp(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> i64 {
        zone()
            .with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .unwrap()
            .timestamp_millis()
    }

    fn application(identity: &str, intervals: &[(i64, i64)]) -> TimelineApplication {
        TimelineApplication {
            identity: identity.to_owned(),
            display_name: "同名应用".to_owned(),
            opened_ms: 0,
            displayed_ms: 0,
            focused_ms: u64::MAX,
            background_ms: 0,
            window_count: intervals.len(),
            keyboard_count: 0,
            left_click_count: 0,
            middle_click_count: 0,
            right_click_count: 0,
            windows: Vec::new(),
            segments: intervals
                .iter()
                .map(|&(started_utc_ms, ended_utc_ms)| TimelineSegment {
                    started_utc_ms,
                    ended_utc_ms,
                    displayed: true,
                    focused: true,
                    inferred_tray: false,
                })
                .collect(),
        }
    }

    fn snapshot(
        started: i64,
        ended: i64,
        applications: Vec<TimelineApplication>,
    ) -> TimelineSnapshot {
        TimelineSnapshot {
            range_started_utc_ms: started,
            range_ended_utc_ms: ended,
            applications,
            keyboard_count: 0,
            left_click_count: 0,
            middle_click_count: 0,
            right_click_count: 0,
            monitoring_gaps: Vec::new(),
        }
    }

    #[test]
    fn overlapping_windows_and_touching_intervals_do_not_multiply_focus_time() {
        let start = timestamp(2026, 9, 5, 9, 0);
        let mut app = application(
            "editor",
            &[
                (start + 10 * MINUTE, start + 25 * MINUTE),
                (start, start + 12 * MINUTE),
                (start + 25 * MINUTE, start + 30 * MINUTE),
                (start + 45 * MINUTE, start + 60 * MINUTE),
            ],
        );
        app.segments.push(TimelineSegment {
            started_utc_ms: start + 30 * MINUTE,
            ended_utc_ms: start + 45 * MINUTE,
            displayed: true,
            focused: false,
            inferred_tray: false,
        });
        let snapshot = snapshot(start, start + 60 * MINUTE, vec![app]);
        let rows = build_activity_rows_in(&snapshot, &zone());
        assert_eq!(rows.len(), 2);
        assert_eq!(
            (rows[0].started_utc_ms, rows[0].ended_utc_ms),
            (start, start + 30 * MINUTE)
        );
        assert_eq!(
            (rows[1].started_utc_ms, rows[1].ended_utc_ms),
            (start + 45 * MINUTE, start + 60 * MINUTE)
        );
        assert_eq!(total_focus_ms(&snapshot), 45 * MINUTE as u64);
        assert!(rows.iter().all(|row| row.detail == "应用聚焦记录"));
    }

    #[test]
    fn interleaved_same_name_applications_keep_their_identities_and_union_total() {
        let start = timestamp(2026, 9, 5, 9, 0);
        let snapshot = snapshot(
            start,
            start + 60 * MINUTE,
            vec![
                application(
                    "editor",
                    &[
                        (start, start + 20 * MINUTE),
                        (start + 40 * MINUTE, start + 50 * MINUTE),
                    ],
                ),
                application("browser", &[(start + 10 * MINUTE, start + 30 * MINUTE)]),
                application("terminal", &[(start + 45 * MINUTE, start + 60 * MINUTE)]),
            ],
        );
        let rows = build_activity_rows_in(&snapshot, &zone());
        let identities = rows
            .iter()
            .map(|row| row.identity.as_deref())
            .collect::<Vec<_>>();
        assert_eq!(
            identities,
            vec![
                Some("editor"),
                Some("browser"),
                Some("editor"),
                Some("terminal")
            ]
        );
        assert_eq!(
            rows.iter().map(|row| row.duration_ms).sum::<u64>(),
            65 * MINUTE as u64
        );
        assert_eq!(total_focus_ms(&snapshot), 50 * MINUTE as u64);
    }

    #[test]
    fn query_boundaries_clip_activity_and_gaps_and_drop_empty_or_reversed_ranges() {
        let start = timestamp(2026, 9, 5, 9, 0);
        let end = start + 60 * MINUTE;
        let mut snapshot = snapshot(
            start,
            end,
            vec![application(
                "editor",
                &[
                    (start - 5 * MINUTE, start),
                    (start - MINUTE, start + MINUTE),
                    (end - MINUTE, end + MINUTE),
                    (end, end + 5 * MINUTE),
                    (end, start),
                    (start + 30 * MINUTE, start + 30 * MINUTE),
                ],
            )],
        );
        snapshot.monitoring_gaps = vec![
            TimelineGap {
                data_class: "activity".into(),
                started_utc_ms: start - MINUTE,
                ended_utc_ms: start + 2 * MINUTE,
                reason: "collector_restart".into(),
            },
            TimelineGap {
                data_class: "input".into(),
                started_utc_ms: end,
                ended_utc_ms: end + MINUTE,
                reason: "input_overflow".into(),
            },
        ];
        let rows = build_activity_rows_in(&snapshot, &zone());
        assert_eq!(rows.len(), 3);
        assert!(rows[0].gap);
        assert_eq!(
            (rows[0].started_utc_ms, rows[0].ended_utc_ms),
            (start, start + 2 * MINUTE)
        );
        assert!(
            rows.iter()
                .all(|row| row.started_utc_ms >= start && row.ended_utc_ms <= end)
        );
        assert_eq!(total_focus_ms(&snapshot), 2 * MINUTE as u64);
        snapshot.range_ended_utc_ms = start;
        assert!(build_activity_rows_in(&snapshot, &zone()).is_empty());
        assert_eq!(total_focus_ms(&snapshot), 0);
    }

    #[test]
    fn monitoring_gaps_preserve_data_class_reason_and_chronological_position() {
        let start = timestamp(2026, 9, 5, 9, 0);
        let mut snapshot = snapshot(
            start,
            start + 60 * MINUTE,
            vec![application("editor", &[(start, start + 5 * MINUTE)])],
        );
        for (data_class, reason, minute) in [
            ("input", "input_overflow", 30),
            ("activity", "buffer_overflow", 10),
            ("input", "buffer_overflow", 10),
        ] {
            snapshot.monitoring_gaps.push(TimelineGap {
                data_class: data_class.into(),
                reason: reason.into(),
                started_utc_ms: start + minute * MINUTE,
                ended_utc_ms: start + (minute + 5) * MINUTE,
            });
        }
        let rows = build_activity_rows_in(&snapshot, &zone());
        assert_eq!(rows.len(), 3);
        assert!(!rows[0].gap);
        assert!(
            rows[1..]
                .iter()
                .all(|row| row.gap && row.identity.is_none())
        );
        assert!(
            rows.iter()
                .any(|row| row.detail.contains("应用活动") && row.detail.contains("采集缓冲区溢出"))
        );
        assert!(
            rows.iter().any(|row| row.detail.contains("键盘与鼠标计数")
                && row.detail.contains("采集缓冲区溢出"))
        );
        assert_eq!(rows[2].detail, "键盘与鼠标计数 · 输入缓冲区溢出");
        assert_eq!(total_focus_ms(&snapshot), 5 * MINUTE as u64);
    }

    #[test]
    fn continuous_focus_crossing_noon_stays_in_the_starting_morning_group() {
        let start = timestamp(2026, 9, 5, 11, 30);
        let end = timestamp(2026, 9, 5, 12, 10);
        let snapshot = snapshot(start, end, vec![application("browser", &[(start, end)])]);
        let rows = build_activity_rows_in(&snapshot, &zone());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].group, "2026-09-05 · 上午");
        assert_eq!((rows[0].started_utc_ms, rows[0].ended_utc_ms), (start, end));
        assert_eq!(rows[0].duration_ms, 40 * MINUTE as u64);
        assert_eq!(total_focus_ms(&snapshot), 40 * MINUTE as u64);
    }

    #[test]
    fn records_split_only_at_local_midnight_without_changing_duration() {
        let start = timestamp(2026, 9, 5, 23, 30);
        let end = timestamp(2026, 9, 6, 0, 30);
        let snapshot = snapshot(start, end, vec![application("editor", &[(start, end)])]);
        let rows = build_activity_rows_in(&snapshot, &zone());
        assert_eq!(
            rows.iter()
                .map(|row| row.group.as_str())
                .collect::<Vec<_>>(),
            vec!["2026-09-05 · 下午", "2026-09-06 · 上午"]
        );
        assert_eq!(rows[0].ended_utc_ms, timestamp(2026, 9, 6, 0, 0));
        assert_eq!(rows[1].started_utc_ms, timestamp(2026, 9, 6, 0, 0));
        assert_eq!(
            rows.iter().map(|row| row.duration_ms).sum::<u64>(),
            end.abs_diff(start)
        );
        assert!(
            rows.windows(2)
                .all(|pair| pair[0].ended_utc_ms == pair[1].started_utc_ms)
        );
    }

    #[test]
    fn week_starts_on_monday_and_preserves_dates_across_the_year_boundary() {
        let selected = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
        let today = NaiveDate::from_ymd_opt(2025, 12, 31).unwrap();
        let days = week_days_for_date(selected, today);
        assert_eq!(
            days.iter().map(|day| day.date.as_str()).collect::<Vec<_>>(),
            vec![
                "2025-12-29",
                "2025-12-30",
                "2025-12-31",
                "2026-01-01",
                "2026-01-02",
                "2026-01-03",
                "2026-01-04",
            ]
        );
        assert_eq!(
            days.iter().map(|day| day.offset).collect::<Vec<_>>(),
            vec![-3, -2, -1, 0, 1, 2, 3]
        );
        assert_eq!(days[0].weekday, "周一");
        assert_eq!(days[6].weekday, "周日");
        assert_eq!(days.iter().filter(|day| day.selected).count(), 1);
        assert_eq!(days.iter().filter(|day| day.is_today).count(), 1);
        assert!(days[3].selected && days[2].is_today);
    }

    #[test]
    fn local_calendar_navigation_handles_months_leap_days_and_invalid_inputs() {
        let anchor = Local
            .with_ymd_and_hms(2026, 1, 31, 23, 30, 0)
            .earliest()
            .unwrap()
            .timestamp_millis();
        let (started, ended) = selected_day_bounds(anchor, 1).unwrap();
        assert_eq!(
            local_datetime_at(&Local, started).unwrap().date(),
            NaiveDate::from_ymd_opt(2026, 2, 1).unwrap()
        );
        assert_eq!(
            local_datetime_at(&Local, ended).unwrap().date(),
            NaiveDate::from_ymd_opt(2026, 2, 2).unwrap()
        );
        let leap_anchor = timestamp(2024, 2, 28, 23, 59);
        let (started, ended) = selected_day_bounds_in(&zone(), leap_anchor, 1).unwrap();
        assert_eq!(started, timestamp(2024, 2, 29, 0, 0));
        assert_eq!(ended, timestamp(2024, 3, 1, 0, 0));
        let week = week_days(anchor);
        assert_eq!(week.len(), 7);
        assert_eq!(week[5].date, "2026-01-31");
        assert!(week[5].selected);
        for invalid in [i64::MIN, i64::MAX] {
            assert!(selected_day_bounds(invalid, 0).is_none());
            assert!(week_days(invalid).is_empty());
        }
        assert!(selected_day_bounds(anchor, i32::MAX).is_none());
        assert!(selected_day_bounds(anchor, i32::MIN).is_none());
    }

    // A deterministic two-transition timezone avoids changing the process-wide
    // TZ variable or adding a timezone database dependency just for these tests.
    #[derive(Clone, Copy, Debug)]
    struct SeasonalZone {
        forward_utc: NaiveDateTime,
        backward_utc: NaiveDateTime,
    }

    #[derive(Clone, Copy, Debug)]
    struct SeasonalOffset {
        zone: SeasonalZone,
        seconds: i32,
    }

    impl Offset for SeasonalOffset {
        fn fix(&self) -> FixedOffset {
            FixedOffset::east_opt(self.seconds).unwrap()
        }
    }

    impl TimeZone for SeasonalZone {
        type Offset = SeasonalOffset;

        fn from_offset(offset: &Self::Offset) -> Self {
            offset.zone
        }

        fn offset_from_local_date(&self, local: &NaiveDate) -> LocalResult<Self::Offset> {
            self.offset_from_local_datetime(&local.and_hms_opt(0, 0, 0).unwrap())
        }

        fn offset_from_local_datetime(&self, local: &NaiveDateTime) -> LocalResult<Self::Offset> {
            let candidates = [7_200, 3_600]
                .into_iter()
                .filter_map(|seconds| {
                    let utc = local.checked_sub_offset(FixedOffset::east_opt(seconds).unwrap())?;
                    let offset = self.offset_from_utc_datetime(&utc);
                    (offset.seconds == seconds).then_some(offset)
                })
                .collect::<Vec<_>>();
            match candidates.as_slice() {
                [] => LocalResult::None,
                [offset] => LocalResult::Single(*offset),
                [earlier, later] => LocalResult::Ambiguous(*earlier, *later),
                _ => unreachable!(),
            }
        }

        fn offset_from_utc_date(&self, utc: &NaiveDate) -> Self::Offset {
            self.offset_from_utc_datetime(&utc.and_hms_opt(0, 0, 0).unwrap())
        }

        fn offset_from_utc_datetime(&self, utc: &NaiveDateTime) -> Self::Offset {
            SeasonalOffset {
                zone: *self,
                seconds: if *utc >= self.forward_utc && *utc < self.backward_utc {
                    7_200
                } else {
                    3_600
                },
            }
        }
    }

    #[test]
    fn dst_calendar_days_count_23_or_25_hours_and_keep_adjacent_boundaries() {
        let timezone = SeasonalZone {
            forward_utc: NaiveDate::from_ymd_opt(2026, 3, 29)
                .unwrap()
                .and_hms_opt(1, 0, 0)
                .unwrap(),
            backward_utc: NaiveDate::from_ymd_opt(2026, 10, 25)
                .unwrap()
                .and_hms_opt(1, 0, 0)
                .unwrap(),
        };
        for (month, day, hours) in [(3, 29, 23), (10, 25, 25)] {
            let anchor = timezone
                .with_ymd_and_hms(2026, month, day, 12, 0, 0)
                .single()
                .unwrap()
                .timestamp_millis();
            let (started, ended) = selected_day_bounds_in(&timezone, anchor, 0).unwrap();
            assert_eq!(ended - started, hours * 3_600_000);
            assert_eq!(
                selected_day_bounds_in(&timezone, anchor, -1).unwrap().1,
                started
            );
            assert_eq!(
                selected_day_bounds_in(&timezone, anchor, 1).unwrap().0,
                ended
            );
            let snapshot = snapshot(
                started,
                ended,
                vec![application("editor", &[(started, ended)])],
            );
            let rows = build_activity_rows_in(&snapshot, &timezone);
            assert_eq!(
                rows.iter().map(|row| row.duration_ms).sum::<u64>(),
                (hours * 3_600_000) as u64
            );
        }
    }

    #[test]
    fn skipped_or_repeated_midnight_uses_the_first_actual_instant_of_the_day() {
        let timezone = SeasonalZone {
            forward_utc: NaiveDate::from_ymd_opt(2026, 3, 28)
                .unwrap()
                .and_hms_opt(23, 0, 0)
                .unwrap(),
            backward_utc: NaiveDate::from_ymd_opt(2026, 10, 24)
                .unwrap()
                .and_hms_opt(23, 0, 0)
                .unwrap(),
        };
        for (month, day, start_hour, hours) in [(3, 29, 1, 23), (10, 25, 0, 25)] {
            let anchor = timezone
                .with_ymd_and_hms(2026, month, day, 12, 0, 0)
                .single()
                .unwrap()
                .timestamp_millis();
            let (started, ended) = selected_day_bounds_in(&timezone, anchor, 0).unwrap();
            assert_eq!(
                local_datetime_at(&timezone, started).unwrap().hour(),
                start_hour
            );
            assert_eq!(ended - started, hours * 3_600_000);
            assert_eq!(
                selected_day_bounds_in(&timezone, anchor, -1).unwrap().1,
                started
            );
        }
    }
}
