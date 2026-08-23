use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    env,
    ffi::c_void,
    fs::File,
    io::{BufWriter, Write},
    path::PathBuf,
    sync::{
        Mutex, OnceLock,
        mpsc::{self, Sender},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows::{
    Win32::{
        Foundation::{
            CloseHandle, FILETIME, HANDLE, HWND, LPARAM, LRESULT, PROPERTYKEY, RECT, WAIT_TIMEOUT,
            WPARAM,
        },
        Graphics::{
            Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute},
            Gdi::{MONITOR_DEFAULTTONULL, MonitorFromWindow},
        },
        Storage::Packaging::Appx::{GetApplicationUserModelId, GetPackageFullName},
        System::{
            Com::StructuredStorage::PropVariantToStringAlloc,
            Com::{
                CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
            },
            LibraryLoader::GetModuleHandleW,
            Power::RegisterSuspendResumeNotification,
            RemoteDesktop::{NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification},
            Threading::{
                GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
                PROCESS_SYNCHRONIZE, QueryFullProcessImageNameW, WaitForSingleObject,
            },
        },
        UI::{
            Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent},
            Shell::PropertiesSystem::{IPropertyStore, SHGetPropertyStoreForWindow},
            Shell::{IVirtualDesktopManager, VirtualDesktopManager},
            WindowsAndMessaging::*,
        },
    },
    core::{BOOL, GUID, PWSTR, w},
};

static EVENT_SENDER: OnceLock<Mutex<Sender<RawEvent>>> = OnceLock::new();
const APP_USER_MODEL_ID: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x9f4c2855_9f79_4b39_a8d0_e1d42de1d5f3),
    pid: 5,
};

