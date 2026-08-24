use std::{
    collections::{HashMap, HashSet},
    marker::PhantomData,
    rc::Rc,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use windows::{
    Win32::{
        Foundation::{LPARAM, LRESULT, WPARAM},
        UI::{
            Input::KeyboardAndMouse::GetKeyboardLayout,
            WindowsAndMessaging::{
                CallNextHookEx, GetForegroundWindow, GetWindowThreadProcessId, HHOOK,
                KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_INJECTED, LLMHF_INJECTED, MSLLHOOKSTRUCT,
                SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN,
                WM_KEYUP, WM_LBUTTONDOWN, WM_MBUTTONDOWN, WM_RBUTTONDOWN, WM_SYSKEYDOWN,
                WM_SYSKEYUP,
            },
        },
    },
    core::Error,
};

const MAX_DISTINCT_INPUT_SAMPLES: usize = 4096;
const OTHER_SCAN_CODE: u16 = 0;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum InputSampleKind {
    Keyboard {
        scan_code: u16,
        keyboard_layout: u64,
    },
    Mouse(MouseButton),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InputSample {
    pub minute_started_at_utc_ms: i64,
    pub window_id: u64,
    pub kind: InputSampleKind,
    pub count: u32,
}

#[derive(Debug, Default, Eq, PartialEq)]
pub struct InputDrain {
    pub samples: Vec<InputSample>,
    pub overflowed: bool,
}

pub struct InputMonitor {
    keyboard_hook: HHOOK,
    mouse_hook: HHOOK,
    _not_send: PhantomData<Rc<()>>,
}

impl InputMonitor {
    pub fn new() -> Result<Self, Error> {
        if let Ok(mut state) = hook_state().lock() {
            state.reset();
        }
        hook_contention().store(false, Ordering::Release);
        let keyboard_hook =
            unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_callback), None, 0)? };
        let mouse_hook =
            match unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook_callback), None, 0) } {
                Ok(hook) => hook,
                Err(error) => {
                    let _ = unsafe { UnhookWindowsHookEx(keyboard_hook) };
                    return Err(error);
                }
            };
        Ok(Self {
            keyboard_hook,
            mouse_hook,
            _not_send: PhantomData,
        })
    }

    pub fn drain(&self) -> InputDrain {
        let contention = hook_contention().swap(false, Ordering::AcqRel);
        hook_state()
            .lock()
            .map(|mut state| state.drain(contention))
            .unwrap_or(InputDrain {
                samples: Vec::new(),
                overflowed: true,
            })
    }
}

impl Drop for InputMonitor {
    fn drop(&mut self) {
        let _ = unsafe { UnhookWindowsHookEx(self.keyboard_hook) };
        let _ = unsafe { UnhookWindowsHookEx(self.mouse_hook) };
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct SampleKey {
    minute_started_at_utc_ms: i64,
    window_id: u64,
    kind: InputSampleKind,
}

#[derive(Clone, Copy)]
struct SampleContext {
    minute_started_at_utc_ms: i64,
    window_id: u64,
}

#[derive(Default)]
struct HookState {
    pressed_scan_codes: HashSet<u16>,
    counts: HashMap<SampleKey, u32>,
    overflowed: bool,
}

impl HookState {
    fn reset(&mut self) {
        self.pressed_scan_codes.clear();
        self.counts.clear();
        self.overflowed = false;
    }

    fn record_keyboard(
        &mut self,
        raw_scan_code: u32,
        extended: bool,
        injected: bool,
        key_down: bool,
        keyboard_layout: u64,
        context: SampleContext,
    ) {
        if injected {
            return;
        }
        let scan_code = normalized_scan_code(raw_scan_code, extended);
        if key_down {
            if self.pressed_scan_codes.insert(scan_code) {
                self.increment(SampleKey {
                    minute_started_at_utc_ms: context.minute_started_at_utc_ms,
                    window_id: context.window_id,
                    kind: InputSampleKind::Keyboard {
                        scan_code,
                        keyboard_layout,
                    },
                });
            }
        } else {
            self.pressed_scan_codes.remove(&scan_code);
        }
    }

    fn record_mouse(&mut self, button: MouseButton, injected: bool, context: SampleContext) {
        if !injected {
            self.increment(SampleKey {
                minute_started_at_utc_ms: context.minute_started_at_utc_ms,
                window_id: context.window_id,
                kind: InputSampleKind::Mouse(button),
            });
        }
    }

    fn increment(&mut self, key: SampleKey) {
        if let Some(count) = self.counts.get_mut(&key) {
            *count = count.saturating_add(1);
            return;
        }
        if self.counts.len() >= MAX_DISTINCT_INPUT_SAMPLES {
            self.overflowed = true;
            return;
        }
        self.counts.insert(key, 1);
    }

    fn drain(&mut self, external_overflow: bool) -> InputDrain {
        if external_overflow {
            self.pressed_scan_codes.clear();
        }
        let samples = self
            .counts
            .drain()
            .map(|(key, count)| InputSample {
                minute_started_at_utc_ms: key.minute_started_at_utc_ms,
                window_id: key.window_id,
                kind: key.kind,
                count,
            })
            .collect();
        let overflowed = std::mem::take(&mut self.overflowed) || external_overflow;
        InputDrain {
            samples,
            overflowed,
        }
    }
}

fn hook_state() -> &'static Mutex<HookState> {
    static STATE: OnceLock<Mutex<HookState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(HookState::default()))
}

