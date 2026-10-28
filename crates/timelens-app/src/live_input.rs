//! Live input counts on the statistics page. While the page is on screen, the
//! collector previews the input of the minute it has not sealed yet. The page
//! adds each minute's newest preview to the stored totals until those include
//! the minute, so the counts move as the user types and clicks. Previews are
//! never stored, and leave out what the statistics leave out: paused
//! collection and input the rules keep anonymous.

use std::{
    cell::RefCell,
    collections::BTreeMap,
    sync::atomic::{AtomicBool, Ordering},
};

use timelens_ipc::LiveInput;
use timelens_storage::TimelineSnapshot;

use crate::{AppWindow, grouped_count};

static WANTED: AtomicBool = AtomicBool::new(false);
/// Stored totals catch up within seconds; this bounds what waits for them.
const KEPT_MINUTES: usize = 10;

/// The overview's stored totals: keyboard, left, middle and right, what they
/// cover, and the latest minute they include.
struct Overview {
    start: i64,
    end: i64,
    stored: Option<i64>,
    counts: [u64; 4],
}

thread_local! {
    /// The newest preview of each recent minute, oldest first.
    static MINUTES: RefCell<Vec<LiveInput>> = const { RefCell::new(Vec::new()) };
    static OVERVIEW: RefCell<Option<Overview>> = const { RefCell::new(None) };
}

/// Whether the core shows previews; the collector sends them only then.
pub fn wanted() -> bool {
    WANTED.load(Ordering::Relaxed)
}

/// Previews are wanted while the statistics page is on screen. Those kept
/// from before would be out of date by the next time it is.
pub fn set_wanted(wanted: bool) {
    if WANTED.swap(wanted, Ordering::Relaxed) && !wanted {
        MINUTES.with(|minutes| minutes.borrow_mut().clear());
    }
}

/// Keep a preview, replacing the one before of its minute, and show it.
pub fn receive(window: &AppWindow, input: LiveInput) {
    MINUTES.with(|minutes| {
        let mut minutes = minutes.borrow_mut();
        minutes.retain(|kept| kept.minute_started_at_utc_ms != input.minute_started_at_utc_ms);
        minutes.push(input);
        minutes.sort_by_key(|kept| kept.minute_started_at_utc_ms);
        let excess = minutes.len().saturating_sub(KEPT_MINUTES);
        minutes.drain(..excess);
    });
    show_overview(window);
    crate::collection_ui::show_keys(window);
}

/// Input previewed after stored totals that include minutes up to `stored`.
#[derive(Default)]
pub struct Unstored {
    /// Keyboard, left, middle and right.
    pub counts: [u64; 4],
    /// Presses per scan code.
    pub keys: BTreeMap<u32, u64>,
}

/// What the previews `keep` takes add to totals stored up to `stored`.
pub fn unstored(stored: Option<i64>, keep: impl Fn(&LiveInput) -> bool) -> Unstored {
    let mut total = Unstored::default();
    MINUTES.with(|minutes| {
        for input in minutes.borrow().iter().filter(|input| {
            stored.is_none_or(|stored| input.minute_started_at_utc_ms > stored) && keep(input)
        }) {
            for (sum, count) in total.counts.iter_mut().zip([
                input.keyboard_count,
                input.left_click_count,
                input.middle_click_count,
                input.right_click_count,
            ]) {
                *sum += u64::from(count);
            }
            for key in &input.key_counts {
                *total.keys.entry(key.scan_code).or_default() += u64::from(key.count);
            }
        }
    });
    total
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
        stored: snapshot.latest_input_minute_utc_ms,
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
        let live = unstored(overview.stored, |input| {
            input.minute_started_at_utc_ms + 60_000 > overview.start
                && input.minute_started_at_utc_ms < overview.end
        });
        let [keyboard, left, middle, right] =
            std::array::from_fn(|index| overview.counts[index].saturating_add(live.counts[index]));
        let mouse = left.saturating_add(middle).saturating_add(right);
        window.set_range_keyboard(grouped_count(keyboard).into());
        window.set_range_mouse(grouped_count(mouse).into());
        window.set_range_keyboard_count(keyboard as f32);
        window.set_range_mouse_count(mouse as f32);
    });
}