#[derive(Clone)]
struct RawEvent {
    source: &'static str,
    event: u32,
    hwnd: isize,
    object_id: i32,
    child_id: i32,
    event_time_ms: u32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RectSnapshot {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WindowSnapshot {
    hwnd: String,
    pid: u32,
    process_creation_100ns: Option<u64>,
    path: Option<String>,
    process_app_user_model_id: Option<String>,
    window_app_user_model_id: Option<String>,
    package_full_name: Option<String>,
    identity_key: Option<String>,
    class_name: String,
    root_hwnd: String,
    owner_hwnd: Option<String>,
    ex_style: String,
    rect: Option<RectSnapshot>,
    visible: bool,
    minimized: bool,
    cloaked: Option<bool>,
    monitor_present: bool,
    on_current_virtual_desktop: Option<bool>,
    virtual_desktop_id: Option<String>,
    structural_candidate: bool,
    onboarding_candidate: bool,
    tracked_user_window: bool,
    displayed_estimate: bool,
    focused: bool,
    classifier_state: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Record<'a> {
    unix_ms: u128,
    elapsed_ms: u128,
    source: &'a str,
    event: &'a str,
    event_code: Option<u32>,
    event_time_ms: Option<u32>,
    object_id: Option<i32>,
    child_id: Option<i32>,
    hwnd: Option<String>,
    snapshot: Option<WindowSnapshot>,
    note: Option<String>,
}

struct Options {
    output: PathBuf,
    duration: Duration,
    reconcile: Duration,
    class_prefix: Option<String>,
}

struct TrackedProcess {
    pid: u32,
    creation_100ns: Option<u64>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let options = parse_options()?;
    let (sender, receiver) = mpsc::channel();
    EVENT_SENDER
        .set(Mutex::new(sender))
        .map_err(|_| "event sender already initialized")?;

    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok()?;
    let virtual_desktop: Option<IVirtualDesktopManager> =
        unsafe { CoCreateInstance(&VirtualDesktopManager, None, CLSCTX_ALL) }.ok();
    let receiver_window = create_receiver_window()?;
    let wts_registered =
        unsafe { WTSRegisterSessionNotification(receiver_window, NOTIFY_FOR_THIS_SESSION).is_ok() };
    let power_registered = unsafe {
        RegisterSuspendResumeNotification(HANDLE(receiver_window.0), DEVICE_NOTIFY_WINDOW_HANDLE)
            .is_ok()
    };
    let hooks = install_hooks()?;

    let file = File::create(&options.output)?;
    let mut output = BufWriter::new(file);
    let started = Instant::now();
    let mut next_reconcile = started;
    let mut tracked = HashMap::<String, TrackedProcess>::new();
    let mut tracked_windows = HashSet::<usize>::new();

    write_record(
        &mut output,
        &Record {
            unix_ms: unix_ms(),
            elapsed_ms: 0,
            source: "observer",
            event: "started",
            event_code: None,
            event_time_ms: None,
            object_id: None,
            child_id: None,
            hwnd: Some(hwnd_hex(receiver_window)),
            snapshot: None,
            note: Some(format!(
                "wts_registered={wts_registered};power_registered={power_registered};hooks={}",
                hooks.len()
            )),
        },
    )?;

    while started.elapsed() < options.duration {
        pump_messages();
        while let Ok(raw) = receiver.try_recv() {
            let hwnd = HWND(raw.hwnd as *mut c_void);
            let was_tracked = tracked_windows.contains(&(raw.hwnd as usize));
            if raw.event == EVENT_OBJECT_DESTROY {
                tracked_windows.remove(&(raw.hwnd as usize));
            }
            let mut snapshot = if raw.hwnd != 0 && raw.event != EVENT_OBJECT_DESTROY {
                inspect_window(hwnd, virtual_desktop.as_ref())
            } else {
                None
            };
            if snapshot.is_none() && options.class_prefix.is_some() && !was_tracked {
                continue;
            }
            if snapshot
                .as_ref()
                .is_some_and(|value| !matches_filter(value, &options))
            {
                continue;
            }
            if let Some(value) = snapshot.as_mut() {
                if value.onboarding_candidate {
                    tracked_windows.insert(hwnd.0 as usize);
                }
                apply_classification(value, tracked_windows.contains(&(hwnd.0 as usize)));
            }
            write_record(
                &mut output,
                &Record {
                    unix_ms: unix_ms(),
                    elapsed_ms: started.elapsed().as_millis(),
                    source: raw.source,
                    event: event_name(raw.source, raw.event),
                    event_code: Some(raw.event),
                    event_time_ms: Some(raw.event_time_ms),
                    object_id: Some(raw.object_id),
                    child_id: Some(raw.child_id),
                    hwnd: (raw.hwnd != 0).then(|| hwnd_hex(hwnd)),
                    snapshot,
                    note: None,
                },
            )?;
        }

        if Instant::now() >= next_reconcile {
            reconcile(
                &mut output,
                started,
                virtual_desktop.as_ref(),
                &options,
                &mut tracked_windows,
                &mut tracked,
            )?;
            next_reconcile += options.reconcile;
        }
        thread::sleep(Duration::from_millis(10));
    }

    for hook in hooks {
        let _ = unsafe { UnhookWinEvent(hook) };
    }
    unsafe { DestroyWindow(receiver_window) }?;
    write_record(
        &mut output,
        &Record {
            unix_ms: unix_ms(),
            elapsed_ms: started.elapsed().as_millis(),
            source: "observer",
            event: "stopped",
            event_code: None,
            event_time_ms: None,
            object_id: None,
            child_id: None,
            hwnd: None,
            snapshot: None,
            note: None,
        },
    )?;
    output.flush()?;
    Ok(())
}

fn parse_options() -> Result<Options, String> {
    let mut args = env::args().skip(1);
    let mut output = None;
    let mut duration_seconds = 20_u64;
    let mut reconcile_ms = 500_u64;
    let mut class_prefix = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--output" => output = args.next().map(PathBuf::from),
            "--duration-seconds" => {
                duration_seconds = args
                    .next()
                    .ok_or("missing duration")?
                    .parse()
                    .map_err(|_| "invalid duration")?
            }
            "--reconcile-ms" => {
                reconcile_ms = args
                    .next()
                    .ok_or("missing reconcile interval")?
                    .parse()
                    .map_err(|_| "invalid reconcile interval")?
            }
            "--class-prefix" => class_prefix = args.next(),
            _ => return Err(format!("unknown argument: {arg}")),
        }
    }
    Ok(Options {
        output: output.ok_or("--output is required")?,
        duration: Duration::from_secs(duration_seconds),
        reconcile: Duration::from_millis(reconcile_ms.max(100)),
        class_prefix,
    })
}

