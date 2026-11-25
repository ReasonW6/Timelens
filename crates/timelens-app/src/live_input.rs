//! Live input counts on the statistics page. While the page is on screen, the
//! collector previews the input of the minute it has not sealed yet. The page
//! adds each minute's newest preview to the stored totals until those include
//! the minute, so the counts move as the user types and clicks. While the keys
//! are on screen, previews also count which keys: those counts are added to
//! the stored key counts the same way, and each key lights as its count goes
//! up from one preview to the next (tickets 20 and 21). Which keys a minute
//! counted is let go once the minute shows up in the stored key counts read
//! next, or the keys leave the screen; at the latest the preview expires.
//! Previews are never stored, and leave out what the statistics leave out:
//! paused collection and input the rules keep anonymous.

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

/// The newest preview of each recent minute, oldest first, with when each
/// arrived and the dataset they were counted against; and the keys of the
/// latest one, which the next is told apart from.
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
    /// the keys pressed since the preview before. With the keys off screen,
    /// which keys it counted goes at once: a preview asked for before they
    /// left can still arrive after, and nothing else would let its keys go
    /// until the minute is stored or the preview expires.
    fn keep(&mut self, mut input: LiveInput, keys: bool, now: Instant) -> Vec<u32> {
        let pressed = if keys {
            self.press(&input)
        } else {
            input.key_counts = Vec::new();
            self.pressed = None;
            Vec::new()
        };
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

    /// What the previews `keep` takes add to key counts that include the
    /// `stored` minutes, by scan code. Which keys a stored minute counted is
    /// let go.
    fn unstored_keys(
        &mut self,
        stored: &BTreeSet<i64>,
        keep: impl Fn(&LiveInput) -> bool,
        now: Instant,
    ) -> BTreeMap<u32, u64> {
        self.current(now);
        let mut keys = BTreeMap::<u32, u64>::new();
        for (input, _) in &mut self.minutes {
            if stored.contains(&input.minute_started_at_utc_ms) {
                input.key_counts = Vec::new();
            } else if keep(input) {
                for key in &input.key_counts {
                    *keys.entry(key.scan_code).or_default() += u64::from(key.count);
                }
            }
        }
        keys
    }

    /// The keys left the screen: keep the totals alone.
    fn forget_keys(&mut self) {
        self.pressed = None;
        for (input, _) in &mut self.minutes {
            input.key_counts = Vec::new();
        }
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
/// when the page shows again. Which keys they counted goes as the keys leave
/// the screen.
pub fn set_wanted(wanted: bool, keys: bool) {
    WANTED.store(wanted, Ordering::Relaxed);
    let keys = wanted && keys;
    if KEYS_WANTED.swap(keys, Ordering::Relaxed) && !keys {
        PREVIEWS.with(|previews| previews.borrow_mut().forget_keys());
    }
}

/// Keep a preview, replacing the one before of its minute, and show it,
/// lighting the keys pressed since the one before.
pub fn receive(window: &AppWindow, input: LiveInput) {
    let date = input.local_date.clone();
    let pressed = PREVIEWS.with(|previews| {
        previews
            .borrow_mut()
            .keep(input, keys_wanted(), Instant::now())
    });
    crate::collection_ui::flash_keys(&date, &pressed);
    show_overview(window);
    crate::collection_ui::show_keys(window);
}

/// What the previews `keep` takes add to totals that include the `stored`
/// minutes: keyboard, left, middle and right.
pub fn unstored(stored: &BTreeSet<i64>, keep: impl Fn(&LiveInput) -> bool) -> [u64; 4] {
    PREVIEWS.with(|previews| previews.borrow_mut().unstored(stored, keep, Instant::now()))
}

/// What the previews `keep` takes add to key counts that include the
/// `stored` minutes, by scan code.
pub fn unstored_keys(
    stored: &BTreeSet<i64>,
    keep: impl Fn(&LiveInput) -> bool,
) -> BTreeMap<u32, u64> {
    PREVIEWS.with(|previews| {
        previews
            .borrow_mut()
            .unstored_keys(stored, keep, Instant::now())
    })
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
                .keep(with_keys(60_000, &[(0x1e, 2)]), true, now)
                .is_empty()
        );
        assert_eq!(
            previews.keep(with_keys(60_000, &[(0x1e, 2), (0x1f, 1)]), true, now),
            [0x1f]
        );
        assert_eq!(
            previews.keep(with_keys(60_000, &[(0x1e, 4), (0x1f, 1)]), true, now),
            [0x1e]
        );
        // The next minute counts afresh.
        assert_eq!(
            previews.keep(with_keys(120_000, &[(0x1f, 1)]), true, now),
            [0x1f]
        );
        // The latest keys are what the next preview is told apart from.
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
        assert!(previews.keep(preview(120_000, 3), true, now).is_empty());
        assert!(previews.pressed.is_none());
        assert!(
            previews
                .keep(with_keys(120_000, &[(0x1f, 3)]), true, now)
                .is_empty()
        );
        // A minute skipped is no frame to go by either.
        assert!(
            previews
                .keep(with_keys(240_000, &[(0x1e, 1)]), true, now)
                .is_empty()
        );
    }

    #[test]
    fn key_counts_go_up_live_until_their_minute_is_stored() {
        let now = Instant::now();
        let mut previews = Previews {
            generation: crate::data_ui::data_generation(),
            ..Previews::default()
        };
        let all = |_: &LiveInput| true;
        previews.keep(with_keys(60_000, &[(0x1e, 2)]), true, now);
        previews.keep(with_keys(60_000, &[(0x1e, 3), (0x1f, 1)]), true, now);
        // A newer preview of a minute replaces the one before.
        assert_eq!(
            previews.unstored_keys(&BTreeSet::new(), all, now),
            BTreeMap::from([(0x1e, 3), (0x1f, 1)])
        );
        // The next minute adds to the one before until that one is stored.
        previews.keep(with_keys(120_000, &[(0x1e, 1)]), true, now);
        assert_eq!(
            previews.unstored_keys(&BTreeSet::new(), all, now),
            BTreeMap::from([(0x1e, 4), (0x1f, 1)])
        );
        assert_eq!(
            previews.unstored_keys(&BTreeSet::from([60_000]), all, now),
            BTreeMap::from([(0x1e, 1)])
        );
        // Stored, a minute's keys are let go; its totals wait as before.
        assert!(previews.minutes[0].0.key_counts.is_empty());
        assert_eq!(
            previews.unstored_keys(&BTreeSet::new(), all, now),
            BTreeMap::from([(0x1e, 1)])
        );
        // Only the dates shown count.
        assert!(
            previews
                .unstored_keys(&BTreeSet::new(), |_| false, now)
                .is_empty()
        );
        // As the keys leave the screen, which keys any minute counted goes.
        previews.forget_keys();
        assert!(previews.pressed.is_none());
        assert!(
            previews
                .unstored_keys(&BTreeSet::new(), all, now)
                .is_empty()
        );
        assert_eq!(previews.unstored(&BTreeSet::new(), all, now), [5, 2, 0, 0]);
    }

    #[test]
    fn previews_arriving_with_the_keys_off_screen_keep_no_keys() {
        let now = Instant::now();
        let mut previews = Previews {
            generation: crate::data_ui::data_generation(),
            ..Previews::default()
        };
        let all = |_: &LiveInput| true;
        previews.keep(with_keys(60_000, &[(0x1e, 2)]), true, now);
        previews.forget_keys();
        // A preview of keys asked for before the keys left can still arrive
        // after: its totals count, which keys it counted is let go at once.
        assert!(
            previews
                .keep(with_keys(60_000, &[(0x1e, 3), (0x1f, 1)]), false, now)
                .is_empty()
        );
        assert!(previews.pressed.is_none());
        assert!(
            previews
                .minutes
                .iter()
                .all(|(input, _)| input.key_counts.is_empty())
        );
        assert!(
            previews
                .unstored_keys(&BTreeSet::new(), all, now)
                .is_empty()
        );
        assert_eq!(previews.unstored(&BTreeSet::new(), all, now), [4, 1, 0, 0]);
        // Back on screen, the next preview of keys is only a frame to go by.
        assert!(
            previews
                .keep(with_keys(60_000, &[(0x1e, 4), (0x1f, 1)]), true, now)
                .is_empty()
        );
        assert_eq!(
            previews.unstored_keys(&BTreeSet::new(), all, now),
            BTreeMap::from([(0x1e, 4), (0x1f, 1)])
        );
    }

    #[test]
    fn previews_add_only_the_minutes_not_stored() {
        let now = Instant::now();
        let mut previews = Previews {
            generation: crate::data_ui::data_generation(),
            ..Previews::default()
        };
        previews.keep(preview(60_000, 3), true, now);
        previews.keep(preview(120_000, 2), true, now);
        // A newer preview of a minute replaces the one before.
        previews.keep(preview(120_000, 5), true, now);
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
