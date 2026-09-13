//! Local, synthetic data for the Paper UI acceptance pass.
//!
//! Usage: design_fixture <existing-empty-absolute-directory>
//!        [vscode.exe] [obsidian.exe] [edge.exe] [explorer.exe]
//! The optional EXE paths are metadata only: they are never launched or read here.
//! This example never creates an AI provider, credentials, a job, or a schedule.
use chrono::{Local, NaiveDate, NaiveTime, TimeZone};
use serde_json::json;
use std::{error::Error, fs, io::Cursor, path::PathBuf};
use timelens_ipc::{
    CollectorEvent, EventBatch, IdentitySource, InputMinute, MonitoringGap, MonitoringGapReason,
    PhysicalKeyCount, SystemInterval, WindowObservation, WindowTransition, WindowTransitionKind,
    collector_event,
};
use timelens_storage::{SnapshotDisplay, SnapshotStoreRequest, SnapshotTrigger, Storage};

type FixtureResult<T> = Result<T, Box<dyn Error>>;
const MINUTE_MS: i64 = 60_000;

struct FixtureApp {
    label: &'static str,
    executable: String,
    identity: String,
    focus_minutes: u32,
    input: [u32; 4],
}

struct FocusSpan {
    app: usize,
    start: i64,
    end: i64,
}

