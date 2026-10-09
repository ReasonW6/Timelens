//! Native window care: blend the Windows 11 title bar into the Timelens canvas,
//! and repaint the whole window after it returns from being minimized or hidden.
//!
//! The system caption stays in charge of dragging, snap layouts, resize borders
//! and the window menu; only its colors change. Older systems ignore the request.
use std::time::Duration;

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::{ComponentHandle, Timer, TimerMode};

use crate::AppWindow;
use windows::Win32::{
    Foundation::{COLORREF, HWND},
    Graphics::Dwm::{
        DWMWA_BORDER_COLOR, DWMWA_CAPTION_COLOR, DWMWA_TEXT_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE,
        DwmSetWindowAttribute,
    },
};

/// The canvas color at the window's top edge, the title bar text and the frame.
const CAPTION: (u8, u8, u8) = (0xe8, 0xec, 0xf8);
const CAPTION_TEXT: (u8, u8, u8) = (0x1b, 0x21, 0x30);
const BORDER: (u8, u8, u8) = (0xd3, 0xd8, 0xe6);

/// Returns whether the window had a native handle to style.
pub fn apply(window: &slint::Window) -> bool {
    let window_handle = window.window_handle();
    let Ok(handle) = window_handle.window_handle() else {
        return false;
    };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return false;
    };
    let hwnd = HWND(handle.hwnd.get() as *mut _);
    // The UI is light only; keep the caption light even under a dark system theme.
    set(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE.0, 0_u32);
    set(hwnd, DWMWA_CAPTION_COLOR.0, colorref(CAPTION));
    set(hwnd, DWMWA_TEXT_COLOR.0, colorref(CAPTION_TEXT));
    set(hwnd, DWMWA_BORDER_COLOR.0, colorref(BORDER));
    true
}

/// Windows discards a software window's pixels while it is minimized or hidden,
/// but Slint 1.17's software renderer still trusts the buffer it gets back on
/// restore and repaints only what changed, leaving the rest transparent. After
/// every restore, invalidate the whole window: once at once, and once more after
/// the first frames, in case the cleared buffer arrives after the first repaint.
pub fn repaint_after_restore(window: &AppWindow) -> Timer {
    let weak = window.as_weak();
    let mut away = false;
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(40), move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let native = window.window();
        let hidden = !native.is_visible() || native.is_minimized();
        if away && !hidden {
            invalidate(&window);
            let weak = window.as_weak();
            Timer::single_shot(Duration::from_millis(250), move || {
                if let Some(window) = weak.upgrade() {
                    invalidate(&window);
                }
            });
        }
        away = hidden;
    });
    timer
}

fn invalidate(window: &AppWindow) {
    window.set_repaint_epoch(window.get_repaint_epoch().wrapping_add(1));
}

fn colorref((r, g, b): (u8, u8, u8)) -> COLORREF {
    COLORREF(u32::from(r) | (u32::from(g) << 8) | (u32::from(b) << 16))
}

fn set<T>(hwnd: HWND, attribute: i32, value: T) {
    let _ = unsafe {
        DwmSetWindowAttribute(
            hwnd,
            windows::Win32::Graphics::Dwm::DWMWINDOWATTRIBUTE(attribute),
            (&raw const value).cast(),
            size_of::<T>() as u32,
        )
    };
}
