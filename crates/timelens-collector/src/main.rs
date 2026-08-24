#![cfg(windows)]
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    collections::HashSet,
    env, thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use timelens_ipc::{
    CollectorEvent, EventBatch, IdentitySource as ProtocolIdentitySource,
    WindowObservation as ProtocolWindowObservation, WindowTransition as ProtocolWindowTransition,
    WindowTransitionKind as ProtocolWindowTransitionKind, collector_event, current_pipe_name,
    new_collector_run_id, run_client_event_batch, run_client_probe,
};
use timelens_observer::{
    IdentitySource as ObserverIdentitySource, WinEventMonitor, WindowObserver,
    WindowTransition as ObserverWindowTransition, WindowTransitionKind,
};

const CORE_NAMES: &[&str] = &["timelens.exe", "Timelens.exe"];
const MAX_WINDOW_EVENTS_PER_BATCH: usize = 16;
const RECONCILE_INTERVAL: Duration = Duration::from_secs(30);

fn main() -> Result<()> {
    let options = Options::parse(env::args_os().skip(1))?;
    if options.observe_windows_once {
        return observe_windows_once();
    }
    if let Some(duration) = options.observe_window_events {
        return observe_window_events(duration);
    }
    let pipe_name = options.pipe_name.unwrap_or(current_pipe_name()?);

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

    collect_window_events(&pipe_name)
}

fn collect_window_events(pipe_name: &str) -> Result<()> {
    let mut observer = WindowObserver::new().context("failed to initialize window observation")?;
    let monitor = WinEventMonitor::new().context("failed to install Windows event hooks")?;
    let collector_run_id = new_collector_run_id().context("failed to create collector run ID")?;
    let mut next_sequence = 1_u64;

    let initial = observer
        .reconcile_transitions()
        .context("failed to reconcile the initial Windows desktop")?;
    deliver_transitions(
        pipe_name,
        &collector_run_id,
        &mut next_sequence,
        &initial.transitions,
    )?;

    loop {
        monitor.wait_for_change(RECONCILE_INTERVAL);
        let result = observer
            .reconcile_transitions()
            .context("failed to reconcile a Windows event")?;
        if result.transitions.is_empty() {
            match probe(pipe_name) {
                Ok(report) => println!(
                    "core heartbeat acknowledged by pid {}",
                    report.peer_process_id
                ),
                Err(error) => eprintln!("core heartbeat failed: {error}"),
            }
        } else {
            deliver_transitions(
                pipe_name,
                &collector_run_id,
                &mut next_sequence,
                &result.transitions,
            )?;
        }
    }
}

fn deliver_transitions(
    pipe_name: &str,
    collector_run_id: &[u8],
    next_sequence: &mut u64,
    transitions: &[ObserverWindowTransition],
) -> Result<()> {
    for chunk in transitions.chunks(MAX_WINDOW_EVENTS_PER_BATCH) {
        let batch = EventBatch {
            collector_run_id: collector_run_id.to_vec(),
            first_sequence: *next_sequence,
            events: chunk.iter().map(protocol_event).collect(),
        };
        let last_sequence = batch.last_sequence()?;
        loop {
            match run_client_event_batch(pipe_name, CORE_NAMES, &batch) {
                Ok(report) => {
                    println!(
                        "window event batch acknowledged: first={} last={} core_pid={}",
                        batch.first_sequence, last_sequence, report.peer_process_id
                    );
                    break;
                }
                Err(error) => {
                    eprintln!(
                        "window event batch delivery failed: first={} last={} error={error}",
                        batch.first_sequence, last_sequence
                    );
                    thread::sleep(Duration::from_secs(2));
                }
            }
        }
        *next_sequence = last_sequence
            .checked_add(1)
            .context("collector event sequence overflow")?;
    }
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
                unknown => bail!("unknown argument: {unknown}"),
            }
        }
        Ok(options)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use timelens_observer::WindowObservation;

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
}
