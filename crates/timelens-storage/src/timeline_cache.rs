//! Bounded, memory-only view cache. Durable storage and its encryption are unchanged.
use super::*;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

#[derive(Default)]
pub(crate) struct TimelineCache {
    changes: u64,
    entries: VecDeque<Cached>,
}

struct Cached {
    snapshot: TimelineSnapshot,
    projections: HashMap<String, Projection>,
    next_boundary: i64,
    dirty: BTreeSet<String>,
}

#[derive(Default)]
struct Projection {
    opened: bool,
    displayed: bool,
    focused: bool,
    windows: HashMap<usize, (bool, bool)>,
    segments: HashSet<usize>,
}

impl TimelineCache {
    pub(crate) fn clear(&mut self, changes: u64) {
        self.entries.clear();
        self.changes = changes;
    }

    pub(crate) fn unchanged_write(&mut self, before: u64, after: u64) {
        if self.changes != before {
            self.entries.clear();
        }
        self.changes = after;
    }

    pub(crate) fn ingested(&mut self, before: u64, after: u64, batch: &EventBatch, new_run: bool) {
        if self.changes != before || new_run {
            self.clear(after);
            return;
        }
        let mut dirty = BTreeSet::new();
        for event in &batch.events {
            match event.body.as_ref() {
                Some(collector_event::Body::WindowTransition(t)) => {
                    if let Some(w) = &t.window {
                        dirty.insert(w.application_identity.clone());
                    }
                }
                Some(collector_event::Body::TrayTransition(t)) => {
                    dirty.insert(t.application_identity.clone());
                }
                Some(collector_event::Body::InputMinute(m)) => {
                    if let Some(identity) = &m.focused_application_identity {
                        dirty.insert(identity.clone());
                    } else {
                        // Anonymous counts still contribute to the global totals.
                        self.clear(after);
                        return;
                    }
                }
                Some(collector_event::Body::MonitoringGap(_)) => {
                    self.clear(after);
                    return;
                }
                _ => {}
            }
        }
        for entry in &mut self.entries {
            entry.dirty.extend(dirty.iter().cloned());
        }
        self.changes = after;
    }
}

fn root(identity: &str, edges: &[(String, String)]) -> String {
    let mut members = BTreeSet::from([identity.to_owned()]);
    loop {
        let before = members.len();
        for (left, right) in edges {
            if members.contains(left) || members.contains(right) {
                members.insert(left.clone());
                members.insert(right.clone());
            }
        }
        if before == members.len() {
            return members.into_iter().next().unwrap();
        }
    }
}

fn sort_applications(applications: &mut [TimelineApplication]) {
    applications.sort_by(|a, b| {
        b.focused_ms
            .cmp(&a.focused_ms)
            .then_with(|| b.opened_ms.cmp(&a.opened_ms))
            .then_with(|| a.display_name.cmp(&b.display_name))
    });
}

impl Cached {
    fn advance(&mut self, end: i64) {
        let delta = end.saturating_sub(self.snapshot.range_ended_utc_ms) as u64;
        for app in &mut self.snapshot.applications {
            let Some(p) = self.projections.get(&app.identity) else {
                continue;
            };
            let opened = if p.opened { delta } else { 0 };
            let displayed = if p.displayed { delta } else { 0 };
            app.opened_ms += opened;
            app.displayed_ms += displayed;
            app.focused_ms += if p.focused { delta } else { 0 };
            app.background_ms = app.opened_ms.saturating_sub(app.displayed_ms);
            for (&index, &(visible, focused)) in &p.windows {
                let window = &mut app.windows[index];
                window.ended_utc_ms = end;
                window.opened_ms += delta;
                window.displayed_ms += if visible { delta } else { 0 };
                window.focused_ms += if focused { delta } else { 0 };
                window.background_ms = window.opened_ms.saturating_sub(window.displayed_ms);
            }
            for &index in &p.segments {
                app.segments[index].ended_utc_ms = end;
            }
            app.segments
                .sort_unstable_by_key(|s| (s.started_utc_ms, s.ended_utc_ms));
        }
        self.snapshot.range_ended_utc_ms = end;
        sort_applications(&mut self.snapshot.applications);
    }
}

