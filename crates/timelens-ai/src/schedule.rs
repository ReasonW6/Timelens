use chrono::{DateTime, Datelike, Duration, LocalResult, NaiveDate, TimeZone, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScheduleKind {
    Daily { hour: u32, minute: u32 },
    Interval { hours: u32, anchor_utc_ms: i64 },
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Schedule {
    pub id: String,
    pub enabled: bool,
    pub provider_id: String,
    pub model_id: String,
    pub prompt_id: String,
    pub kind: ScheduleKind,
    pub next_due_utc_ms: i64,
    pub retries: Option<u8>,
    pub paused_reason: Option<String>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DueWindow {
    pub started_utc_ms: i64,
    pub ended_utc_ms: i64,
    pub missed: bool,
}

fn local_time<T: TimeZone>(
    zone: &T,
    date: NaiveDate,
    hour: u32,
    minute: u32,
) -> Result<DateTime<T>, String> {
    let desired = date.and_hms_opt(hour, minute, 0).ok_or("每日时间无效")?;
    // A nonexistent spring-forward time runs at the first valid minute. Repeated
    // fall-back times use the earlier occurrence and are never scheduled twice.
    for shift in 0..=180 {
        match zone.from_local_datetime(&(desired + Duration::minutes(shift))) {
            LocalResult::Single(value) => return Ok(value),
            LocalResult::Ambiguous(early, late) => {
                return Ok(if early < late { early } else { late });
            }
            LocalResult::None => {}
        }
    }
    Err("无法解析本地日期的时间边界".into())
}

pub fn next_daily<T: TimeZone>(
    zone: &T,
    after_utc_ms: i64,
    hour: u32,
    minute: u32,
) -> Result<i64, String> {
    let after = DateTime::<Utc>::from_timestamp_millis(after_utc_ms)
        .ok_or("计划时间无效")?
        .with_timezone(zone);
    for offset in 0..=2 {
        let date = after
            .date_naive()
            .checked_add_signed(Duration::days(offset))
            .ok_or("日期溢出")?;
        let due = local_time(zone, date, hour, minute)?.timestamp_millis();
        if due > after_utc_ms {
            return Ok(due);
        }
    }
    Err("无法建立每日计划".into())
}

impl Schedule {
    pub fn enable<T: TimeZone>(&mut self, now: i64, zone: &T) -> Result<(), String> {
        self.next_due_utc_ms = match &mut self.kind {
            ScheduleKind::Daily { hour, minute } => next_daily(zone, now, *hour, *minute)?,
            ScheduleKind::Interval {
                hours,
                anchor_utc_ms,
            } => {
                if !(1..=168).contains(hours) {
                    return Err("总结间隔须为 1–168 小时".into());
                }
                *anchor_utc_ms = now;
                now.checked_add(i64::from(*hours) * 3_600_000)
                    .ok_or("计划时间溢出")?
            }
        };
        self.enabled = true;
        self.paused_reason = None;
        Ok(())
    }

    pub fn take_due<T: TimeZone>(&mut self, now: i64, zone: &T) -> Result<Vec<DueWindow>, String> {
        if !self.enabled || self.next_due_utc_ms > now {
            return Ok(vec![]);
        }
        let mut windows = vec![];
        // Durable cursor changes must be committed with the jobs by the core.
        while self.next_due_utc_ms <= now {
            let (start, end, next) = match self.kind {
                ScheduleKind::Interval { hours, .. } => {
                    if !(1..=168).contains(&hours) {
                        return Err("总结间隔无效".into());
                    }
                    let span = i64::from(hours) * 3_600_000;
                    (
                        self.next_due_utc_ms
                            .checked_sub(span)
                            .ok_or("计划范围溢出")?,
                        self.next_due_utc_ms,
                        self.next_due_utc_ms
                            .checked_add(span)
                            .ok_or("计划范围溢出")?,
                    )
                }
                ScheduleKind::Daily { hour, minute } => {
                    let local = DateTime::<Utc>::from_timestamp_millis(self.next_due_utc_ms)
                        .ok_or("计划时间无效")?
                        .with_timezone(zone);
                    let end_date =
                        NaiveDate::from_ymd_opt(local.year(), local.month(), local.day())
                            .ok_or("计划日期无效")?;
                    let start_date = end_date.pred_opt().ok_or("计划日期溢出")?;
                    (
                        local_time(zone, start_date, 0, 0)?.timestamp_millis(),
                        local_time(zone, end_date, 0, 0)?.timestamp_millis(),
                        next_daily(zone, self.next_due_utc_ms, hour, minute)?,
                    )
                }
            };
            windows.push(DueWindow {
                started_utc_ms: start,
                ended_utc_ms: end,
                missed: true,
            });
            self.next_due_utc_ms = next;
            if windows.len() > 100_000 {
                return Err("计划历史过长，请重新启用计划".into());
            }
        }
        if let Some(last) = windows.last_mut() {
            last.missed = false;
        }
        Ok(windows)
    }
}

pub fn natural_day_ranges<T: TimeZone>(
    start: i64,
    end: i64,
    zone: &T,
) -> Result<Vec<(i64, i64)>, String> {
    if end <= start {
        return Err("时间范围无效".into());
    }
    let mut ranges = vec![];
    let mut cursor = start;
    while cursor < end {
        let date = DateTime::<Utc>::from_timestamp_millis(cursor)
            .ok_or("时间范围无效")?
            .with_timezone(zone)
            .date_naive();
        let next_date = date.succ_opt().ok_or("日期溢出")?;
        let next = local_time(zone, next_date, 0, 0)?
            .timestamp_millis()
            .min(end);
        if next <= cursor {
            return Err("自然日边界未前进".into());
        }
        ranges.push((cursor, next));
        cursor = next;
        if ranges.len() > 36_500 {
            return Err("范围超过支持的 100 年".into());
        }
    }
    Ok(ranges)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn schedule(kind: ScheduleKind) -> Schedule {
        Schedule {
            id: "test".into(),
            enabled: false,
            provider_id: "p".into(),
            model_id: "m".into(),
            prompt_id: "default".into(),
            kind,
            next_due_utc_ms: 0,
            retries: None,
            paused_reason: None,
        }
    }
    #[test]
    fn interval_restart_only_runs_latest_complete_window_and_never_overlaps() {
        let mut schedule = schedule(ScheduleKind::Interval {
            hours: 1,
            anchor_utc_ms: 0,
        });
        schedule.enable(1000, &Utc).unwrap();
        let windows = schedule.take_due(4 * 3_600_000 + 1900, &Utc).unwrap();
        assert_eq!(windows.len(), 4);
        assert!(windows[..3].iter().all(|w| w.missed));
        assert!(!windows[3].missed);
        for pair in windows.windows(2) {
            assert_eq!(pair[0].ended_utc_ms, pair[1].started_utc_ms);
        }
        assert!(
            schedule
                .take_due(4 * 3_600_000 + 1900, &Utc)
                .unwrap()
                .is_empty()
        );
        assert!(schedule.take_due(0, &Utc).unwrap().is_empty());
        schedule.enable(9_000_000, &Utc).unwrap();
        assert_eq!(schedule.next_due_utc_ms, 12_600_000);
    }
    #[test]
    fn daily_run_summarizes_previous_calendar_day_without_first_enable_backfill() {
        let mut schedule = schedule(ScheduleKind::Daily {
            hour: 22,
            minute: 0,
        });
        let now = Utc
            .with_ymd_and_hms(2026, 9, 5, 12, 0, 0)
            .unwrap()
            .timestamp_millis();
        schedule.enable(now, &Utc).unwrap();
        assert!(schedule.take_due(now, &Utc).unwrap().is_empty());
        let windows = schedule.take_due(now + 10 * 3_600_000, &Utc).unwrap();
        assert_eq!(
            windows[0].started_utc_ms,
            Utc.with_ymd_and_hms(2026, 9, 4, 0, 0, 0)
                .unwrap()
                .timestamp_millis()
        );
        assert_eq!(
            windows[0].ended_utc_ms - windows[0].started_utc_ms,
            86_400_000
        );
    }
}
