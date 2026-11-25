//! The statistics trends: focus over time and against the period before, by
//! hour of day and by category. Everything is counted from focused spans, so
//! the totals match the application totals of the same range.
use crate::i18n::{tr, trn};
use crate::{AppWindow, AppsScope, TrendBar, TrendCategory, TrendPart, TrendState, UiState};
use chrono::{Datelike, Days, Local, Months, NaiveDate, NaiveTime, TimeZone, Timelike};
use slint::{ComponentHandle, ModelRc, VecModel};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use timelens_storage::{Category, FocusSpans, Storage};

const HOUR_MS: i64 = 3_600_000;

/// How long one bar lasts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Hour,
    Day,
    Month,
}

/// What a trend lays out: from `start` to its planned `end`, with records up
/// to `until`, which is now for a range still running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stretch {
    start: i64,
    end: i64,
    until: i64,
}

/// What was read for a trend.
struct Read {
    stretch: Stretch,
    spans: FocusSpans,
    chosen: BTreeMap<String, Option<i64>>,
    categories: Vec<Category>,
    /// The period before and its focus, unless all records are shown.
    previous: Option<((i64, i64), u64)>,
}

#[derive(Default)]
struct Reads {
    /// When the trend was last read and for which scope.
    requested: Option<(AppsScope, Instant)>,
    /// The scope on screen.
    shown: Option<AppsScope>,
    pending: bool,
    /// Categories changed since, so it is read again.
    stale: bool,
    /// The merges it was read with.
    generation: u64,
}

thread_local! {
    static READS: RefCell<Reads> = RefCell::new(Reads::default());
}

/// How often the trend on screen is read again.
const REFRESH: Duration = Duration::from_secs(30);

/// Read the trend again as soon as it shows, as after categories changed.
pub fn invalidate() {
    READS.with(|reads| reads.borrow_mut().stale = true);
}

/// Read the trend of the statistics range in the background when its tab is
/// on screen and due, and show it once read.
pub fn request(window: &AppWindow, storage: &Arc<Mutex<Storage>>, state: &UiState) {
    if window.get_page() != 3 || window.get_stats_tab() != 2 {
        return;
    }
    let scope = crate::apps_scope(window, state);
    let generation = crate::history::generation();
    let due = READS.with(|reads| {
        let reads = &mut *reads.borrow_mut();
        let due = !reads.pending
            && (reads.stale
                || reads.generation != generation
                || reads
                    .requested
                    .is_none_or(|(was, at)| was != scope || at.elapsed() >= REFRESH));
        if due {
            reads.stale = false;
            reads.generation = generation;
            reads.requested = Some((scope, Instant::now()));
            reads.pending = true;
        }
        due
    });
    // Only a scope not shown yet reads as loading.
    window
        .global::<TrendState>()
        .set_loading(READS.with(|reads| reads.borrow().shown != Some(scope)));
    if !due {
        return;
    }
    let storage = Arc::clone(storage);
    let weak = window.as_weak();
    std::thread::spawn(move || {
        let now = crate::unix_time_ms();
        let result = storage
            .lock()
            .map_err(|_| anyhow::anyhow!(tr!("存储锁不可用")))
            .and_then(|storage| read(&storage, scope, now));
        let _ = weak.upgrade_in_event_loop(move |window| {
            READS.with(|reads| reads.borrow_mut().pending = false);
            match result {
                Ok(read) => {
                    READS.with(|reads| reads.borrow_mut().shown = Some(scope));
                    window.global::<TrendState>().set_loading(false);
                    render(&window, scope, &read);
                }
                Err(error) => window.set_action_status(tr!("读取趋势失败：{}", error).into()),
            }
        });
    });
}

