//! Debug-only visual QA: with TIMELENS_QA_CAPTURE=<directory>, walk the main pages
//! once, write each rendered frame as a BMP, then exit. Release builds omit this.
use std::{cell::Cell, fs, io::Write, path::PathBuf, rc::Rc, time::Duration};

use slint::{ComponentHandle, Model, Timer, TimerMode};

use crate::{AiState, AppWindow, CollectionState, DataState};

type Step = (&'static str, fn(&AppWindow));

fn settings(window: &AppWindow, tab: i32) {
    window.set_settings_open(true);
    window.set_settings_tab(tab);
    window.set_snapshot_open(tab == 1);
    window.global::<AiState>().set_open(tab == 4);
    match tab {
        1 => window.invoke_snapshot_opened(),
        2 => window.global::<CollectionState>().invoke_opened(),
        4 => window.global::<AiState>().invoke_opened(),
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
    ("06-stats", |w| {
        w.set_stats_tab(2);
        w.invoke_navigate(3);
        w.invoke_report_generate();
    }),
    ("07-ai", |w| w.invoke_navigate(4)),
    ("08-settings-privacy", |w| settings(w, 0)),
    ("09-settings-snapshots", |w| settings(w, 1)),
    ("10-settings-rules", |w| settings(w, 2)),
    ("11-settings-data", |w| settings(w, 3)),
    ("12-settings-ai", |w| settings(w, 4)),
    ("13-settings-about", |w| settings(w, 5)),
    ("14-move-progress", |w| {
        let data = w.global::<DataState>();
        data.set_move_progress(0.42);
        data.set_move_step("正在复制 12.4 MB / 29.5 MB".into());
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