fn hook_contention() -> &'static AtomicBool {
    static CONTENTION: AtomicBool = AtomicBool::new(false);
    &CONTENTION
}

unsafe extern "system" fn keyboard_hook_callback(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if code >= 0 && lparam.0 != 0 {
        let message = wparam.0 as u32;
        let key_down = message == WM_KEYDOWN || message == WM_SYSKEYDOWN;
        let key_up = message == WM_KEYUP || message == WM_SYSKEYUP;
        if key_down || key_up {
            let event = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
            let window = unsafe { GetForegroundWindow() };
            let thread_id = unsafe { GetWindowThreadProcessId(window, None) };
            let layout = unsafe { GetKeyboardLayout(thread_id) };
            match hook_state().try_lock() {
                Ok(mut state) => state.record_keyboard(
                    event.scanCode,
                    event.flags.contains(LLKHF_EXTENDED),
                    event.flags.contains(LLKHF_INJECTED),
                    key_down,
                    layout.0 as usize as u64,
                    SampleContext {
                        minute_started_at_utc_ms: unix_minute_started_at_ms(),
                        window_id: window_id(window),
                    },
                ),
                Err(_) => hook_contention().store(true, Ordering::Release),
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

unsafe extern "system" fn mouse_hook_callback(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if code >= 0 && lparam.0 != 0 {
        let button = match wparam.0 as u32 {
            WM_LBUTTONDOWN => Some(MouseButton::Left),
            WM_MBUTTONDOWN => Some(MouseButton::Middle),
            WM_RBUTTONDOWN => Some(MouseButton::Right),
            _ => None,
        };
        if let Some(button) = button {
            let event = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
            let window = unsafe { GetForegroundWindow() };
            match hook_state().try_lock() {
                Ok(mut state) => state.record_mouse(
                    button,
                    event.flags & LLMHF_INJECTED != 0,
                    SampleContext {
                        minute_started_at_utc_ms: unix_minute_started_at_ms(),
                        window_id: window_id(window),
                    },
                ),
                Err(_) => hook_contention().store(true, Ordering::Release),
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

fn window_id(window: windows::Win32::Foundation::HWND) -> u64 {
    window.0 as usize as u64
}

fn normalized_scan_code(raw: u32, extended: bool) -> u16 {
    if raw == 0 || raw > 0xff {
        OTHER_SCAN_CODE
    } else {
        raw as u16 | if extended { 0x100 } else { 0 }
    }
}

fn unix_minute_started_at_ms() -> i64 {
    let utc_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64;
    utc_ms.div_euclid(60_000) * 60_000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_only_physical_up_to_down_transitions() {
        let mut state = HookState::default();
        let context = SampleContext {
            minute_started_at_utc_ms: 60_000,
            window_id: 10,
        };
        state.record_keyboard(0x1e, false, false, true, 20, context);
        state.record_keyboard(0x1e, false, false, true, 20, context);
        state.record_keyboard(0x30, false, true, true, 20, context);
        state.record_keyboard(0x1e, false, false, false, 20, context);
        state.record_keyboard(0x1e, false, false, true, 20, context);

        let drain = state.drain(false);
        assert!(!drain.overflowed);
        assert_eq!(drain.samples.len(), 1);
        assert_eq!(drain.samples[0].count, 2);
        assert_eq!(
            drain.samples[0].kind,
            InputSampleKind::Keyboard {
                scan_code: 0x1e,
                keyboard_layout: 20
            }
        );
    }

    #[test]
    fn mouse_counts_only_supported_non_injected_buttons() {
        let mut state = HookState::default();
        let context = SampleContext {
            minute_started_at_utc_ms: 60_000,
            window_id: 10,
        };
        state.record_mouse(MouseButton::Left, false, context);
        state.record_mouse(MouseButton::Left, false, context);
        state.record_mouse(MouseButton::Right, true, context);
        let drain = state.drain(false);
        assert_eq!(drain.samples.len(), 1);
        assert_eq!(drain.samples[0].count, 2);
        assert_eq!(
            drain.samples[0].kind,
            InputSampleKind::Mouse(MouseButton::Left)
        );
    }

    #[test]
    fn splits_counts_at_minute_boundaries_and_reports_external_overflow() {
        let mut state = HookState::default();
        let first_minute = SampleContext {
            minute_started_at_utc_ms: 60_000,
            window_id: 10,
        };
        let second_minute = SampleContext {
            minute_started_at_utc_ms: 120_000,
            window_id: 10,
        };
        state.record_keyboard(0x1e, false, false, true, 20, first_minute);
        state.record_keyboard(0x1e, false, false, false, 20, first_minute);
        state.record_keyboard(0x1e, false, false, true, 20, second_minute);
        let drain = state.drain(true);
        assert!(drain.overflowed);
        assert_eq!(drain.samples.len(), 2);
        assert_eq!(
            drain
                .samples
                .iter()
                .map(|sample| sample.minute_started_at_utc_ms)
                .collect::<HashSet<_>>(),
            HashSet::from([60_000, 120_000])
        );

        state.record_keyboard(0x1e, false, false, true, 20, second_minute);
        assert_eq!(state.drain(false).samples.len(), 1);
    }
}
