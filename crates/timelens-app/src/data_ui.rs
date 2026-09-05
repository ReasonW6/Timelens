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
use timelens_storage::{BackupOptions, ExportFormat, PreparedRestore, Storage};

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
    let filter: Vec<u16> = format!("Timelens files\0*.{extension}\0\0")
        .encode_utf16()
        .collect();
    let ext: Vec<u16> = extension.encode_utf16().chain(Some(0)).collect();
    let title: Vec<u16> = if save {
        "导出 Timelens 文件"
    } else {
        "选择 Timelens 备份"
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
    Ok(Some(PathBuf::from(String::from_utf16(
        &path[..path.iter().position(|c| *c == 0).unwrap_or(path.len())],
    )?)))
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
    Ready(String),
    Done(String),
    Failed(String),
    Canceled,
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
        let destination = PathBuf::from(g.get_destination().as_str());
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
        std::thread::spawn(move || {
            let result: Result<Update> = (|| {
                if action == 0 || action == 1 {
                    return Ok(
                        choose_file(action == 0, "zip")?.map_or(Update::Canceled, Update::Path)
                    );
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
                        let _ai = crate::ai::pause_and_wait()?;
                        crate::snapshot::run_heavy_task(|| {
                            let control = lock(&storage)?.control_directory().to_path_buf();
                            let _collector = pause_collector(&control)?;
                            let remaining = lock(&storage)?.relocate(&destination, |path| {
                                crate::local_config::publish(&control, path)
                            })?;
                            let suffix = if remaining == 0 {
                                "原目录内部数据已清理。".to_owned()
                            } else {
                                format!("原目录有 {remaining} 个内部文件未能删除，请检查原位置。")
                            };
                            Ok(Update::Done(format!(
                                "数据已迁移到 {}，下次启动继续使用。{suffix}",
                                destination.display()
                            )))
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
            g.set_busy(false);
            match update {
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