fn main() -> FixtureResult<()> {
    let mut args = std::env::args().skip(1);
    let requested_directory = PathBuf::from(
        args.next()
            .ok_or("expected an existing, empty, absolute fixture directory")?,
    );
    let supplied_executables = args.collect::<Vec<_>>();
    if supplied_executables.len() > 4 {
        return Err("expected at most four EXE paths: VS Code, Obsidian, Edge, Explorer".into());
    }
    let directory = empty_fixture_directory(&requested_directory)?;
    let definitions = [
        (
            "Visual Studio Code",
            "C:/Synthetic/Code.exe",
            166,
            [12_486, 300, 0, 28],
        ),
        (
            "Obsidian",
            "C:/Synthetic/Obsidian.exe",
            78,
            [6_420, 136, 4, 12],
        ),
        (
            "Microsoft Edge",
            "C:/Synthetic/msedge.exe",
            62,
            [980, 210, 0, 24],
        ),
        (
            "文件资源管理器",
            "C:/Synthetic/explorer.exe",
            18,
            [84, 26, 0, 2],
        ),
    ];
    let mut apps = Vec::with_capacity(definitions.len());
    for (index, (label, fallback, focus_minutes, input)) in definitions.into_iter().enumerate() {
        let executable = local_executable(
            supplied_executables
                .get(index)
                .map_or(fallback, String::as_str),
        )?;
        let identity = format!("path:{}", executable.to_lowercase());
        if apps.iter().any(|app: &FixtureApp| app.identity == identity) {
            return Err("the four application identities must be distinct".into());
        }
        apps.push(FixtureApp {
            label,
            executable,
            identity,
            focus_minutes,
            input,
        });
    }

    let now = Local::now();
    let cutoff = NaiveTime::from_hms_opt(18, 30, 0).ok_or("invalid cutoff")?;
    let day = if now.time() < cutoff {
        now.date_naive()
            .pred_opt()
            .ok_or("previous local day is unavailable")?
    } else {
        now.date_naive()
    };
    let start = local_timestamp(day, 9, 0, 0)?;
    let end = local_timestamp(day, 18, 0, 0)?;
    if end - start != 540 * MINUTE_MS || end > now.timestamp_millis() {
        return Err("the selected local 09:00–18:00 range is not a completed nine-hour day".into());
    }
    let gap_start = local_timestamp(day, 13, 0, 0)?;
    let gap_end = local_timestamp(day, 13, 17, 0)?;
    let mut focus_spans = Vec::new();
    for (app, sh, sm, eh, em) in [
        (0, 9, 0, 10, 34),
        (1, 10, 45, 11, 30),
        (2, 11, 30, 12, 10),
        (0, 14, 10, 15, 22),
        (2, 15, 22, 15, 44),
        (1, 16, 7, 16, 40),
        (3, 17, 0, 17, 18),
    ] {
        focus_spans.push(FocusSpan {
            app,
            start: local_timestamp(day, sh, sm, 0)?,
            end: local_timestamp(day, eh, em, 0)?,
        });
    }

    let mut events = Vec::new();
    use WindowTransitionKind::{Closed, Opened, Updated};
    // Three Code windows, with no overlapping focus. Their union deliberately gives
    // opened 5h32, displayed 4h05, focused 2h46, and background 1h27.
    add_window(
        &mut events,
        &apps[0],
        1,
        day,
        &[
            (9, 0, 0, Opened, true, true),
            (10, 34, 0, Updated, true, false),
            (11, 22, 0, Updated, false, false),
            (12, 38, 0, Closed, false, false),
        ],
    )?;
    add_window(
        &mut events,
        &apps[0],
        2,
        day,
        &[
            (14, 10, 0, Opened, true, true),
            (15, 9, 12, Updated, true, false),
            (15, 53, 0, Updated, false, false),
            (16, 4, 0, Closed, false, false),
        ],
    )?;
    add_window(
        &mut events,
        &apps[0],
        3,
        day,
        &[
            (15, 9, 12, Opened, true, true),
            (15, 22, 0, Closed, false, false),
        ],
    )?;
    add_window(
        &mut events,
        &apps[1],
        4,
        day,
        &[
            (10, 45, 0, Opened, true, true),
            (11, 30, 0, Updated, true, false),
            (12, 0, 0, Updated, false, false),
            (16, 7, 0, Updated, true, true),
            (16, 40, 0, Updated, true, false),
            (16, 50, 0, Closed, false, false),
        ],
    )?;
    add_window(
        &mut events,
        &apps[2],
        5,
        day,
        &[
            (11, 30, 0, Opened, true, true),
            (12, 10, 0, Updated, true, false),
            (12, 20, 0, Updated, false, false),
            (15, 22, 0, Updated, true, true),
            (15, 44, 0, Updated, true, false),
            (16, 0, 0, Closed, false, false),
        ],
    )?;
    add_window(
        &mut events,
        &apps[3],
        6,
        day,
        &[
            (17, 0, 0, Opened, true, true),
            (17, 18, 0, Closed, false, false),
        ],
    )?;

    events.push(CollectorEvent {
        observed_at_utc_ms: gap_end,
        body: Some(collector_event::Body::MonitoringGap(MonitoringGap {
            started_at_utc_ms: gap_start,
            reason: MonitoringGapReason::BufferOverflow as i32,
        })),
        ..Default::default()
    });
    // All other non-focused minutes were observed as idle. The final interval ends
    // at 18:00, so coverage is genuinely 09:00–18:00 minus the explicit 17-minute gap.
    for (sh, sm, eh, em) in [
        (10, 34, 10, 45),
        (12, 10, 13, 0),
        (13, 17, 14, 10),
        (15, 44, 16, 7),
        (16, 40, 17, 0),
        (17, 18, 18, 0),
    ] {
        let interval_start = local_timestamp(day, sh, sm, 0)?;
        let interval_end = local_timestamp(day, eh, em, 0)?;
        let offset = Local
            .timestamp_millis_opt(interval_start)
            .single()
            .ok_or("invalid system timestamp")?
            .offset()
            .local_minus_utc()
            / 60;
        events.push(CollectorEvent {
            observed_at_utc_ms: interval_end,
            body: Some(collector_event::Body::SystemInterval(SystemInterval {
                kind: "desktop_idle".into(),
                started_utc_ms: interval_start,
                duration_ms: u64::try_from(interval_end - interval_start)?,
                timezone_offset_minutes: offset,
            })),
            ..Default::default()
        });
    }
    add_input(&mut events, &apps, &focus_spans)?;
    // End/loss-of-focus transitions precede gains at a shared boundary.
    events.sort_by_key(|event| (event.observed_at_utc_ms, event_priority(event)));
    for event in &mut events {
        event.monotonic_ms = u64::try_from(event.observed_at_utc_ms - start)? + 1;
    }
    if events.first().map(|e| e.observed_at_utc_ms) != Some(start)
        || events.last().map(|e| e.observed_at_utc_ms) != Some(end)
    {
        return Err("fixture event stream does not cover the requested day".into());
    }

    // Prepare all inputs before the first write, then recheck the caller's directory.
    empty_fixture_directory(&directory)?;
    let storage = Storage::open(&directory)?;
    let mut policy = storage.collection_policy()?;
    policy.paused = true;
    storage.set_collection_policy(policy)?;
    let mut snapshot_policy = storage.snapshot_policy()?;
    snapshot_policy.enabled = false;
    storage.set_snapshot_policy(snapshot_policy, now.timestamp_millis())?;
    storage.ingest_event_batch(&EventBatch {
        collector_run_id: b"design-ui-qa-v01".to_vec(),
        first_sequence: 1,
        events,
    })?;

    let captured_at = local_timestamp(day, 14, 30, 0)?;
    let snapshot_id = storage.store_snapshot(&SnapshotStoreRequest {
        slot_started_utc_ms: captured_at,
        captured_at_utc_ms: captured_at,
        display: SnapshotDisplay {
            key: "synthetic-design-qa-640x360".into(),
            x: 0,
            y: 0,
            width: 640,
            height: 360,
            orientation_degrees: 0,
        },
        pixel_width: 640,
        pixel_height: 360,
        trigger: SnapshotTrigger::Manual,
        webp: synthetic_webp()?,
    })?;
    verify_fixture(
        &storage,
        &apps,
        &focus_spans,
        start,
        end,
        gap_start,
        gap_end,
    )?;
    let report = storage.generate_local_report(start, end, now.timestamp_millis())?;
    if report.covered_ms != 523 * MINUTE_MS as u64
        || report.snapshot_success_count != 1
        || report.applications.len() != 4
    {
        return Err("stored report does not match the synthetic day".into());
    }
    storage.verify_integrity()?;
    storage.checkpoint()?;

    let metadata = json!({
        "synthetic": true,
        "day": day.to_string(),
        "data_path": directory,
        "range": {
            "local_start": format!("{day} 09:00:00"),
            "local_end": format!("{day} 18:00:00"),
            "started_utc_ms": start,
            "ended_utc_ms": end,
            "total_minutes": 540,
            "focused_minutes": 324,
            "covered_minutes": 523,
            "desktop_idle_minutes": 199
        },
        "gap": {
            "local_start": "13:00", "local_end": "13:17",
            "started_utc_ms": gap_start, "ended_utc_ms": gap_end,
            "data_classes": ["activity", "input"], "reason": "buffer_overflow"
        },
        "applications": apps.iter().map(|app| json!({
            "label": app.label, "identity": app.identity,
            "executable_path": app.executable, "focused_minutes": app.focus_minutes,
            "keyboard_count": app.input[0],
            "mouse_count": app.input[1] + app.input[2] + app.input[3]
        })).collect::<Vec<_>>(),
        "focus_intervals": focus_spans.iter().map(|span| json!({
            "application_identity": apps[span.app].identity,
            "started_utc_ms": span.start, "ended_utc_ms": span.end
        })).collect::<Vec<_>>(),
        "snapshot_slot_id": snapshot_id,
        "snapshot_is_synthetic": true,
        "local_report_id": report.id,
        "collection_paused": true,
        "snapshots_enabled": false,
        "ai_provider_count": 0,
        "ai_schedule_count": 0,
        "verified_from_storage": true
    });
    println!("{}", serde_json::to_string_pretty(&metadata)?);
    Ok(())
}

