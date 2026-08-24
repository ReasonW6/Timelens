#![cfg(windows)]
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    cell::RefCell,
    env,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use slint::{ModelRc, Timer, TimerMode, VecModel};
use timelens_ipc::{
    COLLECTOR_RESET_PAUSED_FILE, COLLECTOR_RESET_REQUEST_FILE, PROTOCOL_VERSION,
    SingleInstanceGuard, current_pipe_name, run_server_collector_message, run_server_probe,
};
use timelens_storage::{RetentionPolicy, Storage, TimelineApplication, TimelineSnapshot};

const COLLECTOR_NAMES: &[&str] = &["timelens-collector.exe", "Timelens.Collector.exe"];

slint::slint! {
    import { ListView } from "std-widgets.slint";

    export struct AppRow {
        name: string,
        summary: string,
        open-ratio: float,
        display-ratio: float,
        focus-ratio: float,
    }

    export struct WindowRow {
        label: string,
        summary: string,
    }

    export struct SegmentRow {
        offset: float,
        width: float,
        kind: int,
    }

    component FlatButton inherits Rectangle {
        in property <string> text;
        in property <bool> danger: false;
        callback clicked;
        height: 34px;
        border-radius: 8px;
        background: touch.pressed ? (root.danger ? #7b3039 : #234d72)
                                  : (root.danger ? #4b252c : #1a3147);
        border-width: 1px;
        border-color: root.danger ? #8c4652 : #2b506e;
        Text {
            text: root.text;
            color: root.danger ? #ffc1c8 : #d8eaff;
            horizontal-alignment: center;
            vertical-alignment: center;
            font-size: 12px;
        }
        touch := TouchArea { clicked => { root.clicked(); } }
    }

    component MetricCard inherits Rectangle {
        in property <string> label;
        in property <string> value;
        background: #151f2b;
        border-radius: 10px;
        VerticalLayout {
            padding: 12px;
            spacing: 4px;
            Text { text: root.label; color: #7e92a8; font-size: 11px; }
            Text { text: root.value; color: #eef5fb; font-size: 17px; font-weight: 600; }
        }
    }

    export component AppWindow inherits Window {
        title: "Timelens";
        width: 1120px;
        height: 720px;
        background: rgb(11, 17, 24);

        in property <string> storage-status;
        in property <string> collector-status;
        in property <string> data-path;
        in property <string> range-label;
        in property <string> selected-name: "尚无应用数据";
        in property <string> opened-value: "0 秒";
        in property <string> displayed-value: "0 秒";
        in property <string> focused-value: "0 秒";
        in property <string> background-value: "0 秒";
        in property <string> input-value: "键盘 0 · 鼠标 0";
        in property <string> coverage-value: "数据完整";
        in property <string> retention-value: "30 天 · 100 MiB";
        in property <string> action-status: "";
        in property <[AppRow]> apps;
        in property <[WindowRow]> windows;
        in property <[SegmentRow]> segments;
        in-out property <int> selected-index: -1;
        in-out property <bool> windows-expanded: false;
        in-out property <float> selection-left: 0;
        in-out property <float> selection-right: 1;
        private property <bool> middle-dragging: false;
        private property <float> middle-start: 0;

        callback app-selected(int);
        callback horizon-selected(int);
        callback range-selected(float, float);
        callback retention-selected(int);
        callback run-cleanup;
        callback clear-all;

        VerticalLayout {
            padding: 22px;
            spacing: 14px;

            HorizontalLayout {
                height: 54px;
                VerticalLayout {
                    spacing: 2px;
                    Text { text: "TIMELENS"; color: #eef6ff; font-size: 25px; font-weight: 700; }
                    Text { text: root.collector-status; color: #6f91ae; font-size: 11px; }
                }
                Rectangle { horizontal-stretch: 1; }
                VerticalLayout {
                    alignment: end;
                    Text { text: root.range-label; color: #cfe8ff; font-size: 13px; horizontal-alignment: right; }
                    Text { text: root.coverage-value; color: #70d6a1; font-size: 11px; horizontal-alignment: right; }
                }
            }

            Rectangle {
                height: 122px;
                background: #111b26;
                border-radius: 12px;
                VerticalLayout {
                    padding: 14px;
                    spacing: 8px;
                    HorizontalLayout {
                        Text { text: "时间范围"; color: #a8bacb; font-size: 12px; }
                        Rectangle { horizontal-stretch: 1; }
                        FlatButton { width: 58px; text: "6 小时"; clicked => { root.horizon-selected(6); } }
                        FlatButton { width: 62px; text: "24 小时"; clicked => { root.horizon-selected(24); } }
                        FlatButton { width: 54px; text: "7 天"; clicked => { root.horizon-selected(168); } }
                    }
                    Text { text: "在下方时间带按住鼠标中键拖动可截取区间"; color: rgb(97, 121, 142); font-size: 10px; }
                    axis := Rectangle {
                        height: 42px;
                        background: rgb(11, 19, 28);
                        border-radius: 8px;
                        for segment in root.segments : Rectangle {
                            x: segment.offset * parent.width;
                            width: max(2px, segment.width * parent.width);
                            height: parent.height;
                            background: segment.kind == 2 ? #62b6ff55
                                      : segment.kind == 1 ? #54d7a255
                                      : #a58bff3c;
                        }
                        Rectangle {
                            x: min(root.selection-left, root.selection-right) * parent.width;
                            width: abs(root.selection-right - root.selection-left) * parent.width;
                            height: parent.height;
                            background: #8cc8ff24;
                            border-width: 1px;
                            border-color: #8cc8ff;
                        }
                        axis-touch := TouchArea {
                            pointer-event(event) => {
                                if (event.button == PointerEventButton.middle && event.kind == PointerEventKind.down) {
                                    root.middle-dragging = true;
                                    root.middle-start = axis-touch.mouse-x / axis-touch.width;
                                    root.selection-left = root.middle-start;
                                    root.selection-right = root.middle-start;
                                }
                                if (event.button == PointerEventButton.middle && event.kind == PointerEventKind.up && root.middle-dragging) {
                                    root.middle-dragging = false;
                                    root.selection-right = axis-touch.mouse-x / axis-touch.width;
                                    root.range-selected(root.middle-start, root.selection-right);
                                }
                            }
                            moved => {
                                if (root.middle-dragging) {
                                    root.selection-right = axis-touch.mouse-x / axis-touch.width;
                                }
                            }
                        }
                    }
                }
            }

            HorizontalLayout {
                spacing: 14px;
                vertical-stretch: 1;

                Rectangle {
                    width: 330px;
                    background: #101923;
                    border-radius: 12px;
                    VerticalLayout {
                        padding: 12px;
                        spacing: 8px;
                        Text { text: "选中时段内的应用"; color: #94a9bc; font-size: 12px; }
                        ListView {
                            for app[index] in root.apps : Rectangle {
                                height: 70px;
                                border-radius: 9px;
                                background: index == root.selected-index ? #1b3650 : row-touch.has-hover ? #152535 : #111c27;
                                VerticalLayout {
                                    padding: 10px;
                                    spacing: 5px;
                                    HorizontalLayout {
                                        Text { text: app.name; color: #edf6ff; font-size: 13px; font-weight: 600; overflow: elide; }
                                        Rectangle { horizontal-stretch: 1; }
                                        Text { text: app.summary; color: #7890a6; font-size: 10px; }
                                    }
                                    Rectangle {
                                        height: 7px;
                                        background: #0a1118;
                                        border-radius: 4px;
                                        Rectangle { width: app.open-ratio * parent.width; height: parent.height; background: #7c6fc766; border-radius: 4px; }
                                        Rectangle { width: app.display-ratio * parent.width; height: parent.height; background: #4ac58a99; border-radius: 4px; }
                                        Rectangle { width: app.focus-ratio * parent.width; height: parent.height; background: #5caeff; border-radius: 4px; }
                                    }
                                }
                                row-touch := TouchArea { clicked => { root.app-selected(index); } }
                            }
                        }
                    }
                }

                Rectangle {
                    horizontal-stretch: 1;
                    background: #101923;
                    border-radius: 12px;
                    VerticalLayout {
                        padding: 16px;
                        spacing: 12px;
                        HorizontalLayout {
                            VerticalLayout {
                                Text { text: root.selected-name; color: #f2f7fc; font-size: 21px; font-weight: 650; overflow: elide; }
                                Text { text: root.input-value; color: #6f8da6; font-size: 11px; }
                            }
                            Rectangle { horizontal-stretch: 1; }
                            FlatButton {
                                width: 108px;
                                text: root.windows-expanded ? "收起窗口" : "展开窗口";
                                clicked => { root.windows-expanded = !root.windows-expanded; }
                            }
                        }
                        HorizontalLayout {
                            spacing: 8px;
                            MetricCard { label: "打开"; value: root.opened-value; }
                            MetricCard { label: "显示中"; value: root.displayed-value; }
                            MetricCard { label: "聚焦中"; value: root.focused-value; }
                            MetricCard { label: "后台"; value: root.background-value; }
                        }
                        Rectangle {
                            height: 54px;
                            background: rgb(11, 19, 28);
                            border-radius: 8px;
                            for segment in root.segments : Rectangle {
                                x: segment.offset * parent.width;
                                width: max(2px, segment.width * parent.width);
                                height: segment.kind == 2 ? 16px : segment.kind == 1 ? 14px : 12px;
                                y: segment.kind == 2 ? 5px : segment.kind == 1 ? 21px : 37px;
                                background: segment.kind == 2 ? #62b6ff : segment.kind == 1 ? #54d7a2 : #9a82df;
                                border-radius: 3px;
                            }
                        }
                        if root.windows-expanded : ListView {
                            for item in root.windows : Rectangle {
                                height: 48px;
                                background: #121e2a;
                                border-radius: 7px;
                                HorizontalLayout {
                                    padding: 10px;
                                    Text { text: item.label; color: #cfe1ef; font-size: 12px; }
                                    Rectangle { horizontal-stretch: 1; }
                                    Text { text: item.summary; color: #6f879c; font-size: 10px; }
                                }
                            }
                        }
                        Rectangle { vertical-stretch: 1; }
                    }
                }
            }

            Rectangle {
                height: 74px;
                background: #101923;
                border-radius: 11px;
                HorizontalLayout {
                    padding: 12px;
                    spacing: 8px;
                    VerticalLayout {
                        Text { text: "数据保留  " + root.retention-value; color: #a7bacb; font-size: 12px; }
                        Text { text: root.storage-status + "  ·  " + root.data-path; color: #536d83; font-size: 10px; overflow: elide; }
                        Text { text: root.action-status; color: #77cda1; font-size: 10px; }
                    }
                    Rectangle { horizontal-stretch: 1; }
                    FlatButton { width: 50px; text: "7 天"; clicked => { root.retention-selected(7); } }
                    FlatButton { width: 56px; text: "30 天"; clicked => { root.retention-selected(30); } }
                    FlatButton { width: 56px; text: "90 天"; clicked => { root.retention-selected(90); } }
                    FlatButton { width: 54px; text: "永久"; clicked => { root.retention-selected(0); } }
                    FlatButton { width: 78px; text: "立即清理"; clicked => { root.run-cleanup(); } }
                    FlatButton { width: 86px; text: "清空全部"; danger: true; clicked => { root.clear-all(); } }
                }
            }
        }
    }
}

fn main() -> Result<()> {
    let options = Options::parse(env::args_os().skip(1))?;
    let _instance = SingleInstanceGuard::acquire_core().context("Timelens is already running")?;
    let data_directory = options.data_directory()?;
    let storage = Storage::open(&data_directory).context("failed to open encrypted storage")?;
    let retention = storage
        .apply_retention(unix_time_ms())
        .context("failed to apply the configured retention policy")?;
    if retention.activity_items > 0 || retention.input_items > 0 {
        println!(
            "retention cleanup complete: activity_items={} input_items={} released_bytes={}",
            retention.activity_items, retention.input_items, retention.released_bytes
        );
    }
    storage.record_component_health("core", PROTOCOL_VERSION, None)?;
    let pipe_name = options.pipe_name.unwrap_or(current_pipe_name()?);

    if options.handshake_once {
        let report = run_server_probe(&pipe_name, COLLECTOR_NAMES)?;
        storage.record_component_health(
            "collector",
            PROTOCOL_VERSION,
            Some(report.peer_process_id),
        )?;
        println!(
            "collector handshake ok: pid={} session={} path={} verification={:?}",
            report.peer_process_id,
            report.peer_session_id,
            report.peer_path.display(),
            report.verification
        );
        return Ok(());
    }

    if options.background {
        return run_background(Arc::new(Mutex::new(storage)), data_directory, pipe_name);
    }

    run_window(Arc::new(Mutex::new(storage)), data_directory, pipe_name)
}

fn run_background(
    storage: Arc<Mutex<Storage>>,
    data_directory: PathBuf,
    pipe_name: String,
) -> Result<()> {
    loop {
        match run_server_collector_message(&pipe_name, COLLECTOR_NAMES, |batch| {
            ingest_if_not_resetting(&storage, &data_directory, batch)
        }) {
            Ok(report) => storage
                .lock()
                .map_err(|_| anyhow::anyhow!("storage lock poisoned"))?
                .record_component_health(
                    "collector",
                    PROTOCOL_VERSION,
                    Some(report.peer_process_id),
                )?,
            Err(error) => {
                eprintln!("collector probe failed: {error}");
                thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

enum CollectorStatus {
    Connected(String),
    Failed(String),
}

struct ActionStatus {
    message: String,
    refresh: bool,
}

struct UiState {
    horizon_started_utc_ms: i64,
    horizon_ended_utc_ms: i64,
    range_started_utc_ms: i64,
    range_ended_utc_ms: i64,
    selected_identity: Option<String>,
    application_identities: Vec<String>,
    follow_now: bool,
}

impl UiState {
    fn recent_hours(hours: i64) -> Self {
        let ended = unix_time_ms();
        let started = ended.saturating_sub(hours * 3_600_000);
        Self {
            horizon_started_utc_ms: started,
            horizon_ended_utc_ms: ended,
            range_started_utc_ms: started,
            range_ended_utc_ms: ended,
            selected_identity: None,
            application_identities: Vec::new(),
            follow_now: true,
        }
    }

    fn select_fraction_range(&mut self, first: f32, second: f32) -> bool {
        let first = first.clamp(0.0, 1.0);
        let second = second.clamp(0.0, 1.0);
        let left = first.min(second);
        let right = first.max(second);
        if right - left < 0.002 {
            return false;
        }
        let span = self
            .horizon_ended_utc_ms
            .saturating_sub(self.horizon_started_utc_ms);
        self.range_started_utc_ms = self
            .horizon_started_utc_ms
            .saturating_add((span as f64 * f64::from(left)) as i64);
        self.range_ended_utc_ms = self
            .horizon_started_utc_ms
            .saturating_add((span as f64 * f64::from(right)) as i64);
        self.follow_now = false;
        true
    }
}

fn run_window(
    storage: Arc<Mutex<Storage>>,
    data_directory: PathBuf,
    pipe_name: String,
) -> Result<()> {
    let window = AppWindow::new()?;
    let storage_status = {
        let storage = storage
            .lock()
            .map_err(|_| anyhow::anyhow!("storage lock poisoned"))?;
        format!(
            "SQLCipher {} · schema v{}",
            storage.cipher_version(),
            storage.schema_version()?
        )
    };
    window.set_storage_status(storage_status.into());
    window.set_collector_status("等待受信采集器事件…".into());
    window.set_data_path(data_directory.display().to_string().into());

    let (status_sender, status_receiver) = mpsc::channel();
    let server_storage = Arc::clone(&storage);
    let server_data_directory = data_directory.clone();
    thread::spawn(move || {
        loop {
            let status = match run_server_collector_message(&pipe_name, COLLECTOR_NAMES, |batch| {
                ingest_if_not_resetting(&server_storage, &server_data_directory, batch)
            }) {
                Ok(report) => match server_storage
                    .lock()
                    .map_err(|_| anyhow::anyhow!("storage lock poisoned"))
                    .and_then(|storage| {
                        storage
                            .record_component_health(
                                "collector",
                                PROTOCOL_VERSION,
                                Some(report.peer_process_id),
                            )
                            .map_err(Into::into)
                    }) {
                    Ok(()) => CollectorStatus::Connected(format!(
                        "已验证采集器 · PID {} · {:?}",
                        report.peer_process_id, report.verification
                    )),
                    Err(error) => CollectorStatus::Failed(format!("数据库状态写入失败：{error}")),
                },
                Err(error) => CollectorStatus::Failed(error.to_string()),
            };
            if status_sender.send(status).is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(250));
        }
    });

    let ui_state = Rc::new(RefCell::new(UiState::recent_hours(24)));
    let action_busy = Arc::new(AtomicBool::new(false));
    let (action_sender, action_receiver) = mpsc::channel::<ActionStatus>();

    {
        let weak = window.as_weak();
        let storage = Arc::clone(&storage);
        let ui_state = Rc::clone(&ui_state);
        let action_busy = Arc::clone(&action_busy);
        window.on_app_selected(move |index| {
            if action_busy.load(Ordering::Acquire) || index < 0 {
                return;
            }
            let mut state = ui_state.borrow_mut();
            state.selected_identity = state.application_identities.get(index as usize).cloned();
            drop(state);
            if let Some(window) = weak.upgrade()
                && let Err(error) = refresh_timeline(&window, &storage, &ui_state)
            {
                window.set_action_status(format!("刷新失败：{error}").into());
            }
        });
    }
    {
        let weak = window.as_weak();
        let storage = Arc::clone(&storage);
        let ui_state = Rc::clone(&ui_state);
        let action_busy = Arc::clone(&action_busy);
        window.on_horizon_selected(move |hours| {
            if action_busy.load(Ordering::Acquire) || hours <= 0 {
                return;
            }
            *ui_state.borrow_mut() = UiState::recent_hours(i64::from(hours));
            if let Some(window) = weak.upgrade()
                && let Err(error) = refresh_timeline(&window, &storage, &ui_state)
            {
                window.set_action_status(format!("刷新失败：{error}").into());
            }
        });
    }
    {
        let weak = window.as_weak();
        let storage = Arc::clone(&storage);
        let ui_state = Rc::clone(&ui_state);
        let action_busy = Arc::clone(&action_busy);
        window.on_range_selected(move |left, right| {
            if action_busy.load(Ordering::Acquire) {
                return;
            }
            let mut state = ui_state.borrow_mut();
            if !state.select_fraction_range(left, right) {
                return;
            }
            drop(state);
            if let Some(window) = weak.upgrade()
                && let Err(error) = refresh_timeline(&window, &storage, &ui_state)
            {
                window.set_action_status(format!("刷新失败：{error}").into());
            }
        });
    }

    install_retention_callback(
        &window,
        Arc::clone(&storage),
        Arc::clone(&action_busy),
        action_sender.clone(),
    );
    install_cleanup_callback(
        &window,
        Arc::clone(&storage),
        Arc::clone(&action_busy),
        action_sender.clone(),
    );
    install_clear_callback(
        &window,
        Arc::clone(&storage),
        Arc::clone(&action_busy),
        action_sender,
    );

    refresh_timeline(&window, &storage, &ui_state)?;

    let weak = window.as_weak();
    let timer_storage = Arc::clone(&storage);
    let timer_state = Rc::clone(&ui_state);
    let timer_busy = Arc::clone(&action_busy);
    let last_refresh = Rc::new(RefCell::new(Instant::now()));
    let timer_last_refresh = Rc::clone(&last_refresh);
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(200), move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        while let Ok(status) = status_receiver.try_recv() {
            match status {
                CollectorStatus::Connected(message) => window.set_collector_status(message.into()),
                CollectorStatus::Failed(error) => {
                    if collector_reset_active(&data_directory) {
                        window.set_collector_status("正在安全暂停采集器…".into());
                    } else {
                        window.set_collector_status(format!("握手失败：{error}").into());
                    }
                }
            }
        }
        let mut refresh_requested = false;
        while let Ok(status) = action_receiver.try_recv() {
            window.set_action_status(status.message.into());
            refresh_requested |= status.refresh;
        }
        if !timer_busy.load(Ordering::Acquire)
            && (refresh_requested
                || timer_last_refresh.borrow().elapsed() >= Duration::from_secs(2))
        {
            match refresh_timeline(&window, &timer_storage, &timer_state) {
                Ok(()) => *timer_last_refresh.borrow_mut() = Instant::now(),
                Err(error) => window.set_action_status(format!("刷新失败：{error}").into()),
            }
        }
    });
    window.run()?;
    drop(timer);
    Ok(())
}

fn ingest_if_not_resetting(
    storage: &Arc<Mutex<Storage>>,
    data_directory: &Path,
    batch: &timelens_ipc::EventBatch,
) -> timelens_storage::Result<()> {
    if collector_reset_active(data_directory) {
        return Err(timelens_storage::StorageError::Integrity(
            "collector delivery is paused for clear all".to_owned(),
        ));
    }
    storage
        .lock()
        .map_err(|_| timelens_storage::StorageError::Integrity("storage lock poisoned".to_owned()))?
        .ingest_event_batch(batch)
        .map(|_| ())
}

fn collector_reset_active(data_directory: &Path) -> bool {
    data_directory.join(COLLECTOR_RESET_REQUEST_FILE).exists()
        || data_directory.join(COLLECTOR_RESET_PAUSED_FILE).exists()
}

fn refresh_timeline(
    window: &AppWindow,
    storage: &Arc<Mutex<Storage>>,
    ui_state: &Rc<RefCell<UiState>>,
) -> Result<()> {
    let (range_started, range_ended) = {
        let mut state = ui_state.borrow_mut();
        if state.follow_now {
            let horizon_duration = state
                .horizon_ended_utc_ms
                .saturating_sub(state.horizon_started_utc_ms);
            state.horizon_ended_utc_ms = unix_time_ms();
            state.horizon_started_utc_ms =
                state.horizon_ended_utc_ms.saturating_sub(horizon_duration);
            state.range_started_utc_ms = state.horizon_started_utc_ms;
            state.range_ended_utc_ms = state.horizon_ended_utc_ms;
        }
        (state.range_started_utc_ms, state.range_ended_utc_ms)
    };
    let (snapshot, policy) = {
        let storage = storage
            .lock()
            .map_err(|_| anyhow::anyhow!("storage lock poisoned"))?;
        (
            storage.timeline_snapshot(range_started, range_ended)?,
            storage.retention_policy()?,
        )
    };
    render_snapshot(window, ui_state, snapshot, policy);
    Ok(())
}

fn render_snapshot(
    window: &AppWindow,
    ui_state: &Rc<RefCell<UiState>>,
    snapshot: TimelineSnapshot,
    policy: RetentionPolicy,
) {
    let range_ms = snapshot
        .range_ended_utc_ms
        .saturating_sub(snapshot.range_started_utc_ms)
        .max(1) as f64;
    let mut state = ui_state.borrow_mut();
    if state.selected_identity.as_ref().is_none_or(|selected| {
        !snapshot
            .applications
            .iter()
            .any(|application| &application.identity == selected)
    }) {
        state.selected_identity = snapshot
            .applications
            .first()
            .map(|application| application.identity.clone());
    }
    state.application_identities = snapshot
        .applications
        .iter()
        .map(|application| application.identity.clone())
        .collect();
    let selected_index = state
        .selected_identity
        .as_ref()
        .and_then(|identity| {
            snapshot
                .applications
                .iter()
                .position(|application| &application.identity == identity)
        })
        .map_or(-1, |index| index as i32);
    let horizon_span = state
        .horizon_ended_utc_ms
        .saturating_sub(state.horizon_started_utc_ms)
        .max(1) as f64;
    let selection_left = (state
        .range_started_utc_ms
        .saturating_sub(state.horizon_started_utc_ms) as f64
        / horizon_span)
        .clamp(0.0, 1.0) as f32;
    let selection_right = (state
        .range_ended_utc_ms
        .saturating_sub(state.horizon_started_utc_ms) as f64
        / horizon_span)
        .clamp(0.0, 1.0) as f32;
    drop(state);

    let app_rows = snapshot
        .applications
        .iter()
        .map(|application| AppRow {
            name: application.display_name.clone().into(),
            summary: format!(
                "{} · {} 个窗口",
                format_duration(application.opened_ms),
                application.window_count
            )
            .into(),
            open_ratio: ratio(application.opened_ms, range_ms),
            display_ratio: ratio(application.displayed_ms, range_ms),
            focus_ratio: ratio(application.focused_ms, range_ms),
        })
        .collect::<Vec<_>>();
    window.set_apps(ModelRc::new(VecModel::from(app_rows)));
    window.set_selected_index(selected_index);
    window.set_selection_left(selection_left);
    window.set_selection_right(selection_right);
    window.set_range_label(
        format!(
            "选中 {} · {} 个应用",
            format_duration(range_ms as u64),
            snapshot.applications.len()
        )
        .into(),
    );
    window.set_coverage_value(if snapshot.monitoring_gaps.is_empty() {
        "数据完整".into()
    } else {
        format!("存在 {} 个明确缺口", snapshot.monitoring_gaps.len()).into()
    });
    window.set_retention_value(retention_label(policy).into());

    let selected = (selected_index >= 0).then(|| &snapshot.applications[selected_index as usize]);
    render_application(window, selected, &snapshot);
}

fn render_application(
    window: &AppWindow,
    application: Option<&TimelineApplication>,
    snapshot: &TimelineSnapshot,
) {
    let Some(application) = application else {
        window.set_selected_name("尚无应用数据".into());
        window.set_opened_value("0 秒".into());
        window.set_displayed_value("0 秒".into());
        window.set_focused_value("0 秒".into());
        window.set_background_value("0 秒".into());
        window.set_input_value("键盘 0 · 鼠标 0".into());
        window.set_windows(ModelRc::new(VecModel::<WindowRow>::default()));
        window.set_segments(ModelRc::new(VecModel::<SegmentRow>::default()));
        return;
    };
    window.set_selected_name(application.display_name.clone().into());
    window.set_opened_value(format_duration(application.opened_ms).into());
    window.set_displayed_value(format_duration(application.displayed_ms).into());
    window.set_focused_value(format_duration(application.focused_ms).into());
    window.set_background_value(format_duration(application.background_ms).into());
    window.set_input_value(
        format!(
            "键盘 {} · 鼠标 {}",
            application.keyboard_count,
            application
                .left_click_count
                .saturating_add(application.middle_click_count)
                .saturating_add(application.right_click_count)
        )
        .into(),
    );
    let windows = application
        .windows
        .iter()
        .map(|item| WindowRow {
            label: format!("窗口 {}", item.number).into(),
            summary: format!(
                "打开 {} · 显示 {} · 聚焦 {} · 后台 {}",
                format_duration(item.opened_ms),
                format_duration(item.displayed_ms),
                format_duration(item.focused_ms),
                format_duration(item.background_ms)
            )
            .into(),
        })
        .collect::<Vec<_>>();
    window.set_windows(ModelRc::new(VecModel::from(windows)));

    let span = snapshot
        .range_ended_utc_ms
        .saturating_sub(snapshot.range_started_utc_ms)
        .max(1) as f64;
    let segments = application
        .segments
        .iter()
        .map(|segment| SegmentRow {
            offset: ((segment
                .started_utc_ms
                .saturating_sub(snapshot.range_started_utc_ms) as f64
                / span)
                .clamp(0.0, 1.0)) as f32,
            width: ((segment.ended_utc_ms.saturating_sub(segment.started_utc_ms) as f64 / span)
                .clamp(0.0, 1.0)) as f32,
            kind: if segment.focused {
                2
            } else if segment.displayed {
                1
            } else {
                0
            },
        })
        .collect::<Vec<_>>();
    window.set_segments(ModelRc::new(VecModel::from(segments)));
}

fn install_retention_callback(
    window: &AppWindow,
    storage: Arc<Mutex<Storage>>,
    busy: Arc<AtomicBool>,
    sender: mpsc::Sender<ActionStatus>,
) {
    window.on_retention_selected(move |days| {
        if busy.swap(true, Ordering::AcqRel) {
            return;
        }
        let storage = Arc::clone(&storage);
        let busy = Arc::clone(&busy);
        let sender = sender.clone();
        thread::spawn(move || {
            let result = (|| -> Result<String> {
                let storage = storage
                    .lock()
                    .map_err(|_| anyhow::anyhow!("storage lock poisoned"))?;
                let mut policy = storage.retention_policy()?;
                policy.days = (days > 0).then_some(days as u32);
                storage.set_retention_policy(policy)?;
                let report = storage.apply_retention(unix_time_ms())?;
                Ok(format!(
                    "保留策略已更新；清理活动 {} 项、输入 {} 项",
                    report.activity_items, report.input_items
                ))
            })();
            busy.store(false, Ordering::Release);
            let _ = sender.send(ActionStatus {
                message: result.unwrap_or_else(|error| format!("保留策略失败：{error}")),
                refresh: true,
            });
        });
    });
}

fn install_cleanup_callback(
    window: &AppWindow,
    storage: Arc<Mutex<Storage>>,
    busy: Arc<AtomicBool>,
    sender: mpsc::Sender<ActionStatus>,
) {
    window.on_run_cleanup(move || {
        if busy.swap(true, Ordering::AcqRel) {
            return;
        }
        let storage = Arc::clone(&storage);
        let busy = Arc::clone(&busy);
        let sender = sender.clone();
        thread::spawn(move || {
            let result = storage
                .lock()
                .map_err(|_| anyhow::anyhow!("storage lock poisoned"))
                .and_then(|storage| storage.apply_retention(unix_time_ms()).map_err(Into::into));
            busy.store(false, Ordering::Release);
            let message = match result {
                Ok(report) => format!(
                    "清理完成：活动 {} 项、输入 {} 项、释放 {}",
                    report.activity_items,
                    report.input_items,
                    format_bytes(report.released_bytes)
                ),
                Err(error) => format!("清理失败：{error}"),
            };
            let _ = sender.send(ActionStatus {
                message,
                refresh: true,
            });
        });
    });
}

fn install_clear_callback(
    window: &AppWindow,
    storage: Arc<Mutex<Storage>>,
    busy: Arc<AtomicBool>,
    sender: mpsc::Sender<ActionStatus>,
) {
    window.on_clear_all(move || {
        if busy.swap(true, Ordering::AcqRel) {
            return;
        }
        let storage = Arc::clone(&storage);
        let busy = Arc::clone(&busy);
        let sender = sender.clone();
        let _ = sender.send(ActionStatus {
            message: "正在暂停采集器并轮换全部数据密钥…".to_owned(),
            refresh: false,
        });
        thread::spawn(move || {
            let result = (|| -> Result<String> {
                let storage = storage
                    .lock()
                    .map_err(|_| anyhow::anyhow!("storage lock poisoned"))?;
                let report = storage.clear_all_coordinated(Duration::from_secs(12))?;
                storage.record_component_health("core", PROTOCOL_VERSION, None)?;
                Ok(format!(
                    "已清空 {} 行本地数据并轮换数据密钥",
                    report.deleted_rows
                ))
            })();
            busy.store(false, Ordering::Release);
            let _ = sender.send(ActionStatus {
                message: result.unwrap_or_else(|error| format!("清空失败：{error}")),
                refresh: true,
            });
        });
    });
}

fn ratio(value_ms: u64, range_ms: f64) -> f32 {
    (value_ms as f64 / range_ms).clamp(0.0, 1.0) as f32
}

fn format_duration(milliseconds: u64) -> String {
    let seconds = milliseconds / 1_000;
    let hours = seconds / 3_600;
    let minutes = seconds % 3_600 / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours} 小时 {minutes} 分")
    } else if minutes > 0 {
        format!("{minutes} 分 {seconds} 秒")
    } else {
        format!("{seconds} 秒")
    }
}

fn retention_label(policy: RetentionPolicy) -> String {
    let time = policy
        .days
        .map_or_else(|| "永久".to_owned(), |days| format!("{days} 天"));
    format!("{time} · {}", format_bytes(policy.max_bytes))
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn unix_time_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

#[derive(Default)]
struct Options {
    background: bool,
    handshake_once: bool,
    pipe_name: Option<String>,
    data_directory: Option<PathBuf>,
}

impl Options {
    fn parse(arguments: impl IntoIterator<Item = std::ffi::OsString>) -> Result<Self> {
        let mut options = Self::default();
        let mut arguments = arguments.into_iter();
        while let Some(argument) = arguments.next() {
            match argument.to_string_lossy().as_ref() {
                "--background" => options.background = true,
                "--handshake-once" => options.handshake_once = true,
                "--pipe-name" => {
                    options.pipe_name = Some(
                        arguments
                            .next()
                            .context("--pipe-name requires a value")?
                            .to_string_lossy()
                            .into_owned(),
                    );
                }
                "--data-dir" => {
                    options.data_directory = Some(PathBuf::from(
                        arguments.next().context("--data-dir requires a value")?,
                    ));
                }
                unknown => bail!("unknown argument: {unknown}"),
            }
        }
        Ok(options)
    }

    fn data_directory(&self) -> Result<PathBuf> {
        if let Some(path) = &self.data_directory {
            return Ok(path.clone());
        }
        if let Some(path) = env::var_os("TIMELENS_DATA_DIR") {
            return Ok(PathBuf::from(path));
        }
        let local_app_data = env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is unavailable")?;
        Ok(PathBuf::from(local_app_data).join("Timelens"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_ui_state() -> UiState {
        UiState {
            horizon_started_utc_ms: 1_000,
            horizon_ended_utc_ms: 11_000,
            range_started_utc_ms: 1_000,
            range_ended_utc_ms: 11_000,
            selected_identity: None,
            application_identities: Vec::new(),
            follow_now: true,
        }
    }

    #[test]
    fn middle_drag_range_normalizes_reverse_direction_and_stops_following_now() {
        let mut state = fixed_ui_state();
        assert!(state.select_fraction_range(0.8, 0.2));
        assert_eq!(state.range_started_utc_ms, 3_000);
        assert_eq!(state.range_ended_utc_ms, 9_000);
        assert!(!state.follow_now);
    }

    #[test]
    fn middle_drag_range_rejects_clicks_and_clamps_outside_the_axis() {
        let mut state = fixed_ui_state();
        assert!(!state.select_fraction_range(0.5, 0.500_5));
        assert_eq!(
            (state.range_started_utc_ms, state.range_ended_utc_ms),
            (1_000, 11_000)
        );
        assert!(state.follow_now);
        assert!(state.select_fraction_range(-1.0, 2.0));
        assert_eq!(
            (state.range_started_utc_ms, state.range_ended_utc_ms),
            (1_000, 11_000)
        );
        assert!(!state.follow_now);
    }
}