impl Storage {
    /// Reuse completed application statistics, replacing only changed applications
    /// and extending ongoing intervals. Late batches invalidate their identities.
    pub fn timeline_snapshot_cached(&self, start: i64, end: i64) -> Result<TimelineSnapshot> {
        let changes = self.connection.total_changes();
        let cached = {
            let mut cache = self.timeline_cache.borrow_mut();
            if cache.changes != changes {
                // Any write outside ingestion, including delete/merge/restore or
                // retention, invalidates the view rather than guessing its effect.
                cache.clear(changes);
            }
            cache
                .entries
                .iter()
                .position(|c| {
                    c.snapshot.range_started_utc_ms == start
                        && c.snapshot.range_ended_utc_ms <= end
                        && end < c.next_boundary
                })
                .and_then(|i| cache.entries.remove(i))
        };
        let mut cached = if let Some(mut cached) = cached {
            cached.advance(end);
            if !cached.dirty.is_empty() {
                let edges = self.merge_links()?;
                let roots = cached
                    .dirty
                    .iter()
                    .map(|id| root(id, &edges))
                    .collect::<BTreeSet<_>>();
                let mut sources = cached.dirty.clone();
                for id in cached.dirty.iter() {
                    sources.extend(self.merged_members(id)?);
                }
                let filter = serde_json::to_string(&sources)
                    .map_err(|e| StorageError::Integrity(e.to_string()))?;
                let refreshed = self.timeline_snapshot_selected(start, end, Some(&filter))?;
                cached
                    .snapshot
                    .applications
                    .retain(|a| !roots.contains(&a.identity));
                cached.snapshot.applications.extend(refreshed.applications);
                cached.snapshot.keyboard_count = refreshed.keyboard_count;
                cached.snapshot.left_click_count = refreshed.left_click_count;
                cached.snapshot.middle_click_count = refreshed.middle_click_count;
                cached.snapshot.right_click_count = refreshed.right_click_count;
                cached.snapshot.monitoring_gaps = refreshed.monitoring_gaps;
                sort_applications(&mut cached.snapshot.applications);
                cached.dirty.clear();
                cached.projections = self.timeline_projections(&cached.snapshot)?;
                cached.next_boundary = self.timeline_next_boundary(end)?;
            }
            cached
        } else {
            let snapshot = self.timeline_snapshot(start, end)?;
            Cached {
                projections: self.timeline_projections(&snapshot)?,
                next_boundary: self.timeline_next_boundary(end)?,
                snapshot,
                dirty: BTreeSet::new(),
            }
        };
        // Sorting can change a segment's index when two windows start together.
        // Rebuild the tiny live projection without revisiting completed intervals.
        cached.projections = self.timeline_projections(&cached.snapshot)?;
        let result = cached.snapshot.clone();
        let mut cache = self.timeline_cache.borrow_mut();
        cache.changes = changes;
        cache.entries.push_back(cached);
        while cache.entries.len() > 3 {
            cache.entries.pop_front();
        }
        Ok(result)
    }

