//! Native window care: blend the Windows 11 title bar into the Timelens canvas,
//! and repaint the whole window after it returns from being minimized or hidden.
//!
//! The system caption stays in charge of dragging, snap layouts, resize borders
//! and the window menu; only its colors change. Older systems ignore the request.
use std::{
    cell::RefCell,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::{ComponentHandle, Timer};

use crate::AppWindow;
use windows::Win32::{
    Foundation::{COLORREF, HWND, LPARAM, LRESULT, WPARAM},
    Graphics::Dwm::{
        DWMWA_BORDER_COLOR, DWMWA_CAPTION_COLOR, DWMWA_TEXT_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE,
        DwmSetWindowAttribute,
    },
    UI::{
        Shell::{DefSubclassProc, SetWindowSubclass},
        WindowsAndMessaging::{
            IsIconic, IsWindowVisible, SW_RESTORE, SW_SHOW, SetForegroundWindow, ShowWindow,
            WM_DPICHANGED, WM_SHOWWINDOW, WM_SIZE,
        },
    },
};

/// The canvas color at the window's top edge, the title bar text and the
/// frame, light and dark; they match `Theme.canvas-brush`, `ink` and `line`.
const CAPTION: [(u8, u8, u8); 2] = [(0xe8, 0xec, 0xf8), (0x19, 0x1c, 0x24)];
const CAPTION_TEXT: [(u8, u8, u8); 2] = [(0x1b, 0x21, 0x30), (0xe4, 0xe6, 0xec)];
const BORDER: [(u8, u8, u8); 2] = [(0xd3, 0xd8, 0xe6), (0x2c, 0x30, 0x3a)];

/// Colors the caption to match the light or dark UI, whatever the system
/// theme. Returns whether the window had a native handle to style.
pub fn apply(window: &slint::Window, dark: bool) -> bool {
    let Some(hwnd) = native_handle(window) else {
        return false;
    };
    let tone = usize::from(dark);
    set(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE.0, u32::from(dark));
    set(hwnd, DWMWA_CAPTION_COLOR.0, colorref(CAPTION[tone]));
    set(hwnd, DWMWA_TEXT_COLOR.0, colorref(CAPTION_TEXT[tone]));
    set(hwnd, DWMWA_BORDER_COLOR.0, colorref(BORDER[tone]));
    true
}

pub(crate) fn native_handle(window: &slint::Window) -> Option<HWND> {
    let window_handle = window.window_handle();
    let handle = window_handle.window_handle().ok()?;
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return None;
    };
    Some(HWND(handle.hwnd.get() as *mut _))
}

/// Show, restore and activate the main window after an explicit user request.
pub fn show_and_activate(window: &AppWindow) -> Result<(), slint::PlatformError> {
    window.show()?;
    hook(window);
    window.window().set_minimized(false);
    crate::window_placement::open_main(window);
    if let Some(hwnd) = native_handle(window.window()) {
        unsafe {
            // SW_SHOW preserves a maximized window. Only a minimized native
            // window needs SW_RESTORE; showing alone does not foreground it.
            let command = if IsIconic(hwnd).as_bool() {
                SW_RESTORE
            } else {
                SW_SHOW
            };
            let _ = ShowWindow(hwnd, command);
            let _ = SetForegroundWindow(hwnd);
        }
    }
    Ok(())
}

struct Watch {
    window: slint::Weak<AppWindow>,
    shown: bool,
    changed: Box<dyn Fn(bool)>,
}

thread_local! {
    static WATCH: RefCell<Option<Watch>> = const { RefCell::new(None) };
}
static HOOKED: AtomicBool = AtomicBool::new(false);

/// Calls `changed` on the UI thread whenever the main window starts or stops
/// being seen: shown, hidden, minimized or restored. Nothing polls for it; the
/// native window reports it once [`hook`] has run after its first show.
///
/// Windows discards a software window's pixels while it is minimized or hidden,
/// but Slint 1.17's software renderer still trusts the buffer it gets back on
/// restore and repaints only what changed, leaving the rest transparent. After
/// every restore, invalidate the whole window: once at once, and once more after
/// the first frames, in case the cleared buffer arrives after the first repaint.
pub fn watch(window: &AppWindow, changed: impl Fn(bool) + 'static) {
    WATCH.with(|watch| {
        *watch.borrow_mut() = Some(Watch {
            window: window.as_weak(),
            shown: false,
            changed: Box::new(changed),
        });
    });
}

/// Listens to the native main window, which exists only once it is shown.
pub fn hook(window: &AppWindow) {
    let Some(hwnd) = native_handle(window.window()) else {
        // A show before the event loop runs gets its native window only once
        // the loop starts; look once more when the placement does.
        let weak = window.as_weak();
        Timer::single_shot(Duration::from_millis(50), move || {
            if let Some(window) = weak.upgrade()
                && native_handle(window.window()).is_some()
            {
                hook(&window);
            }
        });
        return;
    };
    if !HOOKED.swap(true, Ordering::AcqRel) {
        let _ = unsafe { SetWindowSubclass(hwnd, Some(subclass), 1, 0) };
    }
    // The show that created the native window came before the hook.
    seen_changed();
    crate::ui_scale::apply(window);
}

unsafe extern "system" fn subclass(
    hwnd: HWND,
    message: u32,
    w: WPARAM,
    l: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    if message == WM_SHOWWINDOW || message == WM_SIZE {
        // Look once the message is done and the window has its new state.
        let _ = slint::invoke_from_event_loop(seen_changed);
    }
    if message == WM_DPICHANGED {
        // Slint takes up the new display scale alone, without the user's.
        let _ = slint::invoke_from_event_loop(crate::ui_scale::refresh);
    }
    unsafe { DefSubclassProc(hwnd, message, w, l) }
}

fn seen_changed() {
    WATCH.with(|watch| {
        let mut watch = watch.borrow_mut();
        let Some(watch) = watch.as_mut() else {
            return;
        };
        let Some(window) = watch.window.upgrade() else {
            return;
        };
        let shown = native_handle(window.window()).is_some_and(|hwnd| unsafe {
            IsWindowVisible(hwnd).as_bool() && !IsIconic(hwnd).as_bool()
        });
        if shown == watch.shown {
            return;
        }
        watch.shown = shown;
        if shown {
            invalidate(&window);
            let weak = window.as_weak();
            Timer::single_shot(Duration::from_millis(250), move || {
                if let Some(window) = weak.upgrade() {
                    invalidate(&window);
                }
            });
        }
        (watch.changed)(shown);
    });
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
