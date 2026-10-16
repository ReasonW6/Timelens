#![cfg(windows)]
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ai;
mod ai_ui;
mod app_icon;
mod appearance;
mod collection_ui;
mod collector_task;
mod data_ui;
mod general_ui;
mod history;
mod icon_cache;
mod local_config;
mod markdown;
mod pause;
#[cfg(debug_assertions)]
mod qa;
mod recovery;
mod snapshot;
mod snapshot_grid;
mod startup;
mod timeline_ui;
mod timeline_view;
mod tray;
mod ui_model;
mod ui_refresh;
mod window_chrome;
mod window_placement;

use std::{
    cell::RefCell,
    env,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc, Mutex, MutexGuard, TryLockError,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use chrono::TimeZone;
use slint::{Image, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};
use timelens_ipc::{
    BatchRejection, COLLECTOR_RESET_PAUSED_FILE, COLLECTOR_RESET_REQUEST_FILE, PROTOCOL_VERSION,
    SingleInstanceGuard, current_pipe_name, run_server_collector_message, run_server_probe,
    trust_peer_directory,
};
use timelens_storage::{
    ExportFormat, LocalReport, RetentionPolicy, SnapshotMissingReason, SnapshotPolicy,
    SnapshotSlot, Storage, TimelineApplication, TimelineSnapshot,
};

const COLLECTOR_NAMES: &[&str] = &["timelens-collector.exe", "Timelens.Collector.exe"];

slint::slint! {
    export { AppWindow, AppRow, WindowRow, SegmentRow, SnapshotRow, ReportAppRow,
             HistoryItem, DayAnchor, Participant, ShareSlice, StackApp, AiState, DataState,
             CollectionState, KeyCell, ButtonHeat, SystemRow, AppIcons, Theme }
        from "ui/main-window.slint";
    export { AiHistoryRow, AiConnectionRow, MdBlock } from "ui/ai-panel.slint";
}
fn main() -> Result<()> {
    let options = Options::parse(env::args_os().skip(1))?;
    timelens_ai::credentials::require_ordinary_privilege().map_err(anyhow::Error::msg)?;
    if options.shutdown {
        tray::activate_existing(true);
        return Ok(());
    }
    // The installer runs this once as the signed-in user, before the core's task
    // first starts, to place a new dataset in the directory chosen at setup.
    if let Some(destination) = &options.initial_data_location {
        if options.data_directory.is_some() || env::var_os("TIMELENS_DATA_DIR").is_some() {
            bail!("--set-data-location only applies to the default dataset");
        }
        let _instance = SingleInstanceGuard::acquire_core()?;
        let control = options.control_directory()?;
        if !local_config::adopt_initial(&control, destination)? {
            println!("existing Timelens data keeps its location");
        }
        return Ok(());
    }
    if options.uninstall_data {
        let _instance = SingleInstanceGuard::acquire_core()?;
        let _collector =
            SingleInstanceGuard::acquire_collector().context("删除数据前必须先停止采集器")?;
        let data = options.data_directory()?;
        let control = options.control_directory()?;
        if options.data_directory.is_some() {
            for id in Storage::dataset_credential_ids(&data)? {
                timelens_ai::credentials::delete(&id).map_err(anyhow::Error::msg)?;
            }
        } else {
            timelens_ai::credentials::delete_all().map_err(anyhow::Error::msg)?;
        }
        Storage::delete_owned_dataset(&data)?;
        if data != control {
            Storage::delete_owned_dataset(&control)?;
        }
        local_config::delete_pointer(&control)?;
        return Ok(());
    }
    let waiting_since = std::time::Instant::now();
    let _instance = loop {
        match SingleInstanceGuard::acquire_core() {
            Ok(instance) => break instance,
            Err(_) if options.recover && waiting_since.elapsed() < Duration::from_secs(30) => {
                thread::sleep(Duration::from_millis(100));
            }
            Err(error) => {
                // The running core may still be starting, for example right after
                // install or logon; give its tray window a moment to appear.
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    if tray::activate_existing(false) {
                        return Ok(());
                    }
                    if Instant::now() >= deadline {
                        break;
                    }
                    thread::sleep(Duration::from_millis(200));
                }
                return Err(error)
                    .context("Timelens is already running, but its window is not ready");
            }
        }
    };
    let control_directory = options.control_directory()?;
    // The singleton proves no previous core maintenance operation remains alive.
    match std::fs::remove_file(control_directory.join(COLLECTOR_RESET_REQUEST_FILE)) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let mut storage = recovery::open(
        options.data_directory(),
        &control_directory,
        options.recover,
    )?;
    storage.set_control_directory(&control_directory)?;
    let data_directory = storage.data_directory().to_path_buf();
    let retention = storage
        .apply_retention(unix_time_ms())
        .context("failed to apply the configured retention policy")?;
    if retention.activity_items > 0
        || retention.input_items > 0
        || retention.snapshot_items > 0
        || retention.report_items > 0
    {
        println!(
            "retention cleanup complete: snapshot_items={} report_items={} activity_items={} input_items={} released_bytes={}",
            retention.snapshot_items,
            retention.report_items,
            retention.activity_items,
            retention.input_items,
            retention.released_bytes
        );
    }
    storage.record_component_health("core", PROTOCOL_VERSION, None)?;
    let collector_task = options.collector_task();
    if let Some(task) = &collector_task {
        match collector_task::registered_directory(task) {
            Ok(Some(directory)) => {
                if let Err(error) = trust_peer_directory(&directory) {
                    eprintln!(
                        "collector directory {} is unavailable: {error}",
                        directory.display()
                    );
                }
            }
            Ok(None) => {}
            Err(error) => eprintln!("collector task {task} could not be read: {error}"),
        }
    }
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

    let storage = Arc::new(Mutex::new(storage));
    if options.snapshot_once {
        let summary = snapshot::capture_now(&storage, &data_directory)?;
        println!("snapshot capture complete: {}", capture_message(&summary));
        return Ok(());
    }
    snapshot::spawn_scheduler(Arc::clone(&storage), data_directory.clone());
    if let Some(task) = &collector_task {
        collector_task::spawn_watchdog(task.clone());
    }
    let ai_service = ai::spawn(Arc::clone(&storage));
    let maintenance_storage = Arc::clone(&storage);
    thread::spawn(move || {
        loop {
            let _ = snapshot::run_heavy_task(|| {
                let s = maintenance_storage
                    .lock()
                    .map_err(|_| anyhow::anyhow!("storage lock poisoned"))?;
                s.periodic_integrity_check(unix_time_ms())?;
                s.apply_retention(unix_time_ms())?;
                Ok(())
            });
            thread::sleep(Duration::from_secs(60));
        }
    });

    run_window(
        storage,
        data_directory,
        pipe_name,
        ai_service,
        options.background,
        collector_task,
    )
}

enum CollectorStatus {
    Connected(String),
    Failed(String),
}

struct ActionStatus {
    message: String,
    refresh: bool,
    report: Option<std::result::Result<i64, String>>,
}

#[derive(Default)]
struct SnapshotUiState {
    slots: Vec<SnapshotSlot>,
    selected_slot_id: Option<i64>,
    /// Images ticked to delete, and the one ticked last, where a Shift span starts.
    checked: std::collections::BTreeSet<i64>,
    anchor: Option<i64>,
    /// The first and last snapshot the grid shows.
    visible: Option<(usize, usize)>,
    /// For each listed slot, the stretch of identical omissions it stands for:
    /// first and last slot start, and how many.
    spans: Vec<(i64, i64, usize)>,
    /// The range listed, to leave selecting when it changes.
    range: Option<(i64, i64)>,
    /// A slot to select once listed, such as one just captured.
    pending_select: Option<i64>,
}

struct UiState {
    horizon_started_utc_ms: i64,
    horizon_ended_utc_ms: i64,
    range_started_utc_ms: i64,
    range_ended_utc_ms: i64,
    selected_identity: Option<String>,
    application_identities: Vec<String>,
    follow_now: bool,
    calendar_day: Option<i64>,
    apps: AppsState,
    /// The range the pages had before the timeline pointed them at an
    /// activity, and that activity in words.
    origin: Option<RangeOrigin>,
    /// The local report on screen: its id and range.
    report: Option<(i64, i64, i64)>,
    /// The range a stored report was last looked up for.
    report_checked: Option<(i64, i64)>,
}

#[derive(Clone)]
struct RangeOrigin {
    label: String,
    horizon_started_utc_ms: i64,
    horizon_ended_utc_ms: i64,
    range_started_utc_ms: i64,
    range_ended_utc_ms: i64,
    selected_identity: Option<String>,
    follow_now: bool,
    calendar_day: Option<i64>,
}

impl RangeOrigin {
    fn of(state: &UiState) -> Self {
        Self {
            label: String::new(),
            horizon_started_utc_ms: state.horizon_started_utc_ms,
            horizon_ended_utc_ms: state.horizon_ended_utc_ms,
            range_started_utc_ms: state.range_started_utc_ms,
            range_ended_utc_ms: state.range_ended_utc_ms,
            selected_identity: state.selected_identity.clone(),
            follow_now: state.follow_now,
            calendar_day: state.calendar_day,
        }
    }

    fn restore(self, state: &mut UiState) {
        state.horizon_started_utc_ms = self.horizon_started_utc_ms;
        state.horizon_ended_utc_ms = self.horizon_ended_utc_ms;
        state.range_started_utc_ms = self.range_started_utc_ms;
        state.range_ended_utc_ms = self.range_ended_utc_ms;
        state.selected_identity = self.selected_identity;
        state.follow_now = self.follow_now;
        state.calendar_day = self.calendar_day;
    }
}

/// What the application totals cover: all records, the shared range by its
/// start and planned end, or a recent stretch by its length, which moves with
/// the clock.
#[derive(Clone, Copy, PartialEq, Debug)]
enum AppsScope {
    All,
    Range(i64, i64),
    Recent(i64),
}

/// The application totals, gathered off the UI thread: all records can take a
/// while to total, and storage stays locked meanwhile.
#[derive(Default)]
struct AppsState {
    /// When the totals were last requested and for which scope; `None`
    /// requests them again.
    requested: Option<(AppsScope, Instant)>,
    pending: Option<(AppsScope, mpsc::Receiver<Result<Arc<TimelineSnapshot>>>)>,
    /// The totals on screen, kept to show another selection without a reload.
    shown: Option<(AppsScope, Arc<TimelineSnapshot>)>,
    /// The history generation these totals belong to; a merge changes it.
    generation: u64,
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
            calendar_day: None,
            apps: AppsState::default(),
            origin: None,
            report: None,
            report_checked: None,
        }
    }
}

