//! Restarts the elevated collector through its scheduled task.
//!
//! The collector task only triggers at logon. After the tray's Quit, a crash that
//! outlasts the task's restart budget, or a core opened later from the Start menu,
//! nothing else would start it again and the timeline would silently stop growing.
//! The task already carries the elevation, so running it needs no UAC prompt.

use std::{
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::Duration,
};

use timelens_ipc::SingleInstanceGuard;
use windows::{
    Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND},
        System::{
            Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx},
            TaskScheduler::{ITaskService, TASK_STATE_QUEUED, TASK_STATE_RUNNING, TaskScheduler},
            Variant::VARIANT,
        },
    },
    core::{BSTR, HRESULT},
};

pub const DEFAULT_TASK: &str = r"\Timelens\Collector";
/// A logon starts the core and collector tasks together; give the collector time
/// to take its mutex before treating it as missing.
const STARTUP_GRACE: Duration = Duration::from_secs(15);
const CHECK_INTERVAL: Duration = Duration::from_secs(60);
const MAX_BACKOFF_DOUBLINGS: u32 = 5;

static SUSPENDED: AtomicBool = AtomicBool::new(false);

/// Stop restarting the collector while the core is shutting it down on purpose.
pub fn suspend() {
    SUSPENDED.store(true, Ordering::Release);
}

pub fn resume() {
    SUSPENDED.store(false, Ordering::Release);
}

/// Accept only the product's own collector tasks, including isolated acceptance
/// installs such as `\Timelens-Acceptance-<id>\Collector`.
pub fn valid_task_path(path: &str) -> bool {
    path.starts_with(r"\Timelens")
        && path.ends_with(r"\Collector")
        && path.matches('\\').count() == 2
}

pub fn spawn_watchdog(task_path: String) {
    let spawned = thread::Builder::new()
        .name("timelens-collector-watchdog".to_owned())
        .spawn(move || watchdog(&task_path));
    if let Err(error) = spawned {
        eprintln!("collector watchdog could not start: {error}");
    }
}

fn watchdog(task_path: &str) {
    if let Err(error) = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok() {
        eprintln!("collector watchdog could not initialize COM: {error}");
        return;
    }
    let mut consecutive_starts = 0_u32;
    let mut reported_missing_task = false;
    thread::sleep(STARTUP_GRACE);
    loop {
        if !SUSPENDED.load(Ordering::Acquire) {
            match SingleInstanceGuard::collector_running() {
                Ok(true) => consecutive_starts = 0,
                Ok(false) => {
                    consecutive_starts = consecutive_starts.saturating_add(1);
                    match start_task(task_path) {
                        Ok(()) => println!("started the collector task {task_path}"),
                        Err(error) if task_missing(&error) => {
                            if !reported_missing_task {
                                eprintln!("collector task {task_path} is not registered");
                                reported_missing_task = true;
                            }
                        }
                        Err(error) => {
                            eprintln!("collector task {task_path} did not start: {error}")
                        }
                    }
                }
                Err(error) => {
                    eprintln!("collector watchdog could not check the collector: {error}")
                }
            }
        }
        // A collector that keeps exiting, or a task that cannot start, is retried
        // with exponential backoff instead of once a minute forever.
        let doublings = consecutive_starts
            .saturating_sub(1)
            .min(MAX_BACKOFF_DOUBLINGS);
        thread::sleep(CHECK_INTERVAL * (1 << doublings));
    }
}

/// A missing task reports file-not-found, and a missing task folder path-not-found.
fn task_missing(error: &windows::core::Error) -> bool {
    [ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND]
        .iter()
        .any(|status| error.code() == HRESULT::from_win32(status.0))
}

fn start_task(task_path: &str) -> windows::core::Result<()> {
    let (folder, name) = task_path
        .rsplit_once('\\')
        .filter(|(folder, _)| !folder.is_empty())
        .ok_or_else(|| {
            windows::core::Error::from_hresult(HRESULT::from_win32(ERROR_FILE_NOT_FOUND.0))
        })?;
    unsafe {
        let service: ITaskService = CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER)?;
        let local = VARIANT::default();
        service.Connect(&local, &local, &local, &local)?;
        let task = service
            .GetFolder(&BSTR::from(folder))?
            .GetTask(&BSTR::from(name))?;
        let state = task.State()?;
        if state == TASK_STATE_RUNNING || state == TASK_STATE_QUEUED {
            return Ok(());
        }
        task.Run(&VARIANT::default())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_product_collector_tasks_are_accepted() {
        assert!(valid_task_path(DEFAULT_TASK));
        assert!(valid_task_path(
            r"\Timelens-Acceptance-0f87369f-a4e5\Collector"
        ));
        assert!(!valid_task_path(r"\Timelens\Core"));
        assert!(!valid_task_path(r"\Microsoft\Windows\Collector"));
        assert!(!valid_task_path(r"\Timelens\Nested\Collector"));
    }
}