fn install_hooks() -> Result<Vec<HWINEVENTHOOK>, String> {
    let ranges = [
        (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND),
        (EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND),
        (EVENT_OBJECT_CREATE, EVENT_OBJECT_HIDE),
        (EVENT_OBJECT_CLOAKED, EVENT_OBJECT_UNCLOAKED),
    ];
    let mut hooks = Vec::new();
    for (start, end) in ranges {
        let hook = unsafe {
            SetWinEventHook(
                start,
                end,
                None,
                Some(win_event),
                0,
                0,
                WINEVENT_OUTOFCONTEXT,
            )
        };
        if hook.is_invalid() {
            return Err(format!("SetWinEventHook failed for {start:#x}..={end:#x}"));
        }
        hooks.push(hook);
    }
    Ok(hooks)
}

unsafe extern "system" fn win_event(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    object_id: i32,
    child_id: i32,
    _thread_id: u32,
    event_time_ms: u32,
) {
    let object_event = (EVENT_OBJECT_CREATE..=EVENT_OBJECT_HIDE).contains(&event)
        || (EVENT_OBJECT_CLOAKED..=EVENT_OBJECT_UNCLOAKED).contains(&event);
    if object_event && (object_id != OBJID_WINDOW.0 || child_id != CHILDID_SELF as i32) {
        return;
    }
    send_raw(RawEvent {
        source: "winevent",
        event,
        hwnd: hwnd.0 as isize,
        object_id,
        child_id,
        event_time_ms,
    });
}

unsafe extern "system" fn receiver_wnd_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_WTSSESSION_CHANGE => send_raw(RawEvent {
            source: "session",
            event: wparam.0 as u32,
            hwnd: 0,
            object_id: lparam.0 as i32,
            child_id: 0,
            event_time_ms: 0,
        }),
        WM_POWERBROADCAST => send_raw(RawEvent {
            source: "power",
            event: wparam.0 as u32,
            hwnd: 0,
            object_id: 0,
            child_id: 0,
            event_time_ms: 0,
        }),
        WM_QUERYENDSESSION | WM_ENDSESSION => send_raw(RawEvent {
            source: "session-end",
            event: message,
            hwnd: 0,
            object_id: wparam.0 as i32,
            child_id: lparam.0 as i32,
            event_time_ms: 0,
        }),
        _ => {}
    }
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

fn send_raw(event: RawEvent) {
    if let Some(sender) = EVENT_SENDER.get() {
        if let Ok(sender) = sender.lock() {
            let _ = sender.send(event);
        }
    }
}

fn create_receiver_window() -> windows::core::Result<HWND> {
    let instance = unsafe { GetModuleHandleW(None) }?;
    let class = w!("Timelens.WindowObserver.Receiver");
    let descriptor = WNDCLASSW {
        lpfnWndProc: Some(receiver_wnd_proc),
        hInstance: instance.into(),
        lpszClassName: class,
        ..Default::default()
    };
    unsafe { RegisterClassW(&descriptor) };
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!("Timelens observer receiver"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )
    }
}