fn read(storage: &Storage, scope: AppsScope, now: i64) -> anyhow::Result<Read> {
    let stretch = match scope {
        AppsScope::All => {
            let first = storage.earliest_activity_utc_ms()?.unwrap_or(now).min(now);
            let end = now.max(first + 1);
            Stretch {
                start: first,
                end,
                until: end,
            }
        }
        AppsScope::Range(start, end) => Stretch {
            start,
            end: end.max(start + 1),
            until: end.min(now).max(start),
        },
        AppsScope::Recent(length) => Stretch {
            start: now.saturating_sub(length).max(1),
            end: now,
            until: now,
        },
    };
    let spans = storage.focused_spans(stretch.start, stretch.until)?;
    let previous = previous(scope, stretch)
        .map(|(start, end)| {
            storage
                .focused_spans(start, end)
                .map(|spans| ((start, end), total(&spans)))
        })
        .transpose()?;
    Ok(Read {
        stretch,
        chosen: storage.application_categories(spans.keys().map(String::as_str))?,
        categories: storage.categories()?,
        spans,
        previous,
    })
}

fn total(spans: &FocusSpans) -> u64 {
    spans
        .values()
        .flatten()
        .map(|(start, end)| end.saturating_sub(*start).max(0) as u64)
        .sum()
}

fn local_date(timestamp: i64) -> NaiveDate {
    Local
        .timestamp_millis_opt(timestamp)
        .single()
        .map_or(NaiveDate::MIN, |t| t.date_naive())
}

fn midnight(date: NaiveDate) -> Option<i64> {
    Local
        .from_local_datetime(&date.and_time(NaiveTime::MIN))
        .earliest()
        .map(|t| t.timestamp_millis())
}

/// The local hour of day `timestamp` falls in.
fn hour_of_day(timestamp: i64) -> usize {
    Local
        .timestamp_millis_opt(timestamp)
        .single()
        .map_or(0, |t| t.hour() as usize)
}

fn step_of(stretch: Stretch) -> Step {
    let length = stretch.end.saturating_sub(stretch.start);
    if length <= 26 * HOUR_MS {
        Step::Hour
    } else if length <= 92 * 24 * HOUR_MS {
        Step::Day
    } else {
        Step::Month
    }
}

/// Where the bar holding `timestamp` starts.
fn floor(timestamp: i64, step: Step) -> i64 {
    match step {
        Step::Hour => {
            let offset = Local
                .timestamp_millis_opt(timestamp)
                .single()
                .map_or(0, |t| i64::from(t.offset().local_minus_utc()) * 1_000);
            timestamp - (timestamp + offset).rem_euclid(HOUR_MS)
        }
        Step::Day => midnight(local_date(timestamp)).unwrap_or(timestamp),
        Step::Month => local_date(timestamp)
            .with_day(1)
            .and_then(midnight)
            .unwrap_or(timestamp),
    }
}

/// Where the bar after the one starting at `start` starts.
fn following(start: i64, step: Step) -> i64 {
    let date = local_date(start);
    let next = match step {
        Step::Hour => return start + HOUR_MS,
        Step::Day => date.checked_add_days(Days::new(1)),
        Step::Month => date
            .with_day(1)
            .and_then(|first| first.checked_add_months(Months::new(1))),
    };
    next.and_then(midnight)
        .filter(|next| *next > start)
        .unwrap_or(start + 24 * HOUR_MS)
}

/// The bars of a stretch, each clipped to it.
fn buckets(stretch: Stretch, step: Step) -> Vec<(i64, i64)> {
    let mut bars = Vec::new();
    let mut at = floor(stretch.start, step);
    while at < stretch.end && bars.len() < 500 {
        let next = following(at, step);
        bars.push((at.max(stretch.start), next.min(stretch.end)));
        at = next;
    }
    bars
}

/// Focus in each bar, by category.
fn tally(
    spans: &FocusSpans,
    chosen: &BTreeMap<String, Option<i64>>,
    bars: &[(i64, i64)],
) -> Vec<BTreeMap<Option<i64>, u64>> {
    let mut tallies = vec![BTreeMap::new(); bars.len()];
    for (application, list) in spans {
        let category = chosen.get(application).copied().flatten();
        for &(start, end) in list {
            let first = bars.partition_point(|bar| bar.1 <= start);
            for (index, bar) in bars.iter().enumerate().skip(first) {
                if bar.0 >= end {
                    break;
                }
                let overlap = end.min(bar.1) - start.max(bar.0);
                if overlap > 0 {
                    *tallies[index].entry(category).or_default() += overlap as u64;
                }
            }
        }
    }
    tallies
}

