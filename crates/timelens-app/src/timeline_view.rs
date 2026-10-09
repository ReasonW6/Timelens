//! Calendar and activity projections shared by the native timeline views.
//!
//! Intervals are half-open UTC ranges. Calendar navigation and row grouping use
//! the computer's local time zone; neither requires a UI model or stored titles.

use std::collections::BTreeMap;

use chrono::{Days, Duration, Local, NaiveDateTime, Offset, TimeZone, Timelike};
use timelens_storage::TimelineSnapshot;

type Interval = (i64, i64);

/// Focus runs at most this far apart belong to one stretch of use, and a row
/// whose own application held focus for less than this joins a neighbouring row.
const BRIEF_ABSENCE_MS: i64 = 3 * 60_000;
/// Unrecorded gaps this close together read as one interruption.
const UNRECORDED_JOIN_MS: i64 = 5 * 60_000;
/// Interruptions missing less than this in total are restart seams, not news.
const MIN_UNRECORDED_MS: u64 = 2 * 60_000;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivityEntry {
    /// The application focused longest within the row.
    pub identity: Option<String>,
    pub name: String,
    pub started_utc_ms: i64,
    pub ended_utc_ms: i64,
    /// The row's span, including any brief companions.
    pub duration_ms: u64,
    /// Focus time of the row's own application within the span.
    pub focused_ms: u64,
    /// Applications that briefly held focus within the row, longest first.
    pub companions: Vec<Companion>,
    /// How many separate focus runs the companions contributed.
    pub interruptions: usize,
    pub group: String,
}

/// A stretch in which application activity went unrecorded, merged from
/// neighbouring monitoring gaps.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Unrecorded {
    pub started_utc_ms: i64,
    pub ended_utc_ms: i64,
    /// Time actually missing; less than the span when its gaps are apart.
    pub missing_ms: u64,
    /// How many separate gaps the stretch joins.
    pub count: usize,
    /// The longest gap's reason.
    pub reason: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Companion {
    pub identity: String,
    pub name: String,
    pub focused_ms: u64,
}

struct FocusRun<'a> {
    identity: &'a str,
    name: &'a str,
    started: i64,
    ended: i64,
    group: String,
}

impl FocusRun<'_> {
    fn length(&self) -> u64 {
        self.ended.abs_diff(self.started)
    }
}

struct FocusGroup<'a> {
    name: &'a str,
    intervals: Vec<Interval>,
}

/// Build chronological focus records.
///
/// Each application's touching or overlapping focus intervals are united before
/// splitting only at local day boundaries. A row belongs to the application
/// focused longest within it, not to whichever came first; brief use of other
/// applications folds into it as companions (see `group_runs`), so consecutive
/// rows on the same stretch never repeat an application. Each row's
/// morning/afternoon group follows its starting time. Distinct identities remain
/// distinct, including when their display names or observed intervals coincide.
/// Monitoring gaps do not become rows; see `unrecorded_spans`.
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

    let mut runs = Vec::new();
    for (identity, application) in applications {
        let name = if application.name.trim().is_empty() {
            "未命名应用"
        } else {
            application.name
        };
        for (started, ended) in union_intervals(application.intervals) {
            for (started, ended, group) in grouped_intervals(timezone, started, ended) {
                runs.push(FocusRun {
                    identity,
                    name,
                    started,
                    ended,
                    group,
                });
            }
        }
    }
    runs.sort_by(|left, right| {
        (left.started, left.ended, left.identity).cmp(&(right.started, right.ended, right.identity))
    });
    let unrecorded = unrecorded_spans(snapshot)
        .into_iter()
        .map(|span| (span.started_utc_ms, span.ended_utc_ms))
        .collect::<Vec<_>>();
    group_runs(&runs, &unrecorded)
}

