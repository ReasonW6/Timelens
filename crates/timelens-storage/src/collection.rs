use super::*;
use std::collections::{BTreeMap, BTreeSet};
use timelens_ipc::{SystemInterval, privacy::Policy};
#[derive(serde::Serialize, serde::Deserialize)]
struct MergeRules {
    members: BTreeSet<String>,
    policy: Policy,
    snapshots: BTreeSet<String>,
}
impl Storage {
    pub fn collection_policy(&self) -> Result<Policy> {
        let raw: String = self.connection.query_row(
            "SELECT policy_json FROM collection_policy WHERE id=1",
            [],
            |r| r.get(0),
        )?;
        serde_json::from_str(&raw).map_err(|e| StorageError::Integrity(e.to_string()))
    }
    pub fn set_collection_policy(&self, mut policy: Policy) -> Result<()> {
        policy.revision = self.collection_policy()?.revision.saturating_add(1);
        policy
            .validate()
            .map_err(|e| StorageError::Integrity(e.to_string()))?;
        let raw =
            serde_json::to_string(&policy).map_err(|e| StorageError::Integrity(e.to_string()))?;
        let old = self.collection_policy()?;
        // Publish before acknowledgement; a failed database commit restores the old
        // effective file. The collector observes the revision before its next batch.
        policy
            .save(&self.control_directory)
            .map_err(|e| StorageError::Integrity(e.to_string()))?;
        if let Err(e) = self.connection.execute(
            "UPDATE collection_policy SET policy_json=? WHERE id=1",
            [raw],
        ) {
            let _ = old.save(&self.control_directory);
            return Err(e.into());
        }
        Ok(())
    }
    pub fn sync_collection_policy(&self) -> Result<()> {
        self.collection_policy()?
            .save(&self.control_directory)
            .map_err(|e| StorageError::Integrity(e.to_string()))
    }
    pub fn merge_links(&self) -> Result<Vec<(String, String)>> {
        Ok(self.connection.prepare("SELECT left_identity,right_identity FROM application_merges ORDER BY left_identity,right_identity")?.query_map([],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<rusqlite::Result<_>>()?)
    }
    pub fn merged_members(&self, identity: &str) -> Result<BTreeSet<String>> {
        let edges = self.merge_links()?;
        let mut members = BTreeSet::from([identity.into()]);
        loop {
            let before = members.len();
            for (a, b) in &edges {
                if members.contains(a) || members.contains(b) {
                    members.insert(a.clone());
                    members.insert(b.clone());
                }
            }
            if before == members.len() {
                return Ok(members);
            }
        }
    }
    pub fn set_application_merge(&self, left: &str, right: &str, merge: bool) -> Result<()> {
        if left == right || left.is_empty() || right.is_empty() {
            return Err(StorageError::Integrity("请选择两个不同应用".into()));
        }
        let (a, b) = if left < right {
            (left, right)
        } else {
            (right, left)
        };
        if merge {
            let mut members = self.merged_members(left)?;
            members.extend(self.merged_members(right)?);
            let mut policy = self.collection_policy()?;
            let prior = MergeRules {
                members: members.clone(),
                policy: policy.clone(),
                snapshots: self.snapshot_exclusions()?.into_iter().collect(),
            };
            for list in [&mut policy.activity, &mut policy.input] {
                if list.iter().any(|id| members.contains(id)) {
                    list.extend(members.iter().cloned());
                }
            }
            let snapshots = self.snapshot_exclusions()?;
            if snapshots.iter().any(|id| members.contains(id)) {
                for id in &members {
                    self.set_snapshot_excluded(id, true, unix_time_ms())?;
                }
            }
            self.set_collection_policy(policy)?;
            self.connection.execute(
                "INSERT OR IGNORE INTO application_merges VALUES(?1,?2,?3,?4)",
                params![
                    a,
                    b,
                    unix_time_ms(),
                    serde_json::to_string(&prior)
                        .map_err(|e| StorageError::Integrity(e.to_string()))?
                ],
            )?;
        } else {
            let raw:String=self.connection.query_row("SELECT prior_rules_json FROM application_merges WHERE left_identity=?1 AND right_identity=?2",params![a,b],|r|r.get(0))?;
            let prior: MergeRules =
                serde_json::from_str(&raw).map_err(|e| StorageError::Integrity(e.to_string()))?;
            let mut policy = self.collection_policy()?;
            let mut snapshots: BTreeSet<String> = self.snapshot_exclusions()?.into_iter().collect();
            for (current, original) in [
                (&mut policy.activity, &prior.policy.activity),
                (&mut policy.input, &prior.policy.input),
                (&mut snapshots, &prior.snapshots),
            ] {
                let existed = original.iter().any(|id| prior.members.contains(id));
                let retained = current.iter().any(|id| prior.members.contains(id));
                if existed && retained {
                    for id in &prior.members {
                        if original.contains(id) {
                            current.insert(id.clone());
                        } else {
                            current.remove(id);
                        }
                    }
                }
                // If absent originally but added while merged, retain for both sides.
            }
            self.connection.execute(
                "DELETE FROM application_merges WHERE left_identity=?1 AND right_identity=?2",
                params![a, b],
            )?;
            for id in &prior.members {
                let members = self.merged_members(id)?;
                for list in [&mut policy.activity, &mut policy.input, &mut snapshots] {
                    if list.iter().any(|id| members.contains(id)) {
                        list.extend(members.iter().cloned());
                    }
                }
            }
            for id in &prior.members {
                self.set_snapshot_excluded(id, snapshots.contains(id), unix_time_ms())?;
            }
            self.set_collection_policy(policy)?;
        }
        Ok(())
    }
    pub(crate) fn merge_working_applications(
        &self,
        applications: BTreeMap<String, WorkingApplication>,
    ) -> Result<BTreeMap<String, WorkingApplication>> {
        let mut merged = BTreeMap::<String, WorkingApplication>::new();
        for (id, mut app) in applications {
            let root = self.merged_members(&id)?.into_iter().next().unwrap_or(id);
            let target = merged.entry(root).or_default();
            target.open_intervals.append(&mut app.open_intervals);
            target
                .displayed_intervals
                .append(&mut app.displayed_intervals);
            target.focused_intervals.append(&mut app.focused_intervals);
            target.segments.append(&mut app.segments);
            target.windows.append(&mut app.windows);
            target.keyboard_count = target.keyboard_count.saturating_add(app.keyboard_count);
            target.left_click_count = target.left_click_count.saturating_add(app.left_click_count);
            target.middle_click_count = target
                .middle_click_count
                .saturating_add(app.middle_click_count);
            target.right_click_count = target
                .right_click_count
                .saturating_add(app.right_click_count);
        }
        Ok(merged)
    }
    pub fn system_totals(&self, start: i64, end: i64) -> Result<BTreeMap<String, u64>> {
        let mut intervals = BTreeMap::<String, Vec<(i64, i64)>>::new();
        let mut s=self.connection.prepare("SELECT kind,started_utc_ms,ended_utc_ms FROM system_intervals WHERE started_utc_ms<?2 AND ended_utc_ms>?1")?;
        for row in s.query_map(params![start, end], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })? {
            let (k, s, e) = row?;
            intervals
                .entry(k)
                .or_default()
                .push((s.max(start), e.min(end)));
        }
        Ok(intervals
            .into_iter()
            .map(|(k, v)| (k, union_duration_ms(&v)))
            .collect())
    }
    pub fn permanent_input_totals(&self) -> Result<[u64; 4]> {
        Ok(self.connection.query_row("SELECT COALESCE(SUM(keyboard_count),0),COALESCE(SUM(left_click_count),0),COALESCE(SUM(middle_click_count),0),COALESCE(SUM(right_click_count),0) FROM anonymous_daily_input_ledger",[],|r|Ok([r.get::<_,i64>(0)?.max(0) as u64,r.get::<_,i64>(1)?.max(0) as u64,r.get::<_,i64>(2)?.max(0) as u64,r.get::<_,i64>(3)?.max(0) as u64]))?)
    }
    pub fn physical_key_frequencies(&self, date: &str) -> Result<Vec<(u32, u64)>> {
        Ok(self.connection.prepare("SELECT scan_code,SUM(key_count) FROM daily_physical_key_frequency WHERE local_date=? GROUP BY scan_code ORDER BY SUM(key_count) DESC")?.query_map([date],|r|Ok((r.get::<_,u32>(0)?,r.get::<_,i64>(1)?.max(0) as u64)))?.collect::<rusqlite::Result<_>>()?)
    }
    pub fn known_applications(&self) -> Result<Vec<(String, String)>> {
        let mut ids: BTreeSet<String> = self
            .connection
            .prepare("SELECT identity FROM applications")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let p = self.collection_policy()?;
        ids.extend(p.activity);
        ids.extend(p.input);
        ids.extend(self.snapshot_exclusions()?);
        ids.into_iter()
            .map(|id| Ok((id.clone(), self.application_display_name(&id)?)))
            .collect()
    }
    pub fn application_deletion_impact(&self, identity: &str) -> Result<(u64, u64, u64)> {
        let mut reports = BTreeSet::new();
        let mut versions = BTreeSet::new();
        let mut windows = 0;
        for member in self.merged_members(identity)? {
            windows += self
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM window_instances WHERE application_identity=?",
                    [&member],
                    |r| r.get::<_, i64>(0),
                )?
                .max(0) as u64;
            reports.extend(self.connection.prepare("SELECT report_id FROM local_report_applications WHERE application_identity=?")?.query_map([&member],|r|r.get::<_,i64>(0))?.collect::<rusqlite::Result<Vec<_>>>()?);
            versions.extend(self.connection.prepare("SELECT j.job_id FROM ai_jobs j JOIN ai_version_sources s ON s.job_id=j.job_id WHERE s.application_identity=?")?.query_map([&member],|r|r.get::<_,i64>(0))?.collect::<rusqlite::Result<Vec<_>>>()?);
        }
        Ok((windows, reports.len() as u64, versions.len() as u64))
    }
    pub fn delete_application_history(&self, identity: &str) -> Result<u64> {
        let members = self.merged_members(identity)?;
        let tx = Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let mut rows = 0;
        for member in members {
            let range=tx.query_row("SELECT MIN(opened_utc_ms),MAX(COALESCE(closed_utc_ms,?2)) FROM window_instances WHERE application_identity=?1",params![member,unix_time_ms()],|r|Ok((r.get::<_,Option<i64>>(0)?,r.get::<_,Option<i64>>(1)?)))?;
            tx.execute("DELETE FROM ai_jobs WHERE job_id IN (SELECT job_id FROM ai_version_sources WHERE application_identity=?)",[&member])?;
            tx.execute("DELETE FROM local_reports WHERE report_id IN (SELECT report_id FROM local_report_applications WHERE application_identity=?)",[&member])?;
            rows+=tx.execute("DELETE FROM window_state_intervals WHERE window_instance_id IN (SELECT instance_id FROM window_instances WHERE application_identity=?)",[&member])? as u64;
            for (table, column) in [
                ("window_instances", "application_identity"),
                ("tray_background_intervals", "application_identity"),
                ("input_minute_buckets", "focused_application_identity"),
                ("application_metadata_revisions", "application_identity"),
                ("applications", "identity"),
            ] {
                rows +=
                    tx.execute(&format!("DELETE FROM {table} WHERE {column}=?"), [&member])? as u64;
            }
            if let (Some(start), Some(end)) = range {
                for category in ["activity", "input"] {
                    tx.execute("INSERT INTO data_availability(data_class,status,started_utc_ms,ended_utc_ms,reason,item_count) VALUES(?,'cleaned',?,?,'user_deleted',?)",params![category,start,end,rows as i64])?;
                }
            }
        }
        tx.commit()?;
        self.connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;")?;
        Ok(rows)
    }
}
pub(crate) fn ingest_system_interval(
    tx: &Transaction<'_>,
    event: &CollectorEvent,
    interval: &SystemInterval,
) -> Result<()> {
    if !timelens_ipc::privacy::SYSTEM_KINDS.contains(&interval.kind.as_str())
        || interval.started_utc_ms > event.observed_at_utc_ms
    {
        return Err(StorageError::InvalidBatch("invalid system interval".into()));
    }
    if interval.kind == "active" {
        return Ok(());
    }
    let previous:Option<(i64,i64)>=tx.query_row("SELECT interval_id,ended_utc_ms FROM system_intervals WHERE kind=? AND timezone_offset_minutes=? ORDER BY interval_id DESC LIMIT 1",params![interval.kind,interval.timezone_offset_minutes],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    if let Some((id, end)) = previous
        && end == interval.started_utc_ms
        && interval.kind != "clock_discontinuity"
    {
        tx.execute("UPDATE system_intervals SET ended_utc_ms=?,duration_ms=duration_ms+? WHERE interval_id=?",params![event.observed_at_utc_ms,interval.duration_ms.min(i64::MAX as u64) as i64,id])?;
    } else {
        tx.execute("INSERT INTO system_intervals(kind,started_utc_ms,ended_utc_ms,duration_ms,timezone_offset_minutes) VALUES(?,?,?,?,?)",params![interval.kind,interval.started_utc_ms,event.observed_at_utc_ms,interval.duration_ms.min(i64::MAX as u64) as i64,interval.timezone_offset_minutes])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use timelens_ipc::{WindowTransition, WindowTransitionKind};
    #[test]
    fn anonymous_input_is_only_daily_and_idempotent() {
        let (_dir, s) = ai::tests::fixture();
        let batch = EventBatch {
            collector_run_id: vec![1; 16],
            first_sequence: 3,
            events: vec![CollectorEvent {
                observed_at_utc_ms: 61000,
                monotonic_ms: 61000,
                body: Some(collector_event::Body::InputMinute(InputMinute {
                    anonymous_only: true,
                    minute_started_at_utc_ms: 0,
                    timezone_offset_minutes: 0,
                    local_date: "2026-09-05".into(),
                    keyboard_count: 9,
                    left_click_count: 2,
                    ..Default::default()
                })),
            }],
        };
        s.ingest_event_batch(&batch).unwrap();
        s.ingest_event_batch(&batch).unwrap();
        assert_eq!(s.permanent_input_totals().unwrap(), [9, 2, 0, 0]);
        for table in ["input_minute_buckets", "daily_physical_key_frequency"] {
            assert_eq!(
                s.connection
                    .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r
                        .get::<_, i64>(0))
                    .unwrap(),
                0
            );
        }
        let mut invalid = batch.clone();
        if let Some(collector_event::Body::InputMinute(m)) = &mut invalid.events[0].body {
            m.focused_application_identity = Some("private".into());
        }
        invalid.first_sequence = 4;
        assert!(s.ingest_event_batch(&invalid).is_err());
    }
    #[test]
    fn merge_is_reversible_and_uses_real_interval_union() {
        let (_dir, s) = ai::tests::fixture();
        let left = "path:c:\\private\\editor.exe";
        let right = "path:c:\\public\\other.exe";
        let event = |now, kind| CollectorEvent {
            observed_at_utc_ms: now,
            monotonic_ms: now as u64,
            body: Some(collector_event::Body::WindowTransition(WindowTransition {
                kind: kind as i32,
                window: Some(WindowObservation {
                    window_id: 2,
                    process_id: 3,
                    process_started_at_100ns: 4,
                    application_identity: right.into(),
                    identity_source: IdentitySource::ExecutablePath as i32,
                    executable_path: Some("C:\\public\\other.exe".into()),
                    displayed: true,
                    ..Default::default()
                }),
            })),
        };
        s.ingest_event_batch(&EventBatch {
            collector_run_id: vec![2; 16],
            first_sequence: 1,
            events: vec![
                event(2000, WindowTransitionKind::Opened),
                event(5000, WindowTransitionKind::Closed),
            ],
        })
        .unwrap();
        let mut policy = s.collection_policy().unwrap();
        policy.activity.insert(left.into());
        s.set_collection_policy(policy).unwrap();
        s.set_snapshot_excluded(right, true, 7000).unwrap();
        s.set_application_merge(left, right, true).unwrap();
        let p = s.collection_policy().unwrap();
        assert!(p.activity.contains(right));
        assert!(!p.input.contains(right));
        let apps = s.timeline_snapshot(1000, 6000).unwrap().applications;
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].opened_ms, 5000);
        let mut p = s.collection_policy().unwrap();
        p.input.extend([left.into(), right.into()]);
        s.set_collection_policy(p).unwrap();
        s.set_application_merge(left, right, false).unwrap();
        let p = s.collection_policy().unwrap();
        assert!(p.activity.contains(left));
        assert!(!p.activity.contains(right));
        assert!(p.input.contains(left) && p.input.contains(right));
        assert_eq!(s.snapshot_exclusions().unwrap(), vec![right]);
        assert_eq!(
            s.timeline_snapshot(1000, 6000).unwrap().applications.len(),
            2
        );
    }
    #[test]
    fn explicit_app_deletion_removes_derived_artifacts_and_keeps_rules() {
        let (_dir, s) = ai::tests::fixture();
        let version = ai::tests::summary(&s, 7000);
        let report = s.generate_local_report(1000, 6000, 7000).unwrap();
        let id = "path:c:\\private\\editor.exe";
        let mut p = s.collection_policy().unwrap();
        p.input.insert(id.into());
        s.set_collection_policy(p).unwrap();
        assert_eq!(s.application_deletion_impact(id).unwrap(), (1, 1, 1));
        s.delete_application_history(id).unwrap();
        assert!(s.ai_version(version).is_err());
        assert!(s.load_local_report(report.id).is_err());
        assert!(
            s.timeline_snapshot(1000, 6000)
                .unwrap()
                .applications
                .is_empty()
        );
        assert!(s.collection_policy().unwrap().input.contains(id));
        assert_eq!(s.data_coverage(1000, 6000).unwrap().0, 0);
        s.verify_integrity().unwrap();
    }
    #[test]
    fn system_intervals_are_coalesced_without_excluded_identities() {
        let (_dir, s) = ai::tests::fixture();
        let event = |start, end| CollectorEvent {
            observed_at_utc_ms: end,
            monotonic_ms: end as u64,
            body: Some(collector_event::Body::SystemInterval(SystemInterval {
                kind: "global_pause".into(),
                started_utc_ms: start,
                duration_ms: (end - start) as u64,
                timezone_offset_minutes: 480,
            })),
        };
        s.ingest_event_batch(&EventBatch {
            collector_run_id: vec![1; 16],
            first_sequence: 3,
            events: vec![event(6000, 7000), event(7000, 8000)],
        })
        .unwrap();
        assert_eq!(s.system_totals(6500, 7500).unwrap()["global_pause"], 1000);
        assert_eq!(
            s.connection
                .query_row("SELECT COUNT(*) FROM system_intervals", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        let env = s.ai_envelope(1000, 8000).unwrap();
        assert_eq!(env.system_ms["global_pause"], 2000);
        assert!(env.missing.iter().any(|g| g.reason == "global_pause"));
    }
}
