use crate::{AppWindow, CollectionState};
use anyhow::{Result, anyhow};
use slint::ComponentHandle;
use std::{
    ptr::null,
    sync::{Arc, Mutex, mpsc},
    thread,
};
use windows_sys::Win32::{
    Foundation::*,
    System::LibraryLoader::GetModuleHandleW,
    UI::{Shell::*, WindowsAndMessaging::*},
};
const CALLBACK: u32 = WM_APP + 27;
const SHOW: u32 = WM_APP + 28;
const STOP: u32 = WM_APP + 29;
const TIP: u32 = WM_APP + 30;
/// The file whose presence says the hide-to-tray notice was already shown.
pub const EXPLAINED_FILE: &str = "tray-explained.txt";
static WINDOW: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
static SHUTDOWN_READY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static EVENTS: Mutex<Option<EventSink>> = Mutex::new(None);
static COLLECTION_MENU: Mutex<CollectionMenu> = Mutex::new(CollectionMenu {
    paused: false,
    busy: false,
    resume: String::new(),
});
static NOTICE: Mutex<Option<Notice>> = Mutex::new(None);
static HIDE_EXPLAINED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
struct Notice {
    title: &'static str,
    text: String,
    success: bool,
}
struct EventSink {
    sender: mpsc::Sender<Event>,
    window: slint::Weak<AppWindow>,
}
#[derive(Clone, Default, PartialEq)]
struct CollectionMenu {
    paused: bool,
    busy: bool,
    /// When a timed pause ends, in words; empty otherwise.
    resume: String,
}
impl CollectionMenu {
    /// The tray tooltip and the menu's first line.
    fn status(&self) -> String {
        match (self.paused, self.resume.is_empty()) {
            (false, _) => "正在记录".into(),
            (true, true) => "已暂停记录".into(),
            (true, false) => format!("已暂停记录 · {}", self.resume),
        }
    }
}
#[cfg_attr(test, derive(Debug, PartialEq))]
enum Event {
    Open,
    SetPaused(bool),
    PauseFor(i32),
    Capture,
    Quit,
    QuitReady,
    QuitFailed(String),
    /// A system setting such as the light or dark theme changed.
    SettingChanged,
}
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}
fn emit(event: Event) {
    if let Ok(sink) = EVENTS.lock()
        && let Some(sink) = &*sink
    {
        match event {
            Event::Open => {
                // Handle the user's activation immediately, while Windows still
                // allows this process to take foreground focus.
                let _ = sink.window.upgrade_in_event_loop(move |w| {
                    if let Err(error) = crate::window_chrome::show_and_activate(&w) {
                        w.set_action_status(format!("打开窗口失败：{error}").into());
                    }
                });
            }
            _ => {
                let _ = sink.sender.send(event);
                let _ = sink
                    .window
                    .upgrade_in_event_loop(|w| w.invoke_tray_events_ready());
            }
        }
    }
}
fn collection_menu(w: &AppWindow, quitting: bool) {
    let state = w.global::<CollectionState>();
    let next = CollectionMenu {
        paused: state.get_paused(),
        busy: state.get_busy() || quitting,
        resume: state.get_resume_label().into(),
    };
    if let Ok(mut menu) = COLLECTION_MENU.lock()
        && *menu != next
    {
        let tip_changed = menu.status() != next.status();
        *menu = next;
        if tip_changed {
            let hwnd = WINDOW.load(std::sync::atomic::Ordering::Acquire) as HWND;
            if !hwnd.is_null() {
                unsafe {
                    PostMessageW(hwnd, TIP, 0, 0);
                }
            }
        }
    }
}
/// The pause lengths the tray offers, as menu id and minutes for `pause-for`.
const PAUSES: [(usize, i32, &str); 4] = [
    (10, 15, "15 分钟"),
    (11, 60, "1 小时"),
    (12, -2, "直到明天"),
    (13, -1, "直到手动继续"),
];
fn popup_menu(state: &CollectionMenu) -> HMENU {
    unsafe {
        let menu = CreatePopupMenu();
        if !menu.is_null() {
            AppendMenuW(
                menu,
                MF_STRING | MF_GRAYED,
                0,
                wide(&state.status()).as_ptr(),
            );
            AppendMenuW(menu, MF_SEPARATOR, 0, null());
            AppendMenuW(menu, MF_STRING, 1, wide("打开 Timelens").as_ptr());
            AppendMenuW(menu, MF_STRING, 4, wide("立即拍摄快照").as_ptr());
            let busy = if state.busy { MF_GRAYED } else { 0 };
            if state.paused {
                AppendMenuW(menu, MF_STRING | busy, 2, wide("继续记录").as_ptr());
            } else {
                let lengths = CreatePopupMenu();
                for (id, _, text) in PAUSES {
                    AppendMenuW(lengths, MF_STRING, id, wide(text).as_ptr());
                }
                AppendMenuW(
                    menu,
                    MF_POPUP | busy,
                    lengths as usize,
                    wide("暂停记录").as_ptr(),
                );
            }
            AppendMenuW(menu, MF_SEPARATOR, 0, null());
            AppendMenuW(menu, MF_STRING, 3, wide("退出（停止记录）…").as_ptr());
        }
        menu
    }
}
fn menu_event(selected: i32, state: &CollectionMenu) -> Option<Event> {
    match selected {
        1 => Some(Event::Open),
        2 if !state.busy => Some(Event::SetPaused(false)),
        3 => Some(Event::Quit),
        4 => Some(Event::Capture),
        id if !state.busy => PAUSES
            .iter()
            .find(|(menu_id, ..)| *menu_id as i32 == id)
            .map(|(_, minutes, _)| Event::PauseFor(*minutes)),
        _ => None,
    }
}
/// Asks before quitting from the menu, since recording stops with Timelens.
fn confirm_quit(hwnd: HWND) -> bool {
    unsafe {
        MessageBoxW(
            hwnd,
            wide("退出后不再记录活动、输入和快照，直到下次打开 Timelens。\n\n只想暂时不记录，可以改用「暂停记录」。").as_ptr(),
            wide("退出 Timelens？").as_ptr(),
            MB_OKCANCEL | MB_ICONQUESTION | MB_DEFBUTTON2,
        ) == IDOK
    }
}
/// The embedded Timelens icon (resource 1 in app.rc) at the small-icon size.
/// Shared icons are owned by the system, so repeated loads do not leak.
fn tray_icon() -> HICON {
    let icon = unsafe {
        LoadImageW(
            GetModuleHandleW(null()),
            1 as _,
            IMAGE_ICON,
            GetSystemMetrics(SM_CXSMICON),
            GetSystemMetrics(SM_CYSMICON),
            LR_SHARED,
        )
    };
    if icon.is_null() {
        unsafe { LoadIconW(std::ptr::null_mut(), IDI_APPLICATION) }
    } else {
        icon
    }
}
fn notification_data(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut d = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: CALLBACK,
        hIcon: tray_icon(),
        ..Default::default()
    };
    let status = COLLECTION_MENU
        .lock()
        .map(|menu| menu.status())
        .unwrap_or_default();
    for (slot, c) in d
        .szTip
        .iter_mut()
        .take(127)
        .zip(format!("Timelens · {status}").encode_utf16())
    {
        *slot = c;
    }
    d
}
unsafe extern "system" fn procedure(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match message {
        WM_POWERBROADCAST => {
            crate::snapshot::wake_scheduler();
            1
        }
        WM_TIMECHANGE | WM_SETTINGCHANGE => {
            crate::snapshot::wake_scheduler();
            crate::history::invalidate();
            emit(Event::SettingChanged);
            unsafe { DefWindowProcW(hwnd, message, w, l) }
        }
        WM_QUERYENDSESSION => 1,
        WM_ENDSESSION if w != 0 => {
            // This native message thread can wait while Slint, IPC and the
            // shutdown worker continue flushing on their own threads.
            emit(Event::Quit);
            let started = std::time::Instant::now();
            while !SHUTDOWN_READY.load(std::sync::atomic::Ordering::Acquire)
                && started.elapsed() < std::time::Duration::from_secs(4)
            {
                thread::sleep(std::time::Duration::from_millis(20));
            }
            0
        }
        SHOW => {
            emit(Event::Open);
            0
        }
        STOP => {
            emit(Event::Quit);
            0
        }
        TIP => {
            let mut data = notification_data(hwnd);
            data.uFlags = NIF_TIP;
            unsafe {
                Shell_NotifyIconW(NIM_MODIFY, &data);
            }
            0
        }
        CALLBACK => {
            match l as u32 & 0xffff {
                WM_LBUTTONUP | WM_LBUTTONDBLCLK | NIN_SELECT => emit(Event::Open),
                NIN_BALLOONUSERCLICK => emit(Event::Open),
                WM_RBUTTONUP | WM_CONTEXTMENU => unsafe {
                    let state = COLLECTION_MENU
                        .lock()
                        .map(|s| s.clone())
                        .unwrap_or_default();
                    let menu = popup_menu(&state);
                    if menu.is_null() {
                        return 0;
                    }
                    let mut point = POINT::default();
                    GetCursorPos(&mut point);
                    SetForegroundWindow(hwnd);
                    let selected = TrackPopupMenu(
                        menu,
                        TPM_RETURNCMD | TPM_NONOTIFY,
                        point.x,
                        point.y,
                        0,
                        hwnd,
                        null(),
                    );
                    DestroyMenu(menu);
                    // Complete the tray menu's foreground transition before an
                    // Open command activates the main window on the UI thread.
                    PostMessageW(hwnd, WM_NULL, 0, 0);
                    match menu_event(selected, &state) {
                        Some(Event::Quit) if !confirm_quit(hwnd) => {}
                        Some(event) => emit(event),
                        None => {}
                    }
                },
                _ => {}
            }
            0
        }
        WM_APP => {
            if let Ok(n) = NOTICE.lock()
                && let Some(Notice {
                    title,
                    text,
                    success,
                    ..
                }) = &*n
            {
                let mut data = notification_data(hwnd);
                data.uFlags = NIF_INFO;
                data.dwInfoFlags = if *success { NIIF_INFO } else { NIIF_ERROR };
                for (slot, c) in data.szInfoTitle.iter_mut().zip(title.encode_utf16()) {
                    *slot = c;
                }
                for (slot, c) in data.szInfo.iter_mut().take(255).zip(text.encode_utf16()) {
                    *slot = c;
                }
                unsafe {
                    Shell_NotifyIconW(NIM_MODIFY, &data);
                }
            }
            0
        }
        WM_DESTROY => {
            unsafe {
                Shell_NotifyIconW(NIM_DELETE, &notification_data(hwnd));
                PostQuitMessage(0);
            }
            0
        }
        _ => {
            let restart = unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) };
            if message == restart {
                unsafe {
                    Shell_NotifyIconW(NIM_ADD, &notification_data(hwnd));
                }
                0
            } else {
                unsafe { DefWindowProcW(hwnd, message, w, l) }
            }
        }
    }
}
pub fn activate_existing(stop: bool) -> bool {
    unsafe {
        let hwnd = FindWindowW(wide("Timelens.Tray.v1").as_ptr(), null());
        if hwnd.is_null() {
            return false;
        }
        if !stop {
            // A shortcut launch owns the foreground permission; pass it to the
            // existing core before requesting that it show its window.
            let mut process = 0;
            GetWindowThreadProcessId(hwnd, &mut process);
            if process != 0 {
                AllowSetForegroundWindow(process);
            }
        }
        PostMessageW(hwnd, if stop { STOP } else { SHOW }, 0, 0) != 0
    }
}
/// A notice for a result the hidden window cannot show; a click opens Timelens.
pub fn show_message(title: &'static str, text: String, success: bool) {
    show_notice(Notice {
        title,
        text,
        success,
    });
}
fn show_notice(notice: Notice) {
    if let Ok(mut n) = NOTICE.lock() {
        *n = Some(notice);
    }
    let hwnd = WINDOW.load(std::sync::atomic::Ordering::Acquire) as HWND;
    if !hwnd.is_null() {
        unsafe {
            PostMessageW(hwnd, WM_APP, 0, 0);
        }
    }
}
pub fn install(
    w: &AppWindow,
    storage: Arc<Mutex<timelens_storage::Storage>>,
    control: std::path::PathBuf,
) -> Result<()> {
    let (sender, receiver) = mpsc::channel();
    *EVENTS.lock().map_err(|_| anyhow!("托盘锁不可用"))? = Some(EventSink {
        sender,
        window: w.as_weak(),
    });
    collection_menu(w, false);
    let (ready_sender, ready_receiver) = mpsc::channel();
    thread::spawn(move || unsafe {
        let name = wide("Timelens.Tray.v1");
        let instance = GetModuleHandleW(null());
        let class = WNDCLASSW {
            lpfnWndProc: Some(procedure),
            hInstance: instance,
            lpszClassName: name.as_ptr(),
            ..Default::default()
        };
        RegisterClassW(&class);
        let hwnd = CreateWindowExW(
            0,
            name.as_ptr(),
            wide("Timelens background").as_ptr(),
            0,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            null(),
        );
        if hwnd.is_null() {
            let _ = ready_sender.send(false);
            return;
        }
        WINDOW.store(hwnd as isize, std::sync::atomic::Ordering::Release);
        let added = Shell_NotifyIconW(NIM_ADD, &notification_data(hwnd)) != 0;
        let _ = ready_sender.send(added);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    });
    if !ready_receiver
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap_or(false)
    {
        return Err(anyhow!("系统托盘初始化失败"));
    }
    let weak = w.as_weak();
    w.window().on_close_requested(move || {
        if let Some(w) = weak.upgrade() {
            let _ = w.hide();
        }
        // Closing the window only hides it; say so once, ever, so recording in
        // the background is never a surprise.
        let marker = control.join(EXPLAINED_FILE);
        if !HIDE_EXPLAINED.swap(true, std::sync::atomic::Ordering::AcqRel) && !marker.exists() {
            let _ = std::fs::create_dir_all(&control).and_then(|()| std::fs::write(&marker, b"1"));
            show_notice(Notice {
                title: "Timelens 仍在后台记录",
                text: "窗口已收起到通知区域，记录照常进行。点击托盘图标可重新打开；右键可以暂停记录或退出。".into(),
                success: true,
            });
        }
        slint::CloseRequestResponse::KeepWindowShown
    });
    let quitting = std::rc::Rc::new(std::cell::Cell::new(false));
    let weak = w.as_weak();
    let menu_quitting = std::rc::Rc::clone(&quitting);
    w.global::<CollectionState>().on_menu_changed(move || {
        if let Some(w) = weak.upgrade() {
            collection_menu(&w, menu_quitting.get());
        }
    });
    let weak = w.as_weak();
    w.on_tray_events_ready(move || {
        let Some(w) = weak.upgrade() else {
            return;
        };
        while let Ok(event) = receiver.try_recv() {
            match event {
                Event::Open => unreachable!("activation bypasses this queue"),
                Event::SetPaused(paused) => {
                    let state = w.global::<CollectionState>();
                    if !quitting.get() && !state.get_busy() {
                        state.set_paused(paused);
                        state.invoke_action(0);
                    }
                }
                Event::PauseFor(minutes) => {
                    if !quitting.get() {
                        w.global::<CollectionState>().invoke_pause_for(minutes);
                    }
                }
                Event::Capture => {
                    if !quitting.get() {
                        w.invoke_snapshot_capture_now();
                    }
                }
                Event::Quit => {
                    if quitting.get() {
                        continue;
                    }
                    quitting.set(true);
                    crate::collector_task::suspend();
                    w.set_action_status("正在保存并退出 Timelens…".into());
                    let storage = Arc::clone(&storage);
                    thread::spawn(move || {
                        let result: Result<()> = crate::snapshot::run_heavy_task(|| {
                            let stop = {
                                let s = storage
                                    .lock()
                                    .map_err(|_| anyhow!("storage lock poisoned"))?;
                                s.control_directory().join("collector-stop.request")
                            };
                            std::fs::write(&stop, b"stop")?;
                            let start = std::time::Instant::now();
                            loop {
                                if let Ok(_collector) =
                                    timelens_ipc::SingleInstanceGuard::acquire_collector()
                                {
                                    break;
                                }
                                if start.elapsed() > std::time::Duration::from_secs(15) {
                                    return Err(anyhow!("采集器未能完成停止，请重试"));
                                }
                                thread::sleep(std::time::Duration::from_millis(50));
                            }
                            let _ = std::fs::remove_file(stop);
                            storage
                                .lock()
                                .map_err(|_| anyhow!("storage lock poisoned"))?
                                .checkpoint()?;
                            Ok(())
                        });
                        match result {
                            Ok(()) => emit(Event::QuitReady),
                            Err(e) => emit(Event::QuitFailed(e.to_string())),
                        }
                    });
                }
                Event::QuitReady => {
                    SHUTDOWN_READY.store(true, std::sync::atomic::Ordering::Release);
                    let _ = slint::quit_event_loop();
                }
                Event::QuitFailed(error) => {
                    quitting.set(false);
                    crate::collector_task::resume();
                    w.set_action_status(format!("退出未完成：{error}").into());
                }
                Event::SettingChanged => crate::appearance::follow_system(&w),
            }
        }
        collection_menu(&w, quitting.get());
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_commands_pause_resume_capture_and_quit() {
        let running = CollectionMenu::default();
        assert_eq!(menu_event(1, &running), Some(Event::Open));
        assert_eq!(menu_event(3, &running), Some(Event::Quit));
        assert_eq!(menu_event(4, &running), Some(Event::Capture));
        assert_eq!(menu_event(10, &running), Some(Event::PauseFor(15)));
        assert_eq!(menu_event(13, &running), Some(Event::PauseFor(-1)));
        assert_eq!(menu_event(0, &running), None);
        let paused = CollectionMenu {
            paused: true,
            ..CollectionMenu::default()
        };
        assert_eq!(menu_event(2, &paused), Some(Event::SetPaused(false)));
        let busy = CollectionMenu {
            busy: true,
            ..paused
        };
        assert_eq!(menu_event(2, &busy), None);
        assert_eq!(menu_event(10, &busy), None);
    }

    #[test]
    fn native_menu_shows_recording_state_and_disables_pending_changes() {
        for paused in [false, true] {
            for busy in [false, true] {
                let state = CollectionMenu {
                    paused,
                    busy,
                    resume: if paused {
                        "还剩 5 分钟".into()
                    } else {
                        String::new()
                    },
                };
                let menu = popup_menu(&state);
                assert!(!menu.is_null());
                unsafe {
                    assert_eq!(GetMenuItemCount(menu), 7);
                    let label = |position: u32| {
                        let mut text = [0_u16; 80];
                        let len = GetMenuStringW(
                            menu,
                            position,
                            text.as_mut_ptr(),
                            text.len() as i32,
                            MF_BYPOSITION,
                        );
                        String::from_utf16_lossy(&text[..len as usize])
                    };
                    assert_eq!(label(0), state.status());
                    assert_eq!(label(2), "打开 Timelens");
                    assert_eq!(label(3), "立即拍摄快照");
                    assert_eq!(
                        label(4),
                        if paused {
                            "继续记录"
                        } else {
                            "暂停记录"
                        }
                    );
                    assert_eq!(label(6), "退出（停止记录）…");
                    assert_eq!(GetMenuItemID(menu, 6), 3);
                    assert_ne!(GetMenuState(menu, 5, MF_BYPOSITION) & MF_SEPARATOR, 0);
                    assert_eq!(GetMenuState(menu, 4, MF_BYPOSITION) & MF_GRAYED != 0, busy);
                    assert_eq!(GetMenuState(menu, 3, MF_BYCOMMAND) & MF_GRAYED, 0);
                    DestroyMenu(menu);
                }
            }
        }
    }
}