fn empty_fixture_directory(path: &std::path::Path) -> FixtureResult<PathBuf> {
    if !path.is_absolute() {
        return Err("fixture directory must be an explicit absolute path".into());
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("fixture path must be an existing real directory".into());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err("fixture directory must not be a reparse point".into());
        }
    }
    let canonical = fs::canonicalize(path)?;
    if canonical.parent().is_none() || fs::read_dir(&canonical)?.next().transpose()?.is_some() {
        return Err("fixture directory must be empty and must not be a filesystem root".into());
    }
    Ok(canonical)
}

fn local_executable(raw: &str) -> FixtureResult<String> {
    let normalized = raw.replace('/', "\\");
    let path = normalized.strip_prefix("\\\\?\\").unwrap_or(&normalized);
    let bytes = path.as_bytes();
    if bytes.len() < 4
        || !bytes[0].is_ascii_alphabetic()
        || bytes[1] != b':'
        || bytes[2] != b'\\'
        || !path.to_lowercase().ends_with(".exe")
        || path.split('\\').any(|part| part == "..")
    {
        return Err("EXE metadata must be a local absolute drive path ending in .exe".into());
    }
    Ok(path.to_owned())
}

fn local_timestamp(day: NaiveDate, hour: u32, minute: u32, second: u32) -> FixtureResult<i64> {
    let time = day
        .and_hms_opt(hour, minute, second)
        .ok_or("invalid local time")?;
    Ok(Local
        .from_local_datetime(&time)
        .single()
        .ok_or("ambiguous or missing local time in the fixture day")?
        .timestamp_millis())
}

