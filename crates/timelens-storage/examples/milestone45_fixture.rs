//! Synthetic UI acceptance data; refuses to write a non-empty dataset.
use std::{error::Error, fs, io::Cursor, path::PathBuf};
use timelens_ipc::{
    CollectorEvent, EventBatch, IdentitySource, InputMinute, PhysicalKeyCount, WindowObservation,
    WindowTransition, WindowTransitionKind, collector_event,
};
use timelens_storage::{SnapshotDisplay, SnapshotStoreRequest, SnapshotTrigger, Storage};

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let directory = PathBuf::from(args.next().ok_or("expected empty fixture directory")?);
    let mode = args.next().unwrap_or_else(|| "ui".into());
    if args.next().is_some() || !["ui", "normal", "idle"].contains(&mode.as_str()) {
        return Err("expected mode ui, normal or idle".into());
    }
    if directory.exists() && fs::read_dir(&directory)?.next().is_some() {
        return Err("fixture directory is not empty".into());
    }
    let storage = Storage::open(&directory)?;
    let now = chrono::Utc::now().timestamp_millis();
    let start = now.div_euclid(60_000) * 60_000 - 30 * 60_000;
    let end = start + 29 * 60_000;
    let mut events = vec![];
    let names = ["WritingStudio", "CodeEditor", "ReadingDesk"];
    for (i, name) in names.iter().enumerate() {
        let window = WindowObservation {
            window_id: i as u64 + 1,
            process_id: i as u32 + 4000,
            process_started_at_100ns: i as u64 + 1000,
            application_identity: format!("path:c:\\synthetic\\{name}.exe").to_lowercase(),
            identity_source: IdentitySource::ExecutablePath as i32,
            executable_path: Some(format!("C:\\Synthetic\\{name}.exe")),
            displayed: true,
            focused: i == 0,
            on_current_virtual_desktop: Some(true),
            ..Default::default()
        };
        for (at, kind) in [
            (start + i as i64 * 180_000, WindowTransitionKind::Opened),
            (end, WindowTransitionKind::Closed),
        ] {
            events.push(CollectorEvent {
                observed_at_utc_ms: at,
                monotonic_ms: (at - start + 1) as u64,
                body: Some(collector_event::Body::WindowTransition(WindowTransition {
                    kind: kind as i32,
                    window: Some(window.clone()),
                })),
            });
        }
    }
    for minute in 0..29 {
        let at = start + (minute + 1) * 60_000;
        let local = chrono::DateTime::from_timestamp_millis(at - 60_000)
            .unwrap()
            .with_timezone(&chrono::Local);
        events.push(CollectorEvent {
            observed_at_utc_ms: at,
            monotonic_ms: (at - start + 1) as u64,
            body: Some(collector_event::Body::InputMinute(InputMinute {
                minute_started_at_utc_ms: at - 60_000,
                timezone_offset_minutes: local.offset().local_minus_utc() / 60,
                local_date: local.format("%Y-%m-%d").to_string(),
                focused_application_identity: Some("path:c:\\synthetic\\writingstudio.exe".into()),
                keyboard_count: 36,
                left_click_count: 3,
                key_counts: vec![PhysicalKeyCount {
                    scan_code: 30,
                    keyboard_layout: 0x4090409,
                    count: 36,
                }],
                ..Default::default()
            })),
        });
    }
    events.sort_by_key(|event| event.observed_at_utc_ms);
    storage.ingest_event_batch(&EventBatch {
        collector_run_id: vec![0x61; 16],
        first_sequence: 1,
        events,
    })?;
    let image = image::RgbImage::from_fn(640, 360, |x, y| {
        image::Rgb([((x / 4) % 180 + 40) as u8, ((y / 2) % 180 + 40) as u8, 120])
    });
    let mut encoded = Cursor::new(vec![]);
    image::DynamicImage::ImageRgb8(image).write_to(&mut encoded, image::ImageFormat::WebP)?;
    storage.store_snapshot(&SnapshotStoreRequest {
        slot_started_utc_ms: end,
        captured_at_utc_ms: end,
        display: SnapshotDisplay {
            key: "synthetic-display".into(),
            x: 0,
            y: 0,
            width: 640,
            height: 360,
            orientation_degrees: 0,
        },
        pixel_width: 640,
        pixel_height: 360,
        trigger: SnapshotTrigger::Manual,
        webp: encoded.into_inner(),
    })?;
    let mut snapshots = storage.snapshot_policy()?;
    snapshots.enabled = false;
    storage.set_snapshot_policy(snapshots, now)?;
    let mut policy = storage.collection_policy()?;
    policy.paused = mode == "ui" || mode == "idle";
    storage.set_collection_policy(policy)?;
    storage.generate_local_report(start, end, now)?;
    storage.verify_integrity()?;
    storage.checkpoint()?;
    println!(
        "synthetic dataset: {}\nrange {}..{}",
        directory.display(),
        start,
        end
    );
    Ok(())
}
