#![cfg(windows)]

mod input;

pub use input::{InputDrain, InputMonitor, InputSample, InputSampleKind, MouseButton};

use std::{
    collections::{HashMap, HashSet},
    marker::PhantomData,
    rc::Rc,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use windows::{
    Win32::{
        Foundation::{CloseHandle, FILETIME, HANDLE, HWND, LPARAM, PROPERTYKEY, RECT},
        Graphics::{
            Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute},
            Gdi::{MONITOR_DEFAULTTONULL, MonitorFromWindow},
        },
        Storage::Packaging::Appx::{GetApplicationUserModelId, GetPackageFullName},
        System::{
            Com::StructuredStorage::PropVariantToStringAlloc,
            Com::{
                CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
                CoUninitialize,
            },
            Threading::{
                GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
                QueryFullProcessImageNameW,
            },
        },
        UI::{
            Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent},
            Shell::PropertiesSystem::{IPropertyStore, SHGetPropertyStoreForWindow},
            Shell::{IVirtualDesktopManager, VirtualDesktopManager},
            WindowsAndMessaging::{
                CHILDID_SELF, DispatchMessageW, EVENT_OBJECT_CLOAKED, EVENT_OBJECT_CREATE,
                EVENT_OBJECT_HIDE, EVENT_OBJECT_UNCLOAKED, EVENT_SYSTEM_FOREGROUND,
                EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_MINIMIZESTART, EnumWindows, GA_ROOT,
                GW_OWNER, GWL_EXSTYLE, GetAncestor, GetForegroundWindow, GetWindow,
                GetWindowLongPtrW, GetWindowRect, GetWindowThreadProcessId, IsIconic, IsWindow,
                IsWindowVisible, MSG, MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx,
                OBJID_WINDOW, PM_REMOVE, PeekMessageW, QS_ALLINPUT, TranslateMessage,
                WINEVENT_OUTOFCONTEXT, WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
            },
        },
    },
    core::{BOOL, GUID, PWSTR},
};

