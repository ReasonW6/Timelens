#![cfg(windows)]

use std::{env, error::Error, fs, path::PathBuf, time::Instant};

use timelens_ipc::{
    CollectorEvent, EventBatch, IdentitySource, InputMinute, PhysicalKeyCount, WindowObservation,
    WindowTransition, WindowTransitionKind, collector_event,
};
use timelens_storage::Storage;

const APP_COUNT: usize = 100;
const WINDOW_STATE_UPDATES: usize = 240_000;
const INPUT_MINUTES: usize = 30 * 24 * 60;
const RANGE_MS: i64 = 30 * 24 * 60 * 60 * 1_000;
const BASE_UTC_MS: i64 = 1_700_000_040_000;
const MAX_BATCH_EVENTS: usize = 256;
const BUDGET_BYTES: u64 = 100 * 1024 * 1024;
const COLLECTOR_ARTIFACT_RESERVE_BYTES: u64 = 32 * 1024 * 1024 + 1024 * 1024;

fn main() -> Result<(), Box<dyn Error>> {
    let data_directory = parse_data_directory()?;
    if data_directory.exists() && fs::read_dir(&data_directory)?.next().is_some() {
        return Err(format!(
            "capacity data directory must be empty: {}",
            data_directory.display()
        )
        .into());
    }
    fs::create_dir_all(&data_directory)?;

    let started = Instant::now();
    let storage = Storage::open(&data_directory)?;
    let run_id = vec![0x42; 16];
    let mut next_sequence = 1_u64;
    let mut batch = Vec::with_capacity(MAX_BATCH_EVENTS);

    for app_index in 0..APP_COUNT {
        batch.push(window_event(
            app_index,
            WindowTransitionKind::Opened,
            BASE_UTC_MS,
            1,
            true,
        ));
        flush_if_full(&storage, &run_id, &mut next_sequence, &mut batch)?;
    }

    let mut update_index = 0_usize;
    let mut minute_index = 0_usize;
    while update_index < WINDOW_STATE_UPDATES || minute_index < INPUT_MINUTES {
        let update_at = (update_index < WINDOW_STATE_UPDATES).then(|| {
            BASE_UTC_MS + ((update_index as i64 + 1) * RANGE_MS / (WINDOW_STATE_UPDATES as i64 + 1))
        });
        let input_at = (minute_index < INPUT_MINUTES)
            .then(|| BASE_UTC_MS + (minute_index as i64 + 1) * 60_000);
        if update_at.is_some_and(|timestamp| input_at.is_none_or(|input| timestamp <= input)) {
            let timestamp = update_at.expect("update timestamp checked");
            let app_index = update_index % APP_COUNT;
            let phase = update_index / APP_COUNT;
            batch.push(window_event(
                app_index,
                WindowTransitionKind::Updated,
                timestamp,
                (timestamp - BASE_UTC_MS + 1) as u64,
                !phase.is_multiple_of(2),
            ));
            update_index += 1;
        } else {
            let observed_at = input_at.expect("input timestamp checked");
            let minute_started_at = observed_at - 60_000;
            let (year, month, day) = civil_from_days(minute_started_at.div_euclid(86_400_000));
            batch.push(CollectorEvent {
                observed_at_utc_ms: observed_at,
                monotonic_ms: (observed_at - BASE_UTC_MS + 1) as u64,
                body: Some(collector_event::Body::InputMinute(InputMinute {
                    minute_started_at_utc_ms: minute_started_at,
                    timezone_offset_minutes: 0,
                    local_date: format!("{year:04}-{month:02}-{day:02}"),
                    focused_application_identity: Some(application_identity(0)),
                    keyboard_count: 25,
                    left_click_count: 2,
                    middle_click_count: 1,
                    right_click_count: 1,
                    key_counts: vec![PhysicalKeyCount {
                        scan_code: 0x1e,
                        keyboard_layout: 0x0409_0409,
                        count: 25,
                    }],
                })),
            });
            minute_index += 1;
        }
        flush_if_full(&storage, &run_id, &mut next_sequence, &mut batch)?;
    }

    let closed_at = BASE_UTC_MS + RANGE_MS + 1;
    for app_index in 0..APP_COUNT {
        batch.push(window_event(
            app_index,
            WindowTransitionKind::Closed,
            closed_at,
            (closed_at - BASE_UTC_MS + 1) as u64,
            false,
        ));
        flush_if_full(&storage, &run_id, &mut next_sequence, &mut batch)?;
    }
    flush(&storage, &run_id, &mut next_sequence, &mut batch)?;

    let timeline =
        storage.timeline_snapshot(BASE_UTC_MS + RANGE_MS - 86_400_000, BASE_UTC_MS + RANGE_MS)?;
    if timeline.applications.len() != APP_COUNT {
        return Err(format!(
            "capacity timeline projected {} applications; expected {APP_COUNT}",
            timeline.applications.len()
        )
        .into());
    }
    storage.checkpoint()?;
    let database_path = storage.database_path().to_owned();
    drop(storage);

    let database_bytes = file_bytes(&database_path);
    let wal_bytes = file_bytes(&database_path.with_extension("sqlite3-wal"));
    let shm_bytes = file_bytes(&database_path.with_extension("sqlite3-shm"));
    let data_bytes = directory_bytes(&data_directory)?;
    let product_data_bytes = data_bytes.saturating_add(COLLECTOR_ARTIFACT_RESERVE_BYTES);
    let passed = product_data_bytes <= BUDGET_BYTES;
    println!(
        concat!(
            "{{\n",
            "  \"datasetDays\": 30,\n",
            "  \"applicationCount\": {},\n",
            "  \"windowStateUpdates\": {},\n",
            "  \"inputMinuteRows\": {},\n",
            "  \"databaseBytes\": {},\n",
            "  \"walBytesAfterCheckpoint\": {},\n",
            "  \"shmBytesAfterCheckpoint\": {},\n",
            "  \"storageDataBytes\": {},\n",
            "  \"collectorArtifactReserveBytes\": {},\n",
            "  \"projectedProductNonImageDataBytes\": {},\n",
            "  \"budgetBytes\": {},\n",
            "  \"elapsedSeconds\": {:.3},\n",
            "  \"passed\": {}\n",
            "}}"
        ),
        APP_COUNT,
        WINDOW_STATE_UPDATES,
        INPUT_MINUTES,
        database_bytes,
        wal_bytes,
        shm_bytes,
        data_bytes,
        COLLECTOR_ARTIFACT_RESERVE_BYTES,
        product_data_bytes,
        BUDGET_BYTES,
        started.elapsed().as_secs_f64(),
        passed
    );
    if !passed {
        return Err(format!(
            "30-day product data used {product_data_bytes} bytes; budget is {BUDGET_BYTES} bytes"
        )
        .into());
    }
    Ok(())
}