fn pump_messages() {
    let mut message = MSG::default();
    unsafe {
        while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

fn reconcile(
    output: &mut BufWriter<File>,
    started: Instant,
    virtual_desktop: Option<&IVirtualDesktopManager>,
    options: &Options,
    tracked_windows: &mut HashSet<usize>,
    tracked: &mut HashMap<String, TrackedProcess>,
) -> Result<(), Box<dyn std::error::Error>> {
    let windows = enumerate_windows()?;
    let mut alive_window_keys = HashSet::new();
    let mut observed_windows = HashSet::new();
    for hwnd in windows {
        if let Some(mut snapshot) = inspect_window(hwnd, virtual_desktop) {
            if !matches_filter(&snapshot, options) {
                continue;
            }
            observed_windows.insert(hwnd.0 as usize);
            if snapshot.onboarding_candidate {
                tracked_windows.insert(hwnd.0 as usize);
            }
            apply_classification(&mut snapshot, tracked_windows.contains(&(hwnd.0 as usize)));
            if snapshot.tracked_user_window {
                if let Some(creation) = snapshot.process_creation_100ns {
                    let key = format!("{}:{creation}", snapshot.pid);
                    alive_window_keys.insert(key.clone());
                    tracked.entry(key).or_insert(TrackedProcess {
                        pid: snapshot.pid,
                        creation_100ns: snapshot.process_creation_100ns,
                    });
                }
            }
            write_record(
                output,
                &Record {
                    unix_ms: unix_ms(),
                    elapsed_ms: started.elapsed().as_millis(),
                    source: "reconcile",
                    event: "window_snapshot",
                    event_code: None,
                    event_time_ms: None,
                    object_id: None,
                    child_id: None,
                    hwnd: Some(hwnd_hex(hwnd)),
                    snapshot: Some(snapshot),
                    note: None,
                },
            )?;
        }
    }
    tracked_windows.retain(|hwnd| observed_windows.contains(hwnd));

    let mut terminated = Vec::new();
    for (key, process) in tracked.iter() {
        if alive_window_keys.contains(key) {
            continue;
        }
        let alive = process_is_alive(process.pid, process.creation_100ns);
        write_record(
            output,
            &Record {
                unix_ms: unix_ms(),
                elapsed_ms: started.elapsed().as_millis(),
                source: "reconcile",
                event: if alive {
                    "inferred_tray_background"
                } else {
                    "tracked_process_exited"
                },
                event_code: None,
                event_time_ms: None,
                object_id: None,
                child_id: None,
                hwnd: None,
                snapshot: None,
                note: Some(format!(
                    "pid={};creation100ns={:?}",
                    process.pid, process.creation_100ns
                )),
            },
        )?;
        if !alive {
            terminated.push(key.clone());
        }
    }
    for key in terminated {
        tracked.remove(&key);
    }
    output.flush()?;
    Ok(())
}

fn matches_filter(snapshot: &WindowSnapshot, options: &Options) -> bool {
    options
        .class_prefix
        .as_ref()
        .is_none_or(|prefix| snapshot.class_name.starts_with(prefix))
}

fn apply_classification(snapshot: &mut WindowSnapshot, tracked: bool) {
    snapshot.tracked_user_window = tracked;
    snapshot.classifier_state = if !tracked {
        "ignored"
    } else if snapshot.focused {
        "focused"
    } else if snapshot.displayed_estimate {
        "displayed"
    } else {
        "background"
    };
}

fn enumerate_windows() -> windows::core::Result<Vec<HWND>> {
    unsafe extern "system" fn callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let windows = unsafe { &mut *(lparam.0 as *mut Vec<HWND>) };
        windows.push(hwnd);
        true.into()
    }
    let mut windows = Vec::new();
    unsafe { EnumWindows(Some(callback), LPARAM(&mut windows as *mut _ as isize)) }?;
    Ok(windows)
}

fn inspect_window(
    hwnd: HWND,
    virtual_desktop: Option<&IVirtualDesktopManager>,
) -> Option<WindowSnapshot> {
    if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        return None;
    }
    let mut pid = 0_u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    let process = process_info(pid);
    let class_name = class_name(hwnd);
    let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
    let owner = unsafe { GetWindow(hwnd, GW_OWNER) }.ok();
    let ex_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
    let visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
    let minimized = unsafe { IsIconic(hwnd) }.as_bool();
    let cloaked = cloaked(hwnd);
    let rect = window_rect(hwnd);
    let monitor_present = !unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONULL) }.is_invalid();
    let (on_current_virtual_desktop, virtual_desktop_id) = virtual_desktop
        .map(|manager| unsafe {
            (
                manager
                    .IsWindowOnCurrentVirtualDesktop(hwnd)
                    .ok()
                    .map(|value| value.as_bool()),
                manager
                    .GetWindowDesktopId(hwnd)
                    .ok()
                    .map(|value| format!("{value:?}")),
            )
        })
        .unwrap_or((None, None));
    let tool_window = ex_style & WS_EX_TOOLWINDOW.0 != 0;
    let app_window = ex_style & WS_EX_APPWINDOW.0 != 0;
    let no_activate = ex_style & WS_EX_NOACTIVATE.0 != 0;
    let structural_candidate = root == hwnd
        && (owner.is_none() || app_window)
        && (!tool_window || app_window)
        && !no_activate;
    let has_area = rect
        .as_ref()
        .is_some_and(|value| value.right > value.left && value.bottom > value.top);
    let focused = unsafe { GetForegroundWindow() } == hwnd;
    let cloak_allows_onboarding =
        !cloaked.unwrap_or(false) || on_current_virtual_desktop == Some(false) || focused;
    let onboarding_candidate =
        structural_candidate && visible && monitor_present && has_area && cloak_allows_onboarding;
    let displayed_estimate = structural_candidate
        && visible
        && !minimized
        && !cloaked.unwrap_or(false)
        && monitor_present
        && has_area
        && on_current_virtual_desktop.unwrap_or(true);
    let window_app_user_model_id = window_app_user_model_id(hwnd);
    let identity_key = identity_key(
        window_app_user_model_id.as_deref(),
        process
            .as_ref()
            .and_then(|value| value.app_user_model_id.as_deref()),
        process
            .as_ref()
            .and_then(|value| value.package_full_name.as_deref()),
        process.as_ref().and_then(|value| value.path.as_deref()),
    );

    Some(WindowSnapshot {
        hwnd: hwnd_hex(hwnd),
        pid,
        process_creation_100ns: process.as_ref().and_then(|value| value.creation_100ns),
        path: process.as_ref().and_then(|value| value.path.clone()),
        process_app_user_model_id: process
            .as_ref()
            .and_then(|value| value.app_user_model_id.clone()),
        window_app_user_model_id,
        package_full_name: process
            .as_ref()
            .and_then(|value| value.package_full_name.clone()),
        identity_key,
        class_name,
        root_hwnd: hwnd_hex(root),
        owner_hwnd: owner.map(hwnd_hex),
        ex_style: format!("0x{ex_style:08X}"),
        rect,
        visible,
        minimized,
        cloaked,
        monitor_present,
        on_current_virtual_desktop,
        virtual_desktop_id,
        structural_candidate,
        onboarding_candidate,
        tracked_user_window: false,
        displayed_estimate,
        focused,
        classifier_state: "unclassified",
    })
}