const APP_USER_MODEL_ID: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x9f4c2855_9f79_4b39_a8d0_e1d42de1d5f3),
    pid: 5,
};
static WINDOW_EVENT_PENDING: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum IdentitySource {
    ExecutablePath,
    Package,
    ProcessAppUserModelId,
    WindowAppUserModelId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowObservation {
    pub window_id: u64,
    pub process_id: u32,
    pub process_started_at_100ns: u64,
    pub application_identity: String,
    pub identity_source: IdentitySource,
    pub executable_path: Option<String>,
    pub app_user_model_id: Option<String>,
    pub package_identity: Option<String>,
    pub displayed: bool,
    pub focused: bool,
    pub on_current_virtual_desktop: Option<bool>,
    pub virtual_desktop_id: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowTransitionKind {
    Opened,
    Updated,
    Closed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowTransition {
    pub kind: WindowTransitionKind,
    pub observed_at_utc_ms: i64,
    pub monotonic_ms: u64,
    pub window: WindowObservation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReconcileResult {
    pub current: Vec<WindowObservation>,
    pub transitions: Vec<WindowTransition>,
    pub tray_transitions: Vec<TrayTransition>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrayTransitionKind {
    Started,
    Ended,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrayTransition {
    pub kind: TrayTransitionKind,
    pub observed_at_utc_ms: i64,
    pub monotonic_ms: u64,
    pub application_identity: String,
    pub process_id: u32,
    pub process_started_at_100ns: u64,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct TrayPresence {
    pub application_identity: String,
    pub process_id: u32,
    pub process_started_at_100ns: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum ObserverError {
    #[error("Windows window observation failed: {0}")]
    Windows(#[from] windows::core::Error),
}

pub type Result<T> = std::result::Result<T, ObserverError>;

pub struct WindowObserver {
    virtual_desktop: Option<IVirtualDesktopManager>,
    tracked: HashMap<usize, TrackedWindow>,
    tray_candidates: HashMap<TrayKey, TrayCandidate>,
    started: Instant,
    // COM interfaces above must be released before this apartment guard.
    _com: ComApartment,
}

impl WindowObserver {
    pub fn new() -> Result<Self> {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok()?;
        let virtual_desktop: Option<IVirtualDesktopManager> =
            unsafe { CoCreateInstance(&VirtualDesktopManager, None, CLSCTX_ALL) }.ok();
        Ok(Self {
            virtual_desktop,
            tracked: HashMap::new(),
            tray_candidates: HashMap::new(),
            started: Instant::now(),
            _com: ComApartment(PhantomData),
        })
    }

    pub fn reconcile(&mut self) -> Result<Vec<WindowObservation>> {
        Ok(self.reconcile_transitions()?.current)
    }

    pub fn timestamp(&self) -> (i64, u64) {
        (
            unix_time_ms(),
            self.started.elapsed().as_millis().min(u64::MAX as u128) as u64,
        )
    }

    pub fn seed_tray_presence(&mut self, seeds: impl IntoIterator<Item = TrayPresence>) {
        for seed in seeds {
            if process_matches(seed.process_id, seed.process_started_at_100ns) {
                self.tray_candidates.insert(
                    TrayKey {
                        process_id: seed.process_id,
                        process_started_at_100ns: seed.process_started_at_100ns,
                        application_identity: seed.application_identity,
                    },
                    TrayCandidate,
                );
            }
        }
    }

    pub fn current_tray_presence(&self) -> Vec<TrayPresence> {
        let mut presence = self
            .tray_candidates
            .keys()
            .map(|key| TrayPresence {
                application_identity: key.application_identity.clone(),
                process_id: key.process_id,
                process_started_at_100ns: key.process_started_at_100ns,
            })
            .collect::<Vec<_>>();
        presence.sort();
        presence
    }

    pub fn reconcile_transitions(&mut self) -> Result<ReconcileResult> {
        let windows = enumerate_windows()?;
        let foreground = unsafe { GetForegroundWindow() };
        let observed_at_utc_ms = unix_time_ms();
        let monotonic_ms = self.started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let mut observed_handles = HashSet::new();
        let mut process_cache = HashMap::<u32, Option<ProcessInfo>>::new();
        let mut observations = Vec::new();
        let mut transitions = Vec::new();

        for hwnd in windows {
            let handle = window_id(hwnd) as usize;
            let Some(facts) = inspect_window(hwnd, foreground, self.virtual_desktop.as_ref())
            else {
                continue;
            };
            observed_handles.insert(handle);

            if self
                .tracked
                .get(&handle)
                .is_some_and(|tracked| tracked.process_id != facts.process_id)
            {
                close_tracked(
                    &mut self.tracked,
                    handle,
                    observed_at_utc_ms,
                    monotonic_ms,
                    &mut transitions,
                );
            }

            if !facts.onboarding_candidate && !self.tracked.contains_key(&handle) {
                continue;
            }

            let process = process_cache
                .entry(facts.process_id)
                .or_insert_with(|| process_info(facts.process_id))
                .as_ref();

            if let Some(process) = process {
                if self.tracked.get(&handle).is_some_and(|tracked| {
                    tracked.process_started_at_100ns != process.started_at_100ns
                }) {
                    close_tracked(
                        &mut self.tracked,
                        handle,
                        observed_at_utc_ms,
                        monotonic_ms,
                        &mut transitions,
                    );
                }

                if facts.onboarding_candidate
                    && !self.tracked.contains_key(&handle)
                    && let Some(identity) = resolve_identity(hwnd, process)
                {
                    self.tracked.insert(
                        handle,
                        TrackedWindow::new(facts.process_id, process, identity),
                    );
                }
            }

            if let Some(tracked) = self.tracked.get_mut(&handle) {
                let observation = tracked.observation(handle as u64, &facts);
                if let Some(kind) = transition_kind(tracked.last_observation.as_ref(), &observation)
                {
                    transitions.push(WindowTransition {
                        kind,
                        observed_at_utc_ms,
                        monotonic_ms,
                        window: observation.clone(),
                    });
                }
                tracked.last_observation = Some(observation.clone());
                observations.push(observation);
            }
        }

        let closed = self
            .tracked
            .keys()
            .filter(|handle| !observed_handles.contains(handle))
            .copied()
            .collect::<Vec<_>>();
        for handle in closed {
            close_tracked(
                &mut self.tracked,
                handle,
                observed_at_utc_ms,
                monotonic_ms,
                &mut transitions,
            );
        }
        observations.sort_unstable_by_key(|observation| observation.window_id);
        let tray_transitions = update_tray_candidates(
            &mut self.tray_candidates,
            &observations,
            &transitions,
            observed_at_utc_ms,
            monotonic_ms,
            process_matches,
        );
        Ok(ReconcileResult {
            current: observations,
            transitions,
            tray_transitions,
        })
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct TrayKey {
    process_id: u32,
    process_started_at_100ns: u64,
    application_identity: String,
}

#[derive(Clone, Debug)]
struct TrayCandidate;

fn update_tray_candidates(
    candidates: &mut HashMap<TrayKey, TrayCandidate>,
    current: &[WindowObservation],
    window_transitions: &[WindowTransition],
    observed_at_utc_ms: i64,
    monotonic_ms: u64,
    process_is_alive: impl Fn(u32, u64) -> bool,
) -> Vec<TrayTransition> {
    let current_keys = current.iter().map(tray_key).collect::<HashSet<_>>();
    let mut transitions = Vec::new();

    let ended = candidates
        .keys()
        .filter(|key| {
            current_keys.contains(*key)
                || !process_is_alive(key.process_id, key.process_started_at_100ns)
        })
        .cloned()
        .collect::<Vec<_>>();
    for key in ended {
        candidates.remove(&key);
        transitions.push(tray_transition(
            TrayTransitionKind::Ended,
            key,
            observed_at_utc_ms,
            monotonic_ms,
        ));
    }

    let closed_keys = window_transitions
        .iter()
        .filter(|transition| transition.kind == WindowTransitionKind::Closed)
        .map(|transition| tray_key(&transition.window))
        .collect::<HashSet<_>>();
    for key in closed_keys {
        if !current_keys.contains(&key)
            && !candidates.contains_key(&key)
            && process_is_alive(key.process_id, key.process_started_at_100ns)
        {
            candidates.insert(key.clone(), TrayCandidate);
            transitions.push(tray_transition(
                TrayTransitionKind::Started,
                key,
                observed_at_utc_ms,
                monotonic_ms,
            ));
        }
    }
    transitions
}

fn tray_key(window: &WindowObservation) -> TrayKey {
    TrayKey {
        process_id: window.process_id,
        process_started_at_100ns: window.process_started_at_100ns,
        application_identity: window.application_identity.clone(),
    }
}

fn tray_transition(
    kind: TrayTransitionKind,
    key: TrayKey,
    observed_at_utc_ms: i64,
    monotonic_ms: u64,
) -> TrayTransition {
    TrayTransition {
        kind,
        observed_at_utc_ms,
        monotonic_ms,
        application_identity: key.application_identity,
        process_id: key.process_id,
        process_started_at_100ns: key.process_started_at_100ns,
    }
}

pub struct WinEventMonitor {
    hooks: Vec<HWINEVENTHOOK>,
    _not_send: PhantomData<Rc<()>>,
}

impl WinEventMonitor {
    pub fn new() -> Result<Self> {
        WINDOW_EVENT_PENDING.store(false, Ordering::Release);
        let ranges = [
            (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND),
            (EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND),
            (EVENT_OBJECT_CREATE, EVENT_OBJECT_HIDE),
            (EVENT_OBJECT_CLOAKED, EVENT_OBJECT_UNCLOAKED),
        ];
        let mut hooks = Vec::with_capacity(ranges.len());
        for (start, end) in ranges {
            let hook = unsafe {
                SetWinEventHook(
                    start,
                    end,
                    None,
                    Some(win_event_callback),
                    0,
                    0,
                    WINEVENT_OUTOFCONTEXT,
                )
            };
            if hook.is_invalid() {
                return Err(windows::core::Error::from_thread().into());
            }
            hooks.push(hook);
        }
        Ok(Self {
            hooks,
            _not_send: PhantomData,
        })
    }

    pub fn wait_for_change(&self, timeout: Duration) -> bool {
        let timeout_ms = timeout.as_millis().min(u32::MAX as u128) as u32;
        unsafe {
            MsgWaitForMultipleObjectsEx(None, timeout_ms, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
        }
        pump_messages();
        WINDOW_EVENT_PENDING.swap(false, Ordering::AcqRel)
    }
}

impl Drop for WinEventMonitor {
    fn drop(&mut self) {
        for hook in self.hooks.drain(..) {
            let _ = unsafe { UnhookWinEvent(hook) };
        }
    }
}

unsafe extern "system" fn win_event_callback(
    _hook: HWINEVENTHOOK,
    event: u32,
    _hwnd: HWND,
    object_id: i32,
    child_id: i32,
    _thread_id: u32,
    _event_time_ms: u32,
) {
    let object_event = (EVENT_OBJECT_CREATE..=EVENT_OBJECT_HIDE).contains(&event)
        || (EVENT_OBJECT_CLOAKED..=EVENT_OBJECT_UNCLOAKED).contains(&event);
    if object_event && (object_id != OBJID_WINDOW.0 || child_id != CHILDID_SELF as i32) {
        return;
    }
    WINDOW_EVENT_PENDING.store(true, Ordering::Release);
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

struct ComApartment(PhantomData<Rc<()>>);

impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

#[derive(Clone)]
struct ProcessInfo {
    started_at_100ns: u64,
    path: Option<String>,
    app_user_model_id: Option<String>,
    package_identity: Option<String>,
}

#[derive(Clone)]
struct ResolvedIdentity {
    key: String,
    source: IdentitySource,
    app_user_model_id: Option<String>,
    package_identity: Option<String>,
}

struct TrackedWindow {
    process_id: u32,
    process_started_at_100ns: u64,
    application_identity: String,
    identity_source: IdentitySource,
    executable_path: Option<String>,
    app_user_model_id: Option<String>,
    package_identity: Option<String>,
    last_observation: Option<WindowObservation>,
}

impl TrackedWindow {
    fn new(process_id: u32, process: &ProcessInfo, identity: ResolvedIdentity) -> Self {
        Self {
            process_id,
            process_started_at_100ns: process.started_at_100ns,
            application_identity: identity.key,
            identity_source: identity.source,
            executable_path: process.path.clone(),
            app_user_model_id: identity.app_user_model_id,
            package_identity: identity.package_identity,
            last_observation: None,
        }
    }

    fn observation(&self, window_id: u64, facts: &WindowFacts) -> WindowObservation {
        WindowObservation {
            window_id,
            process_id: self.process_id,
            process_started_at_100ns: self.process_started_at_100ns,
            application_identity: self.application_identity.clone(),
            identity_source: self.identity_source,
            executable_path: self.executable_path.clone(),
            app_user_model_id: self.app_user_model_id.clone(),
            package_identity: self.package_identity.clone(),
            displayed: facts.displayed,
            focused: facts.focused,
            on_current_virtual_desktop: facts.on_current_virtual_desktop,
            virtual_desktop_id: facts.virtual_desktop_id.clone(),
        }
    }
}

fn transition_kind(
    previous: Option<&WindowObservation>,
    current: &WindowObservation,
) -> Option<WindowTransitionKind> {
    match previous {
        None => Some(WindowTransitionKind::Opened),
        Some(previous) if previous != current => Some(WindowTransitionKind::Updated),
        Some(_) => None,
    }
}

fn close_tracked(
    tracked: &mut HashMap<usize, TrackedWindow>,
    handle: usize,
    observed_at_utc_ms: i64,
    monotonic_ms: u64,
    transitions: &mut Vec<WindowTransition>,
) {
    if let Some(tracked) = tracked.remove(&handle)
        && let Some(window) = tracked.last_observation
    {
        transitions.push(WindowTransition {
            kind: WindowTransitionKind::Closed,
            observed_at_utc_ms,
            monotonic_ms,
            window,
        });
    }
}

struct WindowFacts {
    process_id: u32,
    onboarding_candidate: bool,
    displayed: bool,
    focused: bool,
    on_current_virtual_desktop: Option<bool>,
    virtual_desktop_id: Option<String>,
}

#[derive(Clone, Copy, Default)]
struct ClassifierInput {
    root_is_self: bool,
    has_owner: bool,
    app_window: bool,
    tool_window: bool,
    no_activate: bool,
    visible: bool,
    minimized: bool,
    cloaked: Option<bool>,
    monitor_present: bool,
    has_area: bool,
    on_current_virtual_desktop: Option<bool>,
    focused: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Classification {
    onboarding_candidate: bool,
    displayed: bool,
    focused: bool,
}

fn classify(input: ClassifierInput) -> Classification {
    let structural_candidate = input.root_is_self
        && (!input.has_owner || input.app_window)
        && (!input.tool_window || input.app_window)
        && !input.no_activate;
    let cloak_allows_onboarding = !input.cloaked.unwrap_or(false)
        || input.on_current_virtual_desktop == Some(false)
        || input.focused;
    let onboarding_candidate = structural_candidate
        && input.visible
        && input.monitor_present
        && input.has_area
        && cloak_allows_onboarding;
    let displayed = structural_candidate
        && input.visible
        && !input.minimized
        && !input.cloaked.unwrap_or(false)
        && input.monitor_present
        && input.has_area
        && input.on_current_virtual_desktop.unwrap_or(true);
    Classification {
        onboarding_candidate,
        displayed,
        focused: input.focused,
    }
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
    foreground: HWND,
    virtual_desktop: Option<&IVirtualDesktopManager>,
) -> Option<WindowFacts> {
    if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        return None;
    }

    let mut process_id = 0_u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) };
    if process_id == 0 {
        return None;
    }

    let root = unsafe { GetAncestor(hwnd, GA_ROOT) };
    let owner = unsafe { GetWindow(hwnd, GW_OWNER) }.ok();
    let extended_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
    let visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
    let minimized = unsafe { IsIconic(hwnd) }.as_bool();
    let cloaked = window_cloaked(hwnd);
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
    let classification = classify(ClassifierInput {
        root_is_self: root == hwnd,
        has_owner: owner.is_some(),
        app_window: extended_style & WS_EX_APPWINDOW.0 != 0,
        tool_window: extended_style & WS_EX_TOOLWINDOW.0 != 0,
        no_activate: extended_style & WS_EX_NOACTIVATE.0 != 0,
        visible,
        minimized,
        cloaked,
        monitor_present,
        has_area: rect.is_some_and(|value| value.right > value.left && value.bottom > value.top),
        on_current_virtual_desktop,
        focused: foreground == hwnd,
    });

    Some(WindowFacts {
        process_id,
        onboarding_candidate: classification.onboarding_candidate,
        displayed: classification.displayed,
        focused: classification.focused,
        on_current_virtual_desktop,
        virtual_desktop_id,
    })
}

fn process_info(process_id: u32) -> Option<ProcessInfo> {
    let handle =
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) }.ok()?;
    let handle = ProcessHandle(handle);
    let started_at_100ns = process_start_time(handle.0)?;
    let mut path_buffer = vec![0_u16; 32_768];
    let mut path_length = path_buffer.len() as u32;
    let path = unsafe {
        QueryFullProcessImageNameW(
            handle.0,
            Default::default(),
            PWSTR(path_buffer.as_mut_ptr()),
            &mut path_length,
        )
    }
    .ok()
    .map(|_| String::from_utf16_lossy(&path_buffer[..path_length as usize]));

    Some(ProcessInfo {
        started_at_100ns,
        path,
        app_user_model_id: query_process_string(handle.0, GetApplicationUserModelId),
        package_identity: query_process_string(handle.0, GetPackageFullName),
    })
}

fn process_matches(process_id: u32, expected_started_at_100ns: u64) -> bool {
    unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) }
        .ok()
        .and_then(|handle| {
            let handle = ProcessHandle(handle);
            process_start_time(handle.0)
        })
        == Some(expected_started_at_100ns)
}

fn process_start_time(handle: HANDLE) -> Option<u64> {
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) }.ok()?;
    Some(((creation.dwHighDateTime as u64) << 32) | creation.dwLowDateTime as u64)
}

fn query_process_string(
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

fn resolve_identity(hwnd: HWND, process: &ProcessInfo) -> Option<ResolvedIdentity> {
    let window_aumid = window_app_user_model_id(hwnd);
    if let Some(value) = window_aumid {
        return Some(ResolvedIdentity {
            key: format!("aumid:{}", value.to_lowercase()),
            source: IdentitySource::WindowAppUserModelId,
            app_user_model_id: Some(value),
            package_identity: process.package_identity.clone(),
        });
    }
    if let Some(value) = &process.app_user_model_id {
        return Some(ResolvedIdentity {
            key: format!("aumid:{}", value.to_lowercase()),
            source: IdentitySource::ProcessAppUserModelId,
            app_user_model_id: Some(value.clone()),
            package_identity: process.package_identity.clone(),
        });
    }
    if let Some(value) = &process.package_identity {
        return Some(ResolvedIdentity {
            key: format!("package:{}", value.to_lowercase()),
            source: IdentitySource::Package,
            app_user_model_id: None,
            package_identity: Some(value.clone()),
        });
    }
    process.path.as_ref().and_then(|path| {
        (!is_identityless_host(path)).then(|| ResolvedIdentity {
            key: format!("path:{}", normalize_path(path)),
            source: IdentitySource::ExecutablePath,
            app_user_model_id: None,
            package_identity: None,
        })
    })
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

fn is_identityless_host(path: &str) -> bool {
    path.rsplit(['\\', '/']).next().is_some_and(|name| {
        name.eq_ignore_ascii_case("ApplicationFrameHost.exe")
            || name.eq_ignore_ascii_case("RuntimeBroker.exe")
    })
}

fn normalize_path(path: &str) -> String {
    path.replace('/', "\\").to_lowercase()
}

fn window_rect(hwnd: HWND) -> Option<RECT> {
    let mut rect = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut rect) }.ok()?;
    Some(rect)
}

fn window_cloaked(hwnd: HWND) -> Option<bool> {
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

fn window_id(hwnd: HWND) -> u64 {
    hwnd.0 as usize as u64
}

fn unix_time_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

struct ProcessHandle(HANDLE);

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) }.ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(displayed: bool, focused: bool) -> WindowObservation {
        WindowObservation {
            window_id: 10,
            process_id: 20,
            process_started_at_100ns: 30,
            application_identity: "path:c:\\apps\\sample.exe".to_owned(),
            identity_source: IdentitySource::ExecutablePath,
            executable_path: Some(r"C:\Apps\Sample.exe".to_owned()),
            app_user_model_id: None,
            package_identity: None,
            displayed,
            focused,
            on_current_virtual_desktop: Some(true),
            virtual_desktop_id: Some("desktop".to_owned()),
        }
    }

    fn visible_window() -> ClassifierInput {
        ClassifierInput {
            root_is_self: true,
            visible: true,
            monitor_present: true,
            has_area: true,
            on_current_virtual_desktop: Some(true),
            ..Default::default()
        }
    }

    #[test]
    fn hidden_create_is_not_a_user_window() {
        let classified = classify(ClassifierInput {
            root_is_self: true,
            monitor_present: true,
            has_area: true,
            ..Default::default()
        });
        assert!(!classified.onboarding_candidate);
        assert!(!classified.displayed);
    }

    #[test]
    fn visible_top_level_window_onboards_and_displays() {
        let classified = classify(visible_window());
        assert!(classified.onboarding_candidate);
        assert!(classified.displayed);
    }

    #[test]
    fn owned_tool_and_no_activate_windows_do_not_onboard() {
        for input in [
            ClassifierInput {
                has_owner: true,
                ..visible_window()
            },
            ClassifierInput {
                tool_window: true,
                ..visible_window()
            },
            ClassifierInput {
                no_activate: true,
                ..visible_window()
            },
        ] {
            assert!(!classify(input).onboarding_candidate);
        }
    }

    #[test]
    fn minimized_window_is_open_but_not_displayed() {
        let classified = classify(ClassifierInput {
            minimized: true,
            ..visible_window()
        });
        assert!(classified.onboarding_candidate);
        assert!(!classified.displayed);
    }

    #[test]
    fn other_desktop_cloaked_window_can_onboard_as_background() {
        let classified = classify(ClassifierInput {
            cloaked: Some(true),
            on_current_virtual_desktop: Some(false),
            ..visible_window()
        });
        assert!(classified.onboarding_candidate);
        assert!(!classified.displayed);
    }

    #[test]
    fn identity_prefers_explicit_values_and_normalizes_path_fallback() {
        let process = ProcessInfo {
            started_at_100ns: 1,
            path: Some("C:/Apps/Sample.EXE".to_owned()),
            app_user_model_id: Some("Vendor.Process".to_owned()),
            package_identity: Some("Vendor.Package".to_owned()),
        };
        let identity = resolve_identity(HWND::default(), &process).unwrap();
        assert_eq!(identity.source, IdentitySource::ProcessAppUserModelId);
        assert_eq!(identity.key, "aumid:vendor.process");

        let fallback = resolve_identity(
            HWND::default(),
            &ProcessInfo {
                app_user_model_id: None,
                package_identity: None,
                ..process
            },
        )
        .unwrap();
        assert_eq!(fallback.source, IdentitySource::ExecutablePath);
        assert_eq!(fallback.key, "path:c:\\apps\\sample.exe");
    }

    #[test]
    fn tray_background_starts_after_the_last_window_and_ends_on_process_exit() {
        let window = observation(false, false);
        let closed = WindowTransition {
            kind: WindowTransitionKind::Closed,
            observed_at_utc_ms: 100,
            monotonic_ms: 10,
            window: window.clone(),
        };
        let mut candidates = HashMap::new();
        let started = update_tray_candidates(
            &mut candidates,
            &[],
            &[closed],
            100,
            10,
            |process_id, started_at| process_id == 20 && started_at == 30,
        );
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].kind, TrayTransitionKind::Started);

        let ended = update_tray_candidates(&mut candidates, &[], &[], 200, 110, |_, _| false);
        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0].kind, TrayTransitionKind::Ended);
        assert!(candidates.is_empty());
    }

    #[test]
    fn another_window_for_the_same_application_prevents_or_ends_tray_inference() {
        let mut first = observation(false, false);
        let second = WindowObservation {
            window_id: 11,
            ..first.clone()
        };
        let closed = WindowTransition {
            kind: WindowTransitionKind::Closed,
            observed_at_utc_ms: 100,
            monotonic_ms: 10,
            window: first.clone(),
        };
        let mut candidates = HashMap::new();
        assert!(
            update_tray_candidates(
                &mut candidates,
                std::slice::from_ref(&second),
                std::slice::from_ref(&closed),
                100,
                10,
                |_, _| true,
            )
            .is_empty()
        );

        first.window_id = 12;
        let started = update_tray_candidates(
            &mut candidates,
            &[],
            &[WindowTransition {
                window: first,
                ..closed
            }],
            200,
            110,
            |_, _| true,
        );
        assert_eq!(started[0].kind, TrayTransitionKind::Started);
        let ended = update_tray_candidates(&mut candidates, &[second], &[], 300, 210, |_, _| true);
        assert_eq!(ended[0].kind, TrayTransitionKind::Ended);
    }

    #[test]
    fn identityless_system_hosts_are_not_applications() {
        assert!(is_identityless_host(
            r"C:\Windows\System32\ApplicationFrameHost.exe"
        ));
        assert!(is_identityless_host(
            r"C:\Windows\System32\RuntimeBroker.exe"
        ));
        assert!(!is_identityless_host(r"C:\Apps\Timelens.exe"));
    }

    #[test]
    fn transition_diff_reports_only_semantic_changes() {
        let displayed = observation(true, true);
        assert_eq!(
            transition_kind(None, &displayed),
            Some(WindowTransitionKind::Opened)
        );
        assert_eq!(transition_kind(Some(&displayed), &displayed), None);

        let background = observation(false, false);
        assert_eq!(
            transition_kind(Some(&displayed), &background),
            Some(WindowTransitionKind::Updated)
        );
    }
}
