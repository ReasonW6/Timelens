use rusqlite::{Connection, params};
use serde::Serialize;
use std::{
    env, fs,
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, TRUE},
    Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC,
        DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, HGDIOBJ, ReleaseDC, SRCCOPY,
        SelectObject,
    },
    UI::{
        Shell::{NIF_ICON, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW},
        WindowsAndMessaging::{
            EnumWindows, FindWindowW, GetForegroundWindow, GetSystemMetrics, GetWindowTextLengthW,
            GetWindowTextW, IDI_APPLICATION, IsWindowVisible, LoadIconW, SM_CXSCREEN, SM_CYSCREEN,
            SW_HIDE, SW_RESTORE, SetForegroundWindow, ShowWindow,
        },
    },
};

const SEGMENT_COUNT: usize = 10_000;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TimelineSegment {
    id: usize,
    start_minute: u32,
    duration_minutes: u32,
    lane: u8,
    state: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RunMetrics {
    segment_count: usize,
    sampled_window_count: usize,
    focus_sample_count: usize,
    database_bytes: u64,
    snapshot_bytes: u64,
    elapsed_ms: u128,
}

struct Options {
    out_dir: PathBuf,
    ui_title: String,
    duration: Duration,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let options = parse_options()?;
    fs::create_dir_all(&options.out_dir)?;
    let started = Instant::now();

    let segments = generate_segments();
    fs::write(
        options.out_dir.join("timeline.json"),
        serde_json::to_vec(&segments)?,
    )?;

    let database_path = options.out_dir.join("workload.sqlite3");
    let mut connection = Connection::open(&database_path)?;
    initialize_database(&mut connection, &segments)?;
    fs::write(options.out_dir.join("ready"), b"ready")?;

    let ui_window = wait_for_window(&options.ui_title, Duration::from_secs(15));
    if !ui_window.is_null() {
        tray_cycle(ui_window)?;
    }

    let snapshot_path = options.out_dir.join("snapshot.webp");
    capture_primary_display(&snapshot_path)?;

    let (sampled_window_count, focus_sample_count) =
        observe_windows(&connection, options.duration)?;
    connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;

    let metrics = RunMetrics {
        segment_count: segments.len(),
        sampled_window_count,
        focus_sample_count,
        database_bytes: fs::metadata(&database_path)?.len(),
        snapshot_bytes: fs::metadata(&snapshot_path)?.len(),
        elapsed_ms: started.elapsed().as_millis(),
    };
    fs::write(
        options.out_dir.join("driver-metrics.json"),
        serde_json::to_vec_pretty(&metrics)?,
    )?;
    Ok(())
}

fn parse_options() -> Result<Options, String> {
    let mut args = env::args().skip(1);
    let mut out_dir = None;
    let mut ui_title = None;
    let mut duration_seconds = 30;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--out" => out_dir = args.next().map(PathBuf::from),
            "--ui-title" => ui_title = args.next(),
            "--duration-seconds" => {
                duration_seconds = args
                    .next()
                    .ok_or("--duration-seconds requires a value")?
                    .parse()
                    .map_err(|_| "--duration-seconds must be an integer")?;
            }
            _ => return Err(format!("unknown argument: {argument}")),
        }
    }
    Ok(Options {
        out_dir: out_dir.ok_or("--out is required")?,
        ui_title: ui_title.ok_or("--ui-title is required")?,
        duration: Duration::from_secs(duration_seconds),
    })
}

fn generate_segments() -> Vec<TimelineSegment> {
    (0..SEGMENT_COUNT)
        .map(|id| TimelineSegment {
            id,
            start_minute: ((id * 7) % (30 * 24 * 60)) as u32,
            duration_minutes: (2 + (id % 31)) as u32,
            lane: (id % 12) as u8,
            state: match id % 3 {
                0 => "displayed",
                1 => "focused",
                _ => "background",
            },
        })
        .collect()
}

