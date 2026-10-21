//! Debug-only visual QA: with TIMELENS_QA_CAPTURE=<directory>, walk the main pages
//! once, write each rendered frame as a BMP, then exit. Release builds omit this.
//!
//! Adding TIMELENS_QA_HEADLESS=1 renders in memory instead: no window or tray
//! icon appears, so the run can share the desktop with the installed core.
use std::{
    cell::Cell,
    fs,
    io::Write,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

use anyhow::{Context, bail};
use slint::{
    ComponentHandle, EventLoopError, Model, PhysicalSize, PlatformError, Timer, TimerMode,
    platform::{
        EventLoopProxy, Platform, WindowAdapter,
        software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
    },
};

use crate::{AppWindow, CollectionState, DataState};

static HEADLESS: AtomicBool = AtomicBool::new(false);

/// Whether this run renders in memory, without a window, tray icon or collector.
pub fn headless() -> bool {
    HEADLESS.load(Ordering::Relaxed)
}

/// Switch to the in-memory platform when asked. It must run before any window
/// exists. The run then skips the core's singleton, so it only accepts an
/// explicit `--data-dir` that is not the installed core's dataset.
pub fn start_headless(data_directory: Option<&Path>) -> anyhow::Result<bool> {
    if std::env::var_os("TIMELENS_QA_CAPTURE").is_none()
        || std::env::var_os("TIMELENS_QA_HEADLESS").is_none_or(|value| value != "1")
    {
        return Ok(false);
    }
    let Some(directory) = data_directory else {
        bail!("TIMELENS_QA_HEADLESS requires --data-dir with a separate dataset");
    };
    // A directory that does not exist yet cannot be the installed dataset.
    if let Ok(directory) = fs::canonicalize(directory) {
        let control = crate::local_config::control_directory()?;
        let installed = [crate::local_config::resolve(&control)?, control]
            .iter()
            .filter_map(|path| fs::canonicalize(path).ok())
            .collect::<Vec<_>>();
        // The directory may itself point elsewhere through a data-location file.
        let target = crate::local_config::resolve(&directory)?;
        if [directory, target]
            .iter()
            .any(|path| fs::canonicalize(path).is_ok_and(|path| installed.contains(&path)))
        {
            bail!("TIMELENS_QA_HEADLESS must not open the installed Timelens dataset");
        }
    }
    let (sender, receiver) = mpsc::channel();
    slint::platform::set_platform(Box::new(Headless { sender, receiver }))
        .context("the headless QA platform must be set before any window")?;
    HEADLESS.store(true, Ordering::Relaxed);
    Ok(true)
}

enum Message {
    Invoke(Box<dyn FnOnce() + Send>),
    Quit,
}

struct Headless {
    sender: mpsc::Sender<Message>,
    receiver: mpsc::Receiver<Message>,
}

impl Platform for Headless {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        // The main window's preferred size; TIMELENS_QA_VIEWPORT may override it.
        window.set_size(PhysicalSize::new(1440, 900));
        Ok(window)
    }

    fn run_event_loop(&self) -> Result<(), PlatformError> {
        loop {
            slint::platform::update_timers_and_animations();
            // Frames are rendered only when the capture takes a snapshot.
            let message = match slint::platform::duration_until_next_timer_update() {
                Some(wait) => match self.receiver.recv_timeout(wait) {
                    Ok(message) => message,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
                },
                None => match self.receiver.recv() {
                    Ok(message) => message,
                    Err(_) => return Ok(()),
                },
            };
            match message {
                Message::Invoke(event) => event(),
                Message::Quit => return Ok(()),
            }
        }
    }

    fn new_event_loop_proxy(&self) -> Option<Box<dyn EventLoopProxy>> {
        Some(Box::new(Proxy(self.sender.clone())))
    }
}

struct Proxy(mpsc::Sender<Message>);

impl EventLoopProxy for Proxy {
    fn quit_event_loop(&self) -> Result<(), EventLoopError> {
        self.0
            .send(Message::Quit)
            .map_err(|_| EventLoopError::EventLoopTerminated)
    }

    fn invoke_from_event_loop(
        &self,
        event: Box<dyn FnOnce() + Send>,
    ) -> Result<(), EventLoopError> {
        self.0
            .send(Message::Invoke(event))
            .map_err(|_| EventLoopError::EventLoopTerminated)
    }
}

