//! Keep an initially shown native window inside its monitor's work area.
use slint::{ComponentHandle, PhysicalPosition, PhysicalSize, Timer, Window};
use std::time::Duration;
use windows::Win32::{
    Foundation::{POINT, RECT},
    Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint},
};

pub fn fit_after_show<T: ComponentHandle + 'static>(component: &T) {
    let weak = component.as_weak();
    // Winit caches the pre-show client size until its first native resize event.
    // Fit after that event so a scaled preferred size cannot overwrite the fit.
    Timer::single_shot(Duration::from_millis(50), move || {
        if let Some(component) = weak.upgrade()
            && component.window().is_visible()
        {
            fit_available_space(component.window());
        }
    });
}

fn fit_available_space(window: &Window) {
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
    if let Some((position, size)) =
        fitted_geometry(position, size, info.rcWork, window.scale_factor())
    {
        window.set_size(size);
        window.set_position(position);
    }
}

fn fitted_geometry(
    position: PhysicalPosition,
    size: PhysicalSize,
    work: RECT,
    scale: f32,
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
    if fits {
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
                1.0
            )
            .is_none()
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
            let (position, fitted) = fitted_geometry(position, size, work, scale).unwrap();
            assert!(position.x >= work.left && position.y >= work.top);
            assert!(position.x + fitted.width as i32 + (16.0 * scale).ceil() as i32 <= work.right);
            assert!(
                position.y + fitted.height as i32 + (48.0 * scale).ceil() as i32 <= work.bottom
            );
            assert!(fitted.width <= size.width && fitted.height <= size.height);
        }
    }
}