/// Focus in each local hour of day, summed over the days.
fn hours_of_day(spans: &FocusSpans) -> [u64; 24] {
    let mut hours = [0; 24];
    for &(start, end) in spans.values().flatten() {
        let mut at = start;
        while at < end {
            let next = (floor(at, Step::Hour) + HOUR_MS).min(end);
            hours[hour_of_day(at)] += (next - at) as u64;
            at = next;
        }
    }
    hours
}

/// How many local dates a stretch touches.
fn days(stretch: Stretch) -> u64 {
    let first = local_date(stretch.start);
    let last = local_date(stretch.end.saturating_sub(1).max(stretch.start));
    (last - first).num_days().max(0) as u64 + 1
}

/// How many whole local days a stretch is, if it runs from a midnight to a
/// midnight.
fn whole_days(start: i64, end: i64) -> Option<u64> {
    let (first, last) = (local_date(start), local_date(end));
    (midnight(first) == Some(start) && midnight(last) == Some(end))
        .then(|| (last - first).num_days())
        .filter(|days| *days > 0)
        .map(|days| days as u64)
}

/// `timestamp` so many local days earlier, at the same time of day.
fn days_before(timestamp: i64, days: u64) -> Option<i64> {
    let local = Local.timestamp_millis_opt(timestamp).single()?;
    let earlier = local.naive_local().checked_sub_days(Days::new(days))?;
    Local
        .from_local_datetime(&earlier)
        .earliest()
        .map(|t| t.timestamp_millis())
}

/// The period to compare a stretch with: as many whole days before it, or
/// as long before it, and as far into it as the stretch has run.
fn previous(scope: AppsScope, stretch: Stretch) -> Option<(i64, i64)> {
    let period = match scope {
        AppsScope::All => None,
        AppsScope::Recent(length) => Some((stretch.start - length, stretch.until - length)),
        AppsScope::Range(..) => match whole_days(stretch.start, stretch.end) {
            Some(days) => days_before(stretch.start, days).zip(days_before(stretch.until, days)),
            None => {
                let length = stretch.end - stretch.start;
                Some((stretch.start - length, stretch.until - length))
            }
        },
    };
    period.filter(|(start, end)| *start > 0 && end > start)
}

/// How the focus compares with the period before.
fn comparison(current: u64, previous: u64) -> String {
    if previous == 0 {
        return tr!("上一周期没有聚焦记录").to_owned();
    }
    let change = ((current as f64 - previous as f64) / previous as f64 * 100.0).round() as i64;
    match change {
        0 => tr!("与上一周期持平").to_owned(),
        1.. => tr!("比上一周期多 {}%", change),
        _ => tr!("比上一周期少 {}%", -change),
    }
}

/// A round top for a scale reaching `most`.
fn scale(most: u64) -> u64 {
    const MINUTES: [u64; 19] = [
        5, 10, 15, 20, 30, 45, 60, 90, 120, 180, 240, 300, 360, 480, 600, 720, 960, 1200, 1440,
    ];
    let minutes = most.div_ceil(60_000).max(1);
    if let Some(top) = MINUTES.iter().find(|top| **top >= minutes) {
        return top * 60_000;
    }
    let hours = minutes.div_ceil(60);
    let step = match hours {
        0..=100 => 10,
        101..=500 => 50,
        _ => 100,
    };
    hours.div_ceil(step) * step * HOUR_MS as u64
}

fn date_label(date: NaiveDate) -> String {
    date.format(tr!("%-m月%-d日")).to_string()
}