struct ProcessInfo {
    path: Option<String>,
    creation_100ns: Option<u64>,
    app_user_model_id: Option<String>,
    package_full_name: Option<String>,
}

fn process_info(pid: u32) -> Option<ProcessInfo> {
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            false,
            pid,
        )
    }
    .ok()?;
    let mut path_buffer = vec![0_u16; 32_768];
    let mut path_length = path_buffer.len() as u32;
    let path = unsafe {
        QueryFullProcessImageNameW(
            handle,
            Default::default(),
            PWSTR(path_buffer.as_mut_ptr()),
            &mut path_length,
        )
    }
    .ok()
    .map(|_| String::from_utf16_lossy(&path_buffer[..path_length as usize]));
    let creation_100ns = process_creation(handle);
    let app_user_model_id = query_package_string(handle, GetApplicationUserModelId);
    let package_full_name = query_package_string(handle, GetPackageFullName);
    unsafe { CloseHandle(handle) }.ok();
    Some(ProcessInfo {
        path,
        creation_100ns,
        app_user_model_id,
        package_full_name,
    })
}

fn process_creation(handle: HANDLE) -> Option<u64> {
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) }.ok()?;
    Some(((creation.dwHighDateTime as u64) << 32) | creation.dwLowDateTime as u64)
}

fn query_package_string(
    handle: HANDLE,
    function: unsafe fn(HANDLE, *mut u32, Option<PWSTR>) -> windows::Win32::Foundation::WIN32_ERROR,
) -> Option<String> {
    let mut length = 0_u32;
    let _ = unsafe { function(handle, &mut length, None) };
    if length == 0 {
        return None;
    }
    let mut buffer = vec![0_u16; length as usize];
    if unsafe { function(handle, &mut length, Some(PWSTR(buffer.as_mut_ptr()))) }.0 != 0 {
        return None;
    }
    let used = buffer
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(buffer.len());
    (used > 0).then(|| String::from_utf16_lossy(&buffer[..used]))
}

fn process_is_alive(pid: u32, expected_creation: Option<u64>) -> bool {
    let Ok(handle) = (unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            false,
            pid,
        )
    }) else {
        return false;
    };
    let same_process = expected_creation.is_none() || process_creation(handle) == expected_creation;
    let alive = same_process && unsafe { WaitForSingleObject(handle, 0) } == WAIT_TIMEOUT;
    unsafe { CloseHandle(handle) }.ok();
    alive
}