fn add_window(
    events: &mut Vec<CollectorEvent>,
    app: &FixtureApp,
    window_id: u64,
    day: NaiveDate,
    steps: &[(u32, u32, u32, WindowTransitionKind, bool, bool)],
) -> FixtureResult<()> {
    for &(hour, minute, second, kind, displayed, focused) in steps {
        events.push(CollectorEvent {
            observed_at_utc_ms: local_timestamp(day, hour, minute, second)?,
            body: Some(collector_event::Body::WindowTransition(WindowTransition {
                kind: kind as i32,
                window: Some(WindowObservation {
                    window_id,
                    process_id: 4000 + window_id as u32,
                    process_started_at_100ns: 1000 + window_id,
                    application_identity: app.identity.clone(),
                    identity_source: IdentitySource::ExecutablePath as i32,
                    executable_path: Some(app.executable.clone()),
                    displayed,
                    focused,
                    on_current_virtual_desktop: Some(true),
                    ..Default::default()
                }),
            })),
            ..Default::default()
        });
    }
    Ok(())
}

fn event_priority(event: &CollectorEvent) -> u8 {
    match event.body.as_ref() {
        Some(collector_event::Body::WindowTransition(t))
            if t.kind == WindowTransitionKind::Closed as i32 =>
        {
            0
        }
        Some(collector_event::Body::WindowTransition(t))
            if t.window.as_ref().is_some_and(|w| !w.focused) =>
        {
            1
        }
        _ => 2,
    }
}

