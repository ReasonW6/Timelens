#![cfg(windows)]
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{env, path::PathBuf, rc::Rc, sync::mpsc, thread, time::Duration};

use anyhow::{Context, Result, bail};
use slint::{Timer, TimerMode};
use timelens_ipc::{PROTOCOL_VERSION, SingleInstanceGuard, current_pipe_name, run_server_probe};
use timelens_storage::Storage;

const COLLECTOR_NAMES: &[&str] = &["timelens-collector.exe", "Timelens.Collector.exe"];

slint::slint! {
    export component AppWindow inherits Window {
        title: "Timelens";
        width: 720px;
        height: 440px;
        background: #10151d;

        in property <string> storage-status;
        in property <string> collector-status;
        in property <string> data-path;

        VerticalLayout {
            padding: 28px;
            spacing: 16px;

            Text {
                text: "Timelens";
                color: #f5f7fa;
                font-size: 32px;
                font-weight: 700;
            }
            Text {
                text: "里程碑 1 · 基础与权限边界";
                color: #8fa4bd;
                font-size: 15px;
            }
            Rectangle {
                height: 104px;
                background: #192230;
                border-radius: 10px;
                VerticalLayout {
                    padding: 16px;
                    spacing: 8px;
                    Text { text: "加密本地存储"; color: #c9d5e3; font-size: 14px; }
                    Text { text: root.storage-status; color: #74dfa7; font-size: 16px; }
                }
            }
            Rectangle {
                height: 104px;
                background: #192230;
                border-radius: 10px;
                VerticalLayout {
                    padding: 16px;
                    spacing: 8px;
                    Text { text: "提权采集器边界"; color: #c9d5e3; font-size: 14px; }
                    Text { text: root.collector-status; color: #75afff; font-size: 16px; }
                }
            }
            Text {
                text: "数据目录  " + root.data-path;
                color: #738399;
                font-size: 12px;
                overflow: elide;
            }
            Text {
                text: "窗口追踪与输入统计将在里程碑 2 接入。";
                color: #738399;
                font-size: 12px;
            }
        }
    }
}

fn main() -> Result<()> {
    let options = Options::parse(env::args_os().skip(1))?;
    let _instance = SingleInstanceGuard::acquire_core().context("Timelens is already running")?;
    let data_directory = options.data_directory()?;
    let storage = Storage::open(&data_directory).context("failed to open encrypted storage")?;
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
        return run_background(storage, pipe_name);
    }

    run_window(storage, data_directory, pipe_name)
}

fn run_background(storage: Storage, pipe_name: String) -> Result<()> {
    loop {
        match run_server_probe(&pipe_name, COLLECTOR_NAMES) {
            Ok(report) => storage.record_component_health(
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

fn run_window(storage: Storage, data_directory: PathBuf, pipe_name: String) -> Result<()> {
    enum Status {
        Connected(u32, String),
        Failed(String),
    }

    let window = AppWindow::new()?;
    window.set_storage_status(
        format!(
            "SQLCipher {} · schema v{}",
            storage.cipher_version(),
            storage.schema_version()?
        )
        .into(),
    );
    window.set_collector_status("等待受信采集器握手…".into());
    window.set_data_path(data_directory.display().to_string().into());

    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        loop {
            let status = match run_server_probe(&pipe_name, COLLECTOR_NAMES) {
                Ok(report) => Status::Connected(
                    report.peer_process_id,
                    format!(
                        "已验证采集器 · PID {} · {:?}",
                        report.peer_process_id, report.verification
                    ),
                ),
                Err(error) => Status::Failed(error.to_string()),
            };
            if sender.send(status).is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(250));
        }
    });

    let storage = Rc::new(storage);
    let weak = window.as_weak();
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(200), move || {
        let Some(window) = weak.upgrade() else {
            return;
        };
        while let Ok(status) = receiver.try_recv() {
            match status {
                Status::Connected(process_id, message) => {
                    if let Err(error) = storage.record_component_health(
                        "collector",
                        PROTOCOL_VERSION,
                        Some(process_id),
                    ) {
                        window.set_collector_status(format!("数据库状态写入失败：{error}").into());
                    } else {
                        window.set_collector_status(message.into());
                    }
                }
                Status::Failed(error) => {
                    window.set_collector_status(format!("握手失败：{error}").into());
                }
            }
        }
    });
    window.run()?;
    drop(timer);
    Ok(())
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
