#![cfg(windows)]

use std::{
    env,
    error::Error,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use timelens_storage::Storage;

fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = env::args_os().skip(1);
    let data_directory = PathBuf::from(
        arguments
            .next()
            .ok_or("usage: inspect_dataset <data-directory> [range-minutes]")?,
    );
    let range_minutes = arguments
        .next()
        .map(|value| value.to_string_lossy().parse::<i64>())
        .transpose()?
        .unwrap_or(30);
    if range_minutes <= 0 || arguments.next().is_some() {
        return Err("range-minutes must be a positive integer".into());
    }

    let range_ended_utc_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .min(i64::MAX as u128) as i64;
    let range_started_utc_ms =
        range_ended_utc_ms.saturating_sub(range_minutes.saturating_mul(60_000));
    let storage = Storage::open(&data_directory)?;
    let snapshot_policy = storage.snapshot_policy()?;
    let snapshot_exclusions = storage.snapshot_exclusions()?;
    let timeline = storage.timeline_snapshot(range_started_utc_ms, range_ended_utc_ms)?;
    let snapshot_slots =
        storage.list_snapshot_slots(range_started_utc_ms, range_ended_utc_ms, 10_000)?;
    let snapshot_successes = snapshot_slots.iter().filter(|slot| slot.success).count();

    println!("data_directory={}", data_directory.display());
    println!("range_minutes={range_minutes}");
    println!("applications={}", timeline.applications.len());
    println!("monitoring_gaps={}", timeline.monitoring_gaps.len());
    println!("collection_paused={}", storage.collection_policy()?.paused);
    let jobs = storage.ai_jobs(100)?;
    println!("ai_versions={}", storage.ai_versions()?.len());
    for job in jobs {
        println!("ai_job={} state={}", job.id, job.state);
    }
    for (kind, duration) in storage.system_totals(range_started_utc_ms, range_ended_utc_ms)? {
        println!("system_interval={kind} duration_ms={duration}");
    }
    println!(
        "snapshot_policy=enabled:{} interval_minutes:{} all_displays:{} retention_days:{:?} max_bytes:{}",
        snapshot_policy.enabled,
        snapshot_policy.interval_minutes,
        snapshot_policy.capture_all_displays,
        snapshot_policy.retention_days,
        snapshot_policy.max_bytes
    );
    println!("snapshot_exclusions={}", snapshot_exclusions.len());
    for identity in snapshot_exclusions {
        println!("snapshot_exclusion={identity}");
    }
    println!(
        "input=keyboard:{} left:{} middle:{} right:{}",
        timeline.keyboard_count,
        timeline.left_click_count,
        timeline.middle_click_count,
        timeline.right_click_count
    );
    println!(
        "snapshots=total:{} success:{} missing:{}",
        snapshot_slots.len(),
        snapshot_successes,
        snapshot_slots.len().saturating_sub(snapshot_successes)
    );
    for application in timeline.applications {
        println!(
            "application={} opened_ms={} displayed_ms={} focused_ms={} background_ms={} windows={}",
            application.display_name,
            application.opened_ms,
            application.displayed_ms,
            application.focused_ms,
            application.background_ms,
            application.window_count
        );
    }
    for gap in timeline.monitoring_gaps {
        println!(
            "gap={} reason={} started={} ended={}",
            gap.data_class, gap.reason, gap.started_utc_ms, gap.ended_utc_ms
        );
    }
    for slot in snapshot_slots {
        println!(
            "snapshot_slot={} trigger:{:?} display:{} success:{} missing:{:?} pixels:{}x{} bytes:{}",
            slot.slot_started_utc_ms,
            slot.trigger,
            slot.display.key,
            slot.success,
            slot.missing_reason.map(|reason| reason.as_str()),
            slot.pixel_width,
            slot.pixel_height,
            slot.plaintext_bytes
        );
    }
    Ok(())
}
