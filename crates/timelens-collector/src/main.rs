#![cfg(windows)]
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod collection;
mod spool;

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    env,
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use timelens_ipc::{
    COLLECTOR_RESET_PAUSED_FILE, COLLECTOR_RESET_REQUEST_FILE, CollectorEvent, EventBatch,
    IdentitySource as ProtocolIdentitySource, InputMinute, MAX_INPUT_KEYS_PER_MINUTE,
    MonitoringGap, MonitoringGapReason, PhysicalKeyCount, SingleInstanceGuard,
    TrayTransition as ProtocolTrayTransition, TrayTransitionKind as ProtocolTrayTransitionKind,
    WindowObservation as ProtocolWindowObservation, WindowTransition as ProtocolWindowTransition,
    WindowTransitionKind as ProtocolWindowTransitionKind, collector_event, current_pipe_name,
    new_collector_run_id, run_client_event_batch, run_client_probe,
};
use timelens_observer::{
    IdentitySource as ObserverIdentitySource, InputDrain, InputMonitor, InputSampleKind,
    MouseButton, TrayPresence, TrayTransition as ObserverTrayTransition,
    TrayTransitionKind as ObserverTrayTransitionKind, WinEventMonitor, WindowObserver,
    WindowTransition as ObserverWindowTransition, WindowTransitionKind,
};
use windows_sys::Win32::System::Time::{
    GetTimeZoneInformation, TIME_ZONE_ID_INVALID, TIME_ZONE_INFORMATION,
};

use crate::spool::PendingSpool;

const CORE_NAMES: &[&str] = &["timelens.exe", "Timelens.exe"];
const MAX_WINDOW_EVENTS_PER_BATCH: usize = 16;
const RECONCILE_INTERVAL: Duration = Duration::from_secs(30);
const DELIVERY_RETRY_INTERVAL: Duration = Duration::from_secs(2);
const RESET_POLL_INTERVAL: Duration = Duration::from_millis(100);
const RESET_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const MINUTE_MS: i64 = 60_000;
const MAX_INPUT_BUCKETS_PER_MINUTE: usize = 4096;

fn main() -> Result<()> {
    let options = Options::parse(env::args_os().skip(1))?;
    if options.observe_windows_once {
        return observe_windows_once();
    }
    if let Some(duration) = options.observe_window_events {
        return observe_window_events(duration);
    }
    let pipe_name = options.pipe_name.clone().unwrap_or(current_pipe_name()?);

    if options.handshake_once {
        let report = probe(&pipe_name)?;
        println!(
            "core handshake ok: pid={} session={} path={} verification={:?}",
            report.peer_process_id,
            report.peer_session_id,
            report.peer_path.display(),
            report.verification
        );
        return Ok(());
    }

    let _instance = SingleInstanceGuard::acquire_collector()
        .context("Timelens collector is already running")?;
    collect_window_events(&pipe_name, options.data_directory()?)
}

