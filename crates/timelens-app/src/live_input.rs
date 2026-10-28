//! Live input counts on the statistics page. While the page is on screen, the
//! collector previews the input of the minute it has not sealed yet. The page
//! adds each minute's newest preview to the stored totals until those include
//! the minute, so the counts move as the user types and clicks. While the keys
//! are on screen, previews also count which keys, and each key lights as its
//! count goes up from one preview to the next; only the latest of those counts
//! is kept (ticket 20). Previews are never stored, and leave out what the
//! statistics leave out: paused collection and input the rules keep anonymous.

use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use timelens_ipc::LiveInput;
use timelens_storage::TimelineSnapshot;

use crate::{AppWindow, grouped_count};

static WANTED: AtomicBool = AtomicBool::new(false);
static KEYS_WANTED: AtomicBool = AtomicBool::new(false);
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

/// The keys pressed so far in the minute of the latest preview, by scan code:
/// the one frame the next preview is told apart from.
struct Pressed {
    minute: i64,
    keys: BTreeMap<u32, u32>,
}

/// The newest preview of each recent minute, oldest first, as totals alone,
/// with when each arrived and the dataset they were counted against; and the
/// keys of the latest one.
#[derive(Default)]
struct Previews {
    minutes: Vec<(LiveInput, Instant)>,
    generation: u64,
    pressed: Option<Pressed>,
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

    /// Keep a preview, replacing the one before of its minute, and return
    /// the keys pressed since the preview before.
    fn keep(&mut self, mut input: LiveInput, now: Instant) -> Vec<u32> {
        let pressed = self.press(&input);
        input.key_counts = Vec::new();
        self.current(now);
        self.minutes
            .retain(|(kept, _)| kept.minute_started_at_utc_ms != input.minute_started_at_utc_ms);
        self.minutes.push((input, now));
        self.minutes
            .sort_by_key(|(kept, _)| kept.minute_started_at_utc_ms);
        pressed
    }

    /// The keys whose count went up since the preview before, or all those
    /// of a minute right after its minute; then only these counts are kept.
    /// With no preview of keys before to go by, nothing lights.
    fn press(&mut self, input: &LiveInput) -> Vec<u32> {
        let before = self.pressed.take();
        // Key presses with no keys say the keys are not on screen.
        if input.key_counts.is_empty() && input.keyboard_count > 0 {
            return Vec::new();
        }
        let minute = input.minute_started_at_utc_ms;
        let keys = input
            .key_counts
            .iter()
            .map(|key| (key.scan_code, key.count))
            .collect::<BTreeMap<_, _>>();
        let pressed = match before {
            Some(before) if before.minute == minute => keys
                .iter()
                .filter(|(code, count)| **count > before.keys.get(code).copied().unwrap_or(0))
                .map(|(code, _)| *code)
                .collect(),
            Some(before) if before.minute + 60_000 == minute => keys.keys().copied().collect(),
            _ => Vec::new(),
        };
        self.pressed = Some(Pressed { minute, keys });
        pressed
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

/// Whether previews should also say which keys are pressed.
pub fn keys_wanted() -> bool {
    KEYS_WANTED.load(Ordering::Relaxed)
}

/// Previews are wanted while the statistics page is on screen, with `keys`
/// while its keys are. Those kept stay meanwhile: told apart by minute, they
/// still add only what the stored totals lack, so the counts do not drop back
/// when the page shows again. The keys of the last one go as the keys leave
/// the screen.
pub fn set_wanted(wanted: bool, keys: bool) {
    WANTED.store(wanted, Ordering::Relaxed);
    let keys = wanted && keys;
    if KEYS_WANTED.swap(keys, Ordering::Relaxed) && !keys {
        PREVIEWS.with(|previews| previews.borrow_mut().pressed = None);
    }
}

/// Keep a preview, replacing the one before of its minute, and show it,
/// lighting the keys pressed since the one before.
pub fn receive(window: &AppWindow, input: LiveInput) {
    let date = input.local_date.clone();
    let pressed = PREVIEWS.with(|previews| previews.borrow_mut().keep(input, Instant::now()));
    crate::collection_ui::flash_keys(&date, &pressed);
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

    fn with_keys(minute: i64, keys: &[(u32, u32)]) -> LiveInput {
        LiveInput {
            key_counts: keys
                .iter()
                .map(|&(scan_code, count)| timelens_ipc::PhysicalKeyCount {
                    scan_code,
                    keyboard_layout: 0,
                    count,
                })
                .collect(),
            ..preview(minute, keys.iter().map(|(_, count)| count).sum())
        }
    }

    #[test]
    fn keys_light_as_their_count_goes_up_from_one_preview_to_the_next() {
        let now = Instant::now();
        let mut previews = Previews {
            generation: crate::data_ui::data_generation(),
            ..Previews::default()
        };
        // The first preview of keys is only what the next is told apart from.
        assert!(
            previews
                .keep(with_keys(60_000, &[(0x1e, 2)]), now)
                .is_empty()
        );
        assert_eq!(
            previews.keep(with_keys(60_000, &[(0x1e, 2), (0x1f, 1)]), now),
            [0x1f]
        );
        assert_eq!(
            previews.keep(with_keys(60_000, &[(0x1e, 4), (0x1f, 1)]), now),
            [0x1e]
        );
        // The next minute counts afresh.
        assert_eq!(previews.keep(with_keys(120_000, &[(0x1f, 1)]), now), [0x1f]);
        // Only the latest keys stay; earlier minutes keep their totals alone.
        assert!(
            previews
                .minutes
                .iter()
                .all(|(input, _)| input.key_counts.is_empty())
        );
        assert_eq!(
            previews
                .pressed
                .as_ref()
                .map(|p| (p.minute, p.keys.clone())),
            Some((120_000, BTreeMap::from([(0x1f, 1)])))
        );
        assert_eq!(
            previews.unstored(&BTreeSet::new(), |_| true, now),
            [6, 2, 0, 0]
        );
        // Totals alone say the keys left the screen; back, nothing lights at once.
        assert!(previews.keep(preview(120_000, 3), now).is_empty());
        assert!(previews.pressed.is_none());
        assert!(
            previews
                .keep(with_keys(120_000, &[(0x1f, 3)]), now)
                .is_empty()
        );
        // A minute skipped is no frame to go by either.
        assert!(
            previews
                .keep(with_keys(240_000, &[(0x1e, 1)]), now)
                .is_empty()
        );
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