/// Stretches in which the collector did not record application activity.
///
/// Gaps closer than `UNRECORDED_JOIN_MS` merge into one stretch, so a burst of
/// collector restarts reads as one interruption; stretches missing less than
/// `MIN_UNRECORDED_MS` are dropped. Input-only gaps leave application activity
/// intact, and data removed by the user or by retention was recorded, so neither
/// counts.
pub fn unrecorded_spans(snapshot: &TimelineSnapshot) -> Vec<Unrecorded> {
    let mut gaps = snapshot
        .monitoring_gaps
        .iter()
        .filter(|gap| gap.data_class == "activity")
        .filter(|gap| {
            !matches!(
                gap.reason.as_str(),
                "user_deleted" | "retention_time" | "retention_space"
            )
        })
        .filter_map(|gap| {
            clipped_interval(snapshot, gap.started_utc_ms, gap.ended_utc_ms)
                .map(|(started, ended)| (started, ended, gap.reason.as_str()))
        })
        .collect::<Vec<_>>();
    gaps.sort_unstable();
    gaps.dedup_by_key(|(started, ended, _)| (*started, *ended));
    let mut spans = Vec::<(Unrecorded, u64)>::new();
    for (started, ended, reason) in gaps {
        let length = ended.abs_diff(started);
        if let Some((span, longest)) = spans.last_mut()
            && started - span.ended_utc_ms <= UNRECORDED_JOIN_MS
        {
            span.missing_ms += ended.saturating_sub(started.max(span.ended_utc_ms)) as u64;
            span.ended_utc_ms = span.ended_utc_ms.max(ended);
            span.count += 1;
            if length > *longest {
                *longest = length;
                span.reason = reason.to_owned();
            }
            continue;
        }
        spans.push((
            Unrecorded {
                started_utc_ms: started,
                ended_utc_ms: ended,
                missing_ms: length,
                count: 1,
                reason: reason.to_owned(),
            },
            length,
        ));
    }
    spans
        .into_iter()
        .map(|(span, _)| span)
        .filter(|span| span.missing_ms >= MIN_UNRECORDED_MS)
        .collect()
}

/// Focus runs merged into one prospective row.
struct Block<'a> {
    started: i64,
    ended: i64,
    group: String,
    /// Focus time and run count per identity.
    focus: BTreeMap<&'a str, (&'a str, u64, usize)>,
    /// The identity focused longest, and for how long.
    main: (&'a str, u64),
}

impl<'a> Block<'a> {
    fn new(run: &FocusRun<'a>) -> Self {
        Self {
            started: run.started,
            ended: run.ended,
            group: run.group.clone(),
            focus: BTreeMap::from([(run.identity, (run.name, run.length(), 1))]),
            main: (run.identity, run.length()),
        }
    }

    fn date(&self) -> &str {
        self.group.split(" · ").next().unwrap_or("")
    }

    /// Whether `next`, which starts no earlier, continues the same stretch. A
    /// shown unrecorded stretch between them always separates them.
    fn links(&self, next: &Self, unrecorded: &[Interval]) -> bool {
        next.date() == self.date()
            && next.started - self.ended <= BRIEF_ABSENCE_MS
            && !unrecorded
                .iter()
                .any(|&(started, ended)| started < next.started && ended > self.ended)
    }

    fn absorb(&mut self, other: Block<'a>) {
        if other.started < self.started {
            self.group = other.group;
        }
        self.started = self.started.min(other.started);
        self.ended = self.ended.max(other.ended);
        for (identity, (name, focused, runs)) in other.focus {
            let entry = self.focus.entry(identity).or_insert((name, 0, 0));
            entry.1 = entry.1.saturating_add(focused);
            entry.2 += runs;
        }
        // Longest focus wins; ties go to the smaller identity for stable output.
        self.main = self
            .focus
            .iter()
            .map(|(identity, (_, focused, _))| (*identity, *focused))
            .fold(
                ("", 0),
                |best, next| if next.1 > best.1 { next } else { best },
            );
    }

    fn into_entry(self) -> ActivityEntry {
        let (main, focused_ms) = self.main;
        let mut name = "";
        let mut companions = Vec::new();
        let mut interruptions = 0;
        for (identity, (app_name, focused, runs)) in self.focus {
            if identity == main {
                name = app_name;
                continue;
            }
            interruptions += runs;
            companions.push(Companion {
                identity: identity.to_owned(),
                name: app_name.to_owned(),
                focused_ms: focused,
            });
        }
        companions.sort_by(|a, b| {
            b.focused_ms
                .cmp(&a.focused_ms)
                .then_with(|| a.identity.cmp(&b.identity))
        });
        ActivityEntry {
            identity: Some(main.to_owned()),
            name: name.to_owned(),
            started_utc_ms: self.started,
            ended_utc_ms: self.ended,
            duration_ms: self.ended.abs_diff(self.started),
            focused_ms,
            companions,
            interruptions,
            group: self.group,
        }
    }
}