fn run_window(
    storage: Arc<Mutex<Storage>>,
    data_directory: PathBuf,
    pipe_name: String,
    ai_service: ai::AiService,
    background: bool,
    collector_task: Option<String>,
) -> Result<()> {
    let window = AppWindow::new()?;
    window.global::<AppIcons>().on_resize(app_icon::for_display);
    // A debug-only viewport override keeps native visual QA reproducible.
    #[cfg(debug_assertions)]
    if let Ok(viewport) = std::env::var("TIMELENS_QA_VIEWPORT")
        && let Some((width, height)) = viewport
            .split_once('x')
            .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)))
        && (960..=3840).contains(&width)
        && (600..=2160).contains(&height)
    {
        window
            .window()
            .set_size(slint::LogicalSize::new(width as f32, height as f32));
    }
    // The collector control directory never moves, so the UI thread can read it
    // without taking the storage lock.
    let (storage_status, control_directory) = {
        let storage = storage
            .lock()
            .map_err(|_| anyhow::anyhow!("storage lock poisoned"))?;
        (
            format!(
                "SQLCipher {} · schema v{}",
                storage.cipher_version(),
                storage.schema_version()?
            ),
            storage.control_directory().to_path_buf(),
        )
    };
    // Before the first frame, so the window opens in the chosen look.
    let _appearance_timer = appearance::install(&window, control_directory.clone());
    let _pause_timer = pause::install(&window, control_directory.clone());
    window.set_storage_status(storage_status.into());
    window.set_collector_status("等待受信采集器事件…".into());
    window.set_data_path(data_directory.display().to_string().into());
    window.set_build_version(env!("CARGO_PKG_VERSION").into());

    let (status_sender, status_receiver) = mpsc::channel();
    let server_storage = Arc::clone(&storage);
    let server_data_directory = data_directory.clone();
    thread::spawn(move || {
        loop {
            let status = match run_server_collector_message(&pipe_name, COLLECTOR_NAMES, |batch| {
                ingest_if_not_resetting(&server_storage, &server_data_directory, batch)
                    .map_err(batch_rejection)
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
            // Recreate the pipe at once after a delivered batch so a backlog drains
            // quickly; pause only after a failure to avoid spinning.
            let failed = matches!(status, CollectorStatus::Failed(_));
            if status_sender.send(status).is_err() {
                break;
            }
            if failed {
                thread::sleep(Duration::from_millis(250));
            }
        }
    });

    let ui_state = Rc::new(RefCell::new(timeline_ui::today_state()));
    let snapshot_ui_state = Rc::new(RefCell::new(SnapshotUiState::default()));
    let action_busy = Arc::new(AtomicBool::new(false));
    let (action_sender, action_receiver) = mpsc::channel::<ActionStatus>();

    timeline_ui::install(
        &window,
        Arc::clone(&storage),
        Rc::clone(&ui_state),
        Arc::clone(&action_busy),
    );

    window.on_status_kind(|message| status_kind(&message));
    window.on_status_path(|message| {
        status_path(&message)
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default()
            .into()
    });
    window.on_open_folder(|path| {
        // Explorer opens the folder with the exported file selected.
        let _ = std::process::Command::new("explorer.exe")
            .arg(format!("/select,{path}"))
            .spawn();
    });

    {
        let weak = window.as_weak();
        let ui_state = Rc::clone(&ui_state);
        let action_busy = Arc::clone(&action_busy);
        window.on_app_selected(move |index| {
            if action_busy.load(Ordering::Acquire) || index < 0 {
                return;
            }
            let mut state = ui_state.borrow_mut();
            state.selected_identity = state.application_identities.get(index as usize).cloned();
            let shown = state.apps.shown.clone();
            drop(state);
            if let Some((scope, snapshot)) = shown
                && let Some(window) = weak.upgrade()
            {
                render_apps(&window, &ui_state, &snapshot, scope == AppsScope::All);
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
    let history_state = Rc::new(RefCell::new(history::HistoryState::new()));
    history::install(
        &window,
        Arc::clone(&storage),
        Rc::clone(&ui_state),
        Rc::clone(&history_state),
    );

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
        action_sender.clone(),
    );
    install_snapshot_callbacks(
        &window,
        Arc::clone(&storage),
        data_directory.clone(),
        Rc::clone(&ui_state),
        Rc::clone(&snapshot_ui_state),
        Arc::clone(&action_busy),
        action_sender.clone(),
    );
    install_report_callback(
        &window,
        Arc::clone(&storage),
        Rc::clone(&ui_state),
        Arc::clone(&action_busy),
        action_sender,
    );

    refresh_timeline(&window, &storage, &ui_state)?;
    history::refresh(&window, &storage, &history_state)?;

    let _ai_timer = ai_ui::install(
        &window,
        Arc::clone(&storage),
        Rc::clone(&ui_state),
        ai_service,
    )?;
    let _data_timer = data_ui::install(
        &window,
        Arc::clone(&storage),
        Rc::clone(&ui_state),
        Rc::clone(&snapshot_ui_state),
    );
    let _tray_timer = tray::install(&window, Arc::clone(&storage), control_directory.clone())?;
    let _general_timer = general_ui::install(&window, collector_task);
    let _collection_timer =
        collection_ui::install(&window, Arc::clone(&storage), Rc::clone(&ui_state))?;

    let weak = window.as_weak();
    let timer_storage = Arc::clone(&storage);
    let timer_state = Rc::clone(&ui_state);
    let timer_snapshot_state = Rc::clone(&snapshot_ui_state);
    let timer_busy = Arc::clone(&action_busy);
    let timer_history = Rc::clone(&history_state);
    let mut ui_refresh = ui_refresh::UiRefresh::new(Instant::now());
    let mut pending_report = None;
    let timer = Timer::default();
    let mut recovery_shown = false;
    let mut data_generation = ai_ui::data_generation();
    let started = Instant::now();
    let mut collector_seen: Option<Instant> = None;
    // The native window exists only once shown, possibly later from the tray.
    let mut chrome_applied = false;
    timer.start(TimerMode::Repeated, Duration::from_millis(200), move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        if !chrome_applied && window.window().is_visible() {
            chrome_applied =
                window_chrome::apply(window.window(), window.global::<Theme>().get_dark());
        }
        // Data and collection actions report through their own status; show it
        // in the toast wherever the action was started.
        // Taking the message lets the same one show again next time.
        let data = window.global::<DataState>();
        let collection = window.global::<CollectionState>();
        for status in [data.get_status(), collection.get_status()] {
            if !status.is_empty() {
                window.set_action_status(status);
            }
        }
        data.set_status("".into());
        collection.set_status("".into());
        window.set_action_busy(timer_busy.load(Ordering::Acquire));
        let ui_active = window.window().is_visible() && !window.window().is_minimized();
        if ui_active {
            request_apps(&window, &timer_storage, &timer_state);
            if let Err(error) = poll_apps(&window, &timer_state) {
                window.set_action_status(format!("读取应用统计失败：{error}").into());
            }
            let state = timer_state.borrow();
            let scope = apps_scope(&window, &state);
            // Only a scope not shown yet reads as loading; the periodic refresh
            // of the one on screen keeps it as it is instead of dimming it.
            window.set_apps_loading(
                state
                    .apps
                    .shown
                    .as_ref()
                    .is_none_or(|(shown, _)| *shown != scope),
            );
        }
        window.global::<AiState>().set_open(
            ui_active
                && (window.get_page() == 4
                    || (window.get_settings_open() && window.get_settings_tab() == 4)),
        );
        if !recovery_shown && timer_storage.try_lock().is_ok_and(|s| s.is_quarantined()) {
            recovery_shown = true;
            window.global::<DataState>().set_open(true);
            window.set_settings_open(true);
            window.set_settings_tab(3);
            window.global::<DataState>().set_recovery_needed(true);
            window.global::<DataState>().set_status(
                "发现数据损坏，已停止写入。请进入恢复界面，保留原件并恢复到新目录。".into(),
            );
        }
        while let Ok(status) = status_receiver.try_recv() {
            match status {
                CollectorStatus::Connected(message) => {
                    collector_seen = Some(Instant::now());
                    window.set_collector_status(message.into());
                }
                CollectorStatus::Failed(error) => {
                    if collector_reset_active(&control_directory) {
                        window.set_collector_status("正在安全暂停采集器…".into());
                    } else {
                        window.set_collector_status(format!("握手失败：{error}").into());
                    }
                }
            }
        }
        window.set_collector_state(collector_state(
            started.elapsed(),
            collector_seen.map(|seen| seen.elapsed()),
            collector_reset_active(&control_directory),
        ));
        let mut refresh_requested = false;
        if ai_ui::data_generation() != data_generation {
            data_generation = ai_ui::data_generation();
            *timer_snapshot_state.borrow_mut() = SnapshotUiState::default();
            clear_snapshot_preview(&window);
            window.set_snapshot_selected_index(-1);
            window.set_snapshot_detail("数据已变更，请重新选择快照".into());
            window.set_report_apps(ModelRc::new(VecModel::<ReportAppRow>::default()));
            window.set_report_ready(false);
            window.set_report_summary("数据已变更，请重新生成本地报告".into());
            timer_state.borrow_mut().report = None;
            timer_state.borrow_mut().report_checked = None;
            window.global::<DataState>().set_clear_credentials(false);
            history::reset(&timer_history);
            timer_state.borrow_mut().apps = AppsState::default();
            pending_report = None;
            refresh_requested = true;
        }
        while let Ok(status) = action_receiver.try_recv() {
            window.set_action_status(status.message.into());
            if status.refresh {
                // Cleaning or clearing may have removed earlier days' records.
                history::reset(&timer_history);
                timer_state.borrow_mut().apps = AppsState::default();
            }
            refresh_requested |= status.refresh;
            if let Some(report) = status.report {
                match report {
                    Ok(report_id) => {
                        pending_report = Some(report_id);
                    }
                    Err(error) => {
                        window.set_report_summary(format!("报告生成失败：{error}").into())
                    }
                }
            }
        }
        ui_refresh.request(refresh_requested || pending_report.is_some());
        if ui_refresh.due(
            ui_active,
            timer_busy.load(Ordering::Acquire),
            Instant::now(),
        ) {
            match refresh_timeline(&window, &timer_storage, &timer_state).and_then(|refreshed| {
                if refreshed && window.get_snapshot_open() {
                    refresh_snapshot_panel(
                        &window,
                        &timer_storage,
                        &timer_state,
                        &timer_snapshot_state,
                    )?;
                }
                if refreshed {
                    history::refresh(&window, &timer_storage, &timer_history)?;
                }
                if refreshed && window.get_page() == 3 && window.get_stats_tab() == 2 {
                    find_report(&window, &timer_storage, &timer_state)?;
                }
                Ok(refreshed)
            }) {
                Ok(true) => {
                    if let Some(report_id) = pending_report.take()
                        && let Err(error) =
                            refresh_report(&window, &timer_storage, &timer_state, report_id)
                    {
                        window.set_report_summary(format!("报告读取失败：{error}").into());
                    }
                    ui_refresh.refreshed(Instant::now());
                }
                // Storage is busy with a long task; try again on a later tick.
                Ok(false) => {}
                Err(error) => window.set_action_status(format!("刷新失败：{error}").into()),
            }
        }
    });
    if !background {
        window.show()?;
        window_placement::fit_after_show(&window);
    }
    let _repaint_timer = window_chrome::repaint_after_restore(&window);
    #[cfg(debug_assertions)]
    let _qa_timer = qa::install(&window);
    slint::run_event_loop_until_quit()?;
    drop(timer);
    Ok(())
}

fn ingest_if_not_resetting(
    storage: &Arc<Mutex<Storage>>,
    _data_directory: &Path,
    batch: &timelens_ipc::EventBatch,
) -> timelens_storage::Result<()> {
    let storage = storage.lock().map_err(|_| {
        timelens_storage::StorageError::Integrity("storage lock poisoned".to_owned())
    })?;
    if collector_reset_active(storage.control_directory()) {
        return Err(timelens_storage::StorageError::Integrity(
            "collector delivery is paused for clear all".to_owned(),
        ));
    }
    storage.ingest_event_batch(batch).map(|_| ())
}

/// A batch that contradicts durable state can never be stored by resending it, so
/// the collector is told to abandon that run instead of retrying it forever.
fn batch_rejection(error: timelens_storage::StorageError) -> BatchRejection {
    if error.contradicts_durable_state() {
        BatchRejection::permanent(error.to_string())
    } else {
        BatchRejection::transient(error.to_string())
    }
}

/// A healthy collector delivers a batch or heartbeat at least every 30 seconds.
const COLLECTOR_SILENCE: Duration = Duration::from_secs(90);

/// 0: delivering, 1: still waiting for the first delivery, 2: silent. A
/// maintenance pause is intentional, so it never reads as a failure.
fn collector_state(
    since_start: Duration,
    since_delivery: Option<Duration>,
    resetting: bool,
) -> i32 {
    match since_delivery {
        _ if resetting => 0,
        Some(elapsed) if elapsed < COLLECTOR_SILENCE => 0,
        None if since_start < COLLECTOR_SILENCE => 1,
        _ => 2,
    }
}

fn collector_reset_active(data_directory: &Path) -> bool {
    data_directory.join(COLLECTOR_RESET_REQUEST_FILE).exists()
        || data_directory.join(COLLECTOR_RESET_PAUSED_FILE).exists()
}

/// Lock storage from the UI thread without waiting. `None` means a long task such
/// as a backup, relocation or retention cleanup holds it; the caller skips this
/// round instead of freezing the window.
fn try_lock_storage(storage: &Arc<Mutex<Storage>>) -> Result<Option<MutexGuard<'_, Storage>>> {
    match storage.try_lock() {
        Ok(guard) => Ok(Some(guard)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Poisoned(_)) => bail!("storage lock poisoned"),
    }
}

/// Returns `false` without changing the view when storage is busy.
fn refresh_timeline(
    window: &AppWindow,
    storage: &Arc<Mutex<Storage>>,
    ui_state: &Rc<RefCell<UiState>>,
) -> Result<bool> {
    let (range_started, range_ended) = {
        let mut state = ui_state.borrow_mut();
        if state.follow_now && state.calendar_day.is_some() {
            state.range_ended_utc_ms = unix_time_ms().min(state.horizon_ended_utc_ms);
            state.follow_now = state.range_ended_utc_ms < state.horizon_ended_utc_ms;
        } else if state.follow_now {
            let horizon_duration = state
                .horizon_ended_utc_ms
                .saturating_sub(state.horizon_started_utc_ms);
            state.horizon_ended_utc_ms = unix_time_ms();
            state.horizon_started_utc_ms =
                state.horizon_ended_utc_ms.saturating_sub(horizon_duration);
            state.range_started_utc_ms = state.horizon_started_utc_ms;
            state.range_ended_utc_ms = state.horizon_ended_utc_ms;
        }
        (
            state.range_started_utc_ms,
            state.range_ended_utc_ms.min(unix_time_ms()),
        )
    };
    let (snapshot, policy, paused, icon_sources, used, earliest) = {
        let Some(storage) = try_lock_storage(storage)? else {
            return Ok(false);
        };
        (
            storage.timeline_snapshot(range_started, range_ended)?,
            storage.retention_policy()?,
            storage.collection_policy()?.paused,
            storage.application_icon_sources()?,
            storage.retained_bytes()?,
            storage.earliest_activity_utc_ms()?,
        )
    };
    app_icon::register_sources(icon_sources);
    window.set_retention_used_mib(i32::try_from(used.div_ceil(1024 * 1024)).unwrap_or(i32::MAX));
    window.set_retention_usage(
        match earliest.and_then(|ms| chrono::Local.timestamp_millis_opt(ms).single()) {
            Some(first) => format!(
                "已用 {} · 最早可回看到 {}",
                format_bytes(used),
                first.format("%Y-%m-%d")
            ),
            None => format!("已用 {} · 还没有活动记录", format_bytes(used)),
        }
        .into(),
    );
    window.global::<CollectionState>().set_paused(paused);
    render_snapshot(window, ui_state, snapshot, policy);
    Ok(true)
}

/// How often the application totals are gathered while their list is on screen:
/// often while the range runs with the clock, rarely otherwise.
const LIVE_REFRESH: Duration = Duration::from_secs(2);
const SETTLED_REFRESH: Duration = Duration::from_secs(30);

/// The scope the statistics page asks for.
fn apps_scope(window: &AppWindow, state: &UiState) -> AppsScope {
    if window.get_stats_all() {
        AppsScope::All
    } else if state.follow_now && state.calendar_day.is_none() {
        AppsScope::Recent(
            state
                .horizon_ended_utc_ms
                .saturating_sub(state.range_started_utc_ms),
        )
    } else if state.follow_now {
        AppsScope::Range(state.range_started_utc_ms, state.horizon_ended_utc_ms)
    } else {
        AppsScope::Range(state.range_started_utc_ms, state.range_ended_utc_ms)
    }
}

/// Start gathering the application totals in the background when their list is
/// on screen and due; `poll_apps` shows them once ready.
fn request_apps(
    window: &AppWindow,
    storage: &Arc<Mutex<Storage>>,
    ui_state: &Rc<RefCell<UiState>>,
) {
    let visible = window.window().is_visible()
        && !window.window().is_minimized()
        && window.get_page() == 3
        && window.get_stats_tab() == 0;
    let mut state = ui_state.borrow_mut();
    let scope = apps_scope(window, &state);
    let live = state.follow_now;
    let apps = &mut state.apps;
    if apps.generation != history::generation() {
        // Applications were merged or split: drop what was gathered before.
        *apps = AppsState {
            generation: history::generation(),
            ..AppsState::default()
        };
    }
    // A recent stretch is gathered afresh each time, so it refreshes rarely.
    let interval = if matches!(scope, AppsScope::Range(..)) && live {
        LIVE_REFRESH
    } else {
        SETTLED_REFRESH
    };
    let due = apps
        .requested
        .is_none_or(|(was, at)| was != scope || at.elapsed() >= interval);
    if !visible || !due || apps.pending.is_some() {
        return;
    }
    apps.requested = Some((scope, Instant::now()));
    let (sender, receiver) = mpsc::channel();
    apps.pending = Some((scope, receiver));
    let storage = Arc::clone(storage);
    thread::spawn(move || {
        let now = unix_time_ms();
        let result = storage
            .lock()
            .map_err(|_| anyhow::anyhow!("storage lock poisoned"))
            .and_then(|storage| {
                Ok(match scope {
                    AppsScope::All => Arc::new(storage.timeline_snapshot(1, now)?),
                    AppsScope::Range(start, end) => {
                        storage.timeline_snapshot_cached(start, end.min(now).max(start + 1))?
                    }
                    AppsScope::Recent(length) => {
                        Arc::new(storage.timeline_snapshot(now.saturating_sub(length).max(1), now)?)
                    }
                })
            });
        let _ = sender.send(result);
    });
}

/// Show gathered application totals, unless the scope changed meanwhile.
fn poll_apps(window: &AppWindow, ui_state: &Rc<RefCell<UiState>>) -> Result<()> {
    let mut state = ui_state.borrow_mut();
    let Some((scope, receiver)) = &state.apps.pending else {
        return Ok(());
    };
    let scope = *scope;
    let result = match receiver.try_recv() {
        Ok(result) => result,
        Err(mpsc::TryRecvError::Empty) => return Ok(()),
        Err(mpsc::TryRecvError::Disconnected) => {
            state.apps.pending = None;
            return Ok(());
        }
    };
    state.apps.pending = None;
    let snapshot = result?;
    if scope != apps_scope(window, &state) {
        return Ok(());
    }
    state.apps.shown = Some((scope, snapshot.clone()));
    drop(state);
    render_apps(window, ui_state, &snapshot, scope == AppsScope::All);
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
        .max(1);
    window.set_range_label(
        format!(
            "选中 {} · {} 个应用",
            format_duration(range_ms as u64),
            snapshot.applications.len()
        )
        .into(),
    );
    window.set_coverage_value(if snapshot.monitoring_gaps.is_empty() {
        "未记录清理或中断".into()
    } else {
        format!("存在 {} 个明确缺口", snapshot.monitoring_gaps.len()).into()
    });
    window.set_retention_value(retention_label(policy).into());
    window.set_retention_mib(i32::try_from(policy.max_bytes / (1024 * 1024)).unwrap_or(i32::MAX));
    window.set_retention_days(policy.days.unwrap_or(0) as i32);
    timeline_ui::render(window, ui_state, &snapshot);
    report_freshness(window, &ui_state.borrow());
}

/// The application totals: everything recorded, or the chosen range.
fn render_apps(
    window: &AppWindow,
    ui_state: &Rc<RefCell<UiState>>,
    snapshot: &TimelineSnapshot,
    all: bool,
) {
    // All history starts with the first record, not at the query's lower bound.
    let range_start = if all {
        snapshot
            .applications
            .iter()
            .flat_map(|application| &application.segments)
            .map(|segment| segment.started_utc_ms)
            .min()
            .map_or(snapshot.range_started_utc_ms, |earliest| {
                earliest.max(snapshot.range_started_utc_ms)
            })
    } else {
        snapshot.range_started_utc_ms
    };
    let range_ms = snapshot
        .range_ended_utc_ms
        .saturating_sub(range_start)
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
    drop(state);

    // Shares are of all focus in the range, so the list reads like a breakdown.
    let focus_sum = snapshot
        .applications
        .iter()
        .map(|application| application.focused_ms)
        .fold(0_u64, u64::saturating_add)
        .max(1) as f64;
    let app_rows = snapshot
        .applications
        .iter()
        .enumerate()
        .map(|(index, application)| AppRow {
            identity: application.identity.clone().into(),
            original_index: index as i32,
            name: app_icon::display_name(&application.identity, &application.display_name).into(),
            icon: app_icon::for_identity(&application.identity),
            tint: app_icon::tint(&application.identity),
            focused: format_duration(application.focused_ms).into(),
            summary: format!(
                "打开 {} · 显示 {} · {} 个窗口",
                format_duration(application.opened_ms),
                format_duration(application.displayed_ms),
                application.window_count
            )
            .into(),
            open_ratio: ratio(application.opened_ms, range_ms),
            display_ratio: ratio(application.displayed_ms, range_ms),
            focus_ratio: ratio(application.focused_ms, focus_sum),
        })
        .collect::<Vec<_>>();
    window.set_range_keyboard(grouped_count(snapshot.keyboard_count).into());
    window.set_range_mouse(
        grouped_count(
            snapshot
                .left_click_count
                .saturating_add(snapshot.middle_click_count)
                .saturating_add(snapshot.right_click_count),
        )
        .into(),
    );
    window.set_focus_total(format_duration(timeline_view::total_focus_ms(snapshot)).into());
    window.set_apps_range(timeline_ui::apps_range_label(snapshot, range_start, all).into());
    window.set_apps(ui_model::sync(window.get_apps(), app_rows, |a, b| {
        a.identity == b.identity
    }));
    timeline_ui::filter_apps(window);
    window.set_selected_index(selected_index);

    let selected = (selected_index >= 0).then(|| &snapshot.applications[selected_index as usize]);
    render_application(window, selected, snapshot, range_start);
}

/// A moment in words: the time alone today, with the date otherwise.
fn moment_label(timestamp: i64) -> String {
    let Some(value) = chrono::Local.timestamp_millis_opt(timestamp).single() else {
        return "时间不可用".to_owned();
    };
    if value.date_naive() == chrono::Local::now().date_naive() {
        value.format("%H:%M").to_string()
    } else {
        snapshot_time_label(timestamp)
    }
}

/// Windows shown before the list is expanded.
const WINDOWS_SHOWN: usize = 8;

/// Show one application's totals; its activity bar spans `range_start` to the
/// end of the snapshot.
fn render_application(
    window: &AppWindow,
    application: Option<&TimelineApplication>,
    snapshot: &TimelineSnapshot,
    range_start: i64,
) {
    let Some(application) = application else {
        window.set_selected_name("尚无应用数据".into());
        window.set_opened_value("0 秒".into());
        window.set_displayed_value("0 秒".into());
        window.set_focused_value("0 秒".into());
        window.set_background_value("0 秒".into());
        window.set_input_value("键盘 0 · 鼠标 0".into());
        window.set_selected_icon(Image::default());
        window.set_keyboard_value("0 次".into());
        window.set_mouse_value("0 次".into());
        window.set_windows(ModelRc::new(VecModel::<WindowRow>::default()));
        window.set_windows_total(0);
        window.set_segments(ModelRc::new(VecModel::<SegmentRow>::default()));
        return;
    };
    window.set_selected_name(
        app_icon::display_name(&application.identity, &application.display_name).into(),
    );
    window.set_selected_tint(app_icon::tint(&application.identity));
    window.set_selected_icon(app_icon::for_identity(&application.identity));
    let clicks = application
        .left_click_count
        .saturating_add(application.middle_click_count)
        .saturating_add(application.right_click_count);
    window.set_keyboard_value(format!("{} 次", grouped_count(application.keyboard_count)).into());
    window.set_mouse_value(format!("{} 次", grouped_count(clicks)).into());
    window.set_opened_value(format_duration(application.opened_ms).into());
    window.set_displayed_value(format_duration(application.displayed_ms).into());
    window.set_focused_value(format_duration(application.focused_ms).into());
    window.set_background_value(format_duration(application.background_ms).into());
    window.set_input_value(
        format!(
            "键盘 {} · 鼠标 {}",
            grouped_count(application.keyboard_count),
            grouped_count(clicks)
        )
        .into(),
    );
    // Windows are told apart by when they were open; the range clips both ends.
    let now = unix_time_ms();
    let range_end = snapshot.range_ended_utc_ms;
    let mut windows = application.windows.iter().collect::<Vec<_>>();
    windows.sort_by(|a, b| {
        b.focused_ms
            .cmp(&a.focused_ms)
            .then(a.number.cmp(&b.number))
    });
    window.set_windows_total(windows.len() as i32);
    if !window.get_windows_expanded() {
        windows.truncate(WINDOWS_SHOWN);
    }
    let windows = windows
        .into_iter()
        .map(|item| {
            let opened = if item.opened_utc_ms <= snapshot.range_started_utc_ms {
                "更早".to_owned()
            } else {
                moment_label(item.opened_utc_ms)
            };
            let ended = if item.ended_utc_ms < range_end {
                moment_label(item.ended_utc_ms)
            } else if range_end >= now - 60_000 {
                "仍打开".to_owned()
            } else {
                "之后".to_owned()
            };
            WindowRow {
                identity: format!("{}:{}", application.identity, item.number).into(),
                label: format!("{opened} – {ended}").into(),
                focused: format_duration(item.focused_ms).into(),
                summary: format!(
                    "打开 {} · 显示 {} · 后台 {}",
                    format_duration(item.opened_ms),
                    format_duration(item.displayed_ms),
                    format_duration(item.background_ms)
                )
                .into(),
            }
        })
        .collect::<Vec<_>>();
    window.set_windows(ui_model::sync(window.get_windows(), windows, |a, b| {
        a.identity == b.identity
    }));

    let span = snapshot
        .range_ended_utc_ms
        .saturating_sub(range_start)
        .max(1) as f64;
    window.set_segments_start(moment_label(range_start).into());
    window.set_segments_end(
        if range_end >= now - 60_000 {
            "现在".to_owned()
        } else {
            moment_label(range_end)
        }
        .into(),
    );
    let segments = application
        .segments
        .iter()
        .map(|segment| SegmentRow {
            offset: ((segment.started_utc_ms.saturating_sub(range_start) as f64 / span)
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
            tint: app_icon::tint(&application.identity),
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
    window.on_retention_cutoff(|days| {
        let cutoff = chrono::Local::now() - chrono::Duration::days(i64::from(days.max(0)));
        cutoff.format("%Y年%-m月%-d日").to_string().into()
    });
    {
        let storage = Arc::clone(&storage);
        let busy = Arc::clone(&busy);
        let sender = sender.clone();
        window.on_retention_space_selected(move |mib| {
            if mib <= 0 || busy.swap(true, Ordering::AcqRel) {
                return;
            }
            let storage = Arc::clone(&storage);
            let busy = Arc::clone(&busy);
            let sender = sender.clone();
            thread::spawn(move || {
                let result = snapshot::run_heavy_task(|| -> Result<String> {
                    let storage = storage
                        .lock()
                        .map_err(|_| anyhow::anyhow!("storage lock poisoned"))?;
                    let mut policy = storage.retention_policy()?;
                    policy.max_bytes = u64::try_from(mib)? * 1024 * 1024;
                    storage.set_retention_policy(policy)?;
                    let report = storage.apply_retention(unix_time_ms())?;
                    Ok(
                        if report.activity_items + report.input_items + report.report_items == 0 {
                            format!("空间上限已改为 {}", format_bytes(policy.max_bytes))
                        } else {
                            format!(
                                "空间上限已改为 {}；清理活动 {} 项、输入 {} 项、报告 {} 项",
                                format_bytes(policy.max_bytes),
                                report.activity_items,
                                report.input_items,
                                report.report_items
                            )
                        },
                    )
                });
                busy.store(false, Ordering::Release);
                let _ = sender.send(ActionStatus {
                    message: result.unwrap_or_else(|error| format!("空间上限保存失败：{error}")),
                    refresh: true,
                    report: None,
                });
            });
        });
    }
    window.on_retention_selected(move |days| {
        if busy.swap(true, Ordering::AcqRel) {
            return;
        }
        let storage = Arc::clone(&storage);
        let busy = Arc::clone(&busy);
        let sender = sender.clone();
        thread::spawn(move || {
            let result = snapshot::run_heavy_task(|| -> Result<String> {
                let storage = storage
                    .lock()
                    .map_err(|_| anyhow::anyhow!("storage lock poisoned"))?;
                let mut policy = storage.retention_policy()?;
                policy.days = (days > 0).then_some(days as u32);
                storage.set_retention_policy(policy)?;
                let report = storage.apply_retention(unix_time_ms())?;
                Ok(format!(
                    "保留策略已更新；清理快照 {} 项、报告 {} 项、活动 {} 项、输入 {} 项",
                    report.snapshot_items,
                    report.report_items,
                    report.activity_items,
                    report.input_items
                ))
            });
            busy.store(false, Ordering::Release);
            let _ = sender.send(ActionStatus {
                message: result.unwrap_or_else(|error| format!("保留策略失败：{error}")),
                refresh: true,
                report: None,
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
            let result = snapshot::run_heavy_task(|| {
                storage
                    .lock()
                    .map_err(|_| anyhow::anyhow!("storage lock poisoned"))
                    .and_then(|storage| storage.apply_retention(unix_time_ms()).map_err(Into::into))
            });
            busy.store(false, Ordering::Release);
            let message = match result {
                Ok(report) => format!(
                    "清理完成：快照 {} 项、报告 {} 项、活动 {} 项、输入 {} 项、释放 {}",
                    report.snapshot_items,
                    report.report_items,
                    report.activity_items,
                    report.input_items,
                    format_bytes(report.released_bytes)
                ),
                Err(error) => format!("清理失败：{error}"),
            };
            let _ = sender.send(ActionStatus {
                message,
                refresh: true,
                report: None,
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
    let weak = window.as_weak();
    window.on_clear_all(move || {
        if busy.swap(true, Ordering::AcqRel) {
            return;
        }
        let storage = Arc::clone(&storage);
        let busy = Arc::clone(&busy);
        let sender = sender.clone();
        let delete_credentials = weak
            .upgrade()
            .is_some_and(|w| w.global::<DataState>().get_clear_credentials());
        let _ = sender.send(ActionStatus {
            message: "正在暂停采集器并轮换全部数据密钥…".to_owned(),
            refresh: false,
            report: None,
        });
        thread::spawn(move || {
            let result = (|| -> Result<String> {
                let _ai = ai::pause_and_wait()?;
                snapshot::run_heavy_task(|| -> Result<String> {
                    let storage = storage
                        .lock()
                        .map_err(|_| anyhow::anyhow!("storage lock poisoned"))?;
                    let report = storage.clear_all_coordinated(Duration::from_secs(12))?;
                    ai_ui::dataset_changed();
                    let mut credential_failures = 0;
                    if delete_credentials {
                        for mut profile in storage.ai_profiles()? {
                            if timelens_ai::credentials::delete(&profile.id).is_err() {
                                credential_failures += 1;
                            }
                            profile.tested_revision = None;
                            profile.credential_revision =
                                profile.credential_revision.saturating_add(1);
                            storage.save_ai_profile(&profile)?;
                        }
                    }
                    storage.record_component_health("core", PROTOCOL_VERSION, None)?;
                    Ok(format!(
                        "已清空 {} 行本地数据并轮换数据密钥{}",
                        report.deleted_rows,
                        if credential_failures > 0 {
                            format!("；有 {credential_failures} 项凭据未能删除，请检查提供商页")
                        } else if delete_credentials {
                            "；当前提供商凭据已删除".into()
                        } else {
                            String::new()
                        }
                    ))
                })
            })();
            busy.store(false, Ordering::Release);
            let _ = sender.send(ActionStatus {
                message: result.unwrap_or_else(|error| format!("清空失败：{error}")),
                refresh: true,
                report: None,
            });
        });
    });
}

fn install_snapshot_callbacks(
    window: &AppWindow,
    storage: Arc<Mutex<Storage>>,
    data_directory: PathBuf,
    ui_state: Rc<RefCell<UiState>>,
    snapshot_state: Rc<RefCell<SnapshotUiState>>,
    busy: Arc<AtomicBool>,
    sender: mpsc::Sender<ActionStatus>,
) {
    {
        let weak = window.as_weak();
        let storage = Arc::clone(&storage);
        let ui_state = Rc::clone(&ui_state);
        let snapshot_state = Rc::clone(&snapshot_state);
        window.on_snapshot_opened(move || {
            if let Some(window) = weak.upgrade()
                && let Err(error) =
                    refresh_snapshot_panel(&window, &storage, &ui_state, &snapshot_state)
            {
                window.set_action_status(format!("快照列表刷新失败：{error}").into());
            }
        });
    }
    {
        let storage = Arc::clone(&storage);
        let snapshot_state = Rc::clone(&snapshot_state);
        let weak_preview = window.as_weak();
        window.on_snapshot_row_selected(move |index| {
            let weak = weak_preview.clone();
            if index < 0 {
                return;
            }
            let slot = snapshot_state.borrow().slots.get(index as usize).cloned();
            let Some(slot) = slot else {
                return;
            };
            snapshot_state.borrow_mut().selected_slot_id = Some(slot.id);
            let Some(window) = weak.upgrade() else {
                return;
            };
            window.set_snapshot_selected_index(index);
            snapshot_grid::show_neighbours(&window, &snapshot_state.borrow(), index);
            window.set_snapshot_deletable(slot.success);
            window.set_snapshot_selected_time(
                snapshot_time_label(slot.captured_at_utc_ms.unwrap_or(slot.slot_started_utc_ms))
                    .into(),
            );
            let sequence = PREVIEW_SEQUENCE.fetch_add(1, Ordering::AcqRel) + 1;
            if !slot.success {
                let reason = slot
                    .missing_reason
                    .map_or("原因未知", snapshot_reason_label);
                let span = snapshot_state.borrow().spans.get(index as usize).copied();
                window.set_snapshot_preview(Image::default());
                window.set_snapshot_detail(
                    match span {
                        Some((first, last, count)) if count > 1 => format!(
                            "{} 期间的 {count} 个时段没有拍摄：{reason}",
                            span_label(first, last)
                        ),
                        _ => format!("这个时段没有拍摄：{reason}"),
                    }
                    .into(),
                );
                return;
            }
            window.set_snapshot_detail("正在打开…".into());
            // The image is read and decoded off the UI thread; a later
            // selection makes this one moot.
            let storage = Arc::clone(&storage);
            thread::spawn(move || {
                let pixels = storage
                    .lock()
                    .map_err(|_| anyhow::anyhow!("storage lock poisoned"))
                    .and_then(|storage| Ok(storage.load_snapshot_image(slot.id)?))
                    .and_then(|image| {
                        snapshot::decode_preview_pixels(
                            image.webp,
                            slot.pixel_width,
                            slot.pixel_height,
                        )
                    })
                    .map_err(|error| error.to_string());
                let detail = format!(
                    "{} × {} · {}",
                    slot.pixel_width,
                    slot.pixel_height,
                    format_bytes(slot.plaintext_bytes)
                );
                let _ = weak.upgrade_in_event_loop(move |window| {
                    if PREVIEW_SEQUENCE.load(Ordering::Acquire) != sequence {
                        return;
                    }
                    match pixels {
                        Ok(pixels) => {
                            window.set_snapshot_preview(Image::from_rgba8(pixels));
                            window.set_snapshot_detail(detail.into());
                        }
                        Err(error) => {
                            window.set_snapshot_preview(Image::default());
                            window.set_snapshot_detail(format!("预览失败：{error}").into());
                        }
                    }
                });
            });
        });
    }
    {
        let weak = window.as_weak();
        let storage = Arc::clone(&storage);
        let ui_state = Rc::clone(&ui_state);
        let snapshot_state = Rc::clone(&snapshot_state);
        window.on_snapshot_delete_selected(move || {
            let Some(slot_id) = snapshot_state.borrow().selected_slot_id else {
                return;
            };
            let mut postponed = None;
            let result = storage
                .lock()
                .map_err(|_| anyhow::anyhow!("storage lock poisoned"))
                .and_then(|storage| match storage.delete_snapshot(slot_id) {
                    // The slot is deleted; only scrubbing its old data waits for retention.
                    Err(timelens_storage::StorageError::SnapshotCleanupPending(error)) => {
                        postponed = Some(error);
                        Ok(true)
                    }
                    result => result.map_err(Into::into),
                });
            if let Some(window) = weak.upgrade() {
                match result {
                    Ok(true) => {
                        let next = {
                            let state = snapshot_state.borrow();
                            let from = state.slots.iter().position(|slot| slot.id == slot_id);
                            from.and_then(|from| {
                                let from = from as i32;
                                snapshot_grid::neighbour(&state, from, 1)
                                    .or_else(|| snapshot_grid::neighbour(&state, from, -1))
                            })
                            .map(|index| state.slots[index].id)
                        };
                        snapshot_state.borrow_mut().selected_slot_id = next;
                        // The old image stays until the neighbour's decodes, so an
                        // open viewer moves on instead of closing.
                        if next.is_none() {
                            clear_snapshot_preview(&window);
                        }
                        window.set_action_status(match postponed {
                            Some(error) => {
                                format!("已删除所选快照；旧加密数据稍后清理：{error}").into()
                            }
                            None => "已删除所选快照".into(),
                        });
                        if let Err(error) =
                            refresh_snapshot_panel(&window, &storage, &ui_state, &snapshot_state)
                        {
                            window.set_action_status(format!("列表刷新失败：{error}").into());
                        }
                        let index = window.get_snapshot_selected_index();
                        if index >= 0 {
                            window.invoke_snapshot_row_selected(index);
                        }
                    }
                    Ok(false) => window.set_action_status("所选记录没有可删除的图片".into()),
                    Err(error) => window.set_action_status(format!("快照删除失败：{error}").into()),
                }
            }
        });
    }
    {
        // The viewer's previous and next: the nearest snapshot with an image.
        let weak = window.as_weak();
        let snapshot_state = Rc::clone(&snapshot_state);
        window.on_snapshot_step(move |from, step| {
            let next = snapshot_grid::neighbour(&snapshot_state.borrow(), from, step);
            match (next, weak.upgrade()) {
                (Some(index), Some(window)) => {
                    window.invoke_snapshot_row_selected(index as i32);
                    true
                }
                _ => false,
            }
        });
    }
    {
        let weak = window.as_weak();
        let storage = Arc::clone(&storage);
        let snapshot_state = Rc::clone(&snapshot_state);
        window.on_snapshot_visible(move |first, last| {
            let mut state = snapshot_state.borrow_mut();
            state.visible = Some((first.max(0) as usize, last.max(0) as usize));
            if let Some(window) = weak.upgrade() {
                snapshot_grid::load_thumbnails(&window, &storage, &state);
            }
        });
    }
    {
        let weak = window.as_weak();
        let snapshot_state = Rc::clone(&snapshot_state);
        window.on_snapshot_check(move |index, span| {
            let mut state = snapshot_state.borrow_mut();
            if index >= 0 {
                snapshot_grid::tick(&mut state, index as usize, span);
            }
            if let Some(window) = weak.upgrade() {
                snapshot_grid::show_ticks(&window, &state);
            }
        });
    }
    {
        let weak = window.as_weak();
        let snapshot_state = Rc::clone(&snapshot_state);
        window.on_snapshot_check_all(move |all| {
            let mut state = snapshot_state.borrow_mut();
            snapshot_grid::tick_all(&mut state, all);
            if let Some(window) = weak.upgrade() {
                snapshot_grid::show_ticks(&window, &state);
            }
        });
    }
    {
        let weak = window.as_weak();
        let storage = Arc::clone(&storage);
        let snapshot_state = Rc::clone(&snapshot_state);
        let busy = Arc::clone(&busy);
        let sender = sender.clone();
        window.on_snapshot_delete_checked(move || {
            let Some(window) = weak.upgrade() else {
                return;
            };
            let mut state = snapshot_state.borrow_mut();
            let ids = state.checked.iter().copied().collect::<Vec<_>>();
            if ids.is_empty() || busy.swap(true, Ordering::AcqRel) {
                return;
            }
            if state
                .selected_slot_id
                .is_some_and(|id| state.checked.contains(&id))
            {
                state.selected_slot_id = None;
                window.set_snapshot_selected_index(-1);
                window.set_snapshot_deletable(false);
                clear_snapshot_preview(&window);
                window.set_snapshot_detail("从左侧选择一张快照预览".into());
            }
            snapshot_grid::tick_all(&mut state, false);
            snapshot_grid::show_ticks(&window, &state);
            window.set_snapshot_selecting(false);
            let storage = Arc::clone(&storage);
            let busy = Arc::clone(&busy);
            let sender = sender.clone();
            thread::spawn(move || {
                let result = snapshot::run_heavy_task(|| {
                    Ok(storage
                        .lock()
                        .map_err(|_| anyhow::anyhow!("storage lock poisoned"))?
                        .delete_snapshots(&ids)?)
                });
                busy.store(false, Ordering::Release);
                let message = match result {
                    Ok(deletion) => {
                        let mut message = format!(
                            "已删除 {} 张快照，释放 {}",
                            deletion.deleted,
                            format_bytes(deletion.released_bytes)
                        );
                        if let Some(error) = deletion.cleanup_pending {
                            message.push_str(&format!("；旧加密数据稍后清理：{error}"));
                        }
                        message
                    }
                    Err(error) => format!("批量删除失败：{error}"),
                };
                let _ = sender.send(ActionStatus {
                    message,
                    refresh: true,
                    report: None,
                });
            });
        });
    }
    {
        let weak = window.as_weak();
        let capture_storage = Arc::clone(&storage);
        let data_directory = data_directory.clone();
        let busy = Arc::clone(&busy);
        let restore = Rc::new(std::cell::Cell::new(false));
        let restore_after = Rc::clone(&restore);
        window.on_snapshot_capture_now(move || {
            let Some(window) = weak.upgrade() else {
                return;
            };
            if busy.swap(true, Ordering::AcqRel) {
                return;
            }
            // Timelens would otherwise photograph itself: step aside, let the
            // minimize animation finish, then capture.
            let shown = window.window().is_visible() && !window.window().is_minimized();
            restore.set(shown);
            if shown {
                window.window().set_minimized(true);
            }
            let storage = Arc::clone(&capture_storage);
            let data_directory = data_directory.clone();
            let busy = Arc::clone(&busy);
            let weak = window.as_weak();
            Timer::single_shot(
                Duration::from_millis(if shown { 450 } else { 0 }),
                move || {
                    thread::spawn(move || {
                        let result = snapshot::capture_now(&storage, &data_directory);
                        busy.store(false, Ordering::Release);
                        let (message, slot) = match result {
                            Ok(summary) => (capture_message(&summary), summary.slot_utc_ms),
                            Err(error) => (format!("拍摄失败：{error}"), 0),
                        };
                        let _ = weak.upgrade_in_event_loop(move |window| {
                            window
                                .invoke_snapshot_captured(message.into(), slot.to_string().into());
                        });
                    });
                },
            );
        });
        let weak = window.as_weak();
        let storage = Arc::clone(&storage);
        let ui_state = Rc::clone(&ui_state);
        let snapshot_state = Rc::clone(&snapshot_state);
        window.on_snapshot_captured(move |message, slot| {
            let Some(window) = weak.upgrade() else {
                return;
            };
            if restore_after.take() {
                let _ = window_chrome::show_and_activate(&window);
            } else if !window.window().is_visible() {
                // Taken from the tray: the hidden window cannot show the result.
                tray::show_message("Timelens 快照", message.to_string(), slot.as_str() != "0");
            }
            window.set_action_status(message);
            let Ok(slot) = slot.parse::<i64>() else {
                return;
            };
            if slot > 0 {
                // Show the new snapshot, in today's list if the range is elsewhere.
                let outside = {
                    let state = ui_state.borrow();
                    slot < state.range_started_utc_ms || slot >= chosen_range_end(&state)
                };
                if outside {
                    let mut next = timeline_ui::today_state();
                    next.selected_identity = ui_state.borrow().selected_identity.clone();
                    *ui_state.borrow_mut() = next;
                    if let Err(error) = refresh_timeline(&window, &storage, &ui_state) {
                        window.set_action_status(format!("刷新失败：{error}").into());
                    }
                }
                snapshot_state.borrow_mut().pending_select = Some(slot);
            }
            if let Err(error) =
                refresh_snapshot_panel(&window, &storage, &ui_state, &snapshot_state)
            {
                window.set_action_status(format!("快照列表刷新失败：{error}").into());
            }
        });
    }
    install_snapshot_policy_callbacks(
        window,
        Arc::clone(&storage),
        Rc::clone(&ui_state),
        Rc::clone(&snapshot_state),
    );
    {
        let weak = window.as_weak();
        let storage = Arc::clone(&storage);
        let ui_state = Rc::clone(&ui_state);
        let snapshot_state = Rc::clone(&snapshot_state);
        window.on_snapshot_toggle_exclusion(move || {
            let identity = ui_state.borrow().selected_identity.clone();
            let result = (|| -> Result<String> {
                let identity = identity.context("请先在主界面选择一个应用")?;
                let storage = storage
                    .lock()
                    .map_err(|_| anyhow::anyhow!("storage lock poisoned"))?;
                let excluded = storage
                    .snapshot_exclusions()?
                    .iter()
                    .any(|candidate| candidate == &identity);
                storage.set_snapshot_excluded(&identity, !excluded, unix_time_ms())?;
                Ok(if excluded {
                    "已从快照排除名单恢复所选应用".to_owned()
                } else {
                    "已将所选应用加入独立快照排除名单".to_owned()
                })
            })();
            if let Some(window) = weak.upgrade() {
                window.set_action_status(
                    result
                        .as_ref()
                        .map_or_else(|error| format!("排除设置失败：{error}"), Clone::clone)
                        .into(),
                );
                if result.is_ok()
                    && let Err(error) =
                        refresh_snapshot_panel(&window, &storage, &ui_state, &snapshot_state)
                {
                    window.set_action_status(format!("快照设置刷新失败：{error}").into());
                }
            }
        });
    }
}

/// What a capture did, in words.
/// How the toast treats a status: 0 a result that fades, 1 work still going,
/// 2 a failure that stays until dismissed.
fn status_kind(message: &str) -> i32 {
    const FAILURE: [&str; 6] = ["失败", "错误", "无法", "未能", "未完成", "请输入"];
    if FAILURE.iter().any(|word| message.contains(word)) {
        2
    } else if message.ends_with('…') {
        1
    } else {
        0
    }
}

/// The file an export status names, when it still exists.
fn status_path(message: &str) -> Option<PathBuf> {
    let (_, path) = message
        .split_once("导出到 ")
        .or_else(|| message.split_once("个文件到 "))?;
    let path = PathBuf::from(path.trim());
    path.exists().then_some(path)
}

fn capture_message(summary: &snapshot::CaptureSummary) -> String {
    let when = clock_label(summary.slot_utc_ms);
    let reason = summary.reason.map_or("原因未知", snapshot_reason_label);
    match (summary.stored, summary.missing) {
        (0, _) => format!("没有拍摄：{reason}"),
        (1, 0) => format!("已拍摄 {when} 的快照"),
        (stored, 0) => format!("已拍摄 {when} 的快照，共 {stored} 张"),
        (stored, missing) => {
            format!("已拍摄 {when} 的快照 {stored} 张；{missing} 块屏幕没有拍摄：{reason}")
        }
    }
}

/// Where the shared range ends for listing: its planned end while it runs.
fn chosen_range_end(state: &UiState) -> i64 {
    if state.follow_now {
        state.horizon_ended_utc_ms
    } else {
        state.range_ended_utc_ms
    }
}

fn install_snapshot_policy_callbacks(
    window: &AppWindow,
    storage: Arc<Mutex<Storage>>,
    ui_state: Rc<RefCell<UiState>>,
    snapshot_state: Rc<RefCell<SnapshotUiState>>,
) {
    {
        let weak = window.as_weak();
        let storage = Arc::clone(&storage);
        let ui_state = Rc::clone(&ui_state);
        let snapshot_state = Rc::clone(&snapshot_state);
        window.on_snapshot_toggle_enabled(move || {
            update_and_refresh_snapshot_policy(
                &weak,
                &storage,
                &ui_state,
                &snapshot_state,
                |policy| policy.enabled = !policy.enabled,
            );
        });
    }
    {
        let weak = window.as_weak();
        let storage = Arc::clone(&storage);
        let ui_state = Rc::clone(&ui_state);
        let snapshot_state = Rc::clone(&snapshot_state);
        window.on_snapshot_interval_selected(move |minutes| {
            update_and_refresh_snapshot_policy(
                &weak,
                &storage,
                &ui_state,
                &snapshot_state,
                |policy| policy.interval_minutes = minutes as u32,
            );
        });
    }
    {
        let weak = window.as_weak();
        let storage = Arc::clone(&storage);
        let ui_state = Rc::clone(&ui_state);
        let snapshot_state = Rc::clone(&snapshot_state);
        window.on_snapshot_toggle_target(move || {
            update_and_refresh_snapshot_policy(
                &weak,
                &storage,
                &ui_state,
                &snapshot_state,
                |policy| policy.capture_all_displays = !policy.capture_all_displays,
            );
        });
    }
    {
        let weak = window.as_weak();
        let storage = Arc::clone(&storage);
        let ui_state = Rc::clone(&ui_state);
        let snapshot_state = Rc::clone(&snapshot_state);
        window.on_snapshot_retention_selected(move |days| {
            update_and_refresh_snapshot_policy(
                &weak,
                &storage,
                &ui_state,
                &snapshot_state,
                |policy| {
                    policy.retention_days = (days > 0).then_some(days as u32);
                },
            );
        });
    }
}

fn update_and_refresh_snapshot_policy(
    weak: &slint::Weak<AppWindow>,
    storage: &Arc<Mutex<Storage>>,
    ui_state: &Rc<RefCell<UiState>>,
    snapshot_state: &Rc<RefCell<SnapshotUiState>>,
    update: impl FnOnce(&mut SnapshotPolicy),
) {
    let result = snapshot::run_heavy_task(|| -> Result<()> {
        let storage = storage
            .lock()
            .map_err(|_| anyhow::anyhow!("storage lock poisoned"))?;
        let mut policy = storage.snapshot_policy()?;
        update(&mut policy);
        storage.set_snapshot_policy(policy, unix_time_ms())?;
        snapshot::wake_scheduler();
        storage.apply_retention(unix_time_ms())?;
        Ok(())
    });
    if let Some(window) = weak.upgrade() {
        match result {
            Ok(()) => {
                window.set_action_status("快照策略已更新；固定周期从现在重新开始".into());
                if let Err(error) =
                    refresh_snapshot_panel(&window, storage, ui_state, snapshot_state)
                {
                    window.set_action_status(format!("快照设置刷新失败：{error}").into());
                }
            }
            Err(error) => window.set_action_status(format!("快照策略失败：{error}").into()),
        }
    }
}

fn refresh_snapshot_panel(
    window: &AppWindow,
    storage: &Arc<Mutex<Storage>>,
    ui_state: &Rc<RefCell<UiState>>,
    snapshot_state: &Rc<RefCell<SnapshotUiState>>,
) -> Result<bool> {
    let (range_started, range_ended, selected_identity) = {
        let state = ui_state.borrow();
        (
            state.range_started_utc_ms,
            state.range_ended_utc_ms,
            state.selected_identity.clone(),
        )
    };
    let (policy, exclusions, listed, (saved, unsaved)) = {
        let Some(storage) = try_lock_storage(storage)? else {
            return Ok(false);
        };
        (
            storage.snapshot_policy()?,
            storage.snapshot_exclusions()?,
            storage.list_snapshot_slots(range_started, range_ended, SNAPSHOTS_LISTED)?,
            storage.count_snapshot_slots(range_started, range_ended)?,
        )
    };
    let truncated = listed.len() >= SNAPSHOTS_LISTED;
    // Omissions repeat every interval while locked or asleep; a run of the
    // same reason on the same display is one tile. With several displays the
    // rows interleave by time, so each display keeps its own open run.
    let mut slots = Vec::<SnapshotSlot>::new();
    let mut spans = Vec::<(i64, i64, usize)>::new();
    let mut runs = std::collections::HashMap::<String, usize>::new();
    for slot in listed {
        let key = slot.display.key.clone();
        if !slot.success
            && let Some(&index) = runs.get(&key)
            && slots[index].missing_reason == slot.missing_reason
        {
            spans[index].0 = slot.slot_started_utc_ms;
            spans[index].2 += 1;
            continue;
        }
        if slot.success {
            runs.remove(&key);
        } else {
            runs.insert(key, slots.len());
        }
        spans.push((slot.slot_started_utc_ms, slot.slot_started_utc_ms, 1));
        slots.push(slot);
    }
    let displays = slots
        .iter()
        .filter(|slot| slot.success)
        .map(|slot| slot.display.key.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let display_numbers = displays
        .iter()
        .enumerate()
        .map(|(index, key)| (key.to_string(), index + 1))
        .collect::<std::collections::HashMap<_, _>>();
    let several_displays = display_numbers.len() > 1;
    {
        // A different range ends selecting; its ticks would no longer show.
        let mut state = snapshot_state.borrow_mut();
        if state
            .range
            .is_some_and(|range| range != (range_started, range_ended))
        {
            state.checked.clear();
            state.anchor = None;
            window.set_snapshot_selecting(false);
        }
        state.range = Some((range_started, range_ended));
    }
    snapshot_state
        .borrow_mut()
        .checked
        .retain(|id| slots.iter().any(|slot| slot.id == *id && slot.success));
    // Thumbnails already decoded stay; the grid asks for the rest it shows.
    let thumbs = window
        .get_snapshots()
        .iter()
        .filter(|row| row.thumb.size().width > 0)
        .map(|row| (row.identity, row.thumb))
        .collect::<std::collections::HashMap<_, _>>();
    // Missing slots stay listed with their reason, so an empty-looking range
    // still explains why nothing was saved.
    let rows = slots
        .iter()
        .zip(&spans)
        .map(|(slot, &(first, last, count))| SnapshotRow {
            identity: slot.id.to_string().into(),
            success: slot.success,
            thumb: thumbs
                .get(slot.id.to_string().as_str())
                .cloned()
                .unwrap_or_default(),
            checked: snapshot_state.borrow().checked.contains(&slot.id),
            time: if count > 1 {
                format!("{}–{}", moment_label(first), clock_label(last))
            } else {
                snapshot_time_label(slot.slot_started_utc_ms)
            }
            .into(),
            note: if slot.success {
                SharedString::new()
            } else {
                let reason = slot
                    .missing_reason
                    .map_or("原因未知", snapshot_reason_label);
                if count > 1 {
                    format!("{reason} · {count} 次").into()
                } else {
                    reason.into()
                }
            },
            badge: display_numbers
                .get(&slot.display.key)
                .filter(|_| several_displays)
                .map_or_else(SharedString::new, |number| format!("屏 {number}").into()),
            display: slot
                .display
                .key
                .trim_start_matches(r"\\.\")
                .to_owned()
                .into(),
            result: if slot.success {
                format!("已保存 · {}", format_bytes(slot.plaintext_bytes)).into()
            } else {
                format!(
                    "缺失 · {}",
                    slot.missing_reason
                        .map(snapshot_reason_label)
                        .unwrap_or("未知原因")
                )
                .into()
            },
        })
        .collect::<Vec<_>>();
    // A slot just captured is selected once it is listed.
    let pending = snapshot_state.borrow_mut().pending_select.take();
    if let Some(slot_ms) = pending {
        match slots
            .iter()
            .find(|slot| slot.slot_started_utc_ms == slot_ms)
        {
            Some(slot) => snapshot_state.borrow_mut().selected_slot_id = Some(slot.id),
            None => snapshot_state.borrow_mut().pending_select = Some(slot_ms),
        }
    }
    let selected_index = snapshot_state
        .borrow()
        .selected_slot_id
        .and_then(|id| slots.iter().position(|slot| slot.id == id))
        .map_or(-1, |index| index as i32);
    let newly_selected = pending.is_some() && selected_index >= 0;
    {
        let mut state = snapshot_state.borrow_mut();
        state.slots = slots;
        state.spans = spans;
        if selected_index < 0 {
            state.selected_slot_id = None;
        }
    }
    window.set_snapshot_summary(
        if saved == 0 && unsaved == 0 {
            "这段时间没有快照".to_owned()
        } else {
            let mut summary = format!("{} 张快照", grouped_count(saved));
            if unsaved > 0 {
                summary.push_str(&format!(" · {} 个时段没有拍摄", grouped_count(unsaved)));
            }
            if truncated {
                summary.push_str(&format!(" · 显示最近 {SNAPSHOTS_LISTED} 条"));
            }
            summary
        }
        .into(),
    );
    window.set_snapshot_truncated(truncated);
    window.set_snapshots(ui_model::sync(window.get_snapshots(), rows, |a, b| {
        a.identity == b.identity
    }));
    if snapshot_state.borrow().slots.is_empty() {
        window.set_snapshot_selecting(false);
    }
    window.set_snapshot_checked_count(snapshot_state.borrow().checked.len() as i32);
    snapshot_grid::load_thumbnails(window, storage, &snapshot_state.borrow());
    window.set_snapshot_selected_index(selected_index);
    snapshot_grid::show_neighbours(window, &snapshot_state.borrow(), selected_index);
    if selected_index < 0 {
        clear_snapshot_preview(window);
        window.set_snapshot_detail("从左侧选择一张快照预览".into());
    } else if newly_selected {
        window.invoke_snapshot_row_selected(selected_index);
    }
    window.set_snapshot_policy_label(snapshot_policy_label(policy).into());
    window.set_snapshot_enabled(policy.enabled);
    window.set_snapshot_interval_minutes(policy.interval_minutes as i32);
    window.set_snapshot_retention_days(policy.retention_days.unwrap_or(0) as i32);
    window.set_snapshot_capacity(format_bytes(policy.max_bytes).into());
    window.set_snapshot_all_displays(policy.capture_all_displays);
    window.set_snapshot_deletable(
        selected_index >= 0
            && snapshot_state
                .borrow()
                .slots
                .get(selected_index as usize)
                .is_some_and(|slot| slot.success),
    );
    let exclusion_label = match selected_identity {
        Some(identity) if exclusions.iter().any(|candidate| candidate == &identity) => {
            format!("{} · 当前已排除", window.get_selected_name())
        }
        Some(_) => format!("{} · 当前允许", window.get_selected_name()),
        None => "先在主界面选择应用".to_owned(),
    };
    window.set_snapshot_exclusion_label(exclusion_label.into());
    Ok(true)
}

/// Bumped on every snapshot selection, so a slower preview never replaces a
/// newer one.
static PREVIEW_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Clears the preview and voids any decode still under way, so a deleted or
/// no longer listed image cannot come back when it finishes.
fn clear_snapshot_preview(window: &AppWindow) {
    PREVIEW_SEQUENCE.fetch_add(1, Ordering::AcqRel);
    window.set_snapshot_preview(Image::default());
}

/// Slots listed at most; the list says when a range holds more.
const SNAPSHOTS_LISTED: usize = 500;

fn install_report_callback(
    window: &AppWindow,
    storage: Arc<Mutex<Storage>>,
    ui_state: Rc<RefCell<UiState>>,
    busy: Arc<AtomicBool>,
    sender: mpsc::Sender<ActionStatus>,
) {
    {
        let weak = window.as_weak();
        let storage = Arc::clone(&storage);
        let ui_state = Rc::clone(&ui_state);
        let busy = Arc::clone(&busy);
        let sender = sender.clone();
        window.on_report_export(move |format| {
            let Some((report_id, ..)) = ui_state.borrow().report else {
                return;
            };
            let extension = if format == 0 { "json" } else { "csv" };
            let path = match data_ui::choose_file(true, extension) {
                Ok(Some(path)) => path,
                Ok(None) => return,
                Err(error) => {
                    if let Some(window) = weak.upgrade() {
                        window.set_action_status(format!("无法选择位置：{error}").into());
                    }
                    return;
                }
            };
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
                    .and_then(|storage| {
                        storage
                            .export_report(
                                report_id,
                                &path,
                                if format == 0 {
                                    ExportFormat::Json
                                } else {
                                    ExportFormat::Csv
                                },
                                false,
                            )
                            .map_err(Into::into)
                    });
                busy.store(false, Ordering::Release);
                let message = match result {
                    Ok(()) => format!("报告已导出到 {}", path.display()),
                    Err(error) => format!("导出失败：{error}"),
                };
                let _ = sender.send(ActionStatus {
                    message,
                    refresh: false,
                    report: None,
                });
            });
        });
    }
    let weak = window.as_weak();
    window.on_report_generate(move || {
        if busy.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(window) = weak.upgrade() {
            window.set_report_ready(false);
            window.set_report_summary("正在根据当前时段的本地事实生成报告…".into());
            window.set_report_apps(ModelRc::new(VecModel::<ReportAppRow>::default()));
        }
        let (range_started, range_ended) = {
            let state = ui_state.borrow();
            (state.range_started_utc_ms, state.range_ended_utc_ms)
        };
        let storage = Arc::clone(&storage);
        let busy = Arc::clone(&busy);
        let sender = sender.clone();
        thread::spawn(move || {
            let result = snapshot::run_heavy_task(|| {
                storage
                    .lock()
                    .map_err(|_| anyhow::anyhow!("storage lock poisoned"))
                    .and_then(|storage| {
                        storage
                            .generate_local_report(range_started, range_ended, unix_time_ms())
                            .map_err(Into::into)
                    })
            });
            busy.store(false, Ordering::Release);
            let (message, report) = match result {
                Ok(report) => (
                    format!("本地报告 #{} 已生成", report.id),
                    Some(Ok(report.id)),
                ),
                Err(error) => {
                    let message = error.to_string();
                    (format!("报告生成失败：{message}"), Some(Err(message)))
                }
            };
            let _ = sender.send(ActionStatus {
                message,
                refresh: false,
                report,
            });
        });
    });
}

fn refresh_report(
    window: &AppWindow,
    storage: &Arc<Mutex<Storage>>,
    ui_state: &Rc<RefCell<UiState>>,
    report_id: i64,
) -> Result<()> {
    let report = storage
        .lock()
        .map_err(|_| anyhow::anyhow!("storage lock poisoned"))?
        .load_local_report(report_id)?;
    render_report(window, ui_state, &report);
    Ok(())
}

/// Whether a report's range is the one chosen: the same start and end, or for
/// a range that grows with the clock the same start; a recent stretch matches
/// a report of the same length.
fn report_matches(start: i64, end: i64, state: &UiState) -> bool {
    if state.follow_now && state.calendar_day.is_none() {
        // A sliding range moves on: the report stays current only while its
        // ends are within a few minutes of the range's.
        const SLACK_MS: i64 = 5 * 60_000;
        return (start - state.range_started_utc_ms).abs() <= SLACK_MS
            && (end - state.range_ended_utc_ms).abs() <= SLACK_MS;
    }
    start == state.range_started_utc_ms && (state.follow_now || end == state.range_ended_utc_ms)
}

/// Shows the latest stored report of the chosen range, when the one on
/// screen covers another; looked up once per range.
fn find_report(
    window: &AppWindow,
    storage: &Arc<Mutex<Storage>>,
    ui_state: &Rc<RefCell<UiState>>,
) -> Result<()> {
    let key = {
        let state = ui_state.borrow();
        if state.follow_now && state.calendar_day.is_none()
            || state
                .report
                .is_some_and(|(_, start, end)| report_matches(start, end, &state))
            || state.report_checked
                == Some((state.range_started_utc_ms, timeline_ui::chosen_end(&state)))
        {
            return Ok(());
        }
        (state.range_started_utc_ms, timeline_ui::chosen_end(&state))
    };
    let found = {
        let Some(storage) = try_lock_storage(storage)? else {
            return Ok(());
        };
        storage.latest_local_report_starting(key.0)?
    };
    ui_state.borrow_mut().report_checked = Some(key);
    if let Some(report) = found.filter(|report| {
        report_matches(
            report.range_started_utc_ms,
            report.range_ended_utc_ms,
            &ui_state.borrow(),
        )
    }) {
        render_report(window, ui_state, &report);
    }
    Ok(())
}

/// Says when the report on screen covers another range than the chosen one.
fn report_freshness(window: &AppWindow, state: &UiState) {
    let stale = state
        .report
        .filter(|(_, start, end)| !report_matches(*start, *end, state));
    window.set_report_stale(stale.is_some());
    if let Some((_, start, end)) = stale {
        window.set_report_stale_text(
            format!(
                "这份报告是 {} 的，当前选择的是 {}。",
                ai_ui::range_text(start, end, unix_time_ms(), &chrono::Local, true),
                window.get_range_title()
            )
            .into(),
        );
    }
}

/// What a gap in a report stands for.
fn gap_reason_label(reason: &str) -> &str {
    match reason {
        "unobserved" => "采集器没有运行记录",
        "collector_restart" => "采集器重启",
        "buffer_overflow" => "记录过多，部分丢失",
        "input_overflow" => "输入计数溢出",
        "clock_discontinuity" => "系统时间跳变",
        "retention_time" => "超过保留期，已清理",
        "retention_space" => "超过空间上限，已清理",
        "user_deleted" => "已手动删除",
        "recovery_corruption" => "损坏恢复时丢失",
        "global_pause" => "已暂停记录",
        "privacy_exclusion" => "不记录的应用",
        "session_disconnected" => "会话断开",
        "secure_desktop" => "安全桌面",
        _ => reason,
    }
}

const SNAPSHOT_REASONS: [SnapshotMissingReason; 14] = [
    SnapshotMissingReason::DesktopIdle,
    SnapshotMissingReason::GlobalPause,
    SnapshotMissingReason::Locked,
    SnapshotMissingReason::Sleep,
    SnapshotMissingReason::SecureDesktop,
    SnapshotMissingReason::SessionDisconnected,
    SnapshotMissingReason::RemoteSession,
    SnapshotMissingReason::PrivacyExclusion,
    SnapshotMissingReason::CaptureFailed,
    SnapshotMissingReason::LowDisk,
    SnapshotMissingReason::RetentionCleaned,
    SnapshotMissingReason::UserDeleted,
    SnapshotMissingReason::BackupOmitted,
    SnapshotMissingReason::RecoveryCorruption,
];

/// A stored snapshot omission reason in words.
fn snapshot_reason_text(code: &str) -> &str {
    SNAPSHOT_REASONS
        .iter()
        .find(|reason| reason.as_str() == code)
        .map_or(code, |reason| snapshot_reason_label(*reason))
}

/// A stretch in words, with the date where it is not today.
fn span_label(start: i64, end: i64) -> String {
    let date = |timestamp: i64| {
        chrono::Local
            .timestamp_millis_opt(timestamp)
            .single()
            .map(|value| value.date_naive())
    };
    let end_text = if date(start) == date(end - 1) {
        clock_label(end)
    } else {
        moment_label(end)
    };
    format!("{} – {end_text}", moment_label(start))
}

fn clock_label(timestamp: i64) -> String {
    chrono::Local
        .timestamp_millis_opt(timestamp)
        .single()
        .map_or_else(String::new, |value| value.format("%H:%M").to_string())
}

fn render_report(window: &AppWindow, ui_state: &Rc<RefCell<UiState>>, report: &LocalReport) {
    let range_ms = report
        .range_ended_utc_ms
        .saturating_sub(report.range_started_utc_ms)
        .max(1) as u64;
    let coverage = report.covered_ms as f64 / range_ms as f64 * 100.0;
    let mouse = report
        .left_click_count
        .saturating_add(report.middle_click_count)
        .saturating_add(report.right_click_count);
    // The same stretch is often recorded for activity and input alike.
    let mut gaps = report
        .gaps
        .iter()
        .map(|gap| (gap.started_utc_ms, gap.ended_utc_ms, gap.reason.as_str()))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    gaps.retain(|(start, end, _)| end > start);
    let unobserved = gaps
        .iter()
        .filter(|(.., reason)| *reason == "unobserved")
        .map(|(start, end, _)| end.saturating_sub(*start) as u64)
        .sum::<u64>();
    let cleaned = |reason: &str| {
        matches!(
            reason,
            "retention_time" | "retention_space" | "user_deleted"
        )
    };
    let cleaned_count = gaps.iter().filter(|(.., reason)| cleaned(reason)).count();
    let gap_count = gaps
        .iter()
        .filter(|(.., reason)| *reason != "unobserved" && !cleaned(reason))
        .count();
    let mut coverage_note = Vec::new();
    if unobserved > 0 {
        coverage_note.push(format!("未观察 {}", format_duration(unobserved)));
    }
    if gap_count > 0 {
        coverage_note.push(format!("明确缺口 {gap_count} 段"));
    }
    if cleaned_count > 0 {
        coverage_note.push(format!("已清理 {cleaned_count} 段"));
    }
    const GAPS_LISTED: usize = 50;
    let mut gap_rows = gaps
        .iter()
        .take(GAPS_LISTED)
        .map(|(start, end, reason)| {
            SharedString::from(format!(
                "{}  {}",
                span_label(*start, *end),
                gap_reason_label(reason)
            ))
        })
        .collect::<Vec<_>>();
    if gaps.len() > GAPS_LISTED {
        gap_rows.push(format!("另有 {} 段未列出", gaps.len() - GAPS_LISTED).into());
    }
    let reasons = report
        .snapshot_reasons
        .iter()
        .map(|reason| {
            format!(
                "{} {}",
                snapshot_reason_text(&reason.reason),
                grouped_count(reason.slot_count)
            )
        })
        .collect::<Vec<_>>()
        .join(" · ");
    let focus = report
        .applications
        .iter()
        .map(|application| application.focused_ms)
        .fold(0_u64, u64::saturating_add);
    window.set_report_ready(true);
    window.set_report_focus(format_duration(focus).into());
    window.set_report_coverage(format!("{coverage:.1}%").into());
    window.set_report_coverage_note(
        if coverage_note.is_empty() {
            "没有缺口记录".to_owned()
        } else {
            coverage_note.join(" · ")
        }
        .into(),
    );
    window.set_report_gaps(ModelRc::new(VecModel::from(gap_rows)));
    window.set_report_input(grouped_count(report.keyboard_count).into());
    window.set_report_input_note(format!("鼠标点击 {}", grouped_count(mouse)).into());
    window.set_report_snapshots(
        format!("{} 张", grouped_count(report.snapshot_success_count)).into(),
    );
    window.set_report_snapshot_note(
        if report.snapshot_missing_count == 0 {
            "没有缺失".to_owned()
        } else {
            format!(
                "缺失 {}：{reasons}",
                grouped_count(report.snapshot_missing_count)
            )
        }
        .into(),
    );
    // Which rules computed it is part of the report, but small print.
    window.set_report_footer(
        format!(
            "范围 {} · 生成于 {} · 报告 #{} · 计算版本 v{}",
            span_label(report.range_started_utc_ms, report.range_ended_utc_ms),
            snapshot_time_label(report.generated_utc_ms),
            report.id,
            report.rules_version,
        )
        .into(),
    );
    {
        let mut state = ui_state.borrow_mut();
        state.report = Some((
            report.id,
            report.range_started_utc_ms,
            report.range_ended_utc_ms,
        ));
        report_freshness(window, &state);
    }
    let focus_sum = report
        .applications
        .iter()
        .map(|application| application.focused_ms)
        .fold(0_u64, u64::saturating_add)
        .max(1);
    let rows = report
        .applications
        .iter()
        .map(|application| ReportAppRow {
            name: app_icon::display_name(&application.identity, &application.display_name).into(),
            icon: app_icon::for_identity(&application.identity),
            tint: app_icon::tint(&application.identity),
            ratio: (application.focused_ms as f64 / focus_sum as f64).clamp(0.0, 1.0) as f32,
            focused: format_duration(application.focused_ms).into(),
            summary: format!(
                "打开 {} · 显示 {} · 后台 {}",
                format_duration(application.opened_ms),
                format_duration(application.displayed_ms),
                format_duration(application.background_ms),
            )
            .into(),
        })
        .collect::<Vec<_>>();
    window.set_report_apps(ModelRc::new(VecModel::from(rows)));
}

fn snapshot_policy_label(policy: SnapshotPolicy) -> String {
    let enabled = if policy.enabled {
        "已启用"
    } else {
        "已停用"
    };
    let target = if policy.capture_all_displays {
        "全部显示器"
    } else {
        "当前显示器"
    };
    let days = policy
        .retention_days
        .map_or_else(|| "永久".to_owned(), |days| format!("{days} 天"));
    format!(
        "{enabled} · 每 {} 分钟 · {target} · {days} / {}",
        policy.interval_minutes,
        format_bytes(policy.max_bytes)
    )
}

fn snapshot_reason_label(reason: SnapshotMissingReason) -> &'static str {
    match reason {
        SnapshotMissingReason::DesktopIdle => "桌面空闲",
        SnapshotMissingReason::GlobalPause => "已暂停记录",
        SnapshotMissingReason::Locked => "锁屏",
        SnapshotMissingReason::Sleep => "睡眠/恢复",
        SnapshotMissingReason::SecureDesktop => "安全桌面/UAC",
        SnapshotMissingReason::SessionDisconnected => "会话断开",
        SnapshotMissingReason::RemoteSession => "远程会话",
        SnapshotMissingReason::PrivacyExclusion => "隐私排除",
        SnapshotMissingReason::CaptureFailed => "捕获失败",
        SnapshotMissingReason::LowDisk => "可用空间低于 2 GB",
        SnapshotMissingReason::RetentionCleaned => "保留策略已清理",
        SnapshotMissingReason::UserDeleted => "用户已删除",
        SnapshotMissingReason::BackupOmitted => "备份未包含图片",
        SnapshotMissingReason::RecoveryCorruption => "图片损坏，无法恢复",
    }
}

fn snapshot_time_label(timestamp_utc_ms: i64) -> String {
    use chrono::Datelike;
    chrono::DateTime::from_timestamp_millis(timestamp_utc_ms)
        .map(|value| {
            let value = value.with_timezone(&chrono::Local);
            let format = if value.year() == chrono::Local::now().year() {
                "%-m月%-d日 %H:%M"
            } else {
                "%Y年%-m月%-d日 %H:%M"
            };
            value.format(format).to_string()
        })
        .unwrap_or_else(|| "时间不可用".to_owned())
}

fn grouped_count(value: u64) -> String {
    let digits = value.to_string();
    digits
        .chars()
        .enumerate()
        .fold(String::new(), |mut output, (index, digit)| {
            if index > 0 && (digits.len() - index).is_multiple_of(3) {
                output.push(',');
            }
            output.push(digit);
            output
        })
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
        if minutes == 0 {
            format!("{hours}小时")
        } else {
            format!("{hours}小时{minutes}分")
        }
    } else if minutes > 0 {
        if seconds == 0 {
            format!("{minutes}分")
        } else {
            format!("{minutes}分{seconds}秒")
        }
    } else {
        format!("{seconds}秒")
    }
}

fn retention_label(policy: RetentionPolicy) -> String {
    let time = policy
        .days
        .map_or_else(|| "永久保留".to_owned(), |days| format!("保留 {days} 天"));
    format!(
        "{time}。数据库超过 {} 时，从最早的一天开始清理。",
        format_bytes(policy.max_bytes)
    )
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
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
    recover: bool,
    shutdown: bool,
    uninstall_data: bool,
    background: bool,
    handshake_once: bool,
    snapshot_once: bool,
    pipe_name: Option<String>,
    data_directory: Option<PathBuf>,
    collector_task: Option<String>,
    initial_data_location: Option<PathBuf>,
}

impl Options {
    fn parse(arguments: impl IntoIterator<Item = std::ffi::OsString>) -> Result<Self> {
        let mut options = Self::default();
        let mut arguments = arguments.into_iter();
        while let Some(argument) = arguments.next() {
            match argument.to_string_lossy().as_ref() {
                "--recover" => options.recover = true,
                "--shutdown" => options.shutdown = true,
                "--uninstall-data" => options.uninstall_data = true,
                "--background" => options.background = true,
                "--handshake-once" => options.handshake_once = true,
                "--snapshot-once" => options.snapshot_once = true,
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
                "--set-data-location" => {
                    options.initial_data_location = Some(PathBuf::from(
                        arguments
                            .next()
                            .context("--set-data-location requires a value")?,
                    ));
                }
                "--collector-task" => {
                    let task = arguments
                        .next()
                        .context("--collector-task requires a value")?
                        .to_string_lossy()
                        .into_owned();
                    if !collector_task::valid_task_path(&task) {
                        bail!("--collector-task must name a Timelens collector task");
                    }
                    options.collector_task = Some(task);
                }
                unknown => bail!("unknown argument: {unknown}"),
            }
        }
        Ok(options)
    }

    fn data_directory(&self) -> Result<PathBuf> {
        local_config::resolve(&self.control_directory()?)
    }

    /// The scheduled task that runs this core's collector. A custom dataset or pipe
    /// has no known task unless one is named; starting the regular collector would
    /// feed it into the wrong dataset.
    fn collector_task(&self) -> Option<String> {
        if self.collector_task.is_some() {
            return self.collector_task.clone();
        }
        if self.data_directory.is_some()
            || self.pipe_name.is_some()
            || env::var_os("TIMELENS_DATA_DIR").is_some()
        {
            return None;
        }
        Some(collector_task::DEFAULT_TASK.to_owned())
    }

    fn control_directory(&self) -> Result<PathBuf> {
        if let Some(path) = &self.data_directory {
            return Ok(path.clone());
        }
        if let Some(path) = env::var_os("TIMELENS_DATA_DIR") {
            return Ok(PathBuf::from(path));
        }
        local_config::control_directory()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collector_state_waits_then_reports_silence() {
        let second = Duration::from_secs(1);
        assert_eq!(collector_state(5 * second, None, false), 1);
        assert_eq!(collector_state(120 * second, None, false), 2);
        assert_eq!(collector_state(120 * second, Some(10 * second), false), 0);
        assert_eq!(collector_state(600 * second, Some(100 * second), false), 2);
        assert_eq!(collector_state(600 * second, None, true), 0);
    }
}
