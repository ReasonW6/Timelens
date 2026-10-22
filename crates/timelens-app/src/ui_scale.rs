//! The interface scale: the user's own zoom on top of the Windows display
//! scale. The choice is kept beside the data location pointer, outside the
//! dataset.
use crate::i18n::tr;
use std::{
    cell::{Cell, RefCell},
    fs,
    path::{Path, PathBuf},
};

use slint::{ComponentHandle, PhysicalSize, platform::WindowEvent};
use windows::Win32::{
    Foundation::{HWND, LPARAM, RECT, WPARAM},
    Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow},
    UI::{
        HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow},
        Input::Ime::{
            CANDIDATEFORM, COMPOSITIONFORM, IMN_SETCANDIDATEPOS, IMN_SETCOMPOSITIONWINDOW,
            ImmGetCandidateWindow, ImmGetCompositionWindow, ImmGetContext, ImmReleaseContext,
            ImmSetCandidateWindow, ImmSetCompositionWindow,
        },
        WindowsAndMessaging::{
            GWL_EXSTYLE, GWL_STYLE, GetWindowLongW, MINMAXINFO, SPI_SETWORKAREA, WINDOW_EX_STYLE,
            WINDOW_STYLE, WM_DPICHANGED, WM_IME_NOTIFY, WM_SETTINGCHANGE, WM_WINDOWPOSCHANGED,
        },
    },
};

use crate::{AppWindow, window_chrome};

pub const FILE: &str = "ui-scale.txt";

/// The choices in percent, as `ui-scale-choice` indexes them.
const PERCENTS: [u32; 6] = [80, 90, 100, 110, 125, 150];
const DEFAULT: i32 = 2;

/// The main window's minimum size in `main-window.slint`.
const MIN: (f32, f32) = (960.0, 600.0);
/// The same with room for the native frame and caption.
const MIN_OUTER: (f32, f32) = (MIN.0 + 16.0, MIN.1 + 48.0);

thread_local! {
    static WINDOW: RefCell<Option<slint::Weak<AppWindow>>> = const { RefCell::new(None) };
    /// Windows' display scale and the scale applied over it. Winit still sizes
    /// the window's minimum and places IME windows by the former.
    static APPLIED: Cell<(f32, f32)> = const { Cell::new((1.0, 1.0)) };
    /// The monitor the window was last on, raw.
    static MONITOR: Cell<isize> = const { Cell::new(0) };
    /// Set while correcting IME positions, whose writes notify again.
    static CORRECTING: Cell<bool> = const { Cell::new(false) };
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
/// so the window chrome calls this after each show and [`observe`] after each
/// display change.
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
    let system = dpi as f32 / 96.0;
    let scale = effective(system, percent(window.get_ui_scale_choice()), work);
    APPLIED.set((system, scale));
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
    // Windows enforces a new minimum only on the next resize.
    let size = window.size();
    let least = PhysicalSize::new((MIN.0 * scale).ceil() as u32, (MIN.1 * scale).ceil() as u32);
    if size.width < least.width || size.height < least.height {
        window.set_size(PhysicalSize::new(
            size.width.max(least.width),
            size.height.max(least.height),
        ));
    }
}

/// Applies the scale again, for the display the window is now on.
pub fn refresh() {
    let window = WINDOW.with(|window| window.borrow().as_ref().and_then(|weak| weak.upgrade()));
    if let Some(window) = window {
        apply(&window);
    }
}

/// Watches the main window's messages before they are handled. The scale is
/// worked out again when the display scale, the monitor or its work area
/// changes, and IME windows are moved to where the scaled caret is.
pub fn observe(hwnd: HWND, message: u32, w: WPARAM) {
    match message {
        WM_DPICHANGED => {
            // Slint takes up the new display scale alone, without the user's.
            let _ = slint::invoke_from_event_loop(refresh);
        }
        WM_SETTINGCHANGE if w.0 == SPI_SETWORKAREA.0 as usize => {
            let _ = slint::invoke_from_event_loop(refresh);
        }
        WM_WINDOWPOSCHANGED => {
            let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) }.0 as isize;
            if MONITOR.replace(monitor) != monitor {
                let _ = slint::invoke_from_event_loop(refresh);
            }
        }
        WM_IME_NOTIFY
            if (w.0 as u32 == IMN_SETCOMPOSITIONWINDOW || w.0 as u32 == IMN_SETCANDIDATEPOS)
                && !CORRECTING.get() =>
        {
            correct_ime(hwnd);
        }
        _ => {}
    }
}

/// Winit placed the IME windows by Windows' display scale alone.
fn correct_ime(hwnd: HWND) {
    let (system, scale) = APPLIED.get();
    let ratio = scale / system;
    if (ratio - 1.0).abs() < 0.001 {
        return;
    }
    let point = |x: i32| (x as f32 * ratio).round() as i32;
    let rect = |r: RECT| RECT {
        left: point(r.left),
        top: point(r.top),
        right: point(r.right),
        bottom: point(r.bottom),
    };
    CORRECTING.set(true);
    unsafe {
        let context = ImmGetContext(hwnd);
        if !context.is_invalid() {
            let mut composition = COMPOSITIONFORM::default();
            if ImmGetCompositionWindow(context, &mut composition).as_bool() {
                composition.ptCurrentPos.x = point(composition.ptCurrentPos.x);
                composition.ptCurrentPos.y = point(composition.ptCurrentPos.y);
                composition.rcArea = rect(composition.rcArea);
                let _ = ImmSetCompositionWindow(context, &composition);
            }
            let mut candidate = CANDIDATEFORM::default();
            if ImmGetCandidateWindow(context, 0, &mut candidate).as_bool() {
                candidate.ptCurrentPos.x = point(candidate.ptCurrentPos.x);
                candidate.ptCurrentPos.y = point(candidate.ptCurrentPos.y);
                candidate.rcArea = rect(candidate.rcArea);
                let _ = ImmSetCandidateWindow(context, &candidate);
            }
            let _ = ImmReleaseContext(hwnd, context);
        }
    }
    CORRECTING.set(false);
}

/// Sets the window's minimum from the applied scale, after winit set it from
/// Windows' display scale; `l` is the `WM_GETMINMAXINFO` message's.
pub fn limit(hwnd: HWND, l: LPARAM) {
    let (system, scale) = APPLIED.get();
    if (scale - system).abs() < 0.001 {
        return;
    }
    let mut frame = RECT {
        left: 0,
        top: 0,
        right: (MIN.0 * scale).ceil() as i32,
        bottom: (MIN.1 * scale).ceil() as i32,
    };
    let style = WINDOW_STYLE(unsafe { GetWindowLongW(hwnd, GWL_STYLE) } as u32);
    let extended = WINDOW_EX_STYLE(unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) } as u32);
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    if unsafe { AdjustWindowRectExForDpi(&mut frame, style, false, extended, dpi) }.is_err() {
        return;
    }
    let info = unsafe { &mut *(l.0 as *mut MINMAXINFO) };
    info.ptMinTrackSize.x = frame.right - frame.left;
    info.ptMinTrackSize.y = frame.bottom - frame.top;
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
