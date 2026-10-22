//! The interface scale: the user's own zoom on top of the Windows display
//! scale. The choice is kept beside the data location pointer, outside the
//! dataset.
use crate::i18n::tr;
use std::{
    cell::RefCell,
    fs,
    path::{Path, PathBuf},
};

use slint::{ComponentHandle, platform::WindowEvent};
use windows::Win32::{
    Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow},
    UI::HiDpi::GetDpiForWindow,
};

use crate::{AppWindow, window_chrome};

pub const FILE: &str = "ui-scale.txt";

/// The choices in percent, as `ui-scale-choice` indexes them.
const PERCENTS: [u32; 6] = [80, 90, 100, 110, 125, 150];
const DEFAULT: i32 = 2;

/// The main window's minimum size in `main-window.slint`, with room for the
/// native frame and caption.
const MIN_OUTER: (f32, f32) = (960.0 + 16.0, 600.0 + 48.0);

thread_local! {
    static WINDOW: RefCell<Option<slint::Weak<AppWindow>>> = const { RefCell::new(None) };
}

fn parse(text: &str) -> i32 {
    PERCENTS
        .iter()
        .position(|percent| text.trim() == percent.to_string())
        .map_or(DEFAULT, |index| index as i32)
}

fn percent(choice: i32) -> u32 {
    PERCENTS[choice.clamp(0, PERCENTS.len() as i32 - 1) as usize]
}

fn load(control: &Path) -> i32 {
    fs::read_to_string(control.join(FILE)).map_or(DEFAULT, |text| parse(&text))
}

fn save(control: &Path, choice: i32) -> std::io::Result<()> {
    fs::create_dir_all(control)?;
    fs::write(control.join(FILE), percent(choice).to_string())
}

/// Windows' display scale times the choice. Enlarging stops where the window
/// at its minimum size would no longer fit the monitor's work area.
fn effective(system: f32, percent: u32, work: (i32, i32)) -> f32 {
    let scale = system * percent as f32 / 100.0;
    if scale <= system {
        return scale;
    }
    let fit = (work.0 as f32 / MIN_OUTER.0).min(work.1 as f32 / MIN_OUTER.1);
    scale.min(fit).max(system)
}

/// Applies the chosen scale to the main window. Its native window exists only
/// once shown, and Slint resets the scale whenever the display scale changes,
/// so the window chrome calls this after each show and display change.
pub fn apply(window: &AppWindow) {
    let Some(hwnd) = window_chrome::native_handle(window.window()) else {
        return;
    };
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    if dpi == 0 {
        return;
    }
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let work = if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        let work = info.rcWork;
        (work.right - work.left, work.bottom - work.top)
    } else {
        (i32::MAX, i32::MAX)
    };
    let scale = effective(
        dpi as f32 / 96.0,
        percent(window.get_ui_scale_choice()),
        work,
    );
    let window = window.window();
    if (window.scale_factor() - scale).abs() < 0.001 {
        return;
    }
    // The physical size stays; the layout gets the logical size that is left.
    window.dispatch_event(WindowEvent::ScaleFactorChanged {
        scale_factor: scale,
    });
    window.dispatch_event(WindowEvent::Resized {
        size: window.size().to_logical(scale),
    });
    window.request_redraw();
}

/// Applies the scale again after Windows changed the display scale.
pub fn refresh() {
    let window = WINDOW.with(|window| window.borrow().as_ref().and_then(|weak| weak.upgrade()));
    if let Some(window) = window {
        apply(&window);
    }
}

/// Shows the saved choice and saves new ones; [`apply`] takes them up.
pub fn install(window: &AppWindow, control: PathBuf) {
    window.set_ui_scale_choice(load(&control));
    WINDOW.with(|weak| *weak.borrow_mut() = Some(window.as_weak()));

    let weak = window.as_weak();
    window.on_ui_scale_selected(move |choice| {
        let Some(window) = weak.upgrade() else {
            return;
        };
        window.set_ui_scale_choice(choice);
        // Rescaling inside the dropdown's click leaves the next click on the
        // settings close button unanswered; let the dropdown finish first.
        let _ = slint::invoke_from_event_loop(refresh);
        if let Err(error) = save(&control, choice) {
            window.set_action_status(tr!("界面缩放设置未能保存：{}", error).into());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_choice_round_trips_and_anything_else_is_full_size() {
        let directory = tempfile::tempdir().unwrap();
        let control = directory.path().join("control");
        assert_eq!(load(&control), DEFAULT);
        for choice in [0, 5, 3, 2] {
            save(&control, choice).unwrap();
            assert_eq!(load(&control), choice);
        }
        fs::write(control.join(FILE), "175").unwrap();
        assert_eq!(load(&control), DEFAULT);
    }

    #[test]
    fn enlarging_stops_where_the_smallest_window_still_fits() {
        let roomy = (1920, 1032);
        assert_eq!(effective(1.0, 100, roomy), 1.0);
        assert_eq!(effective(1.25, 80, roomy), 1.0);
        assert_eq!(effective(1.0, 150, roomy), 1.5);
        // 1366×728 holds the minimum window at 1.12 at most.
        let small = effective(1.0, 150, (1366, 728));
        assert!((small - 728.0 / 648.0).abs() < 0.001);
        // A display scale that already overflows is not shrunk further.
        assert_eq!(effective(1.5, 125, (1366, 728)), 1.5);
    }
}