/// Group chronological focus runs into rows.
///
/// Every run starts as its own block. Repeatedly, the block whose application
/// held focus the least, below `BRIEF_ABSENCE_MS`, merges into the nearer
/// neighbour of the same stretch, and neighbours with the same main application
/// merge. A block's main application is the one focused longest within it, so a
/// short visit at the start of a stretch never claims the time spent elsewhere.
fn group_runs(runs: &[FocusRun<'_>], unrecorded: &[Interval]) -> Vec<ActivityEntry> {
    let mut blocks = runs.iter().map(Block::new).collect::<Vec<_>>();
    loop {
        let mut index = 1;
        while index < blocks.len() {
            if blocks[index - 1].main.0 == blocks[index].main.0
                && blocks[index - 1].links(&blocks[index], unrecorded)
            {
                let block = blocks.remove(index);
                blocks[index - 1].absorb(block);
            } else {
                index += 1;
            }
        }
        let gap =
            |a: &Block<'_>, b: &Block<'_>| a.links(b, unrecorded).then_some(b.started - a.ended);
        let brief = (0..blocks.len())
            .filter(|&i| blocks[i].main.1 < BRIEF_ABSENCE_MS as u64)
            .filter_map(|i| {
                let before = i
                    .checked_sub(1)
                    .and_then(|j| gap(&blocks[j], &blocks[i]).map(|g| (g, j)));
                let after = blocks
                    .get(i + 1)
                    .and_then(|next| gap(&blocks[i], next).map(|g| (g, i + 1)));
                // The nearer neighbour, or on a tie the one focused longer.
                let target = match (before, after) {
                    (Some(b), Some(a)) if a.0 < b.0 => a.1,
                    (Some(b), Some(a)) if a.0 == b.0 && blocks[a.1].main.1 > blocks[b.1].main.1 => {
                        a.1
                    }
                    (Some(b), _) => b.1,
                    (None, Some(a)) => a.1,
                    (None, None) => return None,
                };
                Some((blocks[i].main.1, i, target))
            })
            .min();
        let Some((_, index, target)) = brief else {
            break;
        };
        let block = blocks.remove(index);
        let target = if target > index { target - 1 } else { target };
        blocks[target].absorb(block);
    }
    blocks.into_iter().map(Block::into_entry).collect()
}

/// An application on screen without focus counts as used alongside a row only if
/// it also had focus within the row or this long before it. Windows cannot tell
/// whether a shown window is covered, so a window merely left open behind a
/// maximized one must not join every row.
const SAME_SCREEN_RECENT_MS: i64 = 10 * 60_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParticipantRole {
    /// The row's own application, the one holding focus.
    Main,
    /// Briefly took focus within the row.
    Interleaved,
    /// Stayed on screen for at least half the row and was used recently.
    SameScreen,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Participant {
    pub identity: String,
    pub name: String,
    pub role: ParticipantRole,
    pub focused_ms: u64,
    /// Shown on screen without focus.
    pub visible_ms: u64,
}

/// The applications used during a row: its own application first, then the
/// others by how long they were focused or shown, longest first.
pub fn participants(snapshot: &TimelineSnapshot, row: &ActivityEntry) -> Vec<Participant> {
    let Some(main) = row.identity.as_deref() else {
        return Vec::new();
    };
    let (start, end) = (row.started_utc_ms, row.ended_utc_ms);
    let span = end.abs_diff(start).max(1);
    let clipped = |segments: &mut dyn Iterator<Item = &timelens_storage::TimelineSegment>,
                   from: i64| {
        union_intervals(
            segments
                .filter_map(|segment| {
                    let started = segment.started_utc_ms.max(from);
                    let ended = segment.ended_utc_ms.min(end);
                    (started < ended).then_some((started, ended))
                })
                .collect(),
        )
    };
    let length = |intervals: &[Interval]| {
        intervals
            .iter()
            .fold(0_u64, |total, (a, b)| total.saturating_add(b.abs_diff(*a)))
    };
    // A snapshot may list an identity more than once; judge each identity once.
    let mut by_identity = BTreeMap::<&str, (&str, Vec<&timelens_storage::TimelineSegment>)>::new();
    for application in &snapshot.applications {
        let entry = by_identity
            .entry(&application.identity)
            .or_insert((&application.display_name, Vec::new()));
        entry.1.extend(&application.segments);
    }
    let mut participants = Vec::new();
    for (identity, (name, segments)) in by_identity {
        let focused = length(&clipped(
            &mut segments.iter().copied().filter(|s| s.focused),
            start,
        ));
        let shown = length(&clipped(
            &mut segments
                .iter()
                .copied()
                .filter(|s| s.displayed || s.focused),
            start,
        ));
        let visible = shown.saturating_sub(focused);
        let role = if identity == main {
            ParticipantRole::Main
        } else if focused > 0 {
            ParticipantRole::Interleaved
        } else if visible.saturating_mul(2) >= span
            && !clipped(
                &mut segments.iter().copied().filter(|s| s.focused),
                start.saturating_sub(SAME_SCREEN_RECENT_MS),
            )
            .is_empty()
        {
            ParticipantRole::SameScreen
        } else {
            continue;
        };
        participants.push(Participant {
            identity: identity.to_owned(),
            name: name.to_owned(),
            role,
            focused_ms: focused,
            visible_ms: visible,
        });
    }
    participants.sort_by(|a, b| {
        (b.role == ParticipantRole::Main)
            .cmp(&(a.role == ParticipantRole::Main))
            .then_with(|| (b.focused_ms + b.visible_ms).cmp(&(a.focused_ms + a.visible_ms)))
            .then_with(|| a.identity.cmp(&b.identity))
    });
    participants
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, LocalResult, NaiveDate};
    use timelens_storage::{TimelineApplication, TimelineGap, TimelineSegment};

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
    fn brief_switches_stay_inside_one_row_as_companions() {
        let start = timestamp(2026, 9, 5, 9, 0);
        let at = |minute: i64| start + minute * MINUTE;
        let snapshot = snapshot(
            start,
            at(60),
            vec![
                application(
                    "editor",
                    &[(at(0), at(20)), (at(21), at(40)), (at(41), at(50))],
                ),
                application("chat", &[(at(20), at(21))]),
                application("browser", &[(at(40), at(41))]),
            ],
        );
        let rows = build_activity_rows_in(&snapshot, &zone());
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.identity.as_deref(), Some("editor"));
        assert_eq!((row.started_utc_ms, row.ended_utc_ms), (at(0), at(50)));
        assert_eq!(row.duration_ms, 50 * MINUTE as u64);
        assert_eq!(row.focused_ms, 48 * MINUTE as u64);
        assert_eq!(row.interruptions, 2);
        assert_eq!(
            row.companions
                .iter()
                .map(|c| (c.identity.as_str(), c.focused_ms))
                .collect::<Vec<_>>(),
            vec![("browser", MINUTE as u64), ("chat", MINUTE as u64)]
        );
    }

    #[test]
    fn a_leading_glance_joins_the_next_row_and_long_absences_split_rows() {
        let start = timestamp(2026, 9, 5, 9, 0);
        let at = |minute: i64| start + minute * MINUTE;
        let snapshot = snapshot(
            start,
            at(90),
            vec![
                application("chat", &[(at(0), at(1)), (at(70), at(71))]),
                application("editor", &[(at(1), at(30))]),
                application("browser", &[(at(40), at(60))]),
            ],
        );
        let rows = build_activity_rows_in(&snapshot, &zone());
        let summary = rows
            .iter()
            .map(|row| {
                (
                    row.identity.as_deref().unwrap(),
                    (row.started_utc_ms - start) / MINUTE,
                    (row.ended_utc_ms - start) / MINUTE,
                    row.companions.len(),
                )
            })
            .collect::<Vec<_>>();
        // The trailing glance has no row to join, so it stays on its own.
        assert_eq!(
            summary,
            vec![
                ("editor", 0, 30, 1),
                ("browser", 40, 60, 0),
                ("chat", 70, 71, 0)
            ]
        );
        assert!(
            rows.windows(2)
                .all(|pair| pair[0].identity != pair[1].identity)
        );
    }

    #[test]
    fn a_row_belongs_to_the_application_focused_longest_not_the_first_one() {
        let start = timestamp(2026, 10, 9, 18, 19);
        let second = 1_000;
        let at = |seconds: i64| start + seconds * second;
        // Recorded on 2026-10-09: two short visits to Timelens around the browser.
        let snapshot = snapshot(
            start,
            at(900),
            vec![
                application("taskmgr", &[(at(0), at(25))]),
                application("timelens", &[(at(180), at(220)), (at(402), at(442))]),
                application("browser", &[(at(220), at(400)), (at(442), at(900))]),
            ],
        );
        let rows = build_activity_rows_in(&snapshot, &zone());
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.identity.as_deref(), Some("browser"));
        assert_eq!((row.started_utc_ms, row.ended_utc_ms), (at(0), at(900)));
        assert_eq!(row.focused_ms, 638 * second as u64);
        assert_eq!(row.interruptions, 3);
        assert_eq!(
            row.companions
                .iter()
                .map(|c| (c.identity.as_str(), c.focused_ms))
                .collect::<Vec<_>>(),
            vec![
                ("timelens", 80 * second as u64),
                ("taskmgr", 25 * second as u64)
            ]
        );
    }

    #[test]
    fn sustained_use_between_stretches_of_another_application_keeps_its_row() {
        let start = timestamp(2026, 9, 5, 9, 0);
        let at = |minute: i64| start + minute * MINUTE;
        let snapshot = snapshot(
            start,
            at(60),
            vec![
                application("browser", &[(at(0), at(10)), (at(15), at(25))]),
                application("chat", &[(at(10), at(15)), (at(25), at(26))]),
            ],
        );
        let rows = build_activity_rows_in(&snapshot, &zone());
        let summary = rows
            .iter()
            .map(|row| {
                (
                    row.identity.as_deref().unwrap(),
                    (row.started_utc_ms - start) / MINUTE,
                    (row.ended_utc_ms - start) / MINUTE,
                )
            })
            .collect::<Vec<_>>();
        // The trailing minute of chat joins the browser row before it.
        assert_eq!(
            summary,
            vec![("browser", 0, 10), ("chat", 10, 15), ("browser", 15, 26)]
        );
    }

    #[test]
    fn an_unrecorded_stretch_separates_rows_of_the_same_application() {
        let start = timestamp(2026, 9, 5, 9, 0);
        let at = |minute: i64| start + minute * MINUTE;
        let mut snapshot = snapshot(
            start,
            at(60),
            vec![application("editor", &[(at(0), at(10)), (at(13), at(20))])],
        );
        snapshot.monitoring_gaps.push(TimelineGap {
            data_class: "activity".into(),
            started_utc_ms: at(10),
            ended_utc_ms: at(13),
            reason: "collector_restart".into(),
        });
        let rows = build_activity_rows_in(&snapshot, &zone());
        assert_eq!(
            rows.iter()
                .map(|row| (
                    (row.started_utc_ms - start) / MINUTE,
                    (row.ended_utc_ms - start) / MINUTE
                ))
                .collect::<Vec<_>>(),
            vec![(0, 10), (13, 20)]
        );
    }

    #[test]
    fn participants_cover_interleaved_and_recently_used_on_screen_apps_only() {
        let start = timestamp(2026, 9, 5, 9, 0);
        let at = |minute: i64| start + minute * MINUTE;
        let segment = |a: i64, b: i64, displayed: bool, focused: bool| TimelineSegment {
            started_utc_ms: at(a),
            ended_utc_ms: at(b),
            displayed,
            focused,
            inferred_tray: false,
        };
        let mut editor = application("editor", &[(at(0), at(20)), (at(21), at(40))]);
        editor.display_name = "Editor".into();
        let chat = application("chat", &[(at(20), at(21))]);
        // A reference document on the second screen, clicked just before the row.
        let mut reference = application("reference", &[]);
        reference.segments = vec![segment(-5, -2, true, true), segment(-2, 40, true, false)];
        // A window left open behind the editor all day, never touched.
        let mut forgotten = application("forgotten", &[]);
        forgotten.segments = vec![segment(-300, 40, true, false)];
        let mut background = application("music", &[]);
        background.segments = vec![segment(0, 40, false, false)];
        let snapshot = snapshot(
            at(-10),
            at(40),
            vec![editor, chat, reference, forgotten, background],
        );
        let rows = build_activity_rows_in(&snapshot, &zone());
        let row = rows
            .iter()
            .find(|row| row.identity.as_deref() == Some("editor"))
            .unwrap();
        let found = participants(&snapshot, row);
        assert_eq!(
            found
                .iter()
                .map(|p| (p.identity.as_str(), p.role))
                .collect::<Vec<_>>(),
            vec![
                ("editor", ParticipantRole::Main),
                ("reference", ParticipantRole::SameScreen),
                ("chat", ParticipantRole::Interleaved),
            ]
        );
        assert_eq!(found[0].focused_ms, 39 * MINUTE as u64);
        assert_eq!(found[1].visible_ms, 40 * MINUTE as u64);
        assert_eq!(found[2].focused_ms, MINUTE as u64);
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
                data_class: "activity".into(),
                started_utc_ms: end - MINUTE,
                ended_utc_ms: end + 5 * MINUTE,
                reason: "collector_restart".into(),
            },
        ];
        let rows = build_activity_rows_in(&snapshot, &zone());
        assert_eq!(rows.len(), 2);
        // The first gap keeps its in-range two minutes; the second only one.
        let spans = unrecorded_spans(&snapshot);
        assert_eq!(spans.len(), 1);
        assert_eq!(
            (spans[0].started_utc_ms, spans[0].ended_utc_ms),
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
    fn unrecorded_spans_merge_restart_bursts_and_skip_seams_and_removed_data() {
        let start = timestamp(2026, 9, 5, 9, 0);
        let mut snapshot = snapshot(
            start,
            start + 180 * MINUTE,
            vec![application("editor", &[(start, start + 5 * MINUTE)])],
        );
        let second = 1_000;
        for (data_class, reason, from, to) in [
            // A burst of restarts a few seconds each, about a minute and a half in all.
            (
                "activity",
                "collector_restart",
                10 * MINUTE,
                10 * MINUTE + 30 * second,
            ),
            (
                "activity",
                "collector_restart",
                11 * MINUTE,
                11 * MINUTE + 20 * second,
            ),
            (
                "activity",
                "collector_restart",
                12 * MINUTE,
                12 * MINUTE + 40 * second,
            ),
            (
                "activity",
                "collector_restart",
                12 * MINUTE,
                12 * MINUTE + 40 * second,
            ),
            // Restarts that add up to more than two minutes.
            ("activity", "collector_restart", 40 * MINUTE, 41 * MINUTE),
            ("activity", "buffer_overflow", 43 * MINUTE, 45 * MINUTE),
            // A long stop.
            ("activity", "collector_restart", 60 * MINUTE, 100 * MINUTE),
            // Not unrecorded activity.
            ("input", "input_overflow", 120 * MINUTE, 150 * MINUTE),
            ("activity", "user_deleted", 120 * MINUTE, 150 * MINUTE),
            ("activity", "retention_time", 155 * MINUTE, 170 * MINUTE),
        ] {
            snapshot.monitoring_gaps.push(TimelineGap {
                data_class: data_class.into(),
                reason: reason.into(),
                started_utc_ms: start + from,
                ended_utc_ms: start + to,
            });
        }
        let spans = unrecorded_spans(&snapshot);
        assert_eq!(
            spans,
            vec![
                Unrecorded {
                    started_utc_ms: start + 40 * MINUTE,
                    ended_utc_ms: start + 45 * MINUTE,
                    missing_ms: 3 * MINUTE as u64,
                    count: 2,
                    reason: "buffer_overflow".into(),
                },
                Unrecorded {
                    started_utc_ms: start + 60 * MINUTE,
                    ended_utc_ms: start + 100 * MINUTE,
                    missing_ms: 40 * MINUTE as u64,
                    count: 1,
                    reason: "collector_restart".into(),
                },
            ]
        );
        assert_eq!(build_activity_rows_in(&snapshot, &zone()).len(), 1);
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
        for invalid in [i64::MIN, i64::MAX] {
            assert!(selected_day_bounds(invalid, 0).is_none());
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
