//! A local diagnostic log for the release builds, which run without a console.
//!
//! Each process appends to one file in the control directory, which keeps its
//! place when the dataset moves (`%LOCALAPPDATA%\Timelens` by default): whatever it writes to
//! stderr, one timestamped line at a time, a panic message, and the error that
//! ended the process. A panic is written directly, because `panic = "abort"`
//! ends the process right after the hook returns. The file never leaves this PC
//! and starts over once it outgrows [`MAX_LOG_BYTES`].
//!
//! The elevated collector writes into a directory the signed-in user can modify,
//! so the file is opened without following links and left alone when it is a
//! reparse point or has another hard link.

use std::{
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Seek, SeekFrom, Write},
    mem::zeroed,
    os::windows::{
        fs::OpenOptionsExt,
        io::{AsRawHandle, FromRawHandle},
    },
    panic,
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    sync::{Mutex, OnceLock},
    thread,
};

use windows_sys::Win32::{
    Foundation::{CloseHandle, SYSTEMTIME},
    Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT,
        GetFileInformationByHandle,
    },
    System::{
        Console::{GetStdHandle, STD_ERROR_HANDLE, SetStdHandle},
        Pipes::CreatePipe,
        SystemInformation::GetSystemTime,
    },
};

/// The core's log.
pub const CORE_LOG_FILE: &str = "timelens.log";
/// The collector's log.
pub const COLLECTOR_LOG_FILE: &str = "collector.log";
const MAX_LOG_BYTES: u64 = 1024 * 1024;

static LOG: OnceLock<PathBuf> = OnceLock::new();
static WRITE: Mutex<()> = Mutex::new(());

/// Starts logging this process to `path`. Stderr is captured only when it goes
/// nowhere, so a console or a redirected stream keeps its output.
pub fn install(path: PathBuf, process: &str) {
    if LOG.set(path).is_err() {
        return;
    }
    record(&format!(
        "{process} {} started, pid {}",
        env!("CARGO_PKG_VERSION"),
        std::process::id()
    ));
    let captured = unsafe { GetStdHandle(STD_ERROR_HANDLE) }.is_null() && capture_stderr();
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let thread = thread::current();
        record(&format!(
            "thread '{}' {info}",
            thread.name().unwrap_or("<unnamed>")
        ));
        if !captured {
            previous(info);
        }
    }));
}

/// Appends one line to the log, if this process has one.
pub fn record(text: &str) {
    let Some(path) = LOG.get() else {
        return;
    };
    let _guard = WRITE.lock().unwrap_or_else(|error| error.into_inner());
    let _ = append_line(path, text);
}

/// Points stderr at a pipe whose lines a background thread copies into the log.
/// The thread outlives every writer, so writing to stderr never fails.
fn capture_stderr() -> bool {
    let (mut read, mut write) = (null_mut(), null_mut());
    if unsafe { CreatePipe(&mut read, &mut write, null(), 0) } == 0 {
        return false;
    }
    if unsafe { SetStdHandle(STD_ERROR_HANDLE, write) } == 0 {
        unsafe {
            CloseHandle(read);
            CloseHandle(write);
        }
        return false;
    }
    let reader = unsafe { File::from_raw_handle(read) };
    thread::Builder::new()
        .name("diagnostics".into())
        .stack_size(64 * 1024)
        .spawn(move || {
            let mut reader = BufReader::new(reader);
            let mut line = Vec::new();
            while reader
                .read_until(b'\n', &mut line)
                .is_ok_and(|read| read > 0)
            {
                record(String::from_utf8_lossy(&line).trim_end());
                line.clear();
            }
        })
        .is_ok()
}

fn append_line(path: &Path, text: &str) -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    if !is_plain_file(&file) {
        return Ok(());
    }
    if file.seek(SeekFrom::End(0))? > MAX_LOG_BYTES {
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
    }
    writeln!(file, "{} {text}", timestamp())
}

fn is_plain_file(file: &File) -> bool {
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { zeroed() };
    let read = unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) };
    read != 0
        && info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0
        && info.nNumberOfLinks == 1
}

fn timestamp() -> String {
    let mut time: SYSTEMTIME = unsafe { zeroed() };
    unsafe { GetSystemTime(&mut time) };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}Z",
        time.wYear,
        time.wMonth,
        time.wDay,
        time.wHour,
        time.wMinute,
        time.wSecond,
        time.wMilliseconds
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_lines_and_starts_over_when_too_large() {
        let directory = std::env::temp_dir().join(format!("timelens-log-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(CORE_LOG_FILE);
        append_line(&path, "first").unwrap();
        append_line(&path, "second").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.lines().count() == 2 && text.ends_with(" second\n"),
            "{text}"
        );

        std::fs::write(&path, vec![b'x'; MAX_LOG_BYTES as usize + 1]).unwrap();
        append_line(&path, "fresh").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.lines().count() == 1 && text.ends_with(" fresh\n"),
            "{text}"
        );
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn leaves_a_file_with_another_hard_link_alone() {
        let directory = std::env::temp_dir().join(format!("timelens-link-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let target = directory.join("target.txt");
        std::fs::write(&target, "kept").unwrap();
        let path = directory.join(COLLECTOR_LOG_FILE);
        std::fs::hard_link(&target, &path).unwrap();
        append_line(&path, "ignored").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "kept");
        std::fs::remove_dir_all(&directory).unwrap();
    }
}
