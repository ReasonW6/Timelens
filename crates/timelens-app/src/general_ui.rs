use crate::{AppWindow, startup};
use slint::ComponentHandle;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
};

struct Update {
    state: Result<startup::StartupState, String>,
    message: Option<String>,
}

fn request(
    window: &AppWindow,
    task: &Option<String>,
    enabled: Option<bool>,
    busy: &Arc<AtomicBool>,
    sender: &mpsc::Sender<Update>,
) {
    let Some(task) = task.clone() else {
        return;
    };
    if busy.swap(true, Ordering::AcqRel) {
        return;
    }
    window.set_startup_busy(true);
    window.set_startup_status(
        if enabled.is_some() {
            "等待管理员确认…"
        } else {
            "正在读取启动设置…"
        }
        .into(),
    );
    let sender = sender.clone();
    let weak = window.as_weak();
    thread::spawn(move || {
        let message = enabled.map(|enabled| match startup::set(&task, enabled) {
            Ok(()) => if enabled {
                "已开启开机自启动"
            } else {
                "已关闭开机自启动，当前记录继续运行"
            }
            .to_owned(),
            Err(error) => format!("{error:#}"),
        });
        let state = startup::read(&task).map_err(|error| format!("{error:#}"));
        let _ = sender.send(Update { state, message });
        let _ = weak.upgrade_in_event_loop(|window| window.invoke_startup_ready());
    });
}

pub fn install(window: &AppWindow, task: Option<String>) {
    let (sender, receiver) = mpsc::channel::<Update>();
    let busy = Arc::new(AtomicBool::new(false));
    if task.is_none() {
        window.set_startup_status("开机自启动仅在安装版中可用".into());
    }
    let weak = window.as_weak();
    let read_task = task.clone();
    let read_busy = Arc::clone(&busy);
    let read_sender = sender.clone();
    window.on_general_opened(move || {
        if let Some(window) = weak.upgrade() {
            request(&window, &read_task, None, &read_busy, &read_sender);
        }
    });
    let weak = window.as_weak();
    let change_task = task.clone();
    let change_busy = Arc::clone(&busy);
    let change_sender = sender.clone();
    window.on_startup_changed(move |enabled| {
        if let Some(window) = weak.upgrade()
            && window.get_startup_available()
        {
            request(
                &window,
                &change_task,
                Some(enabled),
                &change_busy,
                &change_sender,
            );
        }
    });
    request(window, &task, None, &busy, &sender);
    let weak = window.as_weak();
    window.on_startup_ready(move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        for update in receiver.try_iter() {
            busy.store(false, Ordering::Release);
            window.set_startup_busy(false);
            match update.state {
                Ok(state) => {
                    window.set_startup_enabled(state.enabled);
                    window.set_startup_available(true);
                    let message = update.message.unwrap_or_else(|| {
                        if !state.consistent {
                            "主程序与采集器的自启动状态不一致，请开启开关同步设置"
                        } else if state.enabled {
                            "已开启开机自启动"
                        } else {
                            "已关闭开机自启动"
                        }
                        .to_owned()
                    });
                    window.set_startup_status(message.into());
                }
                Err(error) => {
                    window.set_startup_available(false);
                    window.set_startup_status(error.into());
                }
            }
        }
    });
}
