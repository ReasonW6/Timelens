use rusqlite::{Connection, params};
use serde::Serialize;
use std::{
    env, fs,
    hint::black_box,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    ptr::null_mut,
    time::Instant,
};
use windows_sys::Win32::{
    Foundation::HWND,
    Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC,
        DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, HGDIOBJ, ReleaseDC, SRCCOPY,
        SelectObject,
    },
    UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN},
};

const RANGE_MS: i64 = 30 * 24 * 60 * 60 * 1_000;
const APP_COUNT: usize = 100;
const APP_INTERVALS: usize = 120_000;
const WINDOW_INTERVALS: usize = 240_000;
const INPUT_MINUTES: usize = 30 * 24 * 60;
const DAILY_KEY_ROWS: usize = 30 * 256;
const SNAPSHOT_ROWS: usize = 30 * 24 * 12;
const INPUT_BURST_EVENTS: usize = 5_000_000;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PhaseMetric {
    name: &'static str,
    elapsed_ms: f64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct QueryMetric {
    name: String,
    days: u32,
    repetitions: usize,
    rows_per_repetition: usize,
    p50_ms: f64,
    p95_ms: f64,
    max_ms: f64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotMetric {
    source_width: i32,
    source_height: i32,
    encoded_width: usize,
    encoded_height: usize,
    encoded_bytes: usize,
    capture_and_encode_ms: f64,
    pixels_persisted: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InputMetric {
    event_count: usize,
    elapsed_ms: f64,
    events_per_second: f64,
    retained_counter_bytes: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AiMetric {
    protocol: &'static str,
    chunk_count: usize,
    stdout_bytes: usize,
    elapsed_ms: f64,
    exit_code: i32,
    network_used: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SuiteMetric {
    dataset_days: u32,
    app_count: usize,
    app_interval_rows: usize,
    window_interval_rows: usize,
    input_minute_rows: usize,
    daily_key_rows: usize,
    snapshot_metadata_rows: usize,
    database_bytes: u64,
    wal_bytes_after_checkpoint: u64,
    shm_bytes_after_checkpoint: u64,
    phases: Vec<PhaseMetric>,
    queries: Vec<QueryMetric>,
    snapshot: SnapshotMetric,
    input_burst: InputMetric,
    ai_stream: AiMetric,
    total_elapsed_ms: f64,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    if args.next().as_deref() == Some("--ai-worker") {
        return ai_worker();
    }

    let mut args = env::args().skip(1);
    let mut output = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--out" => output = args.next().map(PathBuf::from),
            _ => return Err(format!("unknown argument: {arg}").into()),
        }
    }
    let output = output.ok_or("--out is required")?;
    fs::create_dir_all(&output)?;
    run_suite(&output)
}

fn run_suite(output: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let total_started = Instant::now();
    let database_path = output.join("timelens-30d.sqlite3");
    let mut phases = Vec::new();

    let started = Instant::now();
    let mut connection = Connection::open(&database_path)?;
    create_dataset(&mut connection)?;
    phases.push(phase("create_30_day_database", started));

    let started = Instant::now();
    let queries = measure_queries(&connection)?;
    phases.push(phase("history_queries", started));

    let started = Instant::now();
    let input_burst = measure_input_burst();
    phases.push(phase("input_burst", started));

    let started = Instant::now();
    let snapshot = capture_and_encode_primary_display()?;
    phases.push(phase("snapshot_capture_and_encode", started));

    let started = Instant::now();
    let ai_stream = measure_ai_stream()?;
    phases.push(phase("local_ai_stream", started));

    connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    drop(connection);
    let database_bytes = file_bytes(&database_path);
    let wal_bytes_after_checkpoint = file_bytes(&database_path.with_extension("sqlite3-wal"));
    let shm_bytes_after_checkpoint = file_bytes(&database_path.with_extension("sqlite3-shm"));
    let metrics = SuiteMetric {
        dataset_days: 30,
        app_count: APP_COUNT,
        app_interval_rows: APP_INTERVALS,
        window_interval_rows: WINDOW_INTERVALS,
        input_minute_rows: INPUT_MINUTES,
        daily_key_rows: DAILY_KEY_ROWS,
        snapshot_metadata_rows: SNAPSHOT_ROWS,
        database_bytes,
        wal_bytes_after_checkpoint,
        shm_bytes_after_checkpoint,
        phases,
        queries,
        snapshot,
        input_burst,
        ai_stream,
        total_elapsed_ms: total_started.elapsed().as_secs_f64() * 1_000.0,
    };
    fs::write(
        output.join("suite-metrics.json"),
        serde_json::to_vec_pretty(&metrics)?,
    )?;
    println!("{}", serde_json::to_string_pretty(&metrics)?);
    Ok(())
}

fn phase(name: &'static str, started: Instant) -> PhaseMetric {
    PhaseMetric {
        name,
        elapsed_ms: started.elapsed().as_secs_f64() * 1_000.0,
    }
}

fn create_dataset(connection: &mut Connection) -> rusqlite::Result<()> {
    connection.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         PRAGMA temp_store=MEMORY;
         CREATE TABLE app_identity (
             id INTEGER PRIMARY KEY,
             name TEXT NOT NULL,
             path TEXT NOT NULL,
             icon BLOB NOT NULL
         );
         CREATE TABLE app_interval (
             id INTEGER PRIMARY KEY,
             app_id INTEGER NOT NULL,
             start_ms INTEGER NOT NULL,
             end_ms INTEGER NOT NULL,
             state INTEGER NOT NULL
         );
         CREATE INDEX app_interval_range ON app_interval(start_ms, end_ms, app_id);
         CREATE TABLE window_interval (
             id INTEGER PRIMARY KEY,
             app_id INTEGER NOT NULL,
             window_slot INTEGER NOT NULL,
             start_ms INTEGER NOT NULL,
             end_ms INTEGER NOT NULL,
             state INTEGER NOT NULL
         );
         CREATE INDEX window_interval_range ON window_interval(start_ms, end_ms, app_id);
         CREATE TABLE input_minute (
             minute_ms INTEGER PRIMARY KEY,
             left_clicks INTEGER NOT NULL,
             middle_clicks INTEGER NOT NULL,
             right_clicks INTEGER NOT NULL,
             key_count INTEGER NOT NULL
         );
         CREATE TABLE input_daily_key (
             day INTEGER NOT NULL,
             key_code INTEGER NOT NULL,
             press_count INTEGER NOT NULL,
             PRIMARY KEY(day, key_code)
         ) WITHOUT ROWID;
         CREATE TABLE snapshot_metadata (
             captured_ms INTEGER PRIMARY KEY,
             app_id INTEGER NOT NULL,
             encoded_bytes INTEGER NOT NULL,
             relative_path TEXT NOT NULL
         );
         CREATE TABLE local_report (
             day INTEGER PRIMARY KEY,
             generated_ms INTEGER NOT NULL,
             provider TEXT NOT NULL,
             model TEXT NOT NULL,
             markdown TEXT NOT NULL
         );",
    )?;

    let transaction = connection.transaction()?;
    let icon = vec![0x5a_u8; 16 * 1_024];
    {
        let mut insert = transaction.prepare_cached(
            "INSERT INTO app_identity(id, name, path, icon) VALUES (?1, ?2, ?3, ?4)",
        )?;
        for id in 0..APP_COUNT {
            insert.execute(params![
                id,
                format!("Synthetic App {id:03}"),
                format!(r"C:\Program Files\Synthetic\App{id:03}\application.exe"),
                &icon
            ])?;
        }
    }
    {
        let mut insert = transaction.prepare_cached(
            "INSERT INTO app_interval(id, app_id, start_ms, end_ms, state)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for id in 0..APP_INTERVALS {
            let start = ((id as i64 * 211_003) + (id as i64 % 97) * 1_003) % RANGE_MS;
            let duration = 30_000 + (id as i64 % 571) * 1_000;
            insert.execute(params![id, id % APP_COUNT, start, start + duration, id % 3])?;
        }
    }
    {
        let mut insert = transaction.prepare_cached(
            "INSERT INTO window_interval(id, app_id, window_slot, start_ms, end_ms, state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for id in 0..WINDOW_INTERVALS {
            let start = ((id as i64 * 97_409) + (id as i64 % 89) * 503) % RANGE_MS;
            let duration = 10_000 + (id as i64 % 241) * 1_000;
            insert.execute(params![
                id,
                id % APP_COUNT,
                id % 4,
                start,
                start + duration,
                id % 3
            ])?;
        }
    }
    {
        let mut insert = transaction.prepare_cached(
            "INSERT INTO input_minute(minute_ms, left_clicks, middle_clicks, right_clicks, key_count)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for minute in 0..INPUT_MINUTES {
            insert.execute(params![
                minute as i64 * 60_000,
                (minute * 3) % 41,
                minute % 3,
                minute % 11,
                (minute * 17) % 233
            ])?;
        }
    }
    {
        let mut insert = transaction.prepare_cached(
            "INSERT INTO input_daily_key(day, key_code, press_count) VALUES (?1, ?2, ?3)",
        )?;
        for day in 0..30 {
            for key_code in 0..256 {
                insert.execute(params![day, key_code, (day * 31 + key_code * 17) % 1_003])?;
            }
        }
    }
    {
        let mut insert = transaction.prepare_cached(
            "INSERT INTO snapshot_metadata(captured_ms, app_id, encoded_bytes, relative_path)
             VALUES (?1, ?2, ?3, ?4)",
        )?;
        for index in 0..SNAPSHOT_ROWS {
            insert.execute(params![
                index as i64 * 300_000,
                index % APP_COUNT,
                180_000 + (index % 80_000),
                format!(
                    "snapshots/day-{:02}/frame-{index:05}.webp",
                    index / (24 * 12) + 1
                )
            ])?;
        }
    }
    {
        let mut insert = transaction.prepare_cached(
            "INSERT INTO local_report(day, generated_ms, provider, model, markdown)
             VALUES (?1, ?2, 'synthetic', 'local-worker', ?3)",
        )?;
        let markdown = "# Synthetic report\n\n".to_string() + &"usage summary; ".repeat(512);
        for day in 0..30 {
            insert.execute(params![day, day as i64 * 86_400_000, &markdown])?;
        }
    }
    transaction.commit()
}

fn measure_queries(connection: &Connection) -> rusqlite::Result<Vec<QueryMetric>> {
    let mut metrics = Vec::new();
    for days in [1_u32, 7, 30] {
        let start = RANGE_MS - days as i64 * 86_400_000;
        metrics.push(measure_query(
            connection,
            "timeline",
            days,
            "SELECT app_id, start_ms, end_ms, state FROM window_interval
             WHERE end_ms > ?1 AND start_ms < ?2 ORDER BY start_ms LIMIT 50000",
            start,
            RANGE_MS,
        )?);
        metrics.push(measure_query(
            connection,
            "application_summary",
            days,
            "SELECT app_id, state, SUM(MIN(end_ms, ?2) - MAX(start_ms, ?1))
             FROM app_interval WHERE end_ms > ?1 AND start_ms < ?2 GROUP BY app_id, state",
            start,
            RANGE_MS,
        )?);
        metrics.push(measure_query(
            connection,
            "input_summary",
            days,
            "SELECT SUM(left_clicks), SUM(middle_clicks), SUM(right_clicks), SUM(key_count)
             FROM input_minute WHERE minute_ms >= ?1 AND minute_ms < ?2",
            start,
            RANGE_MS,
        )?);
    }
    Ok(metrics)
}

fn measure_query(
    connection: &Connection,
    name: &str,
    days: u32,
    sql: &str,
    start_ms: i64,
    end_ms: i64,
) -> rusqlite::Result<QueryMetric> {
    let mut samples = Vec::new();
    let mut row_count = 0;
    for iteration in 0..21 {
        let started = Instant::now();
        let mut statement = connection.prepare_cached(sql)?;
        let mut rows = statement.query(params![start_ms, end_ms])?;
        let mut rows_seen = 0;
        while let Some(row) = rows.next()? {
            black_box(row.get_ref(0)?);
            rows_seen += 1;
        }
        let elapsed = started.elapsed().as_secs_f64() * 1_000.0;
        if iteration > 0 {
            samples.push(elapsed);
            row_count = rows_seen;
        }
    }
    samples.sort_by(f64::total_cmp);
    Ok(QueryMetric {
        name: name.to_string(),
        days,
        repetitions: samples.len(),
        rows_per_repetition: row_count,
        p50_ms: percentile(&samples, 0.50),
        p95_ms: percentile(&samples, 0.95),
        max_ms: *samples.last().unwrap_or(&0.0),
    })
}

fn percentile(values: &[f64], percentile: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let index = ((values.len() - 1) as f64 * percentile).ceil() as usize;
    values[index]
}

fn measure_input_burst() -> InputMetric {
    let started = Instant::now();
    let mut key_counts = [0_u64; 256];
    let mut mouse_counts = [0_u64; 3];
    for index in 0..INPUT_BURST_EVENTS {
        if index % 9 == 0 {
            mouse_counts[index % mouse_counts.len()] += 1;
        } else {
            key_counts[(index * 73 + index / 11) & 255] += 1;
        }
    }
    black_box((&key_counts, &mouse_counts));
    let elapsed_ms = started.elapsed().as_secs_f64() * 1_000.0;
    InputMetric {
        event_count: INPUT_BURST_EVENTS,
        elapsed_ms,
        events_per_second: INPUT_BURST_EVENTS as f64 / (elapsed_ms / 1_000.0),
        retained_counter_bytes: std::mem::size_of_val(&key_counts)
            + std::mem::size_of_val(&mouse_counts),
    }
}

fn capture_and_encode_primary_display() -> Result<SnapshotMetric, Box<dyn std::error::Error>> {
    let started = Instant::now();
    let source_width = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let source_height = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    if source_width <= 0 || source_height <= 0 {
        return Err("primary display has invalid dimensions".into());
    }
    let screen_dc = unsafe { GetDC(null_mut()) };
    let memory_dc = unsafe { CreateCompatibleDC(screen_dc) };
    let bitmap = unsafe { CreateCompatibleBitmap(screen_dc, source_width, source_height) };
    if screen_dc.is_null() || memory_dc.is_null() || bitmap.is_null() {
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

    let encoded_width = source_width.min(1280) as usize;
    let encoded_height = source_height as usize * encoded_width / source_width as usize;
    let mut rgba = vec![0_u8; encoded_width * encoded_height * 4];
    for y in 0..encoded_height {
        let source_y = y * source_height as usize / encoded_height;
        for x in 0..encoded_width {
            let source_x = x * source_width as usize / encoded_width;
            let source = (source_y * source_width as usize + source_x) * 4;
            let target = (y * encoded_width + x) * 4;
            rgba[target] = bgra[source + 2];
            rgba[target + 1] = bgra[source + 1];
            rgba[target + 2] = bgra[source];
            rgba[target + 3] = 255;
        }
    }
    let encoded =
        webp::Encoder::from_rgba(&rgba, encoded_width as u32, encoded_height as u32).encode(75.0);
    let encoded_bytes = encoded.len();
    black_box(encoded.as_ref());
    Ok(SnapshotMetric {
        source_width,
        source_height,
        encoded_width,
        encoded_height,
        encoded_bytes,
        capture_and_encode_ms: started.elapsed().as_secs_f64() * 1_000.0,
        pixels_persisted: false,
    })
}

fn cleanup_capture(screen_dc: HWND, memory_dc: HWND, bitmap: HGDIOBJ, previous: HGDIOBJ) {
    unsafe {
        SelectObject(memory_dc, previous);
        DeleteObject(bitmap);
        DeleteDC(memory_dc);
        ReleaseDC(null_mut(), screen_dc);
    }
}

fn measure_ai_stream() -> Result<AiMetric, Box<dyn std::error::Error>> {
    let started = Instant::now();
    let mut child = Command::new(env::current_exe()?)
        .arg("--ai-worker")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take().ok_or("AI worker stdout unavailable")?;
    let mut chunk_count = 0;
    let mut stdout_bytes = 0;
    for line in BufReader::new(stdout).lines() {
        let line = line?;
        stdout_bytes += line.len() + 1;
        chunk_count += 1;
        black_box(&line);
    }
    let status = child.wait()?;
    Ok(AiMetric {
        protocol: "local_jsonl_stream_proxy",
        chunk_count,
        stdout_bytes,
        elapsed_ms: started.elapsed().as_secs_f64() * 1_000.0,
        exit_code: status.code().unwrap_or(-1),
        network_used: false,
    })
}

fn ai_worker() -> Result<(), Box<dyn std::error::Error>> {
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    for index in 0..512 {
        writeln!(
            output,
            "{{\"type\":\"delta\",\"index\":{index},\"text\":\"{}\"}}",
            "analysis-token ".repeat(24)
        )?;
        output.flush()?;
        if index % 32 == 0 {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    Ok(())
}

fn file_bytes(path: &Path) -> u64 {
    fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
}