fn add_input(
    events: &mut Vec<CollectorEvent>,
    apps: &[FixtureApp],
    spans: &[FocusSpan],
) -> FixtureResult<()> {
    let mut minute_indices = vec![0_u32; apps.len()];
    for span in spans {
        let app = &apps[span.app];
        for at in (span.start..span.end).step_by(MINUTE_MS as usize) {
            let index = minute_indices[span.app];
            minute_indices[span.app] += 1;
            let counts = app
                .input
                .map(|n| n / app.focus_minutes + u32::from(index < n % app.focus_minutes));
            let local = Local
                .timestamp_millis_opt(at)
                .single()
                .ok_or("invalid input timestamp")?;
            let mut remaining = counts[0];
            let mut keys = Vec::new();
            // A fixed physical-position distribution; no text or actual keyboard input.
            for (scan_code, weight) in [
                (30, 16),
                (31, 12),
                (32, 10),
                (16, 7),
                (18, 12),
                (19, 5),
                (28, 8),
                (14, 7),
            ] {
                let count = counts[0] * weight / 100;
                remaining -= count;
                if count > 0 {
                    keys.push(PhysicalKeyCount {
                        scan_code,
                        keyboard_layout: 0x4090409,
                        count,
                    });
                }
            }
            if remaining > 0 {
                keys.push(PhysicalKeyCount {
                    scan_code: 57,
                    keyboard_layout: 0x4090409,
                    count: remaining,
                });
            }
            events.push(CollectorEvent {
                observed_at_utc_ms: at + MINUTE_MS,
                body: Some(collector_event::Body::InputMinute(InputMinute {
                    minute_started_at_utc_ms: at,
                    timezone_offset_minutes: local.offset().local_minus_utc() / 60,
                    local_date: local.format("%Y-%m-%d").to_string(),
                    focused_application_identity: Some(app.identity.clone()),
                    keyboard_count: counts[0],
                    left_click_count: counts[1],
                    middle_click_count: counts[2],
                    right_click_count: counts[3],
                    key_counts: keys,
                    ..Default::default()
                })),
                ..Default::default()
            });
        }
    }
    if minute_indices
        .iter()
        .zip(apps)
        .any(|(actual, app)| *actual != app.focus_minutes)
    {
        return Err("input allocation does not match the focus duration".into());
    }
    Ok(())
}

fn merged_intervals(mut intervals: Vec<(i64, i64)>) -> Vec<(i64, i64)> {
    intervals.sort_unstable();
    let mut result: Vec<(i64, i64)> = Vec::new();
    for (start, end) in intervals {
        if let Some(last) = result.last_mut()
            && start <= last.1
        {
            last.1 = last.1.max(end);
        } else {
            result.push((start, end));
        }
    }
    result
}

fn verify_fixture(
    storage: &Storage,
    apps: &[FixtureApp],
    spans: &[FocusSpan],
    start: i64,
    end: i64,
    gap_start: i64,
    gap_end: i64,
) -> FixtureResult<()> {
    let timeline = storage.timeline_snapshot(start, end)?;
    if timeline.applications.len() != 4 {
        return Err("expected exactly four synthetic applications".into());
    }
    let mut all_focus = Vec::new();
    for (index, expected) in apps.iter().enumerate() {
        let actual = timeline
            .applications
            .iter()
            .find(|app| app.identity == expected.identity)
            .ok_or("a fixture application is absent from the stored timeline")?;
        let focused = actual
            .segments
            .iter()
            .filter(|segment| segment.focused)
            .map(|segment| (segment.started_utc_ms, segment.ended_utc_ms))
            .collect::<Vec<_>>();
        all_focus.extend(focused.iter().copied());
        let planned = spans
            .iter()
            .filter(|span| span.app == index)
            .map(|span| (span.start, span.end))
            .collect::<Vec<_>>();
        let input = [
            actual.keyboard_count,
            actual.left_click_count,
            actual.middle_click_count,
            actual.right_click_count,
        ];
        if merged_intervals(focused) != merged_intervals(planned)
            || actual.focused_ms != u64::from(expected.focus_minutes) * MINUTE_MS as u64
            || input != expected.input.map(u64::from)
        {
            return Err(format!("stored focus or input differs for {}", expected.label).into());
        }
        if index == 0
            && (actual.opened_ms != 332 * MINUTE_MS as u64
                || actual.displayed_ms != 245 * MINUTE_MS as u64
                || actual.background_ms != 87 * MINUTE_MS as u64
                || actual.window_count != 3)
        {
            return Err("stored Code window totals differ from the reference".into());
        }
    }
    all_focus.sort_unstable();
    if all_focus.windows(2).any(|pair| pair[0].1 > pair[1].0) {
        return Err("synthetic windows have overlapping focus".into());
    }
    if timeline.monitoring_gaps.len() != 2
        || ["activity", "input"].iter().any(|class| {
            !timeline.monitoring_gaps.iter().any(|gap| {
                gap.data_class == *class
                    && gap.started_utc_ms == gap_start
                    && gap.ended_utc_ms == gap_end
                    && gap.reason == "buffer_overflow"
            })
        })
    {
        return Err("expected the exact 13:00–13:17 activity and input gaps".into());
    }
    let (covered_ms, _) = storage.data_coverage(start, end)?;
    if covered_ms != 523 * MINUTE_MS as u64
        || storage
            .system_totals(start, end)?
            .get("desktop_idle")
            .copied()
            != Some(199 * MINUTE_MS as u64)
        || !storage.collection_policy()?.paused
        || storage.snapshot_policy()?.enabled
        || !storage.ai_profiles()?.is_empty()
        || !storage.ai_schedules()?.is_empty()
    {
        return Err("fixture coverage or disabled collection/AI state is incorrect".into());
    }
    Ok(())
}