    fn timeline_next_boundary(&self, end: i64) -> Result<i64> {
        Ok(self
            .connection
            .query_row(
                "SELECT MIN(t) FROM (
                SELECT opened_utc_ms AS t FROM window_instances UNION ALL
                SELECT closed_utc_ms FROM window_instances UNION ALL
                SELECT started_utc_ms FROM window_state_intervals UNION ALL
                SELECT ended_utc_ms FROM window_state_intervals UNION ALL
                SELECT started_utc_ms FROM tray_background_intervals UNION ALL
                SELECT ended_utc_ms FROM tray_background_intervals UNION ALL
                SELECT minute_started_utc_ms FROM input_minute_buckets UNION ALL
                SELECT minute_started_utc_ms + 60000 FROM input_minute_buckets UNION ALL
                SELECT started_utc_ms FROM data_availability UNION ALL
                SELECT ended_utc_ms FROM data_availability
             ) WHERE t >= ?1",
                [end],
                |r| r.get::<_, Option<i64>>(0),
            )?
            .unwrap_or(i64::MAX))
    }

    fn timeline_projections(
        &self,
        snapshot: &TimelineSnapshot,
    ) -> Result<HashMap<String, Projection>> {
        let start = snapshot.range_started_utc_ms;
        let end = snapshot.range_ended_utc_ms;
        let edges = self.merge_links()?;
        let mut roots = HashMap::new();
        let mut canonical = |id: String| {
            roots
                .entry(id.clone())
                .or_insert_with(|| root(&id, &edges))
                .clone()
        };
        let mut result = HashMap::<String, Projection>::new();
        let mut instance_indices = HashMap::new();
        let mut indices = HashMap::<String, usize>::new();
        let mut windows = self.connection.prepare(
            "SELECT instance_id, application_identity, closed_utc_ms
             FROM window_instances WHERE opened_utc_ms < ?2 AND COALESCE(closed_utc_ms, ?2) > ?1
             ORDER BY MAX(opened_utc_ms, ?1), instance_id",
        )?;
        for row in windows.query_map(params![start, end], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<i64>>(2)?,
            ))
        })? {
            let (instance, identity, closed) = row?;
            let identity = canonical(identity);
            let index = indices.entry(identity.clone()).or_default();
            instance_indices.insert(instance, *index);
            if closed.is_none_or(|t| t > end) {
                let p = result.entry(identity).or_default();
                p.opened = true;
                p.windows.insert(*index, (false, false));
            }
            *index += 1;
        }
        let mut states = self.connection.prepare(
            "SELECT s.window_instance_id, w.application_identity, s.started_utc_ms, s.displayed, s.focused
             FROM window_state_intervals s JOIN window_instances w ON w.instance_id=s.window_instance_id
             WHERE s.started_utc_ms < ?2 AND COALESCE(s.ended_utc_ms, ?2 + 1) > ?2"
        )?;
        for row in states.query_map(params![start, end], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, bool>(3)?,
                r.get::<_, bool>(4)?,
            ))
        })? {
            let (instance, identity, began, visible, focused) = row?;
            let identity = canonical(identity);
            let p = result.entry(identity.clone()).or_default();
            p.displayed |= visible;
            p.focused |= focused;
            if let Some(index) = instance_indices.get(&instance)
                && let Some(flags) = p.windows.get_mut(index)
            {
                flags.0 |= visible;
                flags.1 |= focused;
            }
            mark_segment(
                snapshot,
                &identity,
                began.max(start),
                visible,
                focused,
                false,
                p,
            );
        }
        let mut tray = self.connection.prepare(
            "SELECT application_identity, started_utc_ms FROM tray_background_intervals
             WHERE started_utc_ms < ?2 AND COALESCE(ended_utc_ms, ?2 + 1) > ?2",
        )?;
        for row in tray.query_map(params![start, end], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })? {
            let (identity, began) = row?;
            let identity = canonical(identity);
            let p = result.entry(identity.clone()).or_default();
            p.opened = true;
            mark_segment(snapshot, &identity, began.max(start), false, false, true, p);
        }
        Ok(result)
    }
}

