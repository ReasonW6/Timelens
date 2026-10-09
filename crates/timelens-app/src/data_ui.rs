use crate::{AppWindow, DataState, SnapshotUiState, UiState};
use anyhow::{Context, Result, anyhow, bail};
use slint::{ComponentHandle, Timer, TimerMode};
use std::{
    cell::RefCell,
    fs,
    io::Write,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};
use timelens_ipc::{
    COLLECTOR_RESET_PAUSED_FILE, COLLECTOR_RESET_REQUEST_FILE, SingleInstanceGuard,
};
use timelens_storage::{BackupOptions, ExportFormat, PreparedRestore, RelocationStep, Storage};

fn lock(s: &Arc<Mutex<Storage>>) -> Result<std::sync::MutexGuard<'_, Storage>> {
    s.lock().map_err(|_| anyhow!("存储锁不可用"))
}
pub fn choose_file(save: bool, extension: &str) -> Result<Option<PathBuf>> {
    use windows_sys::Win32::UI::Controls::Dialogs::*;
    let mut path = vec![0_u16; 32768];
    if save {
        let name = format!(
            "Timelens-{}.{}",
            chrono::Local::now().format("%Y%m%d-%H%M%S"),
            extension
        );
        for (index, c) in name.encode_utf16().enumerate() {
            path[index] = c;
        }
    }
    let (kind, open_title) = if extension == "exe" {
        ("应用程序", "选择要设置规则的应用程序")
    } else {
        ("Timelens 文件", "选择 Timelens 备份")
    };
    let filter: Vec<u16> = format!("{kind} (*.{extension})\0*.{extension}\0\0")
        .encode_utf16()
        .collect();
    let ext: Vec<u16> = extension.encode_utf16().chain(Some(0)).collect();
    let title: Vec<u16> = if save {
        "导出 Timelens 文件"
    } else {
        open_title
    }
    .encode_utf16()
    .chain(Some(0))
    .collect();
    let mut dialog = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        lpstrFile: path.as_mut_ptr(),
        nMaxFile: path.len() as u32,
        lpstrFilter: filter.as_ptr(),
        lpstrDefExt: ext.as_ptr(),
        lpstrTitle: title.as_ptr(),
        Flags: OFN_NOCHANGEDIR
            | OFN_PATHMUSTEXIST
            | OFN_EXPLORER
            | if save { 0 } else { OFN_FILEMUSTEXIST },
        ..Default::default()
    };
    let okay = unsafe {
        if save {
            GetSaveFileNameW(&mut dialog)
        } else {
            GetOpenFileNameW(&mut dialog)
        }
    };
    if okay == 0 {
        let code = unsafe { CommDlgExtendedError() };
        if code != 0 {
            bail!("文件选择窗口失败：{code}");
        }
        return Ok(None);
    }
    let chosen = PathBuf::from(String::from_utf16(
        &path[..path.iter().position(|c| *c == 0).unwrap_or(path.len())],
    )?);
    // Exports never overwrite; say so before any work instead of failing after it.
    if save && chosen.exists() {
        bail!("目标文件已存在。Timelens 不会覆盖已有文件，请换一个文件名");
    }
    Ok(Some(chosen))
}
/// Ask for a folder with the system folder picker.
fn choose_folder() -> Result<Option<PathBuf>> {
    use windows::{
        Win32::{
            Foundation::ERROR_CANCELLED,
            System::Com::{
                CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
                CoTaskMemFree,
            },
            UI::Shell::{
                FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST, FOS_PICKFOLDERS, FileOpenDialog,
                IFileOpenDialog, SIGDN_FILESYSPATH,
            },
        },
        core::{HRESULT, HSTRING},
    };
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    let dialog: IFileOpenDialog =
        unsafe { CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER) }
            .context("无法打开文件夹选择窗口")?;
    unsafe {
        dialog.SetOptions(
            dialog.GetOptions()? | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST,
        )?;
        dialog.SetTitle(&HSTRING::from("选择数据的新位置"))?;
    }
    if let Err(error) = unsafe { dialog.Show(None) } {
        if error.code() == HRESULT::from_win32(ERROR_CANCELLED.0) {
            return Ok(None);
        }
        bail!("文件夹选择窗口失败：{error}");
    }
    let name = unsafe { dialog.GetResult()?.GetDisplayName(SIGDN_FILESYSPATH)? };
    let text = unsafe { name.to_string() };
    unsafe { CoTaskMemFree(Some(name.0.cast())) };
    Ok(Some(PathBuf::from(text?)))
}
/// The directory a move fills: the chosen one when it is empty or new, or a
/// Timelens folder inside it, so any existing folder can be picked.
fn move_target(chosen: &str) -> Result<PathBuf> {
    let chosen = PathBuf::from(chosen.trim().trim_matches('"'));
    if chosen.as_os_str().is_empty() {
        bail!("请先选择新的数据位置");
    }
    if !chosen.is_dir() || fs::read_dir(&chosen)?.next().is_none() {
        return Ok(chosen);
    }
    let inner = chosen.join("Timelens");
    if inner.is_dir() && fs::read_dir(&inner)?.next().is_some() {
        bail!(
            "{} 已存在且不是空文件夹。Timelens 不会合并或覆盖已有数据，请换一个位置",
            inner.display()
        );
    }
    Ok(inner)
}
fn move_progress(step: RelocationStep, done: u64, total: u64) -> (f32, String) {
    let part = done as f32 / total.max(1) as f32;
    match step {
        RelocationStep::Checking => (0.02, "正在检查当前数据…".to_owned()),
        RelocationStep::Copying => (
            0.04 + part * 0.78,
            format!(
                "正在复制 {} / {}",
                crate::format_bytes(done),
                crate::format_bytes(total)
            ),
        ),
        RelocationStep::Verifying => (0.82 + part * 0.14, format!("正在校验快照 {done} / {total}")),
        RelocationStep::Switching => (0.96 + part * 0.04, "正在切换到新位置…".to_owned()),
    }
}
pub struct CollectorPause {
    request: PathBuf,
    paused: PathBuf,
    _absent: Option<SingleInstanceGuard>,
}
impl Drop for CollectorPause {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.request);
        let start = Instant::now();
        while self.paused.exists() && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
