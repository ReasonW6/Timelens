#![cfg(windows)]

use std::{
    collections::{HashMap, HashSet},
    marker::PhantomData,
    rc::Rc,
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
            Shell::PropertiesSystem::{IPropertyStore, SHGetPropertyStoreForWindow},
            Shell::{IVirtualDesktopManager, VirtualDesktopManager},
            WindowsAndMessaging::{
                EnumWindows, GA_ROOT, GW_OWNER, GWL_EXSTYLE, GetAncestor, GetForegroundWindow,
                GetWindow, GetWindowLongPtrW, GetWindowRect, GetWindowThreadProcessId, IsIconic,
                IsWindow, IsWindowVisible, WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
            },
        },
    },
    core::{BOOL, GUID, PWSTR},
};

const APP_USER_MODEL_ID: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x9f4c2855_9f79_4b39_a8d0_e1d42de1d5f3),
    pid: 5,
};

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

#[derive(Debug, thiserror::Error)]
pub enum ObserverError {
    #[error("Windows window observation failed: {0}")]
    Windows(#[from] windows::core::Error),
}

pub type Result<T> = std::result::Result<T, ObserverError>;

pub struct WindowObserver {
    virtual_desktop: Option<IVirtualDesktopManager>,
    tracked: HashMap<usize, TrackedWindow>,
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
            _com: ComApartment(PhantomData),
        })
    }

    pub fn reconcile(&mut self) -> Result<Vec<WindowObservation>> {
        let windows = enumerate_windows()?;
        let foreground = unsafe { GetForegroundWindow() };
        let mut observed_handles = HashSet::new();
        let mut process_cache = HashMap::<u32, Option<ProcessInfo>>::new();
        let mut observations = Vec::new();

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
                self.tracked.remove(&handle);
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
                    self.tracked.remove(&handle);
                }

                if facts.onboarding_candidate && !self.tracked.contains_key(&handle) {
                    if let Some(identity) = resolve_identity(hwnd, process) {
                        self.tracked.insert(
                            handle,
                            TrackedWindow::new(facts.process_id, process, identity),
                        );
                    }
                } else if let Some(tracked) = self.tracked.get_mut(&handle)
                    && let Some(identity) = resolve_identity(hwnd, process)
                {
                    tracked.update_identity(process, identity);
                }
            }

            if let Some(tracked) = self.tracked.get(&handle) {
                observations.push(tracked.observation(handle as u64, &facts));
            }
        }

        self.tracked
            .retain(|handle, _| observed_handles.contains(handle));
        observations.sort_unstable_by_key(|observation| observation.window_id);
        Ok(observations)
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
        }
    }

    fn update_identity(&mut self, process: &ProcessInfo, identity: ResolvedIdentity) {
        self.executable_path.clone_from(&process.path);
        if identity.source >= self.identity_source {
            self.application_identity = identity.key;
            self.identity_source = identity.source;
            self.app_user_model_id = identity.app_user_model_id;
            self.package_identity = identity.package_identity;
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

struct ProcessHandle(HANDLE);

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) }.ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn identityless_system_hosts_are_not_applications() {
        assert!(is_identityless_host(
            r"C:\Windows\System32\ApplicationFrameHost.exe"
        ));
        assert!(is_identityless_host(
            r"C:\Windows\System32\RuntimeBroker.exe"
        ));
        assert!(!is_identityless_host(r"C:\Apps\Timelens.exe"));
    }
}
