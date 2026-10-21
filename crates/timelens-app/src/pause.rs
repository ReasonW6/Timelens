//! A pause that ends by itself. When it ends is kept beside the theme choice,
//! outside the dataset, so it survives a restart.
use std::{
    cell::Cell,
    fs,
    path::{Path, PathBuf},
    rc::Rc,
    time::Duration,
};

use chrono::{Local, TimeZone};
use slint::{ComponentHandle, Timer, TimerMode};

use crate::{AppWindow, CollectionState};

pub const FILE: &str = "pause-until.txt";

/// Minutes passed to `pause-for` that mean "until tomorrow" and "until resumed".
const UNTIL_TOMORROW: i32 = -2;
const UNTIL_RESUMED: i32 = -1;
/// How long to wait before trying an automatic resume again after it failed.
const RETRY_MS: i64 = 30_000;

fn load(control: &Path) -> Option<i64> {
    fs::read_to_string(control.join(FILE))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn save(control: &Path, until: Option<i64>) {
    let path = control.join(FILE);
    let _ = match until {
        Some(until) => {
            fs::create_dir_all(control).and_then(|()| fs::write(path, until.to_string()))
        }
        None => fs::remove_file(path),
    };
}

/// When a pause chosen now ends, in UTC milliseconds; `None` lasts until resumed.
fn ends(minutes: i32, now_ms: i64) -> Option<i64> {
    match minutes {
        UNTIL_TOMORROW => {
            let now = Local.timestamp_millis_opt(now_ms).single()?;
            let midnight = now.date_naive().succ_opt()?.and_hms_opt(0, 0, 0)?;
            Some(
                Local
                    .from_local_datetime(&midnight)
                    .earliest()?
                    .timestamp_millis(),
            )
        }
        UNTIL_RESUMED => None,
        minutes => Some(now_ms + i64::from(minutes.max(1)) * 60_000),
    }
}

/// How the sidebar and tray say when recording resumes.
pub fn resume_label(until: i64, now_ms: i64) -> String {
    let left = (until - now_ms).max(0);
    if left <= 60 * 60_000 {
        return format!("还剩 {} 分钟", ((left + 59_999) / 60_000).max(1));
    }
    let (Some(end), Some(now)) = (
        Local.timestamp_millis_opt(until).single(),
        Local.timestamp_millis_opt(now_ms).single(),
    ) else {
        return String::new();
    };
    if end.date_naive() == now.date_naive() {
        format!("{} 自动继续", end.format("%H:%M"))
    } else {
        format!("明天 {} 自动继续", end.format("%H:%M"))
    }
}

/// Wires `CollectionState.pause-for` and resumes recording when a pause ends.
/// The countdown only ticks while a timed pause is set.
pub fn install(window: &AppWindow, control: PathBuf) -> Rc<Timer> {
    let until = Rc::new(Cell::new(load(&control)));
    let timer = Rc::new(Timer::default());

    let weak = window.as_weak();
    let chosen = Rc::clone(&until);
    let chosen_control = control.clone();
    let countdown = Rc::clone(&timer);
    window
        .global::<CollectionState>()
        .on_pause_for(move |minutes| {
            let Some(window) = weak.upgrade() else {
                return;
            };
            let state = window.global::<CollectionState>();
            if state.get_busy() {
                return;
            }
            let end = ends(minutes, crate::unix_time_ms());
            chosen.set(end);
            save(&chosen_control, end);
            state.set_paused(true);
            state.invoke_action(0);
            if end.is_some() {
                countdown.restart();
            }
        });

    let paused = until.get().is_some();
    let weak = window.as_weak();
    let ticking = Rc::downgrade(&timer);
    timer.start(TimerMode::Repeated, Duration::from_secs(1), move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let state = window.global::<CollectionState>();
        let now = crate::unix_time_ms();
        let label = match until.get() {
            // Resumed by hand, or the pause failed and was rolled back.
            Some(_) if !state.get_paused() && !state.get_busy() => {
                until.set(None);
                save(&control, None);
                String::new()
            }
            // The deadline is kept until the resume is seen to succeed: a
            // failed update is rolled back to paused and retried a little later.
            Some(end) if end <= now && state.get_paused() && !state.get_busy() => {
                until.set(Some(now + RETRY_MS));
                state.set_paused(false);
                state.invoke_action(0);
                String::new()
            }
            Some(end) => resume_label(end, now),
            None => String::new(),
        };
        if state.get_resume_label() != label.as_str() {
            state.set_resume_label(label.into());
        }
        if until.get().is_none()
            && let Some(timer) = ticking.upgrade()
        {
            timer.stop();
        }
    });
    if !paused {
        timer.stop();
    }
    timer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_pauses_count_down_in_minutes() {
        let now = 1_700_000_000_000;
        assert_eq!(ends(15, now), Some(now + 15 * 60_000));
        assert_eq!(ends(UNTIL_RESUMED, now), None);
        assert_eq!(resume_label(now + 15 * 60_000, now), "还剩 15 分钟");
        assert_eq!(resume_label(now + 30_000, now), "还剩 1 分钟");
        assert!(ends(UNTIL_TOMORROW, now).is_some_and(|end| end > now));
    }
}