/// The dates or times a period covers, in words.
fn period_label(start: i64, end: i64) -> String {
    match whole_days(start, end) {
        Some(1) => date_label(local_date(start)),
        Some(_) => format!(
            "{}–{}",
            date_label(local_date(start)),
            date_label(local_date(end - 1))
        ),
        None => crate::collection_ui::stretch_label(start, end),
    }
}

fn clock(timestamp: i64) -> String {
    Local
        .timestamp_millis_opt(timestamp)
        .single()
        .map(|t| t.format("%H:%M").to_string())
        .unwrap_or_default()
}

/// The label under a bar, if there is room for it, and what hovering it says.
fn bar_labels(step: Step, bar: (i64, i64), index: usize, count: usize) -> (String, String) {
    let every = count.div_ceil(12).max(1);
    let shown = |text: String| {
        if index.is_multiple_of(every) {
            text
        } else {
            String::new()
        }
    };
    let date = local_date(bar.0);
    match step {
        Step::Hour => {
            let hour = hour_of_day(bar.0);
            let label = if count <= 12 || hour.is_multiple_of(3) {
                clock(floor(bar.0, Step::Hour))
            } else {
                String::new()
            };
            (
                label,
                format!("{} {}–{}", date_label(date), clock(bar.0), clock(bar.1)),
            )
        }
        Step::Day => (
            shown(date_label(date)),
            format!("{} {}", date_label(date), crate::history::weekday(date)),
        ),
        Step::Month => {
            let month = date.format(tr!("%Y年%-m月")).to_string();
            (shown(month.clone()), month)
        }
    }
}

/// Bars over a shared scale, their parts in the order of `order`.
fn bars(
    values: &[BTreeMap<Option<i64>, u64>],
    labels: impl Fn(usize) -> (String, String),
    order: &[(Option<i64>, slint::Color)],
) -> (Vec<TrendBar>, String) {
    let most = values
        .iter()
        .map(|parts| parts.values().sum::<u64>())
        .max()
        .unwrap_or(0);
    let top = scale(most);
    let bars = values
        .iter()
        .enumerate()
        .map(|(index, parts)| {
            let mut start = 0.0;
            let shares = order
                .iter()
                .filter_map(|(category, tint)| {
                    let ms = *parts.get(category)?;
                    let size = ms as f32 / top as f32;
                    let part = TrendPart {
                        start,
                        size,
                        tint: *tint,
                    };
                    start += size;
                    Some(part)
                })
                .collect::<Vec<_>>();
            let (label, tip) = labels(index);
            TrendBar {
                label: label.into(),
                tip: tip.into(),
                value: crate::format_duration(parts.values().sum()).into(),
                parts: ModelRc::new(VecModel::from(shares)),
            }
        })
        .collect();
    (bars, crate::format_duration(top))
}