fn initialize_database(
    connection: &mut Connection,
    segments: &[TimelineSegment],
) -> rusqlite::Result<()> {
    connection.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         CREATE TABLE IF NOT EXISTS timeline (
             id INTEGER PRIMARY KEY,
             start_minute INTEGER NOT NULL,
             duration_minutes INTEGER NOT NULL,
             lane INTEGER NOT NULL,
             state TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS input_minute (
             minute INTEGER PRIMARY KEY,
             left_clicks INTEGER NOT NULL,
             middle_clicks INTEGER NOT NULL,
             right_clicks INTEGER NOT NULL,
             key_count INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS window_sample (
             sampled_at_ms INTEGER NOT NULL,
             visible_count INTEGER NOT NULL,
             focused_window INTEGER NOT NULL
         );
         DELETE FROM timeline;
         DELETE FROM input_minute;
         DELETE FROM window_sample;",
    )?;
    let transaction = connection.transaction()?;
    {
        let mut insert = transaction.prepare_cached(
            "INSERT INTO timeline(id, start_minute, duration_minutes, lane, state)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for segment in segments {
            insert.execute(params![
                segment.id,
                segment.start_minute,
                segment.duration_minutes,
                segment.lane,
                segment.state
            ])?;
        }
        let mut input = transaction.prepare_cached(
            "INSERT INTO input_minute(minute, left_clicks, middle_clicks, right_clicks, key_count)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for minute in 0..1_440_u32 {
            input.execute(params![
                minute,
                (minute * 3) % 41,
                minute % 3,
                minute % 11,
                (minute * 17) % 233
            ])?;
        }
    }
    transaction.commit()
}

fn observe_windows(
    connection: &Connection,
    duration: Duration,
) -> Result<(usize, usize), Box<dyn std::error::Error>> {
    let until = Instant::now() + duration;
    let mut sampled_window_count = 0;
    let mut focus_sample_count = 0;
    while Instant::now() < until {
        let mut titles = Vec::<String>::new();
        unsafe {
            EnumWindows(Some(enum_window), &mut titles as *mut _ as LPARAM);
        }
        let foreground = unsafe { GetForegroundWindow() };
        if !foreground.is_null() {
            focus_sample_count += 1;
        }
        sampled_window_count += titles.len();
        connection.execute(
            "INSERT INTO window_sample(sampled_at_ms, visible_count, focused_window)
             VALUES (?1, ?2, ?3)",
            params![unix_millis(), titles.len(), foreground as isize],
        )?;
        thread::sleep(Duration::from_millis(500));
    }
    Ok((sampled_window_count, focus_sample_count))
}

unsafe extern "system" fn enum_window(window: HWND, context: LPARAM) -> i32 {
    if unsafe { IsWindowVisible(window) } == 0 {
        return TRUE;
    }
    let length = unsafe { GetWindowTextLengthW(window) };
    if length <= 0 {
        return TRUE;
    }
    let mut buffer = vec![0_u16; length as usize + 1];
    let copied = unsafe { GetWindowTextW(window, buffer.as_mut_ptr(), buffer.len() as i32) };
    if copied > 0 {
        let titles = unsafe { &mut *(context as *mut Vec<String>) };
        titles.push(String::from_utf16_lossy(&buffer[..copied as usize]));
    }
    TRUE
}

fn wait_for_window(title: &str, timeout: Duration) -> HWND {
    let title = wide(title);
    let until = Instant::now() + timeout;
    while Instant::now() < until {
        let window = unsafe { FindWindowW(null(), title.as_ptr()) };
        if !window.is_null() {
            return window;
        }
        thread::sleep(Duration::from_millis(100));
    }
    null_mut()
}

fn tray_cycle(window: HWND) -> Result<(), Box<dyn std::error::Error>> {
    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: window,
        uID: 1,
        uFlags: NIF_ICON | NIF_TIP,
        hIcon: unsafe { LoadIconW(null_mut(), IDI_APPLICATION) },
        ..unsafe { std::mem::zeroed() }
    };
    let tip = wide("Timelens architecture spike");
    let copy_length = tip.len().min(data.szTip.len());
    data.szTip[..copy_length].copy_from_slice(&tip[..copy_length]);
    unsafe {
        Shell_NotifyIconW(NIM_ADD, &data);
        ShowWindow(window, SW_HIDE);
    }
    thread::sleep(Duration::from_millis(500));
    unsafe {
        ShowWindow(window, SW_RESTORE);
        SetForegroundWindow(window);
        Shell_NotifyIconW(NIM_DELETE, &data);
    }
    Ok(())
}

fn capture_primary_display(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let source_width = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let source_height = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    if source_width <= 0 || source_height <= 0 {
        return Err("primary display has invalid dimensions".into());
    }

    let screen_dc = unsafe { GetDC(null_mut()) };
    if screen_dc.is_null() {
        return Err("GetDC failed".into());
    }
    let memory_dc = unsafe { CreateCompatibleDC(screen_dc) };
    let bitmap = unsafe { CreateCompatibleBitmap(screen_dc, source_width, source_height) };
    if memory_dc.is_null() || bitmap.is_null() {
        unsafe { ReleaseDC(null_mut(), screen_dc) };
        return Err("failed to allocate capture surface".into());
    }
    let previous = unsafe { SelectObject(memory_dc, bitmap as HGDIOBJ) };
    let copied = unsafe {
        BitBlt(
            memory_dc,
            0,
            0,
            source_width,
            source_height,
            screen_dc,
            0,
            0,
            SRCCOPY,
        )
    };
    if copied == 0 {
        cleanup_capture(screen_dc, memory_dc, bitmap as HGDIOBJ, previous);
        return Err("BitBlt failed".into());
    }

    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: source_width,
            biHeight: -source_height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..unsafe { std::mem::zeroed() }
        },
        ..unsafe { std::mem::zeroed() }
    };
    let mut bgra = vec![0_u8; (source_width * source_height * 4) as usize];
    let scan_lines = unsafe {
        GetDIBits(
            memory_dc,
            bitmap,
            0,
            source_height as u32,
            bgra.as_mut_ptr().cast(),
            &mut info,
            DIB_RGB_COLORS,
        )
    };
    cleanup_capture(screen_dc, memory_dc, bitmap as HGDIOBJ, previous);
    if scan_lines == 0 {
        return Err("GetDIBits failed".into());
    }

    let target_width = source_width.min(1280) as usize;
    let target_height = (source_height as i64 * target_width as i64 / source_width as i64) as usize;
    let mut rgba = vec![0_u8; target_width * target_height * 4];
    for y in 0..target_height {
        let source_y = y * source_height as usize / target_height;
        for x in 0..target_width {
            let source_x = x * source_width as usize / target_width;
            let source = (source_y * source_width as usize + source_x) * 4;
            let target = (y * target_width + x) * 4;
            rgba[target] = bgra[source + 2];
            rgba[target + 1] = bgra[source + 1];
            rgba[target + 2] = bgra[source];
            rgba[target + 3] = 255;
        }
    }
    let encoded =
        webp::Encoder::from_rgba(&rgba, target_width as u32, target_height as u32).encode(75.0);
    fs::write(path, encoded.as_ref())?;
    Ok(())
}

fn cleanup_capture(
    screen_dc: *mut std::ffi::c_void,
    memory_dc: *mut std::ffi::c_void,
    bitmap: HGDIOBJ,
    previous: HGDIOBJ,
) {
    unsafe {
        SelectObject(memory_dc, previous);
        DeleteObject(bitmap);
        DeleteDC(memory_dc);
        ReleaseDC(null_mut(), screen_dc);
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
