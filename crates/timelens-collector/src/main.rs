#![cfg(windows)]
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{collections::HashSet, env, thread, time::Duration};

use anyhow::{Context, Result, bail};
use timelens_ipc::{current_pipe_name, run_client_probe};
use timelens_observer::WindowObserver;

const CORE_NAMES: &[&str] = &["timelens.exe", "Timelens.exe"];

fn main() -> Result<()> {
    let options = Options::parse(env::args_os().skip(1))?;
    if options.observe_windows_once {
        return observe_windows_once();
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

    loop {
        match probe(&pipe_name) {
            Ok(report) => {
                println!(
                    "core heartbeat acknowledged by pid {}",
                    report.peer_process_id
                );
                thread::sleep(Duration::from_secs(15));
            }
            Err(error) => {
                eprintln!("core probe failed: {error}");
                thread::sleep(Duration::from_secs(2));
            }
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