fn synthetic_webp() -> FixtureResult<Vec<u8>> {
    let mut image = image::RgbImage::from_fn(640, 360, |x, y| {
        if (28..612).contains(&x) && (28..123).contains(&y) {
            image::Rgb([157, 62, 40])
        } else if (28..612).contains(&x) && (153..295).contains(&y) && ((x - 28) / 40) % 2 == 0 {
            image::Rgb([229, 223, 214])
        } else {
            image::Rgb([251, 249, 246])
        }
    });
    draw_text(&mut image, "SYNTHETIC FIXTURE", 92, 48, 4, [255, 255, 255]);
    draw_text(
        &mut image,
        "NO REAL SCREEN DATA",
        92,
        94,
        2,
        [255, 255, 255],
    );
    draw_text(&mut image, "LOCAL UI QA", 28, 319, 2, [48, 44, 40]);
    let mut output = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image).write_to(&mut output, image::ImageFormat::WebP)?;
    Ok(output.into_inner())
}

fn draw_text(image: &mut image::RgbImage, text: &str, x: u32, y: u32, scale: u32, color: [u8; 3]) {
    for (index, character) in text.chars().enumerate() {
        let rows = match character {
            'A' => [14, 17, 17, 31, 17, 17, 17],
            'C' => [14, 17, 16, 16, 16, 17, 14],
            'D' => [30, 17, 17, 17, 17, 17, 30],
            'E' => [31, 16, 16, 30, 16, 16, 31],
            'F' => [31, 16, 16, 30, 16, 16, 16],
            'H' => [17, 17, 17, 31, 17, 17, 17],
            'I' => [31, 4, 4, 4, 4, 4, 31],
            'L' => [16, 16, 16, 16, 16, 16, 31],
            'N' => [17, 25, 25, 21, 19, 19, 17],
            'O' => [14, 17, 17, 17, 17, 17, 14],
            'Q' => [14, 17, 17, 17, 21, 18, 13],
            'R' => [30, 17, 17, 30, 20, 18, 17],
            'S' => [15, 16, 16, 14, 1, 1, 30],
            'T' => [31, 4, 4, 4, 4, 4, 4],
            'U' => [17, 17, 17, 17, 17, 17, 14],
            'X' => [17, 17, 10, 4, 10, 17, 17],
            'Y' => [17, 17, 10, 4, 4, 4, 4],
            _ => [0; 7],
        };
        for (row, bits) in rows.into_iter().enumerate() {
            for column in 0..5 {
                if bits & (1 << (4 - column)) != 0 {
                    for dy in 0..scale {
                        for dx in 0..scale {
                            let px = x + index as u32 * 6 * scale + column * scale + dx;
                            let py = y + row as u32 * scale + dy;
                            if px < image.width() && py < image.height() {
                                image.put_pixel(px, py, image::Rgb(color));
                            }
                        }
                    }
                }
            }
        }
    }
}