fn collect_window_events(pipe_name: &str, data_directory: PathBuf) -> Result<()> {
    let mut observer = WindowObserver::new()?;
    let monitor = WinEventMonitor::new()?;
    let input_monitor = InputMonitor::new()?;
    let reset_request_path = data_directory.join(COLLECTOR_RESET_REQUEST_FILE);
    let reset_paused_path = data_directory.join(COLLECTOR_RESET_PAUSED_FILE);
    if reset_paused_path.exists() && !reset_request_path.exists() {
        fs::remove_file(&reset_paused_path)?;
    }
    let pending = PendingSpool::open(&data_directory)?;
    let policy = timelens_ipc::privacy::Policy::load(&data_directory)?;
    let seeds = pending
        .load_tray_state()?
        .into_iter()
        .filter(|t| !policy.paused && !policy.activity.contains(&t.application_identity))
        .map(observer_tray_seed)
        .collect::<Result<Vec<_>>>()?;
    observer.seed_tray_presence(seeds);
    let spool = Arc::new(Mutex::new(pending));
    let delivery_wake = spawn_delivery_worker(
        pipe_name.to_owned(),
        spool.clone(),
        reset_request_path.clone(),
        reset_paused_path.clone(),
    );
    let mut collector_run_id = new_collector_run_id()?;
    let mut next_sequence = 1;
    let initial = observer.reconcile_transitions()?;
    let mut raw = initial.current;
    let mut raw_tray = observer.current_tray_presence();
    let (now, mono) = observer.timestamp();
    let mut filter = collection::Filter::new(&data_directory, now, mono);
    let mut input = InputAggregator::default();
    let mut identities = window_identities(&raw);
    let mut last_reconcile = Instant::now() - RECONCILE_INTERVAL;
    publish_collection(
        &spool,
        &delivery_wake,
        &mut collector_run_id,
        &mut next_sequence,
        &mut filter,
        &raw,
        &raw_tray,
        now,
        mono,
    )?;
    loop {
        let stop = data_directory.join("collector-stop.request");
        if stop.is_file() {
            fs::remove_file(stop)?;
            input.add_private(
                input_monitor.drain(),
                &identities,
                &filter.policy,
                filter.blocked,
            )?;
            let (now, mono) = observer.timestamp();
            let mut events = input.take_completed(
                now,
                mono,
                LocalTimeFacts {
                    minute_started_at_utc_ms: i64::MAX,
                    ..local_time_facts(now)?
                },
            );
            events.extend(filter.windows.iter().map(|w| CollectorEvent {
                observed_at_utc_ms: now,
                monotonic_ms: mono,
                body: Some(collector_event::Body::WindowTransition(
                    ProtocolWindowTransition {
                        kind: ProtocolWindowTransitionKind::Closed as i32,
                        window: Some(protocol_window(w)),
                    },
                )),
            }));
            events.extend(filter.tray.iter().map(|p| CollectorEvent {
                observed_at_utc_ms: now,
                monotonic_ms: mono,
                body: Some(collector_event::Body::TrayTransition(
                    ProtocolTrayTransition {
                        kind: ProtocolTrayTransitionKind::Ended as i32,
                        application_identity: p.application_identity.clone(),
                        process_id: p.process_id,
                        process_started_at_100ns: p.process_started_at_100ns,
                    },
                )),
            }));
            events.push(CollectorEvent {
                observed_at_utc_ms: now,
                monotonic_ms: mono,
                body: Some(collector_event::Body::SystemInterval(
                    timelens_ipc::SystemInterval {
                        kind: "system_end".into(),
                        started_utc_ms: now,
                        duration_ms: 0,
                        timezone_offset_minutes: local_time_facts(now)?.timezone_offset_minutes,
                    },
                )),
            });
            queue_events(
                &spool,
                &delivery_wake,
                &mut collector_run_id,
                &mut next_sequence,
                &events,
                CurrentSnapshot {
                    windows: &[],
                    tray: &[],
                },
            )?;
            save_tray_state(&spool, &[])?;
            let stopping = Instant::now();
            while stopping.elapsed() < Duration::from_secs(10)
                && !spool
                    .lock()
                    .map_err(|_| anyhow::anyhow!("spool lock poisoned"))?
                    .is_empty()
            {
                thread::sleep(Duration::from_millis(50));
            }
            return Ok(());
        }
        if reset_request_path.exists() {
            acknowledge_collector_reset(&reset_paused_path)?;
            let start = Instant::now();
            while reset_request_path.exists() {
                monitor.wait_for_change(RESET_POLL_INTERVAL);
                let _ = input_monitor.drain();
                if start.elapsed() >= RESET_REQUEST_TIMEOUT
                    && let Ok(_absent_core) = SingleInstanceGuard::acquire_core()
                {
                    fs::remove_file(&reset_request_path)?;
                    break;
                }
            }
            spool
                .lock()
                .map_err(|_| anyhow::anyhow!("spool lock poisoned"))?
                .rotate_key()?;
            collector_run_id = new_collector_run_id()?;
            next_sequence = 1;
            input = InputAggregator::default();
            raw = observer.reconcile_transitions()?.current;
            raw_tray = observer.current_tray_presence();
            identities = window_identities(&raw);
            let (now, mono) = observer.timestamp();
            filter = collection::Filter::new(&data_directory, now, mono);
            publish_collection(
                &spool,
                &delivery_wake,
                &mut collector_run_id,
                &mut next_sequence,
                &mut filter,
                &raw,
                &raw_tray,
                now,
                mono,
            )?;
            fs::remove_file(&reset_paused_path)?;
            last_reconcile = Instant::now();
            continue;
        }
        let now = unix_time_ms();
        let until_minute =
            Duration::from_millis((MINUTE_MS - now.rem_euclid(MINUTE_MS)).max(1) as u64);
        let changed = monitor.wait_for_change(
            until_minute
                .min(RECONCILE_INTERVAL.saturating_sub(last_reconcile.elapsed()))
                .min(Duration::from_secs(1)),
        );
        filter.refresh(&data_directory);
        if changed || last_reconcile.elapsed() >= RECONCILE_INTERVAL {
            raw = observer.reconcile_transitions()?.current;
            raw_tray = observer.current_tray_presence();
            identities.extend(window_identities(&raw));
            last_reconcile = Instant::now();
        }
        let (now, mono) = observer.timestamp();
        publish_collection(
            &spool,
            &delivery_wake,
            &mut collector_run_id,
            &mut next_sequence,
            &mut filter,
            &raw,
            &raw_tray,
            now,
            mono,
        )?;
        let local = local_time_facts(now)?;
        input.add_private(
            input_monitor.drain(),
            &identities,
            &filter.policy,
            filter.blocked,
        )?;
        if input.take_new_overflow() {
            let gap = CollectorEvent {
                observed_at_utc_ms: now,
                monotonic_ms: mono,
                body: Some(collector_event::Body::MonitoringGap(MonitoringGap {
                    started_at_utc_ms: local.minute_started_at_utc_ms,
                    reason: MonitoringGapReason::InputOverflow as i32,
                })),
            };
            queue_events(
                &spool,
                &delivery_wake,
                &mut collector_run_id,
                &mut next_sequence,
                &[gap],
                CurrentSnapshot {
                    windows: &filter.windows,
                    tray: &filter.tray,
                },
            )?;
        }
        let completed = input.take_completed(now, mono, local);
        if !completed.is_empty() {
            queue_events(
                &spool,
                &delivery_wake,
                &mut collector_run_id,
                &mut next_sequence,
                &completed,
                CurrentSnapshot {
                    windows: &filter.windows,
                    tray: &filter.tray,
                },
            )?;
            identities = window_identities(&raw);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn publish_collection(
    spool: &Arc<Mutex<PendingSpool>>,
    wake: &mpsc::Sender<()>,
    run: &mut Vec<u8>,
    sequence: &mut u64,
    filter: &mut collection::Filter,
    raw: &[timelens_observer::WindowObservation],
    tray: &[TrayPresence],
    now: i64,
    mono: u64,
) -> Result<()> {
    let previous_tray = filter.tray.clone();
    let (windows, tray_events, mut events) = filter.reconcile(
        raw,
        tray,
        now,
        mono,
        local_time_facts(now)?.timezone_offset_minutes,
    );
    if previous_tray != filter.tray {
        save_tray_state(spool, &filter.tray)?;
    }
    events.extend(windows.iter().map(protocol_event));
    events.extend(tray_events.iter().map(protocol_tray_event));
    events.sort_by_key(|e| (e.monotonic_ms, e.observed_at_utc_ms));
    if !events.is_empty() {
        queue_events(
            spool,
            wake,
            run,
            sequence,
            &events,
            CurrentSnapshot {
                windows: &filter.windows,
                tray: &filter.tray,
            },
        )?;
    }
    Ok(())
}
#[derive(Clone, Copy)]
struct CurrentSnapshot<'a> {
    windows: &'a [timelens_observer::WindowObservation],
    tray: &'a [TrayPresence],
}

fn queue_events(
    spool: &Arc<Mutex<PendingSpool>>,
    delivery_wake: &mpsc::Sender<()>,
    collector_run_id: &mut Vec<u8>,
    next_sequence: &mut u64,
    events: &[CollectorEvent],
    current: CurrentSnapshot<'_>,
) -> Result<()> {
    for chunk in events.chunks(MAX_WINDOW_EVENTS_PER_BATCH) {
        let batch = EventBatch {
            collector_run_id: collector_run_id.to_vec(),
            first_sequence: *next_sequence,
            events: chunk.to_vec(),
        };
        let last_sequence = batch.last_sequence()?;
        let mut pending = spool
            .lock()
            .map_err(|_| anyhow::anyhow!("collector spool lock poisoned"))?;
        if !pending.push(&batch)? {
            let gap_started_at_utc_ms = pending
                .oldest_observed_at_utc_ms()?
                .unwrap_or_else(|| chunk[0].observed_at_utc_ms);
            pending.reset()?;
            drop(pending);
            queue_overflow_snapshot(
                spool,
                collector_run_id,
                next_sequence,
                gap_started_at_utc_ms,
                chunk
                    .last()
                    .map_or(gap_started_at_utc_ms, |event| event.observed_at_utc_ms),
                chunk.last().map_or(0, |event| event.monotonic_ms),
                current,
            )?;
            let _ = delivery_wake.send(());
            return Ok(());
        }
        *next_sequence = last_sequence
            .checked_add(1)
            .context("collector event sequence overflow")?;
        drop(pending);
        let _ = delivery_wake.send(());
    }
    Ok(())
}

fn queue_overflow_snapshot(
    spool: &Arc<Mutex<PendingSpool>>,
    collector_run_id: &mut Vec<u8>,
    next_sequence: &mut u64,
    gap_started_at_utc_ms: i64,
    observed_at_utc_ms: i64,
    monotonic_ms: u64,
    current: CurrentSnapshot<'_>,
) -> Result<()> {
    *collector_run_id = new_collector_run_id().context("failed to rotate collector run ID")?;
    *next_sequence = 1;
    let mut events = Vec::with_capacity(current.windows.len() + current.tray.len() + 1);
    events.push(CollectorEvent {
        observed_at_utc_ms,
        monotonic_ms,
        body: Some(collector_event::Body::MonitoringGap(MonitoringGap {
            started_at_utc_ms: gap_started_at_utc_ms.min(observed_at_utc_ms),
            reason: MonitoringGapReason::BufferOverflow as i32,
        })),
    });
    events.extend(current.windows.iter().map(|window| CollectorEvent {
        observed_at_utc_ms,
        monotonic_ms,
        body: Some(collector_event::Body::WindowTransition(
            ProtocolWindowTransition {
                kind: ProtocolWindowTransitionKind::Opened as i32,
                window: Some(protocol_window(window)),
            },
        )),
    }));
    events.extend(current.tray.iter().map(|presence| CollectorEvent {
        observed_at_utc_ms,
        monotonic_ms,
        body: Some(collector_event::Body::TrayTransition(
            ProtocolTrayTransition {
                kind: ProtocolTrayTransitionKind::Started as i32,
                application_identity: presence.application_identity.clone(),
                process_id: presence.process_id,
                process_started_at_100ns: presence.process_started_at_100ns,
            },
        )),
    }));

    let mut pending = spool
        .lock()
        .map_err(|_| anyhow::anyhow!("collector spool lock poisoned"))?;
    for chunk in events.chunks(MAX_WINDOW_EVENTS_PER_BATCH) {
        let batch = EventBatch {
            collector_run_id: collector_run_id.clone(),
            first_sequence: *next_sequence,
            events: chunk.to_vec(),
        };
        let last_sequence = batch.last_sequence()?;
        if !pending.push(&batch)? {
            bail!("collector spool cannot fit the overflow gap and newest window snapshot");
        }
        *next_sequence = last_sequence
            .checked_add(1)
            .context("collector event sequence overflow")?;
    }
    eprintln!(
        "collector spool overflowed; queued an explicit monitoring gap, {} current windows and {} tray applications",
        current.windows.len(),
        current.tray.len()
    );
    Ok(())
}

fn spawn_delivery_worker(
    pipe_name: String,
    spool: Arc<Mutex<PendingSpool>>,
    reset_request_path: PathBuf,
    reset_paused_path: PathBuf,
) -> mpsc::Sender<()> {
    let (wake_sender, wake_receiver) = mpsc::channel();
    thread::spawn(move || {
        delivery_worker(
            &pipe_name,
            &spool,
            &wake_receiver,
            &reset_request_path,
            &reset_paused_path,
        )
    });
    wake_sender
}

fn delivery_worker(
    pipe_name: &str,
    spool: &Arc<Mutex<PendingSpool>>,
    wake_receiver: &mpsc::Receiver<()>,
    reset_request_path: &std::path::Path,
    reset_paused_path: &std::path::Path,
) {
    let mut last_probe = Instant::now()
        .checked_sub(RECONCILE_INTERVAL)
        .unwrap_or_else(Instant::now);
    loop {
        if reset_request_path.exists() || reset_paused_path.exists() {
            let _ = wake_receiver.recv_timeout(RESET_POLL_INTERVAL);
            continue;
        }
        let front = match spool.lock() {
            Ok(mut pending) => match pending.front() {
                Ok(front) => front,
                Err(error) => {
                    eprintln!("collector spool replay failed: {error}");
                    return;
                }
            },
            Err(_) => {
                eprintln!("collector spool replay failed: lock poisoned");
                return;
            }
        };

        if let Some(batch) = front {
            let last_sequence = match batch.last_sequence() {
                Ok(sequence) => sequence,
                Err(error) => {
                    eprintln!("collector spool replay batch is invalid: {error}");
                    return;
                }
            };
            if reset_request_path.exists() || reset_paused_path.exists() {
                continue;
            }
            match run_client_event_batch(pipe_name, CORE_NAMES, &batch) {
                Ok(report) => {
                    let removed = spool
                        .lock()
                        .ok()
                        .and_then(|mut pending| pending.pop_if_matches(&batch).ok())
                        .unwrap_or(false);
                    if removed {
                        println!(
                            "collector event batch acknowledged: first={} last={} core_pid={}",
                            batch.first_sequence, last_sequence, report.peer_process_id
                        );
                    }
                    continue;
                }
                Err(error) => eprintln!(
                    "collector event batch delivery deferred: first={} last={} error={error}",
                    batch.first_sequence, last_sequence
                ),
            }
            let _ = wake_receiver.recv_timeout(DELIVERY_RETRY_INTERVAL);
            continue;
        }

        let elapsed = last_probe.elapsed();
        if elapsed >= RECONCILE_INTERVAL {
            match probe(pipe_name) {
                Ok(report) => println!(
                    "core heartbeat acknowledged by pid {}",
                    report.peer_process_id
                ),
                Err(error) => eprintln!("core heartbeat deferred: {error}"),
            }
            last_probe = Instant::now();
        }
        let wait = RECONCILE_INTERVAL.saturating_sub(last_probe.elapsed());
        let _ = wake_receiver.recv_timeout(wait.max(Duration::from_millis(100)));
    }
}

fn acknowledge_collector_reset(path: &std::path::Path) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    file.write_all(b"paused")?;
    file.sync_all()?;
    Ok(())
}

fn protocol_event(transition: &ObserverWindowTransition) -> CollectorEvent {
    CollectorEvent {
        observed_at_utc_ms: transition.observed_at_utc_ms,
        monotonic_ms: transition.monotonic_ms,
        body: Some(collector_event::Body::WindowTransition(
            ProtocolWindowTransition {
                kind: protocol_transition_kind(transition.kind) as i32,
                window: Some(protocol_window(&transition.window)),
            },
        )),
    }
}

fn protocol_tray_event(transition: &ObserverTrayTransition) -> CollectorEvent {
    CollectorEvent {
        observed_at_utc_ms: transition.observed_at_utc_ms,
        monotonic_ms: transition.monotonic_ms,
        body: Some(collector_event::Body::TrayTransition(
            ProtocolTrayTransition {
                kind: match transition.kind {
                    ObserverTrayTransitionKind::Started => ProtocolTrayTransitionKind::Started,
                    ObserverTrayTransitionKind::Ended => ProtocolTrayTransitionKind::Ended,
                } as i32,
                application_identity: transition.application_identity.clone(),
                process_id: transition.process_id,
                process_started_at_100ns: transition.process_started_at_100ns,
            },
        )),
    }
}

fn observer_tray_seed(transition: ProtocolTrayTransition) -> Result<TrayPresence> {
    if ProtocolTrayTransitionKind::try_from(transition.kind).ok()
        != Some(ProtocolTrayTransitionKind::Started)
        || transition.application_identity.is_empty()
        || transition.process_id == 0
        || transition.process_started_at_100ns == 0
    {
        bail!("collector tray restart state contains an invalid transition");
    }
    Ok(TrayPresence {
        application_identity: transition.application_identity,
        process_id: transition.process_id,
        process_started_at_100ns: transition.process_started_at_100ns,
    })
}

fn save_tray_state(spool: &Arc<Mutex<PendingSpool>>, presence: &[TrayPresence]) -> Result<()> {
    let transitions = presence
        .iter()
        .map(|presence| ProtocolTrayTransition {
            kind: ProtocolTrayTransitionKind::Started as i32,
            application_identity: presence.application_identity.clone(),
            process_id: presence.process_id,
            process_started_at_100ns: presence.process_started_at_100ns,
        })
        .collect::<Vec<_>>();
    spool
        .lock()
        .map_err(|_| anyhow::anyhow!("collector spool lock poisoned"))?
        .save_tray_state(&transitions)
}

fn protocol_window(window: &timelens_observer::WindowObservation) -> ProtocolWindowObservation {
    ProtocolWindowObservation {
        window_id: window.window_id,
        process_id: window.process_id,
        process_started_at_100ns: window.process_started_at_100ns,
        application_identity: window.application_identity.clone(),
        identity_source: protocol_identity_source(window.identity_source) as i32,
        executable_path: window.executable_path.clone(),
        app_user_model_id: window.app_user_model_id.clone(),
        package_identity: window.package_identity.clone(),
        displayed: window.displayed,
        focused: window.focused,
        on_current_virtual_desktop: window.on_current_virtual_desktop,
        virtual_desktop_id: window.virtual_desktop_id.clone(),
    }
}

fn protocol_transition_kind(kind: WindowTransitionKind) -> ProtocolWindowTransitionKind {
    match kind {
        WindowTransitionKind::Opened => ProtocolWindowTransitionKind::Opened,
        WindowTransitionKind::Updated => ProtocolWindowTransitionKind::Updated,
        WindowTransitionKind::Closed => ProtocolWindowTransitionKind::Closed,
    }
}

fn protocol_identity_source(source: ObserverIdentitySource) -> ProtocolIdentitySource {
    match source {
        ObserverIdentitySource::ExecutablePath => ProtocolIdentitySource::ExecutablePath,
        ObserverIdentitySource::Package => ProtocolIdentitySource::Package,
        ObserverIdentitySource::ProcessAppUserModelId => {
            ProtocolIdentitySource::ProcessAppUserModelId
        }
        ObserverIdentitySource::WindowAppUserModelId => {
            ProtocolIdentitySource::WindowAppUserModelId
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct LocalTimeFacts {
    minute_started_at_utc_ms: i64,
    timezone_offset_minutes: i32,
    year: i32,
    month: u32,
    day: u32,
}

impl LocalTimeFacts {
    fn local_date(self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct InputBucketKey {
    local_time: LocalTimeFacts,
    focused_application_identity: Option<String>,
    anonymous_only: bool,
}

#[derive(Default)]
struct InputBucketCounts {
    keyboard_count: u64,
    left_click_count: u64,
    middle_click_count: u64,
    right_click_count: u64,
    key_counts: BTreeMap<(u16, u64), u64>,
}

#[derive(Default)]
struct InputAggregator {
    buckets: BTreeMap<InputBucketKey, InputBucketCounts>,
    overflowed: bool,
    overflow_reported: bool,
}

impl InputAggregator {
    #[cfg(test)]
    fn add(&mut self, drain: InputDrain, identities: &HashMap<u64, String>) -> Result<()> {
        self.add_private(
            drain,
            identities,
            &timelens_ipc::privacy::Policy::default(),
            false,
        )
    }
    fn add_private(
        &mut self,
        drain: InputDrain,
        identities: &HashMap<u64, String>,
        policy: &timelens_ipc::privacy::Policy,
        blocked: bool,
    ) -> Result<()> {
        if blocked || policy.paused {
            return Ok(());
        }
        if drain.overflowed {
            self.overflowed = true;
        }
        let mut local_times = HashMap::new();
        for sample in drain.samples {
            let local_time =
                if let Some(local_time) = local_times.get(&sample.minute_started_at_utc_ms) {
                    *local_time
                } else {
                    let local_time = local_time_facts(sample.minute_started_at_utc_ms)?;
                    local_times.insert(sample.minute_started_at_utc_ms, local_time);
                    local_time
                };
            let key = InputBucketKey {
                local_time,
                focused_application_identity: identities
                    .get(&sample.window_id)
                    .filter(|id| !policy.activity.contains(*id) && !policy.input.contains(*id))
                    .cloned(),
                anonymous_only: identities
                    .get(&sample.window_id)
                    .is_some_and(|id| policy.input.contains(id)),
            };
            if !self.buckets.contains_key(&key)
                && self.buckets.len() >= MAX_INPUT_BUCKETS_PER_MINUTE
            {
                self.overflowed = true;
                continue;
            }
            let anonymous_only = key.anonymous_only;
            let counts = self.buckets.entry(key).or_default();
            match sample.kind {
                InputSampleKind::Keyboard {
                    scan_code,
                    keyboard_layout,
                } => {
                    if anonymous_only {
                        counts.keyboard_count = counts
                            .keyboard_count
                            .saturating_add(u64::from(sample.count));
                        continue;
                    }
                    let physical_key = (scan_code, keyboard_layout);
                    if !counts.key_counts.contains_key(&physical_key)
                        && counts.key_counts.len() >= MAX_INPUT_KEYS_PER_MINUTE
                    {
                        self.overflowed = true;
                        continue;
                    }
                    let key_count = counts.key_counts.entry(physical_key).or_default();
                    *key_count = key_count.saturating_add(u64::from(sample.count));
                    counts.keyboard_count = counts
                        .keyboard_count
                        .saturating_add(u64::from(sample.count));
                }
                InputSampleKind::Mouse(MouseButton::Left) => {
                    counts.left_click_count = counts
                        .left_click_count
                        .saturating_add(u64::from(sample.count));
                }
                InputSampleKind::Mouse(MouseButton::Middle) => {
                    counts.middle_click_count = counts
                        .middle_click_count
                        .saturating_add(u64::from(sample.count));
                }
                InputSampleKind::Mouse(MouseButton::Right) => {
                    counts.right_click_count = counts
                        .right_click_count
                        .saturating_add(u64::from(sample.count));
                }
            }
        }
        Ok(())
    }

    fn take_new_overflow(&mut self) -> bool {
        if self.overflowed && !self.overflow_reported {
            self.overflow_reported = true;
            true
        } else {
            false
        }
    }

    fn take_completed(
        &mut self,
        observed_at_utc_ms: i64,
        monotonic_ms: u64,
        current_time: LocalTimeFacts,
    ) -> Vec<CollectorEvent> {
        let completed_keys = self
            .buckets
            .keys()
            .filter(|key| {
                key.local_time.minute_started_at_utc_ms < current_time.minute_started_at_utc_ms
            })
            .cloned()
            .collect::<Vec<_>>();
        let mut events = Vec::with_capacity(completed_keys.len());
        for key in completed_keys {
            let counts = self
                .buckets
                .remove(&key)
                .expect("completed input bucket disappeared");
            let Some(minute) = input_minute(key, counts) else {
                self.overflowed = true;
                continue;
            };
            events.push(CollectorEvent {
                observed_at_utc_ms,
                monotonic_ms,
                body: Some(collector_event::Body::InputMinute(minute)),
            });
        }
        if self.buckets.keys().all(|key| {
            key.local_time.minute_started_at_utc_ms >= current_time.minute_started_at_utc_ms
        }) {
            self.overflowed = false;
            self.overflow_reported = false;
        }
        events
    }
}

fn input_minute(key: InputBucketKey, counts: InputBucketCounts) -> Option<InputMinute> {
    Some(InputMinute {
        anonymous_only: key.anonymous_only,
        minute_started_at_utc_ms: if key.anonymous_only {
            0
        } else {
            key.local_time.minute_started_at_utc_ms
        },
        timezone_offset_minutes: key.local_time.timezone_offset_minutes,
        local_date: key.local_time.local_date(),
        focused_application_identity: key.focused_application_identity,
        keyboard_count: u32::try_from(counts.keyboard_count).ok()?,
        left_click_count: u32::try_from(counts.left_click_count).ok()?,
        middle_click_count: u32::try_from(counts.middle_click_count).ok()?,
        right_click_count: u32::try_from(counts.right_click_count).ok()?,
        key_counts: counts
            .key_counts
            .into_iter()
            .map(|((scan_code, keyboard_layout), count)| {
                Some(PhysicalKeyCount {
                    scan_code: u32::from(scan_code),
                    keyboard_layout,
                    count: u32::try_from(count).ok()?,
                })
            })
            .collect::<Option<Vec<_>>>()?,
    })
}

fn window_identities(windows: &[timelens_observer::WindowObservation]) -> HashMap<u64, String> {
    windows
        .iter()
        .map(|window| (window.window_id, window.application_identity.clone()))
        .collect()
}

fn local_time_facts(utc_ms: i64) -> Result<LocalTimeFacts> {
    let mut information = TIME_ZONE_INFORMATION::default();
    let zone_id = unsafe { GetTimeZoneInformation(&mut information) };
    if zone_id == TIME_ZONE_ID_INVALID {
        bail!("Windows could not resolve the current time-zone offset");
    }
    let seasonal_bias = match zone_id {
        1 => information.StandardBias,
        2 => information.DaylightBias,
        _ => 0,
    };
    let timezone_offset_minutes = information
        .Bias
        .checked_add(seasonal_bias)
        .and_then(|bias| bias.checked_neg())
        .context("Windows time-zone offset overflow")?;
    let local_ms = utc_ms
        .checked_add(i64::from(timezone_offset_minutes) * MINUTE_MS)
        .context("local input timestamp overflow")?;
    let (year, month, day) = civil_from_days(local_ms.div_euclid(86_400_000));
    Ok(LocalTimeFacts {
        minute_started_at_utc_ms: utc_ms.div_euclid(MINUTE_MS) * MINUTE_MS,
        timezone_offset_minutes,
        year,
        month,
        day,
    })
}

fn civil_from_days(days_since_unix_epoch: i64) -> (i32, u32, u32) {
    let shifted = days_since_unix_epoch + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year as i32, month as u32, day as u32)
}

fn unix_time_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn observe_window_events(duration: Duration) -> Result<()> {
    let mut observer = WindowObserver::new().context("failed to initialize window observation")?;
    let monitor = WinEventMonitor::new().context("failed to install Windows event hooks")?;
    let started = Instant::now();
    let mut triggers = 0_u64;
    let mut opened = 0_u64;
    let mut updated = 0_u64;
    let mut closed = 0_u64;

    let initial = observer
        .reconcile_transitions()
        .context("failed to reconcile the initial Windows desktop")?;
    count_transitions(&initial.transitions, &mut opened, &mut updated, &mut closed);

    while let Some(remaining) = duration.checked_sub(started.elapsed()) {
        if remaining.is_zero() {
            break;
        }
        if monitor.wait_for_change(remaining.min(Duration::from_secs(1))) {
            triggers += 1;
            let result = observer
                .reconcile_transitions()
                .context("failed to reconcile a Windows event")?;
            count_transitions(&result.transitions, &mut opened, &mut updated, &mut closed);
        }
    }

    println!(
        "window event observation ok: triggers={triggers} opened={opened} updated={updated} closed={closed}"
    );
    Ok(())
}

fn count_transitions(
    transitions: &[timelens_observer::WindowTransition],
    opened: &mut u64,
    updated: &mut u64,
    closed: &mut u64,
) {
    for transition in transitions {
        match transition.kind {
            WindowTransitionKind::Opened => *opened += 1,
            WindowTransitionKind::Updated => *updated += 1,
            WindowTransitionKind::Closed => *closed += 1,
        }
    }
}

fn observe_windows_once() -> Result<()> {
    let mut observer = WindowObserver::new().context("failed to initialize window observation")?;
    let observations = observer
        .reconcile()
        .context("failed to reconcile the current Windows desktop")?;
    let focused = observations.iter().filter(|window| window.focused).count();
    if focused > 1 {
        bail!("window observation returned {focused} focused windows; Windows allows at most one");
    }
    if observations
        .iter()
        .any(|window| window.application_identity.is_empty())
    {
        bail!("window observation returned an empty application identity");
    }
    let applications = observations
        .iter()
        .map(|window| window.application_identity.as_str())
        .collect::<HashSet<_>>()
        .len();
    let displayed = observations
        .iter()
        .filter(|window| window.displayed)
        .count();
    let background = observations.len() - displayed;
    let virtual_desktop_unknown = observations
        .iter()
        .filter(|window| window.on_current_virtual_desktop.is_none())
        .count();
    println!(
        "window observation ok: windows={} applications={applications} displayed={displayed} focused={focused} background={background} virtual_desktop_unknown={virtual_desktop_unknown}",
        observations.len()
    );
    Ok(())
}

fn probe(pipe_name: &str) -> Result<timelens_ipc::HandshakeReport> {
    run_client_probe(pipe_name, CORE_NAMES)
        .context("collector could not authenticate the Timelens core")
}

#[derive(Default)]
struct Options {
    handshake_once: bool,
    observe_window_events: Option<Duration>,
    observe_windows_once: bool,
    pipe_name: Option<String>,
    data_directory: Option<PathBuf>,
}

impl Options {
    fn parse(arguments: impl IntoIterator<Item = std::ffi::OsString>) -> Result<Self> {
        let mut options = Self::default();
        let mut arguments = arguments.into_iter();
        while let Some(argument) = arguments.next() {
            match argument.to_string_lossy().as_ref() {
                "--background" => {}
                "--handshake-once" => options.handshake_once = true,
                "--observe-window-events-seconds" => {
                    let seconds = arguments
                        .next()
                        .context("--observe-window-events-seconds requires a value")?
                        .to_string_lossy()
                        .parse::<u64>()
                        .context("--observe-window-events-seconds must be an integer")?;
                    options.observe_window_events = Some(Duration::from_secs(seconds));
                }
                "--observe-windows-once" => options.observe_windows_once = true,
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
    use timelens_observer::{InputSample, WindowObservation};

    #[test]
    fn maps_an_observer_transition_without_content_fields() {
        let transition = ObserverWindowTransition {
            kind: WindowTransitionKind::Opened,
            observed_at_utc_ms: 1_700_000_000_000,
            monotonic_ms: 25,
            window: WindowObservation {
                window_id: 100,
                process_id: 200,
                process_started_at_100ns: 300,
                application_identity: "path:c:\\apps\\sample.exe".to_owned(),
                identity_source: ObserverIdentitySource::ExecutablePath,
                executable_path: Some(r"C:\Apps\Sample.exe".to_owned()),
                app_user_model_id: None,
                package_identity: None,
                displayed: true,
                focused: true,
                on_current_virtual_desktop: Some(true),
                virtual_desktop_id: Some("desktop".to_owned()),
            },
        };

        let event = protocol_event(&transition);
        assert_eq!(event.observed_at_utc_ms, transition.observed_at_utc_ms);
        let Some(collector_event::Body::WindowTransition(mapped)) = event.body else {
            panic!("window transition body was not mapped");
        };
        assert_eq!(mapped.kind, ProtocolWindowTransitionKind::Opened as i32);
        assert_eq!(
            mapped.window.unwrap().application_identity,
            transition.window.application_identity
        );
    }

    #[test]
    fn maps_every_identity_source_explicitly() {
        assert_eq!(
            protocol_identity_source(ObserverIdentitySource::ExecutablePath),
            ProtocolIdentitySource::ExecutablePath
        );
        assert_eq!(
            protocol_identity_source(ObserverIdentitySource::Package),
            ProtocolIdentitySource::Package
        );
        assert_eq!(
            protocol_identity_source(ObserverIdentitySource::ProcessAppUserModelId),
            ProtocolIdentitySource::ProcessAppUserModelId
        );
        assert_eq!(
            protocol_identity_source(ObserverIdentitySource::WindowAppUserModelId),
            ProtocolIdentitySource::WindowAppUserModelId
        );
    }

    #[test]
    fn input_aggregator_seals_only_completed_minutes_with_app_attribution() {
        let minute = local_time_facts(1_700_000_040_000).unwrap();
        let identities = HashMap::from([(42, "path:c:\\apps\\focused.exe".to_owned())]);
        let mut aggregator = InputAggregator::default();
        aggregator
            .add(
                InputDrain {
                    samples: vec![
                        InputSample {
                            minute_started_at_utc_ms: minute.minute_started_at_utc_ms,
                            window_id: 42,
                            kind: InputSampleKind::Keyboard {
                                scan_code: 0x1e,
                                keyboard_layout: 0x0804_0804,
                            },
                            count: 3,
                        },
                        InputSample {
                            minute_started_at_utc_ms: minute.minute_started_at_utc_ms,
                            window_id: 42,
                            kind: InputSampleKind::Mouse(MouseButton::Left),
                            count: 2,
                        },
                        InputSample {
                            minute_started_at_utc_ms: minute.minute_started_at_utc_ms,
                            window_id: 42,
                            kind: InputSampleKind::Mouse(MouseButton::Middle),
                            count: 1,
                        },
                    ],
                    overflowed: false,
                },
                &identities,
            )
            .unwrap();

        assert!(
            aggregator
                .take_completed(1_700_000_070_000, 30_000, minute)
                .is_empty()
        );
        let next_minute = LocalTimeFacts {
            minute_started_at_utc_ms: minute.minute_started_at_utc_ms + MINUTE_MS,
            ..minute
        };
        let events = aggregator.take_completed(1_700_000_100_000, 60_000, next_minute);
        assert_eq!(events.len(), 1);
        let Some(collector_event::Body::InputMinute(bucket)) = &events[0].body else {
            panic!("completed input bucket was not emitted");
        };
        assert_eq!(
            bucket.focused_application_identity.as_deref(),
            Some("path:c:\\apps\\focused.exe")
        );
        assert_eq!(
            bucket.minute_started_at_utc_ms,
            minute.minute_started_at_utc_ms
        );
        assert_eq!(bucket.local_date, minute.local_date());
        assert_eq!(bucket.keyboard_count, 3);
        assert_eq!(bucket.left_click_count, 2);
        assert_eq!(bucket.middle_click_count, 1);
        assert_eq!(bucket.right_click_count, 0);
        assert_eq!(bucket.key_counts.len(), 1);
        assert_eq!(bucket.key_counts[0].scan_code, 0x1e);
        assert_eq!(bucket.key_counts[0].count, 3);
    }

    #[test]
    fn civil_date_conversion_handles_epoch_leap_day_and_pre_epoch() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }
}