fn parse_data_directory() -> Result<PathBuf, Box<dyn Error>> {
    let mut arguments = env::args().skip(1);
    match (
        arguments.next().as_deref(),
        arguments.next(),
        arguments.next(),
    ) {
        (Some("--data-dir"), Some(path), None) => Ok(PathBuf::from(path)),
        _ => Err("usage: milestone2_capacity --data-dir <empty-directory>".into()),
    }
}

fn window_event(
    app_index: usize,
    kind: WindowTransitionKind,
    observed_at_utc_ms: i64,
    monotonic_ms: u64,
    displayed: bool,
) -> CollectorEvent {
    CollectorEvent {
        observed_at_utc_ms,
        monotonic_ms,
        body: Some(collector_event::Body::WindowTransition(WindowTransition {
            kind: kind as i32,
            window: Some(WindowObservation {
                window_id: app_index as u64 + 1,
                process_id: app_index as u32 + 1_000,
                process_started_at_100ns: app_index as u64 + 10_000,
                application_identity: application_identity(app_index),
                identity_source: IdentitySource::ExecutablePath as i32,
                executable_path: Some(format!(r"C:\Program Files\Synthetic\App{app_index:03}.exe")),
                app_user_model_id: None,
                package_identity: None,
                displayed,
                focused: false,
                on_current_virtual_desktop: Some(true),
                virtual_desktop_id: Some("capacity-desktop".to_owned()),
            }),
        })),
    }
}

fn application_identity(app_index: usize) -> String {
    format!(r"path:c:\program files\synthetic\app{app_index:03}.exe")
}

fn flush_if_full(
    storage: &Storage,
    run_id: &[u8],
    next_sequence: &mut u64,
    events: &mut Vec<CollectorEvent>,
) -> Result<(), Box<dyn Error>> {
    if events.len() == MAX_BATCH_EVENTS {
        flush(storage, run_id, next_sequence, events)?;
    }
    Ok(())
}

fn flush(
    storage: &Storage,
    run_id: &[u8],
    next_sequence: &mut u64,
    events: &mut Vec<CollectorEvent>,
) -> Result<(), Box<dyn Error>> {
    if events.is_empty() {
        return Ok(());
    }
    let event_count = events.len() as u64;
    let batch = EventBatch {
        collector_run_id: run_id.to_vec(),
        first_sequence: *next_sequence,
        events: std::mem::take(events),
    };
    storage.ingest_event_batch(&batch)?;
    *next_sequence += event_count;
    Ok(())
}

fn civil_from_days(days_since_unix_epoch: i64) -> (i32, u32, u32) {
    let shifted = days_since_unix_epoch + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year as i32, month as u32, day as u32)
}

fn file_bytes(path: &std::path::Path) -> u64 {
    fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
}

fn directory_bytes(path: &std::path::Path) -> Result<u64, Box<dyn Error>> {
    let mut bytes = 0_u64;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            bytes = bytes.saturating_add(entry.metadata()?.len());
        }
    }
    Ok(bytes)
}
