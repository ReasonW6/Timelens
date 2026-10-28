//! Live input counts on the statistics page. While the page is on screen, the
//! collector previews the input of the minute it has not sealed yet, as totals
//! alone. The page adds each minute's newest preview to the stored totals
//! until those include the minute, so the counts move as the user types and
//! clicks. Previews are never stored, and leave out what the statistics leave
//! out: paused collection and input the rules keep anonymous.

use std::{
    cell::RefCell,
    collections::BTreeSet,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use timelens_ipc::LiveInput;
use timelens_storage::TimelineSnapshot;

use crate::{AppWindow, grouped_count};

static WANTED: AtomicBool = AtomicBool::new(false);
/// Stored totals catch up within seconds; this bounds how long a preview
/// waits for them, even one whose minute is never stored.
const KEPT_FOR: Duration = Duration::from_secs(10 * 60);

/// The overview's stored totals: keyboard, left, middle and right, what they
/// cover, and the recent minutes they include.
struct Overview {
    start: i64,
    end: i64,
    stored: BTreeSet<i64>,
    counts: [u64; 4],
}

/// The newest preview of each recent minute, oldest first, with when each
/// arrived and the dataset they were counted against.
#[derive(Default)]
struct Previews {
    minutes: Vec<(LiveInput, Instant)>,
    generation: u64,
}

impl Previews {
    /// Previews counted before history was deleted or replaced might stand
    /// in for minutes no longer stored, and old ones for minutes never stored.
    fn current(&mut self, now: Instant) -> &[(LiveInput, Instant)] {
        let generation = crate::data_ui::data_generation();
        if self.generation != generation {
            self.generation = generation;
            self.minutes.clear();
        }
        self.minutes
            .retain(|(_, arrived)| now.duration_since(*arrived) < KEPT_FOR);
        &self.minutes
    }

    fn keep(&mut self, input: LiveInput, now: Instant) {
        self.current(now);
        self.minutes
            .retain(|(kept, _)| kept.minute_started_at_utc_ms != input.minute_started_at_utc_ms);
        self.minutes.push((input, now));
        self.minutes
            .sort_by_key(|(kept, _)| kept.minute_started_at_utc_ms);
    }

    /// What the previews `keep` takes add to totals that include the
    /// `stored` minutes: keyboard, left, middle and right.
    fn unstored(
        &mut self,
        stored: &BTreeSet<i64>,
        keep: impl Fn(&LiveInput) -> bool,
        now: Instant,
    ) -> [u64; 4] {
        let mut total = [0_u64; 4];
        for (input, _) in self
            .current(now)
            .iter()
            .filter(|(input, _)| !stored.contains(&input.minute_started_at_utc_ms) && keep(input))
        {
            for (sum, count) in total.iter_mut().zip([
                input.keyboard_count,
                input.left_click_count,
                input.middle_click_count,
                input.right_click_count,
            ]) {
                *sum += u64::from(count);
            }
        }
        total
    }
}

thread_local! {
    static PREVIEWS: RefCell<Previews> = RefCell::new(Previews::default());
    static OVERVIEW: RefCell<Option<Overview>> = const { RefCell::new(None) };
}

/// Whether the core shows previews; the collector sends them only then.
pub fn wanted() -> bool {
    WANTED.load(Ordering::Relaxed)
}

/// Previews are wanted while the statistics page is on screen. Those kept
/// stay meanwhile: told apart by minute, they still add only what the stored
/// totals lack, so the counts do not drop back when the page shows again.
pub fn set_wanted(wanted: bool) {
    WANTED.store(wanted, Ordering::Relaxed);
}

/// Keep a preview, replacing the one before of its minute, and show it.
pub fn receive(window: &AppWindow, input: LiveInput) {
    PREVIEWS.with(|previews| previews.borrow_mut().keep(input, Instant::now()));
    show_overview(window);
    crate::collection_ui::show_keys(window);
}

/// What the previews `keep` takes add to totals that include the `stored`
/// minutes: keyboard, left, middle and right.
pub fn unstored(stored: &BTreeSet<i64>, keep: impl Fn(&LiveInput) -> bool) -> [u64; 4] {
    PREVIEWS.with(|previews| previews.borrow_mut().unstored(stored, keep, Instant::now()))
}

/// Show the overview's keyboard and click totals for `snapshot`, with the
/// previews of later minutes when `open` says its range runs with the clock.
pub fn set_overview(window: &AppWindow, snapshot: &TimelineSnapshot, open: bool) {
    let overview = Overview {
        start: snapshot.range_started_utc_ms,
        end: if open {
            i64::MAX
        } else {
            snapshot.range_ended_utc_ms
        },
        stored: snapshot.recent_input_minutes.clone(),
        counts: [
            snapshot.keyboard_count,
            snapshot.left_click_count,
            snapshot.middle_click_count,
            snapshot.right_click_count,
        ],
    };
    OVERVIEW.with(|shown| *shown.borrow_mut() = Some(overview));
    show_overview(window);
}

fn show_overview(window: &AppWindow) {
    OVERVIEW.with(|overview| {
        let Some(overview) = &*overview.borrow() else {
            return;
        };
        let live = unstored(&overview.stored, |input| {
            input.minute_started_at_utc_ms + 60_000 > overview.start
                && input.minute_started_at_utc_ms < overview.end
        });
        let [keyboard, left, middle, right] =
            std::array::from_fn(|index| overview.counts[index].saturating_add(live[index]));
        let mouse = left.saturating_add(middle).saturating_add(right);
        window.set_range_keyboard(grouped_count(keyboard).into());
        window.set_range_mouse(grouped_count(mouse).into());
        window.set_range_keyboard_count(keyboard as f32);
        window.set_range_mouse_count(mouse as f32);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preview(minute: i64, keyboard: u32) -> LiveInput {
        LiveInput {
            minute_started_at_utc_ms: minute,
            local_date: "2026-10-28".into(),
            keyboard_count: keyboard,
            left_click_count: 1,
            ..Default::default()
        }
    }

    #[test]
    fn previews_add_only_the_minutes_not_stored() {
        let now = Instant::now();
        let mut previews = Previews {
            generation: crate::data_ui::data_generation(),
            ..Previews::default()
        };
        previews.keep(preview(60_000, 3), now);
        previews.keep(preview(120_000, 2), now);
        // A newer preview of a minute replaces the one before.
        previews.keep(preview(120_000, 5), now);
        let all = |_: &LiveInput| true;
        assert_eq!(previews.unstored(&BTreeSet::new(), all, now), [8, 2, 0, 0]);
        // Told apart by minute, not by the latest stored: deleting that one
        // later, or a clock set back past it, changes nothing for the others.
        assert_eq!(
            previews.unstored(&BTreeSet::from([60_000]), all, now),
            [5, 1, 0, 0]
        );
        assert_eq!(
            previews.unstored(&BTreeSet::from([i64::MAX - 59_999]), all, now),
            [8, 2, 0, 0]
        );
        // Never stored, a preview still goes in time.
        assert_eq!(
            previews.unstored(&BTreeSet::new(), all, now + KEPT_FOR),
            [0; 4]
        );
    }
}