fn mark_segment(
    snapshot: &TimelineSnapshot,
    identity: &str,
    began: i64,
    displayed: bool,
    focused: bool,
    tray: bool,
    projection: &mut Projection,
) {
    if let Some(app) = snapshot
        .applications
        .iter()
        .find(|a| a.identity == identity)
        && let Some(index) = app.segments.iter().enumerate().find_map(|(i, s)| {
            (s.started_utc_ms == began
                && s.ended_utc_ms == snapshot.range_ended_utc_ms
                && s.displayed == displayed
                && s.focused == focused
                && s.inferred_tray == tray
                && !projection.segments.contains(&i))
            .then_some(i)
        })
    {
        projection.segments.insert(index);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use timelens_ipc::{IdentitySource, WindowObservation, WindowTransition};
    const BASE: i64 = 1_700_000_000_000;

    #[test]
    #[ignore = "Manual, read-only benchmark of an explicitly supplied encrypted dataset"]
    fn read_only_dataset_cache_matches_full_queries_and_measures_cpu() {
        let directory = PathBuf::from(
            std::env::var_os("TIMELENS_CPU_DATA").expect("TIMELENS_CPU_DATA required"),
        );
        let output = PathBuf::from(
            std::env::var_os("TIMELENS_CPU_OUTPUT").expect("TIMELENS_CPU_OUTPUT required"),
        );
        let key_path = directory.join(KEY_FILE);
        let database_path = directory.join(DATABASE_FILE);
        let key = load_key(&key_path).unwrap();
        let connection = open_encrypted(&database_path, &key, true).unwrap();
        connection
            .execute_batch("PRAGMA query_only=ON; BEGIN;")
            .unwrap();
        let s = Storage {
            cipher_version: cipher_version(&connection).unwrap(),
            connection: connection.into(),
            timeline_cache: Default::default(),
            data_directory: directory.clone(),
            control_directory: directory,
            key_path,
            database_path,
        };
        let end = unix_time_ms();
        let local = chrono::DateTime::from_timestamp_millis(end)
            .unwrap()
            .with_timezone(&chrono::Local);
        let start = local
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_local_timezone(chrono::Local)
            .earliest()
            .unwrap()
            .timestamp_millis();
        let mut full_ms = 0.0;
        let mut cached_ms = 0.0;
        let mut full_cpu = 0.0;
        let mut cached_cpu = 0.0;
        let mut app_count = 0;
        for index in 0..100 {
            let at = end + index * 2000;
            let cpu = process_cpu_seconds();
            let time = Instant::now();
            let full = s.timeline_snapshot(start, at).unwrap();
            full_ms += time.elapsed().as_secs_f64() * 1000.0;
            full_cpu += process_cpu_seconds() - cpu;
            let cpu = process_cpu_seconds();
            let time = Instant::now();
            let cached = s.timeline_snapshot_cached(start, at).unwrap();
            cached_ms += time.elapsed().as_secs_f64() * 1000.0;
            cached_cpu += process_cpu_seconds() - cpu;
            app_count = full.applications.len();
            assert!(cached == full, "incremental frame {index} differs");
        }
        s.connection.execute_batch("ROLLBACK;").unwrap();
        let policy = s.snapshot_policy().unwrap();
        let collection = s.collection_policy().unwrap();
        fs::write(
            output,
            serde_json::to_vec_pretty(&serde_json::json!({
                "ComparedFrames":100, "DifferentFrames":0, "Applications":app_count,
                "FullQueryMilliseconds":full_ms, "CachedQueryMilliseconds":cached_ms,
                "FullQueryCpuSeconds":full_cpu, "CachedQueryCpuSeconds":cached_cpu,
                "ReadOnly":true, "StartupMigrationOrWritesPerformed":false,
                "Schema":s.schema_version().unwrap(),
                "SnapshotPolicy":{
                    "Enabled":policy.enabled, "IntervalMinutes":policy.interval_minutes,
                    "AllDisplays":policy.capture_all_displays, "RetentionDays":policy.retention_days,
                    "MaxBytes":policy.max_bytes, "CycleStartedUtcMs":policy.cycle_started_utc_ms
                },
                "CollectionPaused":collection.paused,
                "CollectionRevision":collection.revision,
                "CollectionPolicySha256":hex::encode(milestone3::sha256(&serde_json::to_vec(&collection).unwrap()).unwrap()),
                "CollectorBatches":s.connection.query_row("SELECT COUNT(*) FROM collector_batches",[],|r|r.get::<_,i64>(0)).unwrap(),
                "SnapshotSlots":s.connection.query_row("SELECT COUNT(*) FROM snapshot_slots WHERE result='success'",[],|r|r.get::<_,i64>(0)).unwrap(),
                "LastScheduledSlotUtcMs":s.connection.query_row("SELECT MAX(slot_started_utc_ms) FROM snapshot_slots WHERE trigger='scheduled'",[],|r|r.get::<_,Option<i64>>(0)).unwrap(),
                "LastScheduledCaptureUtcMs":s.connection.query_row("SELECT MAX(captured_at_utc_ms) FROM snapshot_slots WHERE trigger='scheduled' AND result='success'",[],|r|r.get::<_,Option<i64>>(0)).unwrap()
            }))
            .unwrap(),
        )
        .unwrap();
    }

    fn process_cpu_seconds() -> f64 {
        use windows_sys::Win32::Foundation::FILETIME;
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
        let mut created = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let mut exited = created;
        let mut kernel = created;
        let mut user = created;
        assert_ne!(
            unsafe {
                GetProcessTimes(
                    GetCurrentProcess(),
                    &mut created,
                    &mut exited,
                    &mut kernel,
                    &mut user,
                )
            },
            0
        );
        let ticks = |t: FILETIME| ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64;
        (ticks(kernel) + ticks(user)) as f64 / 10_000_000.0
    }

    fn window(
        id: u64,
        name: &str,
        at: i64,
        kind: WindowTransitionKind,
        visible: bool,
        focused: bool,
    ) -> CollectorEvent {
        CollectorEvent {
            observed_at_utc_ms: BASE + at,
            monotonic_ms: at.max(0) as u64 + 1,
            body: Some(collector_event::Body::WindowTransition(WindowTransition {
                kind: kind as i32,
                window: Some(WindowObservation {
                    window_id: id,
                    process_id: id as u32,
                    process_started_at_100ns: 100,
                    application_identity: format!("path:c:\\apps\\{name}.exe"),
                    identity_source: IdentitySource::ExecutablePath as i32,
                    executable_path: Some(format!("C:\\Apps\\{name}.exe")),
                    displayed: visible,
                    focused,
                    on_current_virtual_desktop: Some(true),
                    ..Default::default()
                }),
            })),
        }
    }
    fn ingest(s: &Storage, sequence: u64, events: Vec<CollectorEvent>) {
        s.ingest_event_batch(&EventBatch {
            collector_run_id: vec![1; 16],
            first_sequence: sequence,
            events,
        })
        .unwrap();
    }
    fn check(s: &Storage, start: i64, end: i64) -> TimelineSnapshot {
        let cached = s
            .timeline_snapshot_cached(BASE + start, BASE + end)
            .unwrap();
        assert_eq!(
            cached,
            s.timeline_snapshot(BASE + start, BASE + end).unwrap()
        );
        cached
    }
    fn fixture() -> (tempfile::TempDir, Storage) {
        let dir = tempfile::tempdir().unwrap();
        let s = Storage::open(dir.path()).unwrap();
        (dir, s)
    }

    #[test]
    fn ongoing_union_and_per_window_durations_match_full_queries() {
        let (_dir, s) = fixture();
        ingest(
            &s,
            1,
            vec![
                window(1, "a", 0, WindowTransitionKind::Opened, true, true),
                window(2, "a", 0, WindowTransitionKind::Opened, false, false),
            ],
        );
        for end in [100, 101, 500, 1000] {
            let view = check(&s, -1, end);
            assert_eq!(view.applications[0].opened_ms, end as u64);
            assert_eq!(view.applications[0].window_count, 2);
        }
    }

    #[test]
    fn late_focus_transitions_replace_only_the_affected_applications() {
        let (_dir, s) = fixture();
        ingest(
            &s,
            1,
            vec![
                window(1, "a", 0, WindowTransitionKind::Opened, true, true),
                window(1, "a", 100, WindowTransitionKind::Closed, false, false),
                window(2, "b", 120, WindowTransitionKind::Opened, true, true),
            ],
        );
        check(&s, -1, 200);
        ingest(
            &s,
            4,
            vec![window(
                2,
                "b",
                150,
                WindowTransitionKind::Updated,
                true,
                false,
            )],
        );
        let cache = s.timeline_cache.borrow();
        assert_eq!(
            cache.entries[0].dirty,
            BTreeSet::from(["path:c:\\apps\\b.exe".to_owned()])
        );
        drop(cache);
        check(&s, -1, 300);
        ingest(
            &s,
            5,
            vec![window(
                3,
                "c",
                310,
                WindowTransitionKind::Opened,
                true,
                true,
            )],
        );
        check(&s, -1, 400);
    }

    #[test]
    fn merges_deletes_retention_and_clear_discard_cached_statistics() {
        let (_dir, s) = fixture();
        ingest(
            &s,
            1,
            vec![
                window(1, "a", 0, WindowTransitionKind::Opened, true, true),
                window(2, "b", 0, WindowTransitionKind::Opened, true, false),
            ],
        );
        check(&s, -1, 100);
        let a = "path:c:\\apps\\a.exe";
        let b = "path:c:\\apps\\b.exe";
        s.set_application_merge(a, b, true).unwrap();
        assert_eq!(check(&s, -1, 200).applications.len(), 1);
        s.set_application_merge(a, b, false).unwrap();
        check(&s, -1, 300);
        s.delete_application_history(a).unwrap();
        assert!(s.timeline_cache.borrow().entries.is_empty());
        assert_eq!(check(&s, -1, 400).applications.len(), 1);
        s.apply_retention(BASE + 500).unwrap();
        assert!(s.timeline_cache.borrow().entries.is_empty());
        check(&s, -1, 600);
        s.clear_all().unwrap();
        assert!(s.timeline_cache.borrow().entries.is_empty());
        assert!(check(&s, -1, 700).applications.is_empty());
    }

    #[test]
    fn future_boundaries_backwards_ranges_and_new_runs_match_the_uncached_view() {
        let (_dir, s) = fixture();
        ingest(
            &s,
            1,
            vec![
                window(1, "a", 0, WindowTransitionKind::Opened, true, true),
                window(1, "a", 100, WindowTransitionKind::Closed, false, false),
            ],
        );
        for end in [0, 1, 50, 100, 101, 70, 150] {
            check(&s, -1, end);
        }
        s.ingest_event_batch(&EventBatch {
            collector_run_id: vec![2; 16],
            first_sequence: 1,
            events: vec![window(
                2,
                "b",
                200,
                WindowTransitionKind::Opened,
                true,
                true,
            )],
        })
        .unwrap();
        assert!(s.timeline_cache.borrow().entries.is_empty());
        check(&s, -1, 250);
    }

    #[test]
    fn unrelated_health_writes_reuse_the_cache_and_unknown_writes_invalidate_it() {
        let (_dir, s) = fixture();
        ingest(
            &s,
            1,
            vec![window(1, "a", 0, WindowTransitionKind::Opened, true, true)],
        );
        check(&s, -1, 100);
        s.record_component_health("collector", 1, Some(1)).unwrap();
        assert_eq!(s.timeline_cache.borrow().entries.len(), 1);
        check(&s, -1, 200);
        s.connection
            .execute("UPDATE window_state_intervals SET focused=0", [])
            .unwrap();
        assert_eq!(check(&s, -1, 300).applications[0].focused_ms, 0);
        for start in -10..-1 {
            check(&s, start, 400);
        }
        assert!(s.timeline_cache.borrow().entries.len() <= 3);
    }

    #[test]
    fn delayed_input_updates_the_application_and_global_counts() {
        let (_dir, s) = fixture();
        ingest(
            &s,
            1,
            vec![window(1, "a", 0, WindowTransitionKind::Opened, true, true)],
        );
        check(&s, -60_000, 70_000);
        let minute = BASE - BASE.rem_euclid(60_000);
        ingest(
            &s,
            2,
            vec![CollectorEvent {
                observed_at_utc_ms: BASE + 60_000,
                monotonic_ms: 60_001,
                body: Some(collector_event::Body::InputMinute(
                    timelens_ipc::InputMinute {
                        minute_started_at_utc_ms: minute,
                        timezone_offset_minutes: 0,
                        local_date: chrono::DateTime::from_timestamp_millis(minute)
                            .unwrap()
                            .format("%Y-%m-%d")
                            .to_string(),
                        focused_application_identity: Some("path:c:\\apps\\a.exe".into()),
                        left_click_count: 3,
                        middle_click_count: 1,
                        right_click_count: 2,
                        ..Default::default()
                    },
                )),
            }],
        );
        let result = check(&s, -60_000, 80_000);
        assert_eq!(result.left_click_count, 3);
        assert_eq!(result.applications[0].middle_click_count, 1);
    }

    #[test]
    fn inferred_tray_time_extends_without_recounting_closed_window_history() {
        let (_dir, s) = fixture();
        let tray = |kind, at| CollectorEvent {
            observed_at_utc_ms: BASE + at,
            monotonic_ms: at as u64 + 1,
            body: Some(collector_event::Body::TrayTransition(
                timelens_ipc::TrayTransition {
                    kind: kind as i32,
                    application_identity: "path:c:\\apps\\a.exe".into(),
                    process_id: 1,
                    process_started_at_100ns: 100,
                },
            )),
        };
        ingest(
            &s,
            1,
            vec![
                window(1, "a", 0, WindowTransitionKind::Opened, true, true),
                window(1, "a", 100, WindowTransitionKind::Closed, false, false),
                tray(timelens_ipc::TrayTransitionKind::Started, 100),
            ],
        );
        for end in [200, 300, 500] {
            assert_eq!(
                check(&s, -1, end).applications[0].background_ms,
                (end - 100) as u64
            );
        }
        ingest(
            &s,
            4,
            vec![tray(timelens_ipc::TrayTransitionKind::Ended, 600)],
        );
        check(&s, -1, 700);
    }
}