pub fn pause_collector(directory: &Path) -> Result<CollectorPause> {
    // Owning the collector mutex proves no collector can start during replacement.
    let absent = SingleInstanceGuard::acquire_collector().ok();
    let request = directory.join(COLLECTOR_RESET_REQUEST_FILE);
    let paused = directory.join(COLLECTOR_RESET_PAUSED_FILE);
    let mut f = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&request)
        .context("另一个数据维护操作正在进行")?;
    f.write_all(b"replace-dataset")?;
    f.sync_all()?;
    let guard = CollectorPause {
        request,
        paused: paused.clone(),
        _absent: absent,
    };
    if guard._absent.is_none() {
        let start = Instant::now();
        while !paused.exists() {
            if start.elapsed() > Duration::from_secs(10) {
                bail!("采集器未确认暂停，未替换数据");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    Ok(guard)
}
enum Update {
    Path(PathBuf),
    Folder(PathBuf),
    Ready(String),
    Done(String),
    Failed(String),
    Canceled,
    Progress(f32, String),
    Moved(String),
    MoveFailed(String),
}
pub fn install(
    w: &AppWindow,
    storage: Arc<Mutex<Storage>>,
    timeline: Rc<RefCell<UiState>>,
    snapshots: Rc<RefCell<SnapshotUiState>>,
) -> Timer {
    let stage = Arc::new(Mutex::new(None::<PreparedRestore>));
    let ui_storage = Arc::clone(&storage);
    if let Ok(s) = lock(&storage) {
        w.global::<DataState>()
            .set_current_directory(s.data_directory().display().to_string().into());
    }
    let (sender, receiver) = mpsc::channel();
    let weak = w.as_weak();
    w.global::<DataState>().on_action(move |action| {
        let Some(w) = weak.upgrade() else {
            return;
        };
        let g = w.global::<DataState>();
        if g.get_busy() {
            return;
        }
        let path = PathBuf::from(g.get_archive().as_str());
        let destination = g.get_destination().to_string();
        let password = g.get_password().to_string();
        let images = g.get_snapshots();
        let confirm = g.get_replace_confirmed();
        let paths = g.get_export_paths();
        let range = {
            let t = timeline.borrow();
            (t.range_started_utc_ms, t.range_ended_utc_ms)
        };
        let slot = snapshots.borrow().selected_slot_id;
        if action == 10 {
            let result: Result<()> = (|| {
                use std::os::windows::process::CommandExt;
                let control = lock(&storage)?.control_directory().to_path_buf();
                std::process::Command::new(std::env::current_exe()?)
                    .args(["--recover", "--data-dir"])
                    .arg(control)
                    .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW)
                    .spawn()?;
                crate::tray::activate_existing(true);
                Ok(())
            })();
            if let Err(e) = result {
                g.set_status(format!("无法进入恢复：{e}").into());
            }
            return;
        }
        let storage = storage.clone();
        let sender = sender.clone();
        let stage = stage.clone();
        g.set_busy(true);
        g.set_status("".into());
        if action == 9 {
            g.set_move_progress(0.0);
            g.set_move_step("正在准备…".into());
            g.set_move_phase(1);
        }
        std::thread::spawn(move || {
            let result: Result<Update> = (|| {
                if action == 0 || action == 1 {
                    return Ok(
                        choose_file(action == 0, "zip")?.map_or(Update::Canceled, Update::Path)
                    );
                }
                if action == 11 {
                    return Ok(choose_folder()?.map_or(Update::Canceled, Update::Folder));
                }
                let password = if password.is_empty() {
                    None
                } else {
                    Some(password.as_str())
                };
                match action {
                    2 => crate::snapshot::run_heavy_task(|| {
                        let info = lock(&storage)?.export_backup(
                            &path,
                            BackupOptions {
                                include_snapshots: images,
                                password,
                            },
                        )?;
                        Ok(Update::Done(format!(
                            "已导出 {} 个文件到 {}",
                            info.files.len(),
                            path.display()
                        )))
                    }),
                    3 => crate::snapshot::run_heavy_task(|| {
                        let parent = lock(&storage)?
                            .data_directory()
                            .parent()
                            .context("数据父目录不可用")?
                            .to_path_buf();
                        let prepared = Storage::prepare_restore(&path, password, &parent)?;
                        let info = format!(
                            "校验通过 · 格式 {} · 数据结构 {} · {} 个文件 · {}。\n来源：{}",
                            prepared.info.format_version,
                            prepared.info.schema_version,
                            prepared.info.files.len(),
                            if prepared.info.includes_snapshots {
                                "包含快照"
                            } else {
                                "不含快照；相关槽将显示已省略"
                            },
                            path.display()
                        );
                        *stage.lock().map_err(|_| anyhow!("恢复锁不可用"))? = Some(prepared);
                        Ok(Update::Ready(info))
                    }),
                    4 => {
                        if !confirm {
                            bail!("请确认整体替换");
                        }
                        let prepared = stage
                            .lock()
                            .map_err(|_| anyhow!("恢复锁不可用"))?
                            .take()
                            .context("请先重新校验备份")?;
                        let _ai = crate::ai::pause_and_wait()?;
                        crate::snapshot::run_heavy_task(|| {
                            let directory = lock(&storage)?.control_directory().to_path_buf();
                            let _collector = pause_collector(&directory)?;
                            lock(&storage)?.replace_from_backup(prepared, true)?;
                            crate::ai_ui::dataset_changed();
                            Ok(Update::Done(
                                "数据集已恢复。请重新配置 AI 凭据、测试连接后启用计划。".into(),
                            ))
                        })
                    }
                    5 | 6 => {
                        let Some(path) =
                            choose_file(true, if action == 5 { "json" } else { "csv" })?
                        else {
                            return Ok(Update::Canceled);
                        };
                        crate::snapshot::run_heavy_task(|| {
                            let s = lock(&storage)?;
                            let report =
                                s.generate_local_report(range.0, range.1, crate::unix_time_ms())?;
                            s.export_report(
                                report.id,
                                &path,
                                if action == 5 {
                                    ExportFormat::Json
                                } else {
                                    ExportFormat::Csv
                                },
                                paths,
                            )?;
                            Ok(Update::Done(format!("报告已导出到 {}", path.display())))
                        })
                    }
                    7 => {
                        let id = slot.context("请先在快照面板选择一张可用图片")?;
                        let Some(path) = choose_file(true, "webp")? else {
                            return Ok(Update::Canceled);
                        };
                        crate::snapshot::run_heavy_task(|| {
                            let bytes = lock(&storage)?.load_snapshot_image(id)?.webp;
                            let mut file = fs::OpenOptions::new()
                                .create_new(true)
                                .write(true)
                                .open(&path)?;
                            file.write_all(&bytes)?;
                            file.sync_all()?;
                            Ok(Update::Done(format!("快照已导出到 {}", path.display())))
                        })
                    }
                    8 => crate::snapshot::run_heavy_task(|| {
                        lock(&storage)?.verify_integrity()?;
                        Ok(Update::Done("完整性与外键检查通过".into()))
                    }),
                    9 => {
                        let moved = (|| {
                            let target = move_target(&destination)?;
                            let _ai = crate::ai::pause_and_wait()?;
                            crate::snapshot::run_heavy_task(|| {
                                let control = lock(&storage)?.control_directory().to_path_buf();
                                let _collector = pause_collector(&control)?;
                                let mut shown = (-1.0_f32, Instant::now());
                                let mut guard = lock(&storage)?;
                                let remaining = guard.relocate(
                                    &target,
                                    |path| crate::local_config::publish(&control, path),
                                    |step, done, total| {
                                        let (fraction, label) = move_progress(step, done, total);
                                        // A few updates a second are plenty for the bar.
                                        if fraction - shown.0 >= 0.01
                                            || shown.1.elapsed() >= Duration::from_millis(150)
                                        {
                                            shown = (fraction, Instant::now());
                                            let _ = sender.send(Update::Progress(fraction, label));
                                        }
                                    },
                                )?;
                                let location = guard.data_directory().display().to_string();
                                let suffix = if remaining == 0 {
                                    "原位置的数据已清理，记录已恢复。".to_owned()
                                } else {
                                    format!(
                                        "原位置有 {remaining} 个文件未能删除，可以稍后手动检查。"
                                    )
                                };
                                Ok(format!("数据现在保存在：\n{location}\n{suffix}"))
                            })
                        })();
                        Ok(match moved {
                            Ok(message) => Update::Moved(message),
                            Err(error) => Update::MoveFailed(format!(
                                "{error:#}\n原来的数据没有改动，Timelens 会继续使用原位置。"
                            )),
                        })
                    }
                    _ => bail!("未知操作"),
                }
            })();
            let update = result.unwrap_or_else(|e| Update::Failed(e.to_string()));
            let _ = sender.send(update);
        });
    });
    let weak = w.as_weak();
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(200), move || {
        let Some(w) = weak.upgrade() else {
            return;
        };
        let g = w.global::<DataState>();
        while let Ok(update) = receiver.try_recv() {
            if !matches!(update, Update::Progress(..)) {
                g.set_busy(false);
            }
            match update {
                Update::Progress(fraction, label) => {
                    g.set_move_progress(fraction);
                    g.set_move_step(label.into());
                }
                Update::Moved(message) => {
                    g.set_move_progress(1.0);
                    g.set_move_message(message.into());
                    g.set_move_phase(2);
                    g.set_destination("".into());
                    if let Ok(s) = lock(&ui_storage) {
                        let path = s.data_directory().display().to_string();
                        g.set_current_directory(path.clone().into());
                        w.set_data_path(path.into());
                    }
                }
                Update::MoveFailed(message) => {
                    g.set_move_message(message.into());
                    g.set_move_phase(3);
                }
                Update::Folder(path) => {
                    g.set_destination(path.to_string_lossy().into_owned().into());
                }
                Update::Path(path) => {
                    g.set_archive(path.to_string_lossy().into_owned().into());
                    g.set_restore_ready(false);
                    g.set_restore_info("".into());
                }
                Update::Ready(info) => {
                    g.set_restore_info(info.into());
                    g.set_restore_ready(true);
                    g.set_replace_confirmed(false);
                }
                Update::Done(message) => {
                    g.set_status(message.into());
                    if let Ok(s) = lock(&ui_storage) {
                        let path = s.data_directory().display().to_string();
                        g.set_current_directory(path.clone().into());
                        w.set_data_path(path.into());
                    }
                    g.set_restore_ready(false);
                    g.set_password("".into());
                }
                Update::Failed(message) => {
                    g.set_status(format!("操作失败：{message}").into());
                    g.set_restore_ready(false);
                }
                Update::Canceled => {}
            }
        }
    });
    timer
}
