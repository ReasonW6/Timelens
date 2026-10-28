use super::*;
use std::collections::{BTreeMap, BTreeSet};
use timelens_ipc::{SystemInterval, privacy::Policy};
#[derive(serde::Serialize, serde::Deserialize)]
struct MergeRules {
    members: BTreeSet<String>,
    policy: Policy,
    snapshots: BTreeSet<String>,
}
/// Every identity merged with `identity`, itself included; the first is the group's root.
pub(crate) fn merge_group(identity: &str, edges: &[(String, String)]) -> BTreeSet<String> {
    let mut members = BTreeSet::from([identity.into()]);
    loop {
        let before = members.len();
        for (a, b) in edges {
            if members.contains(a) || members.contains(b) {
                members.insert(a.clone());
                members.insert(b.clone());
            }
        }
        if before == members.len() {
            return members;
        }
    }
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
    /// User merges plus the automatic ones. An unpackaged executable that labels
    /// some windows with its own AppUserModelID, such as Steam's main window, and
    /// leaves the rest unlabeled, such as its store and friends windows, is one
    /// application: its `aumid:` and `path:` identities join. An executable
    /// behind several AppUserModelIDs, such as a browser hosting web apps, stays
    /// apart, since the unlabeled windows cannot tell which one they belong to.
    pub(crate) fn merge_edges(&self) -> Result<Vec<(String, String)>> {
        let mut edges = self.merge_links()?;
        let mut labels = BTreeMap::<String, Vec<String>>::new();
        for source in self.application_icon_sources()? {
            if let Some(path) = source.executable_path {
                let identity = format!("path:{}", path.replace('/', "\\").to_lowercase());
                labels.entry(identity).or_default().push(source.identity);
            }
        }
        let mut known = self
            .connection
            .prepare("SELECT 1 FROM applications WHERE identity=?1")?;
        for (path, aumids) in labels {
            if let [aumid] = aumids.as_slice()
                && known.exists([&path])?
            {
                edges.push((aumid.clone(), path));
            }
        }
        Ok(edges)
    }
    pub fn merged_members(&self, identity: &str) -> Result<BTreeSet<String>> {
        Ok(merge_group(identity, &self.merge_edges()?))
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
        let edges = self.merge_edges()?;
        for (id, mut app) in applications {
            let root = merge_group(&id, &edges).pop_first().unwrap_or(id);
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
    /// The system states overlapping `start..end`, clipped to it, earliest
    /// first: kind, start and end.
    pub fn system_intervals_between(
        &self,
        start: i64,
        end: i64,
    ) -> Result<Vec<(String, i64, i64)>> {
        let mut s = self.connection.prepare("SELECT kind,started_utc_ms,ended_utc_ms FROM system_intervals WHERE started_utc_ms<?2 AND ended_utc_ms>?1 ORDER BY started_utc_ms")?;
        let rows = s.query_map(params![start, end], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?.max(start),
                r.get::<_, i64>(2)?.min(end),
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
    /// When the earliest retained system state or instant began.
    pub fn earliest_system_utc_ms(&self) -> Result<Option<i64>> {
        Ok(self.connection.query_row(
            "SELECT MIN(started_utc_ms) FROM system_intervals",
            [],
            |row| row.get(0),
        )?)
    }
    /// How often each instant marker, such as a clean shutdown, was recorded
    /// within `start..end`. Markers take no time, so totals leave them out.
    pub fn system_marks(&self, start: i64, end: i64) -> Result<BTreeMap<String, u64>> {
        let mut s = self.connection.prepare("SELECT kind,COUNT(*) FROM system_intervals WHERE ended_utc_ms=started_utc_ms AND started_utc_ms>=?1 AND started_utc_ms<?2 GROUP BY kind")?;
        let rows = s.query_map(params![start, end], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?.max(0) as u64))
        })?;
        Ok(rows.collect::<rusqlite::Result<BTreeMap<_, _>>>()?)
    }
    pub fn permanent_input_totals(&self) -> Result<[u64; 4]> {
        Ok(self.connection.query_row("SELECT COALESCE(SUM(keyboard_count),0),COALESCE(SUM(left_click_count),0),COALESCE(SUM(middle_click_count),0),COALESCE(SUM(right_click_count),0) FROM anonymous_daily_input_ledger",[],|r|Ok([r.get::<_,i64>(0)?.max(0) as u64,r.get::<_,i64>(1)?.max(0) as u64,r.get::<_,i64>(2)?.max(0) as u64,r.get::<_,i64>(3)?.max(0) as u64]))?)
    }
    /// Keyboard, left, middle and right click counts of the local dates from
    /// `first` to `last`, both included (YYYY-MM-DD). Like the key
    /// frequencies, they follow the retention period and leave out
    /// applications excluded from input statistics.
    pub fn input_totals_between(&self, first: &str, last: &str) -> Result<[u64; 4]> {
        // A local date lies within a day of the same UTC date, since offsets
        // stay within ±1440 minutes; the bound lets the time index narrow the
        // scan. Dates that do not parse leave it open.
        let bound = |date: &str, days: i64| {
            chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
                .ok()
                .and_then(|day| day.and_hms_opt(0, 0, 0))
                .map(|t| t.and_utc().timestamp_millis() + days * 86_400_000)
        };
        let low = bound(first, -1).unwrap_or(i64::MIN);
        let high = bound(last, 2).unwrap_or(i64::MAX);
        Ok(self.connection.query_row("SELECT COALESCE(SUM(keyboard_count),0),COALESCE(SUM(left_click_count),0),COALESCE(SUM(middle_click_count),0),COALESCE(SUM(right_click_count),0) FROM input_minute_buckets WHERE minute_started_utc_ms >= ?3 AND minute_started_utc_ms < ?4 AND local_date BETWEEN ?1 AND ?2",params![first, last, low, high],|r|Ok([r.get::<_,i64>(0)?.max(0) as u64,r.get::<_,i64>(1)?.max(0) as u64,r.get::<_,i64>(2)?.max(0) as u64,r.get::<_,i64>(3)?.max(0) as u64]))?)
    }
    /// The starts of the minutes within twenty minutes of `now` whose input is
    /// stored. Live previews are told apart by minute against these rather
    /// than against the latest one, which deleting history moves back and a
    /// clock set back leaves ahead.
    pub fn stored_input_minutes_near(&self, now: i64) -> Result<BTreeSet<i64>> {
        const NEAR: i64 = 20 * 60_000;
        Ok(self
            .connection
            .prepare(
                "SELECT DISTINCT minute_started_utc_ms FROM input_minute_buckets
                 WHERE minute_started_utc_ms >= ?1 AND minute_started_utc_ms < ?2",
            )?
            .query_map(
                params![now.saturating_sub(NEAR), now.saturating_add(NEAR)],
                |row| row.get(0),
            )?
            .collect::<rusqlite::Result<_>>()?)
    }
    /// Presses per physical key over the local dates from `first` to `last`,
    /// both included (YYYY-MM-DD).
    pub fn physical_key_frequencies_between(
        &self,
        first: &str,
        last: &str,
    ) -> Result<Vec<(u32, u64)>> {
        Ok(self.connection.prepare("SELECT scan_code,SUM(key_count) FROM daily_physical_key_frequency WHERE local_date BETWEEN ?1 AND ?2 GROUP BY scan_code ORDER BY SUM(key_count) DESC")?.query_map([first, last],|r|Ok((r.get::<_,u32>(0)?,r.get::<_,i64>(1)?.max(0) as u64)))?.collect::<rusqlite::Result<_>>()?)
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
    pub fn application_deletion_impact(&self, identity: &str) -> Result<u64> {
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
        }
        Ok(windows)
    }
    pub fn delete_application_history(&self, identity: &str) -> Result<u64> {
        self.timeline_cache
            .borrow_mut()
            .clear(self.connection.total_changes());
        let members = self.merged_members(identity)?;
        let tx = Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let mut rows = 0;
        for member in members {
            let range=tx.query_row("SELECT MIN(opened_utc_ms),MAX(COALESCE(closed_utc_ms,?2)) FROM window_instances WHERE application_identity=?1",params![member,unix_time_ms()],|r|Ok((r.get::<_,Option<i64>>(0)?,r.get::<_,Option<i64>>(1)?)))?;
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
impl Storage {
    /// Remove an application's activity and input within `start..end`, as when
    /// the user deletes one stretch of its use. What lies wholly inside goes and
    /// what crosses one edge is cut back to it. A window or state that runs
    /// through the whole stretch stays: splitting it would invent a boundary.
    /// Nothing after now changes, so open windows and states carry on, and
    /// input goes by whole minutes. What the collector still has on its way
    /// for the stretch is kept out as it arrives. Returns the rows removed or
    /// cut; with none, nothing is written.
    pub fn delete_application_activity_between(
        &self,
        identity: &str,
        start: i64,
        end: i64,
    ) -> Result<u64> {
        let end = end.min(unix_time_ms());
        if end <= start {
            return Ok(0);
        }
        let members = self.merged_members(identity)?;
        let tx = Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let mut rows = 0;
        let mut input_rows = 0;
        const STATES: &str = "window_instance_id IN (
            SELECT instance_id FROM window_instances WHERE application_identity=?3)";
        const TRAY: &str = "application_identity=?3";
        for member in &members {
            let bounds = params![start, end, member];
            for (table, owner, began, ended) in [
                ("window_state_intervals", STATES, "started", "ended"),
                ("tray_background_intervals", TRAY, "started", "ended"),
            ] {
                rows += tx.execute(
                    &format!(
                        "DELETE FROM {table} WHERE {owner}
                         AND {began}_utc_ms>=?1 AND {ended}_utc_ms<=?2"
                    ),
                    bounds,
                )? as u64;
                rows += clip(&tx, table, owner, began, ended, bounds)?;
            }
            // A window goes once nothing of it is left inside.
            rows += tx.execute(
                "DELETE FROM window_instances WHERE application_identity=?3
                 AND opened_utc_ms>=?1 AND closed_utc_ms<=?2
                 AND NOT EXISTS (SELECT 1 FROM window_state_intervals
                     WHERE window_instance_id=window_instances.instance_id)",
                bounds,
            )? as u64;
            rows += clip(
                &tx,
                "window_instances",
                "application_identity=?3",
                "opened",
                "closed",
                bounds,
            )?;
            input_rows += tx.execute(
                "DELETE FROM input_minute_buckets WHERE focused_application_identity=?3
                 AND minute_started_utc_ms>=?1 AND minute_started_utc_ms+60000<=?2",
                bounds,
            )? as u64;
        }
        if rows + input_rows == 0 {
            return Ok(0);
        }
        for member in &members {
            tx.execute(
                "INSERT INTO deleted_application_stretches VALUES(?1,?2,?3)",
                params![member, start, end],
            )?;
        }
        for (category, count) in [("activity", rows), ("input", input_rows)] {
            if count > 0 {
                tx.execute("INSERT INTO data_availability(data_class,status,started_utc_ms,ended_utc_ms,reason,item_count) VALUES(?,'cleaned',?,?,'user_deleted',?)",params![category,start,end,count as i64])?;
            }
        }
        tx.commit()?;
        self.timeline_cache
            .borrow_mut()
            .clear(self.connection.total_changes());
        self.connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;")?;
        Ok(rows + input_rows)
    }
}
/// Cut back to `start..end` (?1, ?2) the rows of `table` that `owner` picks
/// and that cross exactly one edge, moving monotonic time by as much. A row
/// still open crosses the end.
fn clip(
    tx: &Transaction<'_>,
    table: &str,
    owner: &str,
    began: &str,
    ended: &str,
    bounds: &[&dyn rusqlite::ToSql],
) -> Result<u64> {
    let before = tx.execute(
        &format!(
            "UPDATE {table} SET {ended}_monotonic_ms={ended}_monotonic_ms-({ended}_utc_ms-?1),
                {ended}_utc_ms=?1
             WHERE {owner} AND {began}_utc_ms<?1 AND {ended}_utc_ms>?1 AND {ended}_utc_ms<=?2"
        ),
        bounds,
    )?;
    let after = tx.execute(
        &format!(
            "UPDATE {table} SET {began}_monotonic_ms={began}_monotonic_ms+(?2-{began}_utc_ms),
                {began}_utc_ms=?2
             WHERE {owner} AND {began}_utc_ms>=?1 AND {began}_utc_ms<?2
               AND COALESCE({ended}_utc_ms,?2+1)>?2"
        ),
        bounds,
    )?;
    Ok((before + after) as u64)
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
        let (_dir, s) = fixtures::storage();
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
        assert_eq!(
            s.input_totals_between("0000-00-00", "9999-99-99").unwrap(),
            [0; 4]
        );
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
    fn input_totals_sum_minute_buckets_over_an_inclusive_date_range() {
        let (_dir, s) = fixtures::storage();
        // Minutes in UTC+8: local midnight is 16:00 UTC the day before.
        let at = |day: u32, hour: u32| {
            chrono::NaiveDate::from_ymd_opt(2026, 9, day)
                .and_then(|d| d.and_hms_opt(hour, 0, 0))
                .unwrap()
                .and_utc()
                .timestamp_millis()
        };
        for (minute, date, keys, left) in [
            (at(4, 1), "2026-09-04", 5, 1),
            (at(4, 16), "2026-09-05", 2, 3),
            (at(5, 15), "2026-09-05", 7, 0),
            (at(6, 16), "2026-09-07", 1, 1),
        ] {
            s.connection
                .execute(
                    "INSERT INTO input_minute_buckets(minute_started_utc_ms,timezone_offset_minutes,local_date,keyboard_count,left_click_count,middle_click_count,right_click_count) VALUES(?1,480,?2,?3,?4,0,1)",
                    params![minute, date, keys, left],
                )
                .unwrap();
        }
        assert_eq!(
            s.input_totals_between("2026-09-05", "2026-09-05").unwrap(),
            [9, 3, 0, 2]
        );
        assert_eq!(
            s.input_totals_between("0000-00-00", "9999-99-99").unwrap(),
            [15, 5, 0, 4]
        );
        assert_eq!(
            s.input_totals_between("2026-09-08", "2026-09-30").unwrap(),
            [0; 4]
        );
    }
    #[test]
    fn key_frequencies_sum_over_an_inclusive_date_range() {
        let (_dir, s) = fixtures::storage();
        for (date, code, count) in [
            ("2026-09-04", 30, 5),
            ("2026-09-05", 30, 2),
            ("2026-09-05", 31, 7),
            ("2026-09-07", 30, 1),
        ] {
            s.connection
                .execute(
                    "INSERT INTO daily_physical_key_frequency VALUES(?1,480,?2,0,?3)",
                    params![date, code, count],
                )
                .unwrap();
        }
        assert_eq!(
            s.physical_key_frequencies_between("2026-09-05", "2026-09-05")
                .unwrap(),
            [(31, 7), (30, 2)]
        );
        let all = s
            .physical_key_frequencies_between("0000-00-00", "9999-99-99")
            .unwrap()
            .into_iter()
            .collect::<BTreeMap<_, _>>();
        assert_eq!(all, BTreeMap::from([(30, 8), (31, 7)]));
        assert!(
            s.physical_key_frequencies_between("2026-09-08", "2026-09-30")
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn merge_is_reversible_and_uses_real_interval_union() {
        let (_dir, s) = fixtures::storage();
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
    fn an_executable_labeling_some_windows_is_one_application() {
        // The fixture's editor window carries no AppUserModelID.
        let (_dir, s) = fixtures::storage();
        let event = |window_id, aumid: &str, path: &str, now, kind| CollectorEvent {
            observed_at_utc_ms: now,
            monotonic_ms: now as u64,
            body: Some(collector_event::Body::WindowTransition(WindowTransition {
                kind: kind as i32,
                window: Some(WindowObservation {
                    window_id,
                    process_id: 2,
                    process_started_at_100ns: 3,
                    application_identity: format!("aumid:{}", aumid.to_lowercase()),
                    identity_source: IdentitySource::WindowAppUserModelId as i32,
                    executable_path: Some(path.into()),
                    app_user_model_id: Some(aumid.into()),
                    displayed: true,
                    ..Default::default()
                }),
            })),
        };
        let editor = "C:\\Private\\Editor.exe";
        s.ingest_event_batch(&EventBatch {
            collector_run_id: vec![2; 16],
            first_sequence: 1,
            events: vec![
                event(
                    5,
                    "Vendor.Editor",
                    editor,
                    7000,
                    WindowTransitionKind::Opened,
                ),
                event(
                    5,
                    "Vendor.Editor",
                    editor,
                    9000,
                    WindowTransitionKind::Closed,
                ),
            ],
        })
        .unwrap();
        let apps = s.timeline_snapshot(1, 10_000).unwrap().applications;
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].identity, "aumid:vendor.editor");
        assert_eq!(apps[0].window_count, 2);
        assert_eq!(apps[0].opened_ms, 7000);
        assert_eq!(
            s.merged_members("path:c:\\private\\editor.exe").unwrap(),
            BTreeSet::from([
                "aumid:vendor.editor".to_owned(),
                "path:c:\\private\\editor.exe".to_owned()
            ])
        );
        // A second label on the same executable leaves the unlabeled windows apart.
        s.ingest_event_batch(&EventBatch {
            collector_run_id: vec![2; 16],
            first_sequence: 3,
            events: vec![event(
                6,
                "Vendor.WebApp",
                editor,
                9500,
                WindowTransitionKind::Opened,
            )],
        })
        .unwrap();
        assert_eq!(
            s.timeline_snapshot(1, 10_000).unwrap().applications.len(),
            3
        );
        assert!(s.merge_links().unwrap().is_empty());
    }
    #[test]
    fn deleting_a_stretch_cuts_back_what_crosses_it_and_keeps_what_spans_it() {
        // The fixture's editor window is open and focused from 1000 to 6000.
        let (_dir, s) = fixtures::storage();
        let id = "path:c:\\private\\editor.exe";
        let opened = |s: &Storage| {
            s.timeline_snapshot(1, 10_000)
                .unwrap()
                .applications
                .first()
                .map(|app| (app.opened_ms, app.focused_ms))
        };
        let written = |s: &Storage| {
            s.connection
                .query_row(
                    "SELECT (SELECT COUNT(*) FROM data_availability)
                          + (SELECT COUNT(*) FROM deleted_application_stretches)",
                    [],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap()
        };
        // A window running through the whole stretch stays as it is, and
        // deleting nothing leaves no trace.
        assert_eq!(
            s.delete_application_activity_between(id, 2000, 3000)
                .unwrap(),
            0
        );
        assert_eq!(opened(&s), Some((5000, 5000)));
        assert_eq!(written(&s), 0);
        // One crossing an edge is cut back to it.
        assert_eq!(
            s.delete_application_activity_between(id, 5000, 7000)
                .unwrap(),
            2
        );
        assert_eq!(opened(&s), Some((4000, 4000)));
        assert_eq!(s.data_coverage(5000, 7000).unwrap().0, 0);
        // What lies wholly inside goes.
        s.delete_application_activity_between(id, 500, 8000)
            .unwrap();
        assert_eq!(opened(&s), None);
        s.verify_integrity().unwrap();
    }
    #[test]
    fn what_arrives_late_for_a_deleted_stretch_stays_deleted() {
        let (_dir, s) = fixtures::storage();
        let id = "path:c:\\apps\\late.exe";
        let window = |window_id, at: i64, kind: WindowTransitionKind, focused| CollectorEvent {
            observed_at_utc_ms: at,
            monotonic_ms: at as u64,
            body: Some(collector_event::Body::WindowTransition(WindowTransition {
                kind: kind as i32,
                window: Some(WindowObservation {
                    window_id,
                    process_id: 7,
                    process_started_at_100ns: 8,
                    application_identity: id.into(),
                    identity_source: IdentitySource::ExecutablePath as i32,
                    executable_path: Some("C:\\apps\\late.exe".into()),
                    displayed: true,
                    focused,
                    ..Default::default()
                }),
            })),
        };
        let input = |minute: i64| CollectorEvent {
            observed_at_utc_ms: minute + 60_500,
            monotonic_ms: (minute + 60_500) as u64,
            body: Some(collector_event::Body::InputMinute(InputMinute {
                minute_started_at_utc_ms: minute,
                local_date: "1970-01-01".into(),
                focused_application_identity: Some(id.into()),
                left_click_count: 4,
                ..Default::default()
            })),
        };
        let batch = |first_sequence, events| EventBatch {
            collector_run_id: vec![1; 16],
            first_sequence,
            events,
        };
        s.ingest_event_batch(&batch(
            3,
            vec![
                window(9, 10_000, WindowTransitionKind::Opened, true),
                input(60_000),
            ],
        ))
        .unwrap();
        // The open state and window are cut back to the end, the minute goes.
        assert_eq!(
            s.delete_application_activity_between(id, 5_000, 300_000)
                .unwrap(),
            3
        );
        // The collector had more of the stretch on its way, then carried on.
        s.ingest_event_batch(&batch(
            5,
            vec![
                window(9, 130_000, WindowTransitionKind::Updated, false),
                input(120_000),
                window(9, 200_000, WindowTransitionKind::Closed, false),
                window(10, 350_000, WindowTransitionKind::Opened, true),
            ],
        ))
        .unwrap();
        let apps = s.timeline_snapshot(1, 400_000).unwrap().applications;
        let late = apps.iter().find(|app| app.identity == id).unwrap();
        assert_eq!((late.window_count, late.opened_ms), (1, 50_000));
        assert_eq!(
            s.input_totals_between("1970-01-01", "1970-01-01").unwrap(),
            [0; 4]
        );
        let inverted: i64 = s
            .connection
            .query_row(
                "SELECT (SELECT COUNT(*) FROM window_state_intervals
                         WHERE ended_utc_ms < started_utc_ms)
                      + (SELECT COUNT(*) FROM window_instances
                         WHERE closed_utc_ms < opened_utc_ms)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(inverted, 0);
        s.verify_integrity().unwrap();
        // A day on, nothing more is on its way and the record of it goes.
        s.apply_retention(300_000 + 86_400_001).unwrap();
        assert_eq!(
            s.connection
                .query_row(
                    "SELECT COUNT(*) FROM deleted_application_stretches",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
    }
    #[test]
    fn explicit_app_deletion_removes_history_and_keeps_rules() {
        let (_dir, s) = fixtures::storage();
        let id = "path:c:\\private\\editor.exe";
        let mut p = s.collection_policy().unwrap();
        p.input.insert(id.into());
        s.set_collection_policy(p).unwrap();
        assert_eq!(s.application_deletion_impact(id).unwrap(), 1);
        s.delete_application_history(id).unwrap();
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
    fn system_intervals_are_coalesced() {
        let (_dir, s) = fixtures::storage();
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
    }
}
