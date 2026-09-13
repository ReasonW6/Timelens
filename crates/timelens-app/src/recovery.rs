use anyhow::{Context, Result, anyhow, bail};
use slint::{ComponentHandle, Timer, TimerMode};
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};
use timelens_storage::Storage;

slint::slint! { export { RecoveryWindow } from "ui/recovery.slint"; }

enum Update {
    Ready(Storage, String),
    Opened(Storage),
    Archive(PathBuf),
    Upgrade(String),
    Failed(String),
    Canceled,
}

pub fn open(data: Result<PathBuf>, control: &Path, force: bool) -> Result<Storage> {
    let (source, detail, upgrade) = match data {
        Ok(source) => {
            let check = Storage::migration_notice(&source);
            match check {
                Ok(Some(notice)) if !force => (source, notice, true),
                Ok(_) if !force => match Storage::open(&source) {
                    Ok(storage) => return Ok(storage),
                    Err(error) => (source, error.to_string(), false),
                },
                Ok(_) => (
                    source,
                    "你可以检查原位置，或将数据恢复到另一个目录。".into(),
                    false,
                ),
                Err(error) => (source, error.to_string(), false),
            }
        }
        Err(error) => (control.into(), error.to_string(), false),
    };
    let window = RecoveryWindow::new()?;
    window.set_source(source.display().to_string().into());
    window.set_destination(
        source
            .parent()
            .unwrap_or(control)
            .join(format!(
                "Timelens-recovery-{}",
                chrono::Local::now().format("%Y%m%d-%H%M%S")
            ))
            .display()
            .to_string()
            .into(),
    );
    window.set_detail(detail.into());
    window.set_upgrade(upgrade);
    let candidate = Arc::new(Mutex::new(None::<Storage>));
    let candidate_for_updates = Arc::clone(&candidate);
    let (sender, receiver) = mpsc::channel();
    let control = control.to_path_buf();
    let weak = window.as_weak();
    window.on_action(move|action|{
        let Some(w)=weak.upgrade()else{return;};if w.get_busy(){return;}
        let source=PathBuf::from(w.get_source().as_str());
        let destination=PathBuf::from(w.get_destination().as_str());
        let archive=PathBuf::from(w.get_archive().as_str());
        let password=zeroize::Zeroizing::new(w.get_password().to_string());
        let candidate=Arc::clone(&candidate);let sender=sender.clone();let control=control.clone();
        w.set_busy(true);w.set_status("".into());
        std::thread::spawn(move||{
            let result:Result<Update>=(||{
                if action==6{return Ok(crate::data_ui::choose_file(false,"zip")?.map_or(Update::Canceled,Update::Archive));}
                if action==5 && let Some(notice)=Storage::migration_notice(&source)? { return Ok(Update::Upgrade(notice)); }
                if action==0||action==5{
                    let storage=Storage::open(&source)?;
                    if !crate::local_config::resolve(&control).is_ok_and(|current|current==source){crate::local_config::publish(&control,&source)?;}
                    return Ok(Update::Opened(storage));
                }
                std::fs::create_dir_all(&control)?;
                let _collector=crate::data_ui::pause_collector(&control)?;
                if action==4 {
                    let mut storage=candidate.lock().map_err(|_|anyhow!("恢复状态不可用"))?.take().context("请先校验一个新数据集")?;
                    storage.set_control_directory(&control)?;
                    crate::local_config::publish(&control,storage.data_directory())?;
                    return Ok(Update::Opened(storage));
                }
                let destination=Storage::validate_data_destination(&destination)?;
                let source=Storage::validate_data_destination(&source)?;
                if destination==source||destination.starts_with(&source)||source.starts_with(&destination){bail!("新旧目录必须分开，不能互相包含");}
                if destination.exists()&&std::fs::read_dir(&destination)?.next().is_some(){bail!("请选择空的新目录");}
                let parent=destination.parent().context("新目录无效")?;std::fs::create_dir_all(parent)?;
                match action {
                    1=>{
                        let prepared=Storage::prepare_restore(&archive,if password.is_empty(){None}else{Some(password.as_str())},parent)?;
                        let storage=Storage::open(&destination)?;
                        storage.replace_from_backup(prepared,true)?;storage.verify_integrity()?;
                        Ok(Update::Ready(storage,"备份已恢复并通过校验。切换后请重新配置凭据、测试连接并启用 AI 计划。".into()))
                    },
                    2=>Ok(Update::Ready(Storage::open(&destination)?,"已建立空数据集。原件继续保留在原位置。".into())),
                    3=>{
                        let(storage,report)=Storage::recover_to_new_directory(&source,&destination)?;
                        Ok(Update::Ready(storage,format!("已恢复 {} 行；已知无效记录 {} 行，无法完整读取的表 {} 个，缺失图片 {} 张。采集与快照已暂停，请先检查排除规则。",report.copied_rows,report.discarded_rows,report.unreadable_tables,report.unavailable_images)))
                    },
                    _=>bail!("未知恢复操作"),
                }
            })();let _=sender.send(result.unwrap_or_else(|e|Update::Failed(e.to_string())));
        });
    });
    let selected = Rc::new(RefCell::new(None));
    let returned = Rc::clone(&selected);
    let weak = window.as_weak();
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(100), move || {
        let Some(w) = weak.upgrade() else {
            return;
        };
        while let Ok(update) = receiver.try_recv() {
            w.set_busy(false);
            match update {
                Update::Ready(storage, info) => {
                    w.set_status(
                        format!("{info}\n新位置：{}", storage.data_directory().display()).into(),
                    );
                    if let Ok(mut candidate) = candidate_for_updates.lock() {
                        *candidate = Some(storage);
                        w.set_ready(true);
                    }
                    w.set_password("".into());
                }
                Update::Opened(storage) => {
                    *returned.borrow_mut() = Some(storage);
                    let _ = w.hide();
                }
                Update::Upgrade(info) => {
                    w.set_detail(info.into());
                    w.set_upgrade(true);
                }
                Update::Archive(path) => w.set_archive(path.display().to_string().into()),
                Update::Failed(error) => {
                    w.set_status(format!("未切换数据位置：{error}").into());
                    w.set_upgrade(false);
                }
                Update::Canceled => {}
            }
        }
    });
    window.show()?;
    crate::window_placement::fit_after_show(&window);
    window.run()?;
    selected
        .borrow_mut()
        .take()
        .ok_or_else(|| anyhow!("用户关闭了数据恢复窗口；原件仍保留"))
}
