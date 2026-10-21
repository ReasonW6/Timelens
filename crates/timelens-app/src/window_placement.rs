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
/// grows it to most of the monitor.
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
        opening_size(size, info.rcWork, scale)
    } else {
        size
    };
    // A grown window is centered even when it already fits where it is.
    if let Some((position, size)) = fitted_geometry(position, size, info.rcWork, scale, grow) {
        window.set_size(size);
        window.set_position(position);
    }
}

/// Most of the work area, up to a logical size past which lines only get
/// longer, and never smaller than the window already is.
fn opening_size(size: PhysicalSize, work: RECT, scale: f32) -> PhysicalSize {
    let share = |available: i32, part: f32, limit: f32| {
        (available.max(0) as f32 * part).min(limit * scale) as u32
    };
    PhysicalSize::new(
        size.width.max(share(work.right - work.left, 0.84, 1760.0)),
        size.height.max(share(work.bottom - work.top, 0.88, 1100.0)),
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
    fn opens_large_and_centered_but_not_past_the_limit() {
        let work = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        let size = opening_size(PhysicalSize::new(1440, 900), work, 1.0);
        assert_eq!(size, PhysicalSize::new(1612, 915));
        let (position, fitted) =
            fitted_geometry(PhysicalPosition::new(0, 0), size, work, 1.0, true).unwrap();
        assert_eq!(fitted, size);
        assert_eq!(position, PhysicalPosition::new(146, 38));
        // A wide monitor stops at the limit; a window already larger stays so.
        let wide = RECT {
            left: 0,
            top: 0,
            right: 3840,
            bottom: 2100,
        };
        assert_eq!(
            opening_size(PhysicalSize::new(1440, 900), wide, 1.0),
            PhysicalSize::new(1760, 1100)
        );
        assert_eq!(
            opening_size(PhysicalSize::new(2000, 1200), wide, 1.0),
            PhysicalSize::new(2000, 1200)
        );
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
