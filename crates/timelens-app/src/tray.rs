use crate::{AiState, AppWindow, CollectionState};
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
static WINDOW: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
static SHUTDOWN_READY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static EVENTS: Mutex<Option<EventSink>> = Mutex::new(None);
static COLLECTION_MENU: Mutex<CollectionMenu> = Mutex::new(CollectionMenu {
    paused: false,
    busy: false,
});
static NOTICE: Mutex<Option<Notice>> = Mutex::new(None);
static HIDE_EXPLAINED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
struct Notice {
    /// The AI job a click opens; general notices open nothing.
    job: Option<i64>,
    title: &'static str,
    text: &'static str,
    success: bool,
}
struct EventSink {
    sender: mpsc::Sender<Event>,
    window: slint::Weak<AppWindow>,
}
#[derive(Clone, Copy, Default)]
struct CollectionMenu {
    paused: bool,
    busy: bool,
}
#[cfg_attr(test, derive(Debug, PartialEq))]
enum Event {
    Open,
    Summary(i64),
    SetPaused(bool),
    Quit,
    QuitReady,
    QuitFailed(String),
}
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}
fn emit(event: Event) {
    if let Ok(sink) = EVENTS.lock()
        && let Some(sink) = &*sink
    {
        match event {
            Event::Open | Event::Summary(_) => {
                // Handle the user's activation immediately, while Windows still
                // allows this process to take foreground focus.
                let _ = sink.window.upgrade_in_event_loop(move |w| {
                    if let Err(error) = crate::window_chrome::show_and_activate(&w) {
                        w.set_action_status(format!("打开窗口失败：{error}").into());
                        return;
                    }
                    if let Event::Summary(id) = event {
                        // The settings would cover the summary.
                        if w.get_settings_open() {
                            if w.get_settings_tab() == 4 {
                                w.global::<AiState>().invoke_flush_settings();
                            }
                            w.set_settings_open(false);
                        }
                        w.invoke_navigate(4);
                        w.global::<AiState>()
                            .invoke_open_job(i32::try_from(id).unwrap_or(-1));
                    }
                });
            }
            _ => {
                let _ = sink.sender.send(event);
            }
        }
    }
}
fn collection_menu(w: &AppWindow, quitting: bool) {
    if let Ok(mut menu) = COLLECTION_MENU.lock() {
        let state = w.global::<CollectionState>();
        menu.paused = state.get_paused();
        menu.busy = state.get_busy() || quitting;
    }
}
fn popup_menu(state: CollectionMenu) -> HMENU {
    unsafe {
        let menu = CreatePopupMenu();
        if !menu.is_null() {
            AppendMenuW(menu, MF_STRING, 1, wide("打开 Timelens").as_ptr());
            AppendMenuW(
                menu,
                MF_STRING | if state.busy { MF_GRAYED } else { 0 },
                2,
                wide(if state.paused {
                    "开始采集"
                } else {
                    "停止采集"
                })
                .as_ptr(),
            );
            AppendMenuW(menu, MF_SEPARATOR, 0, null());
            AppendMenuW(menu, MF_STRING, 3, wide("退出 Timelens").as_ptr());
        }
        menu
    }
}
fn menu_event(selected: i32, state: CollectionMenu) -> Option<Event> {
    match selected {
        1 => Some(Event::Open),
        2 if !state.busy => Some(Event::SetPaused(!state.paused)),
        3 => Some(Event::Quit),
        _ => None,
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
    for (slot, c) in d
        .szTip
        .iter_mut()
        .zip("Timelens · 记录你的时间".encode_utf16())
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
        CALLBACK => {
            match l as u32 & 0xffff {
                WM_LBUTTONUP | WM_LBUTTONDBLCLK | NIN_SELECT => emit(Event::Open),
                NIN_BALLOONUSERCLICK => {
                    if let Ok(n) = NOTICE.lock()
                        && let Some(notice) = &*n
                    {
                        emit(notice.job.map_or(Event::Open, Event::Summary));
                    }
                }
                WM_RBUTTONUP | WM_CONTEXTMENU => unsafe {
                    let state = COLLECTION_MENU.lock().map(|s| *s).unwrap_or_default();
                    let menu = popup_menu(state);
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
                    if let Some(event) = menu_event(selected, state) {
                        emit(event);
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
pub fn notify(id: i64, success: bool) {
    show_notice(Notice {
        job: Some(id),
        title: "Timelens AI",
        text: if success {
            "总结已完成，点击查看。"
        } else {
            "AI 任务失败，点击查看原因与保留的内容。"
        },
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
) -> Result<slint::Timer> {
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
        // Closing the window only hides it; say so once so recording in the
        // background is never a surprise.
        if !HIDE_EXPLAINED.swap(true, std::sync::atomic::Ordering::AcqRel) {
            show_notice(Notice {
                job: None,
                title: "Timelens 仍在后台运行",
                text: "窗口已收起到通知区域。点击托盘图标可重新打开；右键可以停止／开始采集，或退出 Timelens。",
                success: true,
            });
        }
        slint::CloseRequestResponse::KeepWindowShown
    });
    let weak = w.as_weak();
    let timer = slint::Timer::default();
    let mut quitting = false;
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(200),
        move || {
            let Some(w) = weak.upgrade() else {
                return;
            };
            while let Ok(event) = receiver.try_recv() {
                match event {
                    Event::Open | Event::Summary(_) => unreachable!("activation bypasses polling"),
                    Event::SetPaused(paused) => {
                        let state = w.global::<CollectionState>();
                        if !quitting && !state.get_busy() {
                            state.set_paused(paused);
                            state.invoke_action(0);
                        }
                    }
                    Event::Quit => {
                        if quitting {
                            continue;
                        }
                        quitting = true;
                        crate::collector_task::suspend();
                        w.set_action_status("正在保存并退出 Timelens…".into());
                        let storage = Arc::clone(&storage);
                        thread::spawn(move || {
                            let result: Result<()> = (|| {
                                let ai = crate::ai::pause_and_wait()?;
                                crate::snapshot::run_heavy_task(|| {
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
                                })?;
                                ai.keep_paused();
                                Ok(())
                            })();
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
                        quitting = false;
                        crate::collector_task::resume();
                        w.set_action_status(format!("退出未完成：{error}").into());
                    }
                }
            }
            collection_menu(&w, quitting);
        },
    );
    Ok(timer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_commands_are_distinct_from_exit() {
        for paused in [false, true] {
            let state = CollectionMenu {
                paused,
                busy: false,
            };
            assert_eq!(menu_event(1, state), Some(Event::Open));
            assert_eq!(menu_event(2, state), Some(Event::SetPaused(!paused)));
            assert_eq!(menu_event(3, state), Some(Event::Quit));
            assert_eq!(menu_event(0, state), None);
            assert_eq!(
                menu_event(
                    2,
                    CollectionMenu {
                        busy: true,
                        ..state
                    }
                ),
                None
            );
        }
    }

    #[test]
    fn native_menu_shows_capture_state_and_disables_pending_changes() {
        for paused in [false, true] {
            for busy in [false, true] {
                let menu = popup_menu(CollectionMenu { paused, busy });
                assert!(!menu.is_null());
                unsafe {
                    assert_eq!(GetMenuItemCount(menu), 4);
                    for (position, id, expected) in [
                        (0, 1, "打开 Timelens"),
                        (
                            1,
                            2,
                            if paused {
                                "开始采集"
                            } else {
                                "停止采集"
                            },
                        ),
                        (3, 3, "退出 Timelens"),
                    ] {
                        assert_eq!(GetMenuItemID(menu, position), id);
                        let mut label = [0_u16; 80];
                        let len = GetMenuStringW(
                            menu,
                            id,
                            label.as_mut_ptr(),
                            label.len() as i32,
                            MF_BYCOMMAND,
                        );
                        assert_eq!(String::from_utf16_lossy(&label[..len as usize]), expected);
                    }
                    assert_ne!(GetMenuState(menu, 2, MF_BYPOSITION) & MF_SEPARATOR, 0);
                    assert_eq!(GetMenuState(menu, 2, MF_BYCOMMAND) & MF_GRAYED != 0, busy);
                    assert_eq!(GetMenuState(menu, 3, MF_BYCOMMAND) & MF_GRAYED, 0);
                    DestroyMenu(menu);
                }
            }
        }
    }
}