type Step = (&'static str, fn(&AppWindow));

fn settings(window: &AppWindow, tab: i32) {
    window.set_settings_open(true);
    window.set_settings_tab(tab);
    window.set_snapshot_open(tab == 1);
    match tab {
        1 => window.invoke_snapshot_opened(),
        2 => {
            // As the settings do: the rules page, not the key statistics.
            window.global::<CollectionState>().set_page(0);
            window.global::<CollectionState>().invoke_opened();
        }
        _ => {}
    }
}

/// Pick the history's first activity that has applications used alongside it,
/// or else its first activity.
fn select_activity(window: &AppWindow) {
    let items = window.get_history().iter().collect::<Vec<_>>();
    let entry = items
        .iter()
        .find(|item| item.kind == 2 && item.stack.row_count() > 0)
        .or_else(|| items.iter().find(|item| item.kind == 2));
    if let Some(entry) = entry {
        window.invoke_history_select(entry.key.clone());
    }
}

const STEPS: &[Step] = &[
    ("01-timeline", |w| w.invoke_navigate(0)),
    ("02-timeline-detail", select_activity),
    ("03-timeline-scrolled", |w| {
        w.invoke_history_close();
        w.set_history_follow(false);
        w.set_history_scroll(-300.0);
    }),
    ("03a-timeline-top", |w| w.set_history_scroll(0.0)),
    ("03b-apps", |w| {
        w.set_history_follow(true);
        w.set_history_to_end(w.get_history_to_end() + 1);
        w.set_stats_tab(0);
        w.invoke_navigate(3);
    }),
    ("04-apps-detail", |w| {
        w.set_app_detail_open(true);
        w.invoke_app_selected(1);
    }),
    ("05-snapshots", |w| {
        w.set_app_detail_open(false);
        w.invoke_navigate(2);
    }),
    ("05a-snapshots-wide-list", |w| {
        w.set_snapshot_list_width(760.0);
        w.invoke_snapshot_step(-1, 1);
    }),
    ("05b-snapshot-viewer", |w| {
        w.set_snapshot_list_width(432.0);
        w.set_snapshot_viewer_open(true);
    }),
    ("05c-stats-keys", |w| {
        w.set_snapshot_viewer_open(false);
        w.set_stats_tab(1);
        w.invoke_navigate(3);
        w.global::<CollectionState>().set_page(1);
        w.global::<CollectionState>().invoke_action(7);
    }),
    ("06-stats-gaps", |w| {
        w.set_stats_tab(0);
        w.set_app_detail_open(false);
        w.set_apps_gaps_open(true);
    }),
    ("08-settings-privacy", |w| settings(w, 0)),
    ("09-settings-snapshots", |w| settings(w, 1)),
    ("10-settings-rules", |w| settings(w, 2)),
    ("11-settings-data", |w| settings(w, 3)),
    ("13-settings-about", |w| settings(w, 5)),
    ("14-move-progress", |w| {
        let data = w.global::<DataState>();
        data.set_move_progress(0.42);
        data.set_move_step(crate::i18n::tr!("正在复制 {} / {}", "12.4 MB", "29.5 MB").into());
        data.set_move_phase(1);
    }),
    ("15-move-failed", |w| {
        let data = w.global::<DataState>();
        data.set_move_message(
            "目标目录必须为空；V1 不合并数据集
原来的数据没有改动，Timelens 会继续使用原位置。"
                .into(),
        );
        data.set_move_phase(3);
    }),
    ("16-dark-timeline", |w| {
        w.global::<DataState>().set_move_phase(0);
        w.set_settings_open(false);
        w.invoke_theme_mode_selected(2);
        w.invoke_navigate(0);
        select_activity(w);
    }),
    ("17-dark-apps", |w| {
        w.invoke_history_close();
        w.set_stats_tab(0);
        w.invoke_navigate(3);
    }),
    ("18-dark-keys", |w| {
        w.set_stats_tab(1);
        w.global::<CollectionState>().set_page(1);
        w.global::<CollectionState>().invoke_action(7);
    }),
    ("19-dark-snapshots", |w| w.invoke_navigate(2)),
    ("21-dark-settings-general", |w| settings(w, 6)),
    ("22-dark-settings-data", |w| settings(w, 3)),
];

pub fn install(window: &AppWindow) -> Option<Timer> {
    let directory = PathBuf::from(std::env::var_os("TIMELENS_QA_CAPTURE")?);
    fs::create_dir_all(&directory).ok()?;
    let weak = window.as_weak();
    let step = Rc::new(Cell::new(0_usize));
    let timer = Timer::default();
    // Each tick saves the previous step's frame, then performs the next step.
    timer.start(
        TimerMode::Repeated,
        Duration::from_millis(1600),
        move || {
            let Some(window) = weak.upgrade() else {
                return;
            };
            let index = step.get();
            if index > 0
                && let Ok(frame) = window.window().take_snapshot()
            {
                let path = directory.join(format!("{}.bmp", STEPS[index - 1].0));
                if let Err(error) = write_bmp(&path, &frame) {
                    eprintln!("QA capture failed for {}: {error}", path.display());
                }
            }
            match STEPS.get(index) {
                Some((_, action)) => {
                    action(&window);
                    step.set(index + 1);
                }
                None => std::process::exit(0),
            }
        },
    );
    Some(timer)
}

fn write_bmp(
    path: &std::path::Path,
    frame: &slint::SharedPixelBuffer<slint::Rgba8Pixel>,
) -> std::io::Result<()> {
    let (width, height) = (frame.width(), frame.height());
    let pixels = width as usize * height as usize * 4;
    let mut file = fs::File::create(path)?;
    let mut header = Vec::with_capacity(54);
    header.extend_from_slice(b"BM");
    header.extend_from_slice(&(54 + pixels as u32).to_le_bytes());
    header.extend_from_slice(&[0; 4]);
    header.extend_from_slice(&54_u32.to_le_bytes());
    header.extend_from_slice(&40_u32.to_le_bytes());
    header.extend_from_slice(&(width as i32).to_le_bytes());
    // A negative height stores rows top to bottom.
    header.extend_from_slice(&(-(height as i32)).to_le_bytes());
    header.extend_from_slice(&1_u16.to_le_bytes());
    header.extend_from_slice(&32_u16.to_le_bytes());
    header.extend_from_slice(&[0; 24]);
    file.write_all(&header)?;
    let mut body = Vec::with_capacity(pixels);
    for pixel in frame.as_slice() {
        body.extend_from_slice(&[pixel.b, pixel.g, pixel.r, pixel.a]);
    }
    file.write_all(&body)
}
