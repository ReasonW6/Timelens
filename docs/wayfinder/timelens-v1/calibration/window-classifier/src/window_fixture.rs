use std::{
    env, thread,
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, PROPERTYKEY, WPARAM},
        System::{Com::StructuredStorage::PROPVARIANT, LibraryLoader::GetModuleHandleW},
        UI::{
            Shell::PropertiesSystem::{IPropertyStore, SHGetPropertyStoreForWindow},
            Shell::SetCurrentProcessExplicitAppUserModelID,
            WindowsAndMessaging::*,
        },
    },
    core::{GUID, PCWSTR, w},
};

const APP_USER_MODEL_ID: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x9f4c2855_9f79_4b39_a8d0_e1d42de1d5f3),
    pid: 5,
};

struct Options {
    app_user_model_id: Option<String>,
    hidden_only: bool,
    hide_all: bool,
    step: Duration,
    linger: Duration,
}

fn main() -> windows::core::Result<()> {
    let options = parse_options();
    if let Some(app_id) = &options.app_user_model_id {
        let wide = wide(app_id);
        unsafe { SetCurrentProcessExplicitAppUserModelID(PCWSTR(wide.as_ptr())) }?;
    }
    if options.hidden_only {
        thread::sleep(options.linger);
        return Ok(());
    }

    let instance = unsafe { GetModuleHandleW(None) }?;
    let class = w!("Timelens.WindowClassifier.Fixture");
    let descriptor = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: instance.into(),
        lpszClassName: class,
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }?,
        ..Default::default()
    };
    unsafe { RegisterClassW(&descriptor) };

    let primary = create_window(
        class,
        w!("Timelens fixture primary"),
        WINDOW_EX_STYLE(0),
        None,
        120,
        120,
    )?;
    let secondary = create_window(
        class,
        w!("Timelens fixture secondary"),
        WS_EX_APPWINDOW,
        None,
        620,
        120,
    )?;
    let tool = create_window(
        class,
        w!("Timelens fixture tool"),
        WS_EX_TOOLWINDOW,
        None,
        120,
        500,
    )?;
    let owned = create_window(
        class,
        w!("Timelens fixture owned"),
        WINDOW_EX_STYLE(0),
        Some(primary),
        420,
        500,
    )?;
    if let Some(app_id) = &options.app_user_model_id {
        for window in [primary, secondary, tool, owned] {
            set_window_app_user_model_id(window, app_id)?;
        }
    }
    unsafe {
        let _ = ShowWindow(primary, SW_SHOW);
        let _ = ShowWindow(secondary, SW_SHOW);
        let _ = ShowWindow(tool, SW_SHOW);
        let _ = ShowWindow(owned, SW_SHOW);
        let _ = SetForegroundWindow(primary);
    }

    run_phase(options.step, || unsafe {
        let _ = ShowWindow(primary, SW_MINIMIZE);
    });
    run_phase(options.step, || unsafe {
        let _ = ShowWindow(primary, SW_RESTORE);
    });
    run_phase(options.step, || unsafe {
        let _ = ShowWindow(primary, SW_HIDE);
        if options.hide_all {
            let _ = ShowWindow(secondary, SW_HIDE);
        }
    });
    run_phase(options.step, || unsafe {
        let _ = ShowWindow(primary, SW_SHOW);
        if options.hide_all {
            let _ = ShowWindow(secondary, SW_SHOW);
        }
    });
    run_phase(options.step, || unsafe {
        DestroyWindow(secondary).ok();
    });
    run_phase(options.step, || unsafe {
        DestroyWindow(owned).ok();
        DestroyWindow(tool).ok();
        DestroyWindow(primary).ok();
    });
    pump_for(options.linger);
    Ok(())
}

fn set_window_app_user_model_id(hwnd: HWND, app_id: &str) -> windows::core::Result<()> {
    let store: IPropertyStore = unsafe { SHGetPropertyStoreForWindow(hwnd) }?;
    let value = PROPVARIANT::from(app_id);
    unsafe { store.SetValue(&APP_USER_MODEL_ID, &value) }
}

fn parse_options() -> Options {
    let mut args = env::args().skip(1);
    let mut app_user_model_id = Some("Timelens.Calibration.Sample".to_string());
    let mut hidden_only = false;
    let mut hide_all = false;
    let mut step_ms = 900_u64;
    let mut linger_ms = 1_800_u64;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--aumid" => app_user_model_id = args.next(),
            "--no-aumid" => app_user_model_id = None,
            "--hidden-only" => hidden_only = true,
            "--hide-all" => hide_all = true,
            "--step-ms" => {
                step_ms = args
                    .next()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(step_ms)
            }
            "--linger-ms" => {
                linger_ms = args
                    .next()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(linger_ms)
            }
            _ => {}
        }
    }
    Options {
        app_user_model_id,
        hidden_only,
        hide_all,
        step: Duration::from_millis(step_ms),
        linger: Duration::from_millis(linger_ms),
    }
}

fn create_window(
    class: PCWSTR,
    title: PCWSTR,
    ex_style: WINDOW_EX_STYLE,
    owner: Option<HWND>,
    x: i32,
    y: i32,
) -> windows::core::Result<HWND> {
    let instance = unsafe { GetModuleHandleW(None) }?;
    unsafe {
        CreateWindowExW(
            ex_style,
            class,
            title,
            WS_OVERLAPPEDWINDOW,
            x,
            y,
            420,
            260,
            owner,
            None,
            Some(instance.into()),
            None,
        )
    }
}

fn run_phase(action_delay: Duration, action: impl FnOnce()) {
    pump_for(action_delay);
    action();
}

fn pump_for(duration: Duration) {
    let until = Instant::now() + duration;
    let mut message = MSG::default();
    while Instant::now() < until {
        unsafe {
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_CLOSE => {
            unsafe {
                DestroyWindow(hwnd).ok();
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}