fn render(window: &AppWindow, scope: AppsScope, read: &Read) {
    let trend = window.global::<TrendState>();
    let step = step_of(read.stretch);
    let focus = total(&read.spans);
    trend.set_total(crate::format_duration(focus).into());
    trend.set_empty(if focus > 0 {
        "".into()
    } else if scope == AppsScope::All {
        tr!("还没有聚焦记录。").into()
    } else {
        tr!("这段时间没有聚焦记录。").into()
    });
    match read.previous {
        Some(((start, end), before)) => {
            trend.set_compare(comparison(focus, before).into());
            trend.set_compare_note(
                tr!(
                    "上一周期 {} · {}",
                    period_label(start, end),
                    crate::format_duration(before)
                )
                .into(),
            );
        }
        None => {
            trend.set_compare("".into());
            trend.set_compare_note("".into());
        }
    }
    let days = days(read.stretch);
    trend.set_average(if step == Step::Hour {
        "".into()
    } else {
        crate::format_duration(focus / days.max(1)).into()
    });
    trend.set_step(
        match step {
            Step::Hour => tr!("按小时"),
            Step::Day => tr!("按天"),
            Step::Month => tr!("按月"),
        }
        .into(),
    );
    trend.set_scope(
        if scope == AppsScope::All {
            tr!("全部记录").to_owned()
        } else {
            period_label(read.stretch.start, read.stretch.end)
        }
        .into(),
    );
    // Categories in their own order, then the applications in none.
    let mut order = read
        .categories
        .iter()
        .map(|category| (Some(category.id), crate::categories_ui::color(category)))
        .collect::<Vec<_>>();
    order.push((None, crate::categories_ui::uncategorized_color()));
    let slots = buckets(read.stretch, step);
    let values = tally(&read.spans, &read.chosen, &slots);
    let (shown, top) = bars(
        &values,
        |index| bar_labels(step, slots[index], index, slots.len()),
        &order,
    );
    trend.set_bars(ModelRc::new(VecModel::from(shown)));
    trend.set_bars_top(top.into());
    if step == Step::Hour {
        trend.set_hours(ModelRc::new(VecModel::from(Vec::<TrendBar>::new())));
    } else {
        let hours = hours_of_day(&read.spans).map(|ms| BTreeMap::from([(None, ms / days.max(1))]));
        let (shown, top) = bars(
            &hours,
            |hour| {
                (
                    if hour.is_multiple_of(3) {
                        hour.to_string()
                    } else {
                        String::new()
                    },
                    tr!("{}:00–{}:00 日均", hour, hour + 1),
                )
            },
            &[(None, crate::categories_ui::uncategorized_color())],
        );
        trend.set_hours(ModelRc::new(VecModel::from(shown)));
        trend.set_hours_top(top.into());
    }
    // Each category's share, largest first, with the applications in none last.
    let mut totals = BTreeMap::<Option<i64>, (u64, usize)>::new();
    for (application, list) in &read.spans {
        let category = read.chosen.get(application).copied().flatten();
        let entry = totals.entry(category).or_default();
        entry.0 += list.iter().map(|(s, e)| (e - s).max(0) as u64).sum::<u64>();
        entry.1 += 1;
    }
    let mut rows = order
        .iter()
        .filter_map(|(category, tint)| {
            let (ms, apps) = *totals.get(category)?;
            let name = read
                .categories
                .iter()
                .find(|c| Some(c.id) == *category)
                .map_or_else(|| tr!("未分类").to_owned(), crate::categories_ui::name);
            let ratio = ms as f64 / focus.max(1) as f64;
            Some((
                category.is_none(),
                ms,
                TrendCategory {
                    name: name.into(),
                    tint: *tint,
                    value: crate::format_duration(ms).into(),
                    share: format!("{:.0}%", ratio * 100.0).into(),
                    ratio: ratio as f32,
                    apps: trn!(apps, "{} 个应用", apps).into(),
                },
            ))
        })
        .collect::<Vec<_>>();
    rows.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    trend.set_categories(ModelRc::new(VecModel::from(
        rows.into_iter().map(|(_, _, row)| row).collect::<Vec<_>>(),
    )));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(day: u32, hour: u32, minute: u32) -> i64 {
        Local
            .with_ymd_and_hms(2026, 9, day, hour, minute, 0)
            .earliest()
            .unwrap()
            .timestamp_millis()
    }

    fn stretch(start: i64, end: i64) -> Stretch {
        Stretch {
            start,
            end,
            until: end,
        }
    }

    #[test]
    fn bars_last_an_hour_a_day_or_a_month_by_the_length_of_the_range() {
        let day = stretch(at(5, 0, 0), at(6, 0, 0));
        assert_eq!(step_of(day), Step::Hour);
        let hours = buckets(day, Step::Hour);
        assert_eq!(hours.len(), 24);
        assert_eq!(hours[9], (at(5, 9, 0), at(5, 10, 0)));
        let week = stretch(at(1, 0, 0), at(8, 0, 0));
        assert_eq!(step_of(week), Step::Day);
        let days = buckets(week, Step::Day);
        assert_eq!(days.len(), 7);
        assert_eq!(days[6], (at(7, 0, 0), at(8, 0, 0)));
        // A stretch starting mid-hour clips its first bar.
        assert_eq!(
            buckets(stretch(at(5, 9, 30), at(5, 11, 0)), Step::Hour),
            [(at(5, 9, 30), at(5, 10, 0)), (at(5, 10, 0), at(5, 11, 0))]
        );
        let year = stretch(at(1, 0, 0), at(1, 0, 0) + 200 * 24 * HOUR_MS);
        assert_eq!(step_of(year), Step::Month);
        let october = Local
            .with_ymd_and_hms(2026, 10, 1, 0, 0, 0)
            .earliest()
            .unwrap();
        assert_eq!(buckets(year, Step::Month)[1].0, october.timestamp_millis());
    }

    #[test]
    fn focus_splits_across_bars_and_categories_and_hours_of_day() {
        let spans = FocusSpans::from([
            ("a".into(), vec![(at(5, 9, 30), at(5, 11, 15))]),
            ("b".into(), vec![(at(5, 10, 0), at(5, 10, 30))]),
        ]);
        let chosen = BTreeMap::from([("a".into(), Some(7)), ("b".into(), None)]);
        let bars = buckets(stretch(at(5, 0, 0), at(6, 0, 0)), Step::Hour);
        let values = tally(&spans, &chosen, &bars);
        let minutes = |m: u64| m * 60_000;
        assert_eq!(values[9], BTreeMap::from([(Some(7), minutes(30))]));
        assert_eq!(
            values[10],
            BTreeMap::from([(Some(7), minutes(60)), (None, minutes(30))])
        );
        assert_eq!(values[11], BTreeMap::from([(Some(7), minutes(15))]));
        let hours = hours_of_day(&spans);
        assert_eq!(hours[9..12], [minutes(30), minutes(90), minutes(15)]);
        assert_eq!(hours.iter().sum::<u64>(), total(&spans));
    }

    #[test]
    fn the_period_before_is_as_many_days_back_and_runs_as_far() {
        let today = stretch(at(5, 0, 0), at(6, 0, 0));
        let running = Stretch {
            until: at(5, 15, 7),
            ..today
        };
        let scope = AppsScope::Range(today.start, today.end);
        assert_eq!(previous(scope, running), Some((at(4, 0, 0), at(4, 15, 7))));
        let week = stretch(at(8, 0, 0), at(15, 0, 0));
        assert_eq!(
            previous(AppsScope::Range(week.start, week.end), week),
            Some((at(1, 0, 0), at(8, 0, 0)))
        );
        assert_eq!(period_label(at(1, 0, 0), at(8, 0, 0)), "9月1日–9月7日");
        let custom = stretch(at(5, 13, 0), at(5, 15, 0));
        assert_eq!(
            previous(AppsScope::Range(custom.start, custom.end), custom),
            Some((at(5, 11, 0), at(5, 13, 0)))
        );
        let recent = stretch(at(5, 12, 0), at(5, 15, 0));
        assert_eq!(
            previous(AppsScope::Recent(3 * HOUR_MS), recent),
            Some((at(5, 9, 0), at(5, 12, 0)))
        );
        assert_eq!(previous(AppsScope::All, recent), None);
        assert_eq!(days(week), 7);
        assert_eq!(days(running), 1);
    }

    #[test]
    fn comparisons_and_scales_read_plainly() {
        assert_eq!(comparison(5, 0), "上一周期没有聚焦记录");
        assert_eq!(comparison(1000, 1002), "与上一周期持平");
        assert_eq!(comparison(112, 100), "比上一周期多 12%");
        assert_eq!(comparison(92, 100), "比上一周期少 8%");
        assert_eq!(scale(0), 5 * 60_000);
        assert_eq!(scale(41 * 60_000), 45 * 60_000);
        assert_eq!(scale(25 * HOUR_MS as u64), 30 * HOUR_MS as u64);
        assert_eq!(scale(130 * HOUR_MS as u64), 150 * HOUR_MS as u64);
    }
}
