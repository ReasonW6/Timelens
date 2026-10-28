//! Keep an initially shown native window inside its monitor's work area.
use slint::{ComponentHandle, PhysicalPosition, PhysicalSize, Timer, Window};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use windows::Win32::{
    Foundation::{POINT, RECT},
    Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint},
};

/// Whether the main window has opened once in this run; later shows keep the
/// size the user left it at.
static OPENED: AtomicBool = AtomicBool::new(false);

pub fn fit_after_show<T: ComponentHandle + 'static>(component: &T) {
    after_show(component, false);
}

/// Like [`fit_after_show`], but the main window's first show in a run also
/// sizes it to its monitor.
pub fn open_main<T: ComponentHandle + 'static>(component: &T) {
    after_show(component, true);
}

fn after_show<T: ComponentHandle + 'static>(component: &T, main: bool) {
    // Headless QA keeps its requested viewport whatever the host monitor is.
    #[cfg(debug_assertions)]
    if crate::qa::headless() {
        return;
    }
    let weak = component.as_weak();
    // Winit caches the pre-show client size until its first native resize event.
    // Fit after that event so a scaled preferred size cannot overwrite the fit.
    Timer::single_shot(Duration::from_millis(50), move || {
        if let Some(component) = weak.upgrade()
            && component.window().is_visible()
        {
            let grow = main && !OPENED.swap(true, Ordering::Relaxed);
            fit_available_space(component.window(), grow);
        }
    });
}

fn fit_available_space(window: &Window, grow: bool) {
    if window.is_maximized() || window.is_fullscreen() {
        return;
    }
    let position = window.position();
    let size = window.size();
    if size.width == 0 || size.height == 0 {
        return;
    }
    let center = POINT {
        x: position.x.saturating_add((size.width / 2) as i32),
        y: position.y.saturating_add((size.height / 2) as i32),
    };
    let monitor = unsafe { MonitorFromPoint(center, MONITOR_DEFAULTTONEAREST) };
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        return;
    }
    let scale = window.scale_factor();
    let size = if grow {
        opening_size(info.rcWork, scale)
    } else {
        size
    };
    // A sized window is centered even when it already fits where it is.
    if let Some((position, size)) = fitted_geometry(position, size, info.rcWork, scale, grow) {
        window.set_size(size);
        window.set_position(position);
    }
}

/// About two fifths of the work area, so other windows stay in view: at least
/// wide enough in logical pixels that the sidebar keeps its labels (it folds
/// into a rail below 1100), as on a 1920×1080 laptop at 125%, and at most a
/// logical size past which lines only get longer. A minimum larger than the
/// monitor is fitted afterwards.
fn opening_size(work: RECT, scale: f32) -> PhysicalSize {
    let share = |available: i32, part: f32, least: f32, most: f32| {
        (available.max(0) as f32 * part).clamp(least * scale, most * scale) as u32
    };
    PhysicalSize::new(
        share(work.right - work.left, 0.6, 1120.0, 1600.0),
        share(work.bottom - work.top, 0.66, 600.0, 1000.0),
    )
}

fn fitted_geometry(
    position: PhysicalPosition,
    size: PhysicalSize,
    work: RECT,
    scale: f32,
    center: bool,
) -> Option<(PhysicalPosition, PhysicalSize)> {
    let width = work.right.checked_sub(work.left)?;
    let height = work.bottom.checked_sub(work.top)?;
    if width <= 0 || height <= 0 || !scale.is_finite() || scale <= 0.0 {
        return None;
    }
    // Slint reports client size and outer position. Reserve native frame and
    // caption space, with a small margin, without assuming a particular theme.
    let frame_width = (16.0 * scale).ceil() as u32;
    let frame_height = (48.0 * scale).ceil() as u32;
    let fits = position.x >= work.left
        && position.y >= work.top
        && i64::from(position.x) + i64::from(size.width) + i64::from(frame_width)
            <= i64::from(work.right)
        && i64::from(position.y) + i64::from(size.height) + i64::from(frame_height)
            <= i64::from(work.bottom);
    if fits && !center {
        return None;
    }
    let size = PhysicalSize::new(
        size.width
            .min((width as u32).saturating_sub(frame_width).max(1)),
        size.height
            .min((height as u32).saturating_sub(frame_height).max(1)),
    );
    let position = PhysicalPosition::new(
        work.left + ((width as u32).saturating_sub(size.width + frame_width) / 2) as i32,
        work.top + ((height as u32).saturating_sub(size.height + frame_height) / 2) as i32,
    );
    Some((position, size))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_a_window_already_inside_the_work_area() {
        let work = RECT {
            left: 0,
            top: 0,
            right: 2560,
            bottom: 1392,
        };
        assert!(
            fitted_geometry(
                PhysicalPosition::new(345, 338),
                PhysicalSize::new(1440, 900),
                work,
                1.0,
                false
            )
            .is_none()
        );
    }

    #[test]
    fn opens_at_a_share_of_the_monitor_and_centered() {
        let work = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        let size = opening_size(work, 1.0);
        assert_eq!(size, PhysicalSize::new(1152, 686));
        let (position, fitted) =
            fitted_geometry(PhysicalPosition::new(0, 0), size, work, 1.0, true).unwrap();
        assert_eq!(fitted, size);
        assert_eq!(position, PhysicalPosition::new(376, 153));
        // The same monitor at 125% keeps the logical minimum, which leaves
        // the sidebar its labels, and still fits.
        let scaled = opening_size(work, 1.25);
        assert_eq!(scaled, PhysicalSize::new(1400, 750));
        assert!(scaled.width as f32 / 1.25 >= 1100.0);
        let (_, fitted) =
            fitted_geometry(PhysicalPosition::new(0, 0), scaled, work, 1.25, true).unwrap();
        assert_eq!(fitted, scaled);
        // A wide monitor stops at the limit, in logical pixels.
        let wide = RECT {
            left: 0,
            top: 0,
            right: 3840,
            bottom: 2100,
        };
        assert_eq!(opening_size(wide, 1.0), PhysicalSize::new(1600, 1000));
        assert_eq!(opening_size(wide, 1.5), PhysicalSize::new(2304, 1386));
    }

    #[test]
    fn brings_scaled_windows_inside_positive_and_negative_monitors() {
        for (work, position, size, scale) in [
            (
                RECT {
                    left: 0,
                    top: 0,
                    right: 2560,
                    bottom: 1392,
                },
                PhysicalPosition::new(345, 338),
                PhysicalSize::new(1800, 1125),
                1.25,
            ),
            (
                RECT {
                    left: -2560,
                    top: -200,
                    right: 0,
                    bottom: 1192,
                },
                PhysicalPosition::new(-2200, 100),
                PhysicalSize::new(2160, 1350),
                1.5,
            ),
        ] {
            let (position, fitted) = fitted_geometry(position, size, work, scale, false).unwrap();
            assert!(position.x >= work.left && position.y >= work.top);
            assert!(position.x + fitted.width as i32 + (16.0 * scale).ceil() as i32 <= work.right);
            assert!(
                position.y + fitted.height as i32 + (48.0 * scale).ceil() as i32 <= work.bottom
            );
            assert!(fitted.width <= size.width && fitted.height <= size.height);
        }
    }
}
