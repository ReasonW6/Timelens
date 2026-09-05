use crate::{AiState, AppWindow};
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
static EVENTS: Mutex<Option<mpsc::Sender<Event>>> = Mutex::new(None);
static NOTICE: Mutex<Option<(i64, String, bool)>> = Mutex::new(None);
enum Event {
    Open,
    Summary(i64),
    Quit,
    QuitReady,
    QuitFailed(String),
}
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}
fn emit(event: Event) {
    if let Ok(sender) = EVENTS.lock()
        && let Some(sender) = &*sender
    {
        let _ = sender.send(event);
    }
}
fn notification_data(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut d = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: CALLBACK,
        hIcon: unsafe { LoadIconW(std::ptr::null_mut(), IDI_APPLICATION) },
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
                        && let Some((id, _, _)) = &*n
                    {
                        emit(Event::Summary(*id));
                    }
                }
                WM_RBUTTONUP | WM_CONTEXTMENU => unsafe {
                    let menu = CreatePopupMenu();
                    AppendMenuW(menu, MF_STRING, 1, wide("打开 Timelens").as_ptr());
                    AppendMenuW(
                        menu,
                        MF_STRING,
                        2,
                        wide("退出 Timelens 并停止采集").as_ptr(),
                    );
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
                    if selected == 1 {
                        emit(Event::Open);
                    } else if selected == 2 {
                        emit(Event::Quit);
                    }
                },
                _ => {}
            }
            0
        }
        WM_APP => {
            if let Ok(n) = NOTICE.lock()
                && let Some((_, text, success)) = &*n
            {
                let mut data = notification_data(hwnd);
                data.uFlags = NIF_INFO;
                data.dwInfoFlags = if *success { NIIF_INFO } else { NIIF_ERROR };
                for (slot, c) in data
                    .szInfoTitle
                    .iter_mut()
                    .zip("Timelens AI".encode_utf16())
                {
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
        PostMessageW(hwnd, if stop { STOP } else { SHOW }, 0, 0) != 0
    }
}
pub fn notify(id: i64, success: bool) {
    if let Ok(mut n) = NOTICE.lock() {
        *n = Some((
            id,
            if success {
                "总结已完成，点击查看。"
            } else {
                "AI 任务失败，点击查看原因与保留的内容。"
            }
            .into(),
            success,
        ));
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
    *EVENTS.lock().map_err(|_| anyhow!("托盘锁不可用"))? = Some(sender);
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
                    Event::Open => {
                        let _ = w.show();
                        w.window().set_minimized(false);
                    }
                    Event::Summary(id) => {
                        let _ = w.show();
                        w.window().set_minimized(false);
                        let g = w.global::<AiState>();
                        g.set_open(true);
                        g.invoke_opened();
                        g.set_status(format!("任务 #{id}，可在任务历史中查看结果").into());
                        g.set_page(4);
                    }
                    Event::Quit => {
                        if quitting {
                            continue;
                        }
                        quitting = true;
                        w.set_action_status("正在保存最后的观察并停止采集…".into());
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
                        w.set_action_status(format!("退出未完成：{error}").into());
                    }
                }
            }
        },
    );
    Ok(timer)
}