fn window_app_user_model_id(hwnd: HWND) -> Option<String> {
    let store: IPropertyStore = unsafe { SHGetPropertyStoreForWindow(hwnd) }.ok()?;
    let value = unsafe { store.GetValue(&APP_USER_MODEL_ID) }.ok()?;
    let text = unsafe { PropVariantToStringAlloc(&value) }.ok()?;
    let result = unsafe { text.to_string() }
        .ok()
        .filter(|value| !value.is_empty());
    unsafe { CoTaskMemFree(Some(text.0.cast())) };
    result
}

fn identity_key(
    window_aumid: Option<&str>,
    process_aumid: Option<&str>,
    package: Option<&str>,
    path: Option<&str>,
) -> Option<String> {
    window_aumid
        .map(|value| format!("aumid:{}", value.to_lowercase()))
        .or_else(|| process_aumid.map(|value| format!("aumid:{}", value.to_lowercase())))
        .or_else(|| package.map(|value| format!("package:{}", value.to_lowercase())))
        .or_else(|| path.map(|value| format!("path:{}", value.replace('/', "\\").to_lowercase())))
}

fn class_name(hwnd: HWND) -> String {
    let mut buffer = vec![0_u16; 512];
    let length = unsafe { GetClassNameW(hwnd, &mut buffer) };
    String::from_utf16_lossy(&buffer[..length.max(0) as usize])
}

fn window_rect(hwnd: HWND) -> Option<RectSnapshot> {
    let mut rect = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut rect) }.ok()?;
    Some(RectSnapshot {
        left: rect.left,
        top: rect.top,
        right: rect.right,
        bottom: rect.bottom,
    })
}

fn cloaked(hwnd: HWND) -> Option<bool> {
    let mut value = 0_u32;
    unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            (&mut value as *mut u32).cast(),
            std::mem::size_of::<u32>() as u32,
        )
    }
    .ok()?;
    Some(value != 0)
}

fn event_name(source: &str, event: u32) -> &'static str {
    match source {
        "winevent" => match event {
            EVENT_SYSTEM_FOREGROUND => "foreground",
            EVENT_SYSTEM_MINIMIZESTART => "minimize_start",
            EVENT_SYSTEM_MINIMIZEEND => "minimize_end",
            EVENT_OBJECT_CREATE => "create",
            EVENT_OBJECT_DESTROY => "destroy",
            EVENT_OBJECT_SHOW => "show",
            EVENT_OBJECT_HIDE => "hide",
            EVENT_OBJECT_CLOAKED => "cloaked",
            EVENT_OBJECT_UNCLOAKED => "uncloaked",
            _ => "other_winevent",
        },
        "session" => match event {
            WTS_SESSION_LOCK => "session_lock",
            WTS_SESSION_UNLOCK => "session_unlock",
            _ => "other_session",
        },
        "power" => match event {
            PBT_APMSUSPEND => "suspend",
            PBT_APMRESUMEAUTOMATIC => "resume_automatic",
            PBT_APMRESUMESUSPEND => "resume_user",
            _ => "other_power",
        },
        "session-end" => match event {
            WM_QUERYENDSESSION => "query_end_session",
            WM_ENDSESSION => "end_session",
            _ => "other_session_end",
        },
        _ => "other",
    }
}

fn write_record(
    output: &mut BufWriter<File>,
    record: &Record<'_>,
) -> Result<(), Box<dyn std::error::Error>> {
    serde_json::to_writer(&mut *output, record)?;
    output.write_all(b"\n")?;
    Ok(())
}

fn hwnd_hex(hwnd: HWND) -> String {
    format!("0x{:X}", hwnd.0 as usize)
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_aumid_has_highest_identity_priority() {
        let key = identity_key(
            Some("Vendor.Window"),
            Some("Vendor.Process"),
            Some("Vendor.Package"),
            Some("C:/Apps/sample.exe"),
        );
        assert_eq!(key.as_deref(), Some("aumid:vendor.window"));
    }

    #[test]
    fn missing_aumid_falls_back_to_normalized_path() {
        let key = identity_key(None, None, None, Some("C:/Apps/Sample.EXE"));
        assert_eq!(key.as_deref(), Some("path:c:\\apps\\sample.exe"));
    }

    #[test]
    fn event_names_are_scoped_by_source() {
        assert_eq!(event_name("session", WTS_SESSION_LOCK), "session_lock");
        assert_eq!(event_name("power", PBT_APMRESUMESUSPEND), "resume_user");
        assert_eq!(event_name("session-end", WM_ENDSESSION), "end_session");
    }
}
