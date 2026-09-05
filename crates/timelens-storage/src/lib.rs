#![cfg(windows)]

mod ai;
mod collection;
mod milestone3;
mod portable;
mod recovery;
mod relocation;
pub use portable::{BackupInfo, BackupOptions, ExportFormat, PreparedRestore};
pub use recovery::RecoveryReport;

pub use ai::{
    AiJob, AiJobKind, AiJobSpec, AiMessage, AiRetentionOutcome, AiSettings, AiVersion,
    SnapshotConsent,
};

pub use milestone3::{
    LocalReport, LocalReportApplication, LocalReportGap, LocalReportSnapshotReason,
    SnapshotDisplay, SnapshotImage, SnapshotMissingReason, SnapshotPolicy, SnapshotSlot,
    SnapshotStoreRequest, SnapshotTrigger,
};

use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    mem::size_of,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use prost::Message;
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use timelens_ipc::{
    COLLECTOR_RESET_PAUSED_FILE, COLLECTOR_RESET_REQUEST_FILE, COLLECTOR_SPOOL_FILE,
    COLLECTOR_SPOOL_KEY_FILE, COLLECTOR_TRAY_STATE_FILE, CollectorEvent, EventBatch,
    IdentitySource, InputMinute, MonitoringGap, MonitoringGapReason, TrayTransition,
    TrayTransitionKind, WindowObservation, WindowTransitionKind, collector_event,
};
use windows_sys::Win32::{
    Foundation::{GetLastError, LocalFree},
    Security::Cryptography::{
        BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom, CRYPT_INTEGER_BLOB,
        CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    },
    Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW},
    System::Threading::GetCurrentProcessId,
};
use zeroize::Zeroizing;

const DATABASE_FILE: &str = "timelens.sqlite3";
const KEY_FILE: &str = "data-key.dpapi";
const BACKUP_FILE: &str = "timelens.sqlite3.migration-backup";
const KEY_MAGIC: &[u8; 8] = b"TLKEY\0\0\x01";
const KEY_BYTES: usize = 32;
const DPAPI_ENTROPY: &[u8] = b"Timelens local data key v1";
const LATEST_SCHEMA_VERSION: i64 = 12;

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("portable archive error: {0}")]
    Archive(#[from] zip::result::ZipError),
    #[error("storage I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("encrypted SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("Windows data protection failed with status {0}")]
    DataProtection(i32),
    #[error("the protected data-key file is invalid")]
    InvalidKeyFile,
    #[error("SQLCipher is unavailable in this build")]
    CipherUnavailable,
    #[error("database schema {found} is newer than supported schema {supported}")]
    SchemaTooNew { found: i64, supported: i64 },
    #[error("database integrity check failed: {0}")]
    Integrity(String),
    #[error("migration failed and the encrypted database was restored: {0}")]
    MigrationRolledBack(String),
    #[error("collector event batch is invalid: {0}")]
    InvalidBatch(String),
}

pub type Result<T> = std::result::Result<T, StorageError>;

/// Before the next database access, latch corruption reported by any statement.
/// All storage entrypoints use this connection, including UI and AI operations.
struct GuardedConnection {
    raw: Connection,
    quarantined: std::cell::Cell<bool>,
}
impl From<Connection> for GuardedConnection {
    fn from(raw: Connection) -> Self {
        Self {
            raw,
            quarantined: std::cell::Cell::new(false),
        }
    }
}
impl GuardedConnection {
    fn quarantine(&self) {
        if !self.quarantined.replace(true) {
            let _ = self.raw.execute_batch("PRAGMA query_only=ON;");
        }
    }
    fn is_quarantined(&self) -> bool {
        let code = unsafe { rusqlite::ffi::sqlite3_errcode(self.raw.handle()) };
        if matches!(
            code,
            rusqlite::ffi::SQLITE_CORRUPT | rusqlite::ffi::SQLITE_NOTADB
        ) {
            self.quarantine();
        }
        self.quarantined.get()
    }
}
impl std::ops::Deref for GuardedConnection {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.is_quarantined();
        &self.raw
    }
}
impl std::ops::DerefMut for GuardedConnection {
    fn deref_mut(&mut self) -> &mut Connection {
        self.is_quarantined();
        &mut self.raw
    }
}

pub struct Storage {
    connection: GuardedConnection,
    data_directory: PathBuf,
    control_directory: PathBuf,
    key_path: PathBuf,
    database_path: PathBuf,
    cipher_version: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimelineSnapshot {
    pub range_started_utc_ms: i64,
    pub range_ended_utc_ms: i64,
    pub applications: Vec<TimelineApplication>,
    pub keyboard_count: u64,
    pub left_click_count: u64,
    pub middle_click_count: u64,
    pub right_click_count: u64,
    pub monitoring_gaps: Vec<TimelineGap>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimelineApplication {
    pub identity: String,
    pub display_name: String,
    pub opened_ms: u64,
    pub displayed_ms: u64,
    pub focused_ms: u64,
    pub background_ms: u64,
    pub window_count: usize,
    pub keyboard_count: u64,
    pub left_click_count: u64,
    pub middle_click_count: u64,
    pub right_click_count: u64,
    pub windows: Vec<TimelineWindow>,
    pub segments: Vec<TimelineSegment>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimelineWindow {
    pub number: usize,
    pub opened_utc_ms: i64,
    pub ended_utc_ms: i64,
    pub opened_ms: u64,
    pub displayed_ms: u64,
    pub focused_ms: u64,
    pub background_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimelineSegment {
    pub started_utc_ms: i64,
    pub ended_utc_ms: i64,
    pub displayed: bool,
    pub focused: bool,
    pub inferred_tray: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimelineGap {
    pub data_class: String,
    pub started_utc_ms: i64,
    pub ended_utc_ms: i64,
    pub reason: String,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RetentionReport {
    pub activity_items: u64,
    pub input_items: u64,
    pub snapshot_items: u64,
    pub report_items: u64,
    pub released_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetentionPolicy {
    pub days: Option<u32>,
    pub max_bytes: u64,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            days: Some(30),
            max_bytes: 100 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ClearReport {
    pub deleted_rows: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowBatchOutcome {
    Stored,
    Duplicate,
}

impl Storage {
    pub fn open(data_directory: impl AsRef<Path>) -> Result<Self> {
        let data_directory = data_directory.as_ref();
        fs::create_dir_all(data_directory)?;
        let key_path = data_directory.join(KEY_FILE);
        let database_path = data_directory.join(DATABASE_FILE);
        let backup_path = data_directory.join(BACKUP_FILE);
        if database_path.exists() && !key_path.exists() {
            return Err(StorageError::InvalidKeyFile);
        }
        let key = load_or_create_key(&key_path)?;

        recover_interrupted_migration(&database_path, &backup_path, &key)?;
        // Detect damaged existing data before opening a writable connection.
        if database_path.metadata().is_ok_and(|m| m.len() > 0) {
            inspect_database(&database_path, &key)?;
        }
        let connection = migrate_database(&database_path, &backup_path, &key, MIGRATIONS)?;
        let cipher_version = cipher_version(&connection)?;

        let storage = Self {
            connection: connection.into(),
            data_directory: data_directory.to_owned(),
            control_directory: data_directory.to_owned(),
            key_path,
            database_path,
            cipher_version,
        };
        storage.reconcile_snapshot_files()?;
        storage.sync_collection_policy()?;
        storage.initialize_ai()?;
        Ok(storage)
    }

    pub fn schema_version(&self) -> Result<i64> {
        schema_version(&self.connection)
    }

    pub fn cipher_version(&self) -> &str {
        &self.cipher_version
    }

    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    pub fn record_component_health(
        &self,
        component: &str,
        protocol_version: u32,
        peer_process_id: Option<u32>,
    ) -> Result<()> {
        if !matches!(component, "core" | "collector") {
            return Err(StorageError::Integrity(
                "runtime health component is outside the fixed allowlist".to_owned(),
            ));
        }
        self.connection.execute(
            "INSERT INTO runtime_health (
                component, last_healthy_utc_ms, protocol_version, peer_process_id
             ) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(component) DO UPDATE SET
                last_healthy_utc_ms = excluded.last_healthy_utc_ms,
                protocol_version = excluded.protocol_version,
                peer_process_id = excluded.peer_process_id",
            params![
                component,
                unix_time_ms(),
                i64::from(protocol_version),
                peer_process_id.map(i64::from)
            ],
        )?;
        Ok(())
    }

    pub fn ingest_event_batch(&self, batch: &EventBatch) -> Result<WindowBatchOutcome> {
        let last_sequence = batch
            .last_sequence()
            .map_err(|error| StorageError::InvalidBatch(error.to_string()))?;
        let first_event = batch
            .events
            .first()
            .ok_or_else(|| StorageError::InvalidBatch("batch is empty".to_owned()))?;
        validate_event_order(&batch.events)?;
        let batch_checksum = i64::from(crc32fast::hash(&batch.encode_to_vec()));

        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        if let Some((stored_last, stored_checksum)) = transaction
            .query_row(
                "SELECT last_sequence, batch_checksum FROM collector_batches
                 WHERE run_id = ?1 AND first_sequence = ?2",
                params![
                    &batch.collector_run_id,
                    to_i64(batch.first_sequence, "sequence")?
                ],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?
        {
            if stored_last == to_i64(last_sequence, "sequence")?
                && stored_checksum == batch_checksum
            {
                transaction.commit()?;
                return Ok(WindowBatchOutcome::Duplicate);
            }
            return Err(StorageError::InvalidBatch(
                "batch overlaps stored sequence numbers with different content".to_owned(),
            ));
        }

        let prior_run = transaction
            .query_row(
                "SELECT last_sequence, last_observed_utc_ms, last_monotonic_ms
                 FROM collector_runs WHERE run_id = ?1",
                params![&batch.collector_run_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()?;
        match prior_run {
            Some((prior_sequence, prior_utc_ms, prior_monotonic_ms)) => {
                let expected = prior_sequence.checked_add(1).ok_or_else(|| {
                    StorageError::InvalidBatch("stored sequence overflow".to_owned())
                })?;
                if to_i64(batch.first_sequence, "sequence")? != expected {
                    return Err(StorageError::InvalidBatch(format!(
                        "batch sequence is not contiguous; expected {expected}"
                    )));
                }
                if first_event.observed_at_utc_ms < prior_utc_ms
                    || to_i64(first_event.monotonic_ms, "monotonic timestamp")? < prior_monotonic_ms
                {
                    return Err(StorageError::InvalidBatch(
                        "batch timestamps precede the prior durable event".to_owned(),
                    ));
                }
            }
            None => {
                if batch.first_sequence != 1 {
                    return Err(StorageError::InvalidBatch(
                        "a new collector run must begin at sequence 1".to_owned(),
                    ));
                }
                let has_explicit_gap = matches!(
                    first_event.body,
                    Some(collector_event::Body::MonitoringGap(_))
                );
                close_stale_runs(
                    &transaction,
                    first_event.observed_at_utc_ms,
                    !has_explicit_gap,
                )?;
                transaction.execute(
                    "INSERT INTO collector_runs (
                        run_id, first_observed_utc_ms, last_observed_utc_ms,
                        last_monotonic_ms, last_sequence
                     ) VALUES (?1, ?2, ?2, ?3, 0)",
                    params![
                        &batch.collector_run_id,
                        first_event.observed_at_utc_ms,
                        to_i64(first_event.monotonic_ms, "monotonic timestamp")?
                    ],
                )?;
            }
        }

        for (offset, event) in batch.events.iter().enumerate() {
            let sequence = batch
                .first_sequence
                .checked_add(offset as u64)
                .ok_or_else(|| StorageError::InvalidBatch("sequence overflow".to_owned()))?;
            ingest_collector_event(&transaction, &batch.collector_run_id, sequence, event)?;
        }

        let last_event = batch.events.last().expect("non-empty batch checked above");
        transaction.execute(
            "UPDATE collector_runs SET
                last_observed_utc_ms = ?2,
                last_monotonic_ms = ?3,
                last_sequence = ?4
             WHERE run_id = ?1",
            params![
                &batch.collector_run_id,
                last_event.observed_at_utc_ms,
                to_i64(last_event.monotonic_ms, "monotonic timestamp")?,
                to_i64(last_sequence, "sequence")?
            ],
        )?;
        transaction.execute(
            "INSERT INTO collector_batches (
                run_id, first_sequence, last_sequence, event_count,
                batch_checksum, persisted_utc_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                &batch.collector_run_id,
                to_i64(batch.first_sequence, "sequence")?,
                to_i64(last_sequence, "sequence")?,
                batch.events.len() as i64,
                batch_checksum,
                unix_time_ms()
            ],
        )?;
        transaction.commit()?;
        Ok(WindowBatchOutcome::Stored)
    }

    pub fn timeline_snapshot(
        &self,
        range_started_utc_ms: i64,
        range_ended_utc_ms: i64,
    ) -> Result<TimelineSnapshot> {
        if range_started_utc_ms <= 0 || range_ended_utc_ms <= range_started_utc_ms {
            return Err(StorageError::Integrity(
                "timeline range is invalid".to_owned(),
            ));
        }
        let mut applications = BTreeMap::<String, WorkingApplication>::new();
        {
            let mut statement = self.connection.prepare(
                "SELECT instance_id, application_identity, opened_utc_ms,
                        COALESCE(closed_utc_ms, ?2)
                 FROM window_instances
                 WHERE opened_utc_ms < ?2
                   AND COALESCE(closed_utc_ms, ?2) > ?1
                 ORDER BY opened_utc_ms, instance_id",
            )?;
            let rows =
                statement.query_map(params![range_started_utc_ms, range_ended_utc_ms], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                })?;
            for row in rows {
                let (instance_id, identity, opened, ended) = row?;
                let interval =
                    clipped_interval(opened, ended, range_started_utc_ms, range_ended_utc_ms);
                let application = applications.entry(identity).or_default();
                application.open_intervals.push(interval);
                application.windows.insert(
                    instance_id,
                    WorkingWindow {
                        opened_utc_ms: interval.0,
                        ended_utc_ms: interval.1,
                        ..WorkingWindow::default()
                    },
                );
            }
        }
        {
            let mut statement = self.connection.prepare(
                "SELECT states.window_instance_id, instances.application_identity,
                        states.started_utc_ms, COALESCE(states.ended_utc_ms, ?2),
                        states.displayed, states.focused
                 FROM window_state_intervals AS states
                 JOIN window_instances AS instances
                   ON instances.instance_id = states.window_instance_id
                 WHERE states.started_utc_ms < ?2
                   AND COALESCE(states.ended_utc_ms, ?2) > ?1",
            )?;
            let rows =
                statement.query_map(params![range_started_utc_ms, range_ended_utc_ms], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, bool>(4)?,
                        row.get::<_, bool>(5)?,
                    ))
                })?;
            for row in rows {
                let (instance_id, identity, started, ended, displayed, focused) = row?;
                let interval =
                    clipped_interval(started, ended, range_started_utc_ms, range_ended_utc_ms);
                let application = applications.entry(identity).or_default();
                if displayed {
                    application.displayed_intervals.push(interval);
                }
                if focused {
                    application.focused_intervals.push(interval);
                }
                application.segments.push(TimelineSegment {
                    started_utc_ms: interval.0,
                    ended_utc_ms: interval.1,
                    displayed,
                    focused,
                    inferred_tray: false,
                });
                if let Some(window) = application.windows.get_mut(&instance_id) {
                    if displayed {
                        window.displayed_intervals.push(interval);
                    }
                    if focused {
                        window.focused_intervals.push(interval);
                    }
                }
            }
        }
        {
            let mut statement = self.connection.prepare(
                "SELECT application_identity, started_utc_ms,
                        COALESCE(ended_utc_ms, ?2)
                 FROM tray_background_intervals
                 WHERE started_utc_ms < ?2
                   AND COALESCE(ended_utc_ms, ?2) > ?1",
            )?;
            let rows =
                statement.query_map(params![range_started_utc_ms, range_ended_utc_ms], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })?;
            for row in rows {
                let (identity, started, ended) = row?;
                let interval =
                    clipped_interval(started, ended, range_started_utc_ms, range_ended_utc_ms);
                let application = applications.entry(identity).or_default();
                application.open_intervals.push(interval);
                application.segments.push(TimelineSegment {
                    started_utc_ms: interval.0,
                    ended_utc_ms: interval.1,
                    displayed: false,
                    focused: false,
                    inferred_tray: true,
                });
            }
        }
        {
            let mut statement = self.connection.prepare(
                "SELECT focused_application_identity,
                        SUM(keyboard_count), SUM(left_click_count),
                        SUM(middle_click_count), SUM(right_click_count)
                 FROM input_minute_buckets
                 WHERE minute_started_utc_ms < ?2
                   AND minute_started_utc_ms + 60000 > ?1
                 GROUP BY focused_application_identity",
            )?;
            let rows =
                statement.query_map(params![range_started_utc_ms, range_ended_utc_ms], |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                })?;
            for row in rows {
                let (Some(identity), keyboard, left, middle, right) = row? else {
                    continue;
                };
                let application = applications.entry(identity).or_default();
                application.keyboard_count = nonnegative_u64(keyboard);
                application.left_click_count = nonnegative_u64(left);
                application.middle_click_count = nonnegative_u64(middle);
                application.right_click_count = nonnegative_u64(right);
            }
        }

        let mut timeline_applications = Vec::with_capacity(applications.len());
        for (identity, mut working) in self.merge_working_applications(applications)? {
            working
                .segments
                .sort_unstable_by_key(|segment| (segment.started_utc_ms, segment.ended_utc_ms));
            let opened_ms = union_duration_ms(&working.open_intervals);
            let displayed_ms = union_duration_ms(&working.displayed_intervals);
            let focused_ms = union_duration_ms(&working.focused_intervals);
            let mut windows = working.windows.into_iter().collect::<Vec<_>>();
            windows
                .sort_unstable_by_key(|(instance_id, window)| (window.opened_utc_ms, *instance_id));
            let windows = windows
                .into_iter()
                .enumerate()
                .map(|(index, (_, window))| {
                    let opened_ms = duration_ms(window.opened_utc_ms, window.ended_utc_ms);
                    let displayed_ms = union_duration_ms(&window.displayed_intervals);
                    let focused_ms = union_duration_ms(&window.focused_intervals);
                    TimelineWindow {
                        number: index + 1,
                        opened_utc_ms: window.opened_utc_ms,
                        ended_utc_ms: window.ended_utc_ms,
                        opened_ms,
                        displayed_ms,
                        focused_ms,
                        background_ms: opened_ms.saturating_sub(displayed_ms),
                    }
                })
                .collect::<Vec<_>>();
            timeline_applications.push(TimelineApplication {
                display_name: self.application_display_name(&identity)?,
                identity,
                opened_ms,
                displayed_ms,
                focused_ms,
                background_ms: opened_ms.saturating_sub(displayed_ms),
                window_count: windows.len(),
                keyboard_count: working.keyboard_count,
                left_click_count: working.left_click_count,
                middle_click_count: working.middle_click_count,
                right_click_count: working.right_click_count,
                windows,
                segments: working.segments,
            });
        }
        timeline_applications.sort_by(|left, right| {
            right
                .focused_ms
                .cmp(&left.focused_ms)
                .then_with(|| right.opened_ms.cmp(&left.opened_ms))
                .then_with(|| left.display_name.cmp(&right.display_name))
        });

        let input_totals = self.connection.query_row(
            "SELECT COALESCE(SUM(keyboard_count), 0),
                    COALESCE(SUM(left_click_count), 0),
                    COALESCE(SUM(middle_click_count), 0),
                    COALESCE(SUM(right_click_count), 0)
             FROM input_minute_buckets
             WHERE minute_started_utc_ms < ?2
               AND minute_started_utc_ms + 60000 > ?1",
            params![range_started_utc_ms, range_ended_utc_ms],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )?;
        let mut gap_statement = self.connection.prepare(
            "SELECT data_class, started_utc_ms, ended_utc_ms, reason
             FROM data_availability
             WHERE status = 'monitoring_gap' AND started_utc_ms < ?2 AND ended_utc_ms > ?1
             ORDER BY started_utc_ms",
        )?;
        let gaps = gap_statement
            .query_map(params![range_started_utc_ms, range_ended_utc_ms], |row| {
                Ok(TimelineGap {
                    data_class: row.get(0)?,
                    started_utc_ms: row.get(1)?,
                    ended_utc_ms: row.get(2)?,
                    reason: row.get(3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(TimelineSnapshot {
            range_started_utc_ms,
            range_ended_utc_ms,
            applications: timeline_applications,
            keyboard_count: nonnegative_u64(input_totals.0),
            left_click_count: nonnegative_u64(input_totals.1),
            middle_click_count: nonnegative_u64(input_totals.2),
            right_click_count: nonnegative_u64(input_totals.3),
            monitoring_gaps: gaps,
        })
    }

    fn application_display_name(&self, identity: &str) -> Result<String> {
        let metadata = self
            .connection
            .query_row(
                "SELECT executable_path, app_user_model_id, package_identity
                 FROM application_metadata_revisions
                 WHERE application_identity = ?1
                 ORDER BY effective_utc_ms DESC, revision_id DESC LIMIT 1",
                params![identity],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()?;
        if let Some((Some(path), _, _)) = &metadata
            && let Some(name) = Path::new(path).file_stem().and_then(OsStr::to_str)
            && !name.is_empty()
        {
            return Ok(name.to_owned());
        }
        if let Some((_, Some(app_user_model_id), _)) = &metadata {
            return Ok(app_user_model_id.clone());
        }
        if let Some((_, _, Some(package_identity))) = metadata {
            return Ok(package_identity);
        }
        if let Some(path) = identity.strip_prefix("path:")
            && let Some(name) = Path::new(path).file_stem().and_then(OsStr::to_str)
            && !name.is_empty()
        {
            return Ok(name.to_owned());
        }
        Ok(identity
            .split_once(':')
            .map_or(identity, |(_, value)| value)
            .to_owned())
    }

    pub fn retention_policy(&self) -> Result<RetentionPolicy> {
        self.connection
            .query_row(
                "SELECT retention_days, max_detail_bytes
                 FROM retention_policy WHERE singleton_id = 1",
                [],
                |row| {
                    let days = row.get::<_, Option<i64>>(0)?;
                    let max_bytes = row.get::<_, i64>(1)?;
                    Ok(RetentionPolicy {
                        days: days.and_then(|value| u32::try_from(value).ok()),
                        max_bytes: u64::try_from(max_bytes).unwrap_or_default(),
                    })
                },
            )
            .map_err(Into::into)
    }

    pub fn set_retention_policy(&self, policy: RetentionPolicy) -> Result<()> {
        if policy.days.is_some_and(|days| days == 0 || days > 36_500)
            || policy.max_bytes < 1024 * 1024
            || policy.max_bytes > i64::MAX as u64
        {
            return Err(StorageError::Integrity(
                "retention policy is outside supported bounds".to_owned(),
            ));
        }
        self.connection.execute(
            "UPDATE retention_policy SET retention_days = ?1, max_detail_bytes = ?2
             WHERE singleton_id = 1",
            params![policy.days.map(i64::from), policy.max_bytes as i64],
        )?;
        Ok(())
    }

    pub fn apply_retention(&self, now_utc_ms: i64) -> Result<RetentionReport> {
        let policy = self.retention_policy()?;
        let before_bytes = database_files_bytes(&self.database_path);
        let mut outcome = CleanupOutcome::default();
        let activity_cutoff = policy
            .days
            .map(|days| now_utc_ms.saturating_sub(i64::from(days) * 86_400_000));
        let milestone3 = self.apply_milestone3_retention(now_utc_ms, activity_cutoff)?;
        outcome.report.snapshot_items = milestone3.snapshot_items;
        outcome.report.report_items = milestone3.report_items;
        outcome.report.released_bytes = milestone3.released_bytes;
        if let Some(cutoff) = activity_cutoff {
            outcome.merge(self.cleanup_before(cutoff, "retention_time")?);
        }

        if outcome.has_database_cleanup() {
            self.connection
                .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;")?;
        }

        while database_files_bytes(&self.database_path) > policy.max_bytes
            && self.clean_oldest_local_report()?
        {
            outcome.report.report_items = outcome.report.report_items.saturating_add(1);
            self.connection
                .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;")?;
        }

        while database_files_bytes(&self.database_path) > policy.max_bytes {
            let Some(oldest) = oldest_detail_timestamp(&self.connection)? else {
                break;
            };
            let cutoff = oldest.saturating_add(86_400_000).min(now_utc_ms);
            let removed = self.cleanup_before(cutoff, "retention_space")?;
            if removed.report.activity_items == 0 && removed.report.input_items == 0 {
                break;
            }
            outcome.merge(removed);
            self.connection
                .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;")?;
        }

        if outcome.has_any_cleanup() {
            let released = before_bytes.saturating_sub(database_files_bytes(&self.database_path));
            outcome.report.released_bytes = outcome.report.released_bytes.saturating_add(released);
            if let Some(availability_id) = outcome.availability_ids.last() {
                self.connection.execute(
                    "UPDATE data_availability SET released_bytes = ?2
                     WHERE availability_id = ?1",
                    params![*availability_id, to_sql_u64(released, "released bytes")?],
                )?;
            }
        }
        Ok(outcome.report)
    }

    fn cleanup_before(&self, cutoff_utc_ms: i64, reason: &str) -> Result<CleanupOutcome> {
        if !matches!(reason, "retention_time" | "retention_space") {
            return Err(StorageError::Integrity(
                "cleanup reason is outside the fixed allowlist".to_owned(),
            ));
        }
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let activity_started = transaction.query_row(
            "SELECT MIN(value) FROM (
                SELECT ended_utc_ms AS value FROM window_state_intervals
                 WHERE ended_utc_ms IS NOT NULL AND ended_utc_ms < ?1
                UNION ALL
                SELECT closed_utc_ms FROM window_instances
                 WHERE closed_utc_ms IS NOT NULL AND closed_utc_ms < ?1
                UNION ALL
                SELECT ended_utc_ms FROM tray_background_intervals
                 WHERE ended_utc_ms IS NOT NULL AND ended_utc_ms < ?1
             )",
            params![cutoff_utc_ms],
            |row| row.get::<_, Option<i64>>(0),
        )?;
        let input_started = transaction.query_row(
            "SELECT MIN(minute_started_utc_ms) FROM input_minute_buckets
             WHERE minute_started_utc_ms < ?1",
            params![cutoff_utc_ms],
            |row| row.get::<_, Option<i64>>(0),
        )?;

        let mut activity_items = 0_u64;
        activity_items += transaction.execute(
            "DELETE FROM window_state_intervals
             WHERE ended_utc_ms IS NOT NULL AND ended_utc_ms < ?1",
            params![cutoff_utc_ms],
        )? as u64;
        activity_items += transaction.execute(
            "DELETE FROM window_instances
             WHERE closed_utc_ms IS NOT NULL AND closed_utc_ms < ?1
               AND NOT EXISTS (
                   SELECT 1 FROM window_state_intervals
                   WHERE window_instance_id = window_instances.instance_id
               )",
            params![cutoff_utc_ms],
        )? as u64;
        activity_items += transaction.execute(
            "DELETE FROM tray_background_intervals
             WHERE ended_utc_ms IS NOT NULL AND ended_utc_ms < ?1",
            params![cutoff_utc_ms],
        )? as u64;

        let mut input_items = transaction.execute(
            "DELETE FROM input_minute_buckets WHERE minute_started_utc_ms < ?1",
            params![cutoff_utc_ms],
        )? as u64;
        let cutoff_date = utc_date(cutoff_utc_ms);
        input_items += transaction.execute(
            "DELETE FROM daily_physical_key_frequency WHERE local_date < ?1",
            params![cutoff_date],
        )? as u64;
        transaction.execute(
            "DELETE FROM collector_batches WHERE persisted_utc_ms < ?1",
            params![cutoff_utc_ms],
        )?;

        let mut availability_ids = Vec::new();
        if activity_items > 0 {
            transaction.execute(
                "INSERT INTO data_availability (
                    data_class, status, started_utc_ms, ended_utc_ms, reason,
                    item_count, released_bytes
                 ) VALUES ('activity', 'cleaned', ?1, ?2, ?3, ?4, 0)",
                params![
                    activity_started.unwrap_or(cutoff_utc_ms),
                    cutoff_utc_ms,
                    reason,
                    to_sql_u64(activity_items, "cleaned activity items")?
                ],
            )?;
            availability_ids.push(transaction.last_insert_rowid());
        }
        if input_items > 0 {
            transaction.execute(
                "INSERT INTO data_availability (
                    data_class, status, started_utc_ms, ended_utc_ms, reason,
                    item_count, released_bytes
                 ) VALUES ('input', 'cleaned', ?1, ?2, ?3, ?4, 0)",
                params![
                    input_started.unwrap_or(cutoff_utc_ms),
                    cutoff_utc_ms,
                    reason,
                    to_sql_u64(input_items, "cleaned input items")?
                ],
            )?;
            availability_ids.push(transaction.last_insert_rowid());
        }
        transaction.commit()?;
        Ok(CleanupOutcome {
            report: RetentionReport {
                activity_items,
                input_items,
                released_bytes: 0,
                ..RetentionReport::default()
            },
            availability_ids,
        })
    }

    pub fn clear_all(&self) -> Result<ClearReport> {
        self.ensure_writable()?;
        let old_key = load_key(&self.key_path)?;
        let new_key = random_key()?;
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Exclusive)?;
        let tables = [
            "system_intervals",
            "ai_image_authorizations",
            "ai_version_sources",
            "ai_compressions",
            "ai_messages",
            "ai_versions",
            "ai_jobs",
            "local_report_snapshot_reasons",
            "local_report_gaps",
            "local_report_applications",
            "local_reports",
            "snapshot_slots",
            "snapshot_blobs",
            "window_state_intervals",
            "window_instances",
            "tray_background_intervals",
            "input_minute_buckets",
            "daily_physical_key_frequency",
            "anonymous_daily_input_ledger",
            "application_metadata_revisions",
            "applications",
            "collector_batches",
            "collector_runs",
            "runtime_health",
            "data_availability",
        ];
        let mut deleted_rows = 0_u64;
        for table in tables {
            deleted_rows += transaction.execute(&format!("DELETE FROM {table}"), [])? as u64;
        }
        transaction.commit()?;
        self.connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;")?;

        rekey(&self.connection, &new_key)?;
        if let Err(error) = replace_key_file(&self.key_path, &new_key) {
            let _ = rekey(&self.connection, &old_key);
            return Err(error);
        }
        self.remove_all_snapshot_files()?;
        check_integrity(&self.connection)?;
        Ok(ClearReport { deleted_rows })
    }

    pub fn clear_all_coordinated(&self, timeout: Duration) -> Result<ClearReport> {
        self.ensure_writable()?;
        let request_path = self.control_directory.join(COLLECTOR_RESET_REQUEST_FILE);
        let paused_path = self.control_directory.join(COLLECTOR_RESET_PAUSED_FILE);
        let mut request = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&request_path)?;
        request.write_all(b"clear-all")?;
        request.sync_all()?;
        drop(request);

        let started = Instant::now();
        while !paused_path.exists() && started.elapsed() < timeout {
            thread::sleep(Duration::from_millis(50));
        }
        if !paused_path.exists() {
            remove_file_if_present(&request_path)?;
            let _absent_collector = timelens_ipc::SingleInstanceGuard::acquire_collector()
                .map_err(|_| StorageError::Integrity(
                    "collector did not acknowledge pause; original data and offline buffer were preserved".into()))?;
            remove_file_if_present(&self.control_directory.join(COLLECTOR_SPOOL_FILE))?;
            remove_file_if_present(&self.control_directory.join(COLLECTOR_SPOOL_KEY_FILE))?;
            remove_file_if_present(&self.control_directory.join(COLLECTOR_TRAY_STATE_FILE))?;
            return self.clear_all();
        }

        let clear_result = self.clear_all();
        remove_file_if_present(&request_path)?;
        let resumed_at = Instant::now();
        while paused_path.exists() && resumed_at.elapsed() < timeout {
            thread::sleep(Duration::from_millis(50));
        }
        if paused_path.exists() {
            return Err(StorageError::Integrity(
                "collector did not resume after clear all".to_owned(),
            ));
        }
        clear_result
    }

    pub fn checkpoint(&self) -> Result<()> {
        self.connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        Ok(())
    }
}

#[derive(Default)]
struct WorkingApplication {
    open_intervals: Vec<(i64, i64)>,
    displayed_intervals: Vec<(i64, i64)>,
    focused_intervals: Vec<(i64, i64)>,
    windows: BTreeMap<i64, WorkingWindow>,
    segments: Vec<TimelineSegment>,
    keyboard_count: u64,
    left_click_count: u64,
    middle_click_count: u64,
    right_click_count: u64,
}

#[derive(Default)]
struct WorkingWindow {
    opened_utc_ms: i64,
    ended_utc_ms: i64,
    displayed_intervals: Vec<(i64, i64)>,
    focused_intervals: Vec<(i64, i64)>,
}

#[derive(Default)]
struct CleanupOutcome {
    report: RetentionReport,
    availability_ids: Vec<i64>,
}

impl CleanupOutcome {
    fn merge(&mut self, other: Self) {
        self.report.activity_items = self
            .report
            .activity_items
            .saturating_add(other.report.activity_items);
        self.report.input_items = self
            .report
            .input_items
            .saturating_add(other.report.input_items);
        self.report.snapshot_items = self
            .report
            .snapshot_items
            .saturating_add(other.report.snapshot_items);
        self.report.report_items = self
            .report
            .report_items
            .saturating_add(other.report.report_items);
        self.report.released_bytes = self
            .report
            .released_bytes
            .saturating_add(other.report.released_bytes);
        self.availability_ids.extend(other.availability_ids);
    }

    fn has_database_cleanup(&self) -> bool {
        self.report.activity_items > 0
            || self.report.input_items > 0
            || self.report.report_items > 0
    }

    fn has_any_cleanup(&self) -> bool {
        self.has_database_cleanup() || self.report.snapshot_items > 0
    }
}

fn clipped_interval(started: i64, ended: i64, range_start: i64, range_end: i64) -> (i64, i64) {
    (started.max(range_start), ended.min(range_end))
}

fn duration_ms(started: i64, ended: i64) -> u64 {
    ended
        .checked_sub(started)
        .and_then(|duration| u64::try_from(duration).ok())
        .unwrap_or_default()
}

fn union_duration_ms(intervals: &[(i64, i64)]) -> u64 {
    let mut intervals = intervals.to_vec();
    intervals.sort_unstable();
    let mut total = 0_u64;
    let mut current: Option<(i64, i64)> = None;
    for (started, ended) in intervals {
        if ended <= started {
            continue;
        }
        match current {
            Some((current_start, current_end)) if started <= current_end => {
                current = Some((current_start, current_end.max(ended)));
            }
            Some((current_start, current_end)) => {
                total = total.saturating_add(duration_ms(current_start, current_end));
                current = Some((started, ended));
            }
            None => current = Some((started, ended)),
        }
    }
    if let Some((started, ended)) = current {
        total = total.saturating_add(duration_ms(started, ended));
    }
    total
}

fn nonnegative_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or_default()
}

fn to_sql_u64(value: u64, label: &str) -> Result<i64> {
    i64::try_from(value)
        .map_err(|_| StorageError::Integrity(format!("{label} exceeds SQLite range")))
}

fn database_files_bytes(database_path: &Path) -> u64 {
    [
        database_path.to_owned(),
        wal_path(database_path),
        shm_path(database_path),
    ]
    .iter()
    .filter_map(|path| path.metadata().ok().map(|metadata| metadata.len()))
    .sum()
}

fn oldest_detail_timestamp(connection: &Connection) -> Result<Option<i64>> {
    connection
        .query_row(
            "SELECT MIN(value) FROM (
                SELECT ended_utc_ms AS value FROM window_state_intervals
                 WHERE ended_utc_ms IS NOT NULL
                UNION ALL
                SELECT closed_utc_ms FROM window_instances
                 WHERE closed_utc_ms IS NOT NULL
                UNION ALL
                SELECT ended_utc_ms FROM tray_background_intervals
                 WHERE ended_utc_ms IS NOT NULL
                UNION ALL
                SELECT minute_started_utc_ms FROM input_minute_buckets
             )",
            [],
            |row| row.get(0),
        )
        .map_err(Into::into)
}

fn utc_date(utc_ms: i64) -> String {
    let (year, month, day) = civil_from_days(utc_ms.div_euclid(86_400_000));
    format!("{year:04}-{month:02}-{day:02}")
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
    year += if month <= 2 { 1 } else { 0 };
    (year as i32, month as u32, day as u32)
}

fn rekey(connection: &Connection, key: &[u8]) -> Result<()> {
    connection.execute_batch(&format!("PRAGMA rekey = \"x'{}'\";", hex::encode(key)))?;
    Ok(())
}

fn replace_key_file(path: &Path, key: &[u8]) -> Result<()> {
    let protected = protect_data(key)?;
    let temporary = path.with_extension(format!("dpapi.new.{}", unsafe { GetCurrentProcessId() }));
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&temporary)?;
    file.write_all(KEY_MAGIC)?;
    file.write_all(&(protected.len() as u32).to_le_bytes())?;
    file.write_all(&protected)?;
    file.sync_all()?;
    drop(file);

    let source = wide(&temporary);
    let destination = wide(path);
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        let status = unsafe { GetLastError() };
        let _ = fs::remove_file(&temporary);
        return Err(StorageError::DataProtection(status as i32));
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct Migration {
    version: i64,
    sql: &'static str,
}

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        sql: "
        CREATE TABLE runtime_health (
            component TEXT PRIMARY KEY
                CHECK (component IN ('core', 'collector')),
            last_healthy_utc_ms INTEGER NOT NULL,
            protocol_version INTEGER NOT NULL,
            peer_process_id INTEGER
        ) STRICT;
    ",
    },
    Migration {
        version: 2,
        sql: "
        CREATE TABLE collector_runs (
            run_id BLOB PRIMARY KEY CHECK (length(run_id) = 16),
            first_observed_utc_ms INTEGER NOT NULL,
            last_observed_utc_ms INTEGER NOT NULL,
            last_monotonic_ms INTEGER NOT NULL CHECK (last_monotonic_ms >= 0),
            last_sequence INTEGER NOT NULL CHECK (last_sequence >= 0)
        ) STRICT;

        CREATE TABLE collector_batches (
            run_id BLOB NOT NULL REFERENCES collector_runs(run_id),
            first_sequence INTEGER NOT NULL CHECK (first_sequence > 0),
            last_sequence INTEGER NOT NULL CHECK (last_sequence >= first_sequence),
            event_count INTEGER NOT NULL CHECK (event_count > 0),
            persisted_utc_ms INTEGER NOT NULL,
            PRIMARY KEY (run_id, first_sequence)
        ) STRICT;

        CREATE TABLE applications (
            identity TEXT PRIMARY KEY,
            identity_source INTEGER NOT NULL CHECK (identity_source BETWEEN 1 AND 4),
            first_observed_utc_ms INTEGER NOT NULL,
            last_observed_utc_ms INTEGER NOT NULL
        ) STRICT;

        CREATE TABLE application_metadata_revisions (
            revision_id INTEGER PRIMARY KEY,
            application_identity TEXT NOT NULL REFERENCES applications(identity),
            effective_utc_ms INTEGER NOT NULL,
            run_id BLOB NOT NULL REFERENCES collector_runs(run_id),
            sequence INTEGER NOT NULL,
            executable_path TEXT,
            app_user_model_id TEXT,
            package_identity TEXT,
            UNIQUE (run_id, sequence, application_identity)
        ) STRICT;

        CREATE TABLE window_instances (
            instance_id INTEGER PRIMARY KEY,
            run_id BLOB NOT NULL REFERENCES collector_runs(run_id),
            sensor_window_id INTEGER NOT NULL CHECK (sensor_window_id > 0),
            process_id INTEGER NOT NULL CHECK (process_id > 0),
            process_started_at_100ns INTEGER NOT NULL CHECK (process_started_at_100ns > 0),
            application_identity TEXT NOT NULL REFERENCES applications(identity),
            opened_utc_ms INTEGER NOT NULL,
            opened_monotonic_ms INTEGER NOT NULL CHECK (opened_monotonic_ms >= 0),
            open_sequence INTEGER NOT NULL,
            closed_utc_ms INTEGER,
            closed_monotonic_ms INTEGER,
            close_sequence INTEGER,
            UNIQUE (run_id, open_sequence)
        ) STRICT;

        CREATE UNIQUE INDEX one_open_instance_per_sensor_window
            ON window_instances(run_id, sensor_window_id)
            WHERE closed_utc_ms IS NULL;

        CREATE TABLE window_state_intervals (
            state_id INTEGER PRIMARY KEY,
            window_instance_id INTEGER NOT NULL REFERENCES window_instances(instance_id),
            started_utc_ms INTEGER NOT NULL,
            started_monotonic_ms INTEGER NOT NULL CHECK (started_monotonic_ms >= 0),
            start_run_id BLOB NOT NULL REFERENCES collector_runs(run_id),
            start_sequence INTEGER NOT NULL,
            ended_utc_ms INTEGER,
            ended_monotonic_ms INTEGER,
            displayed INTEGER NOT NULL CHECK (displayed IN (0, 1)),
            focused INTEGER NOT NULL CHECK (focused IN (0, 1)),
            on_current_virtual_desktop INTEGER
                CHECK (on_current_virtual_desktop IS NULL OR on_current_virtual_desktop IN (0, 1)),
            virtual_desktop_id TEXT,
            UNIQUE (start_run_id, start_sequence)
        ) STRICT;

        CREATE TABLE data_availability (
            availability_id INTEGER PRIMARY KEY,
            data_class TEXT NOT NULL CHECK (data_class = 'activity'),
            status TEXT NOT NULL CHECK (status = 'monitoring_gap'),
            started_utc_ms INTEGER NOT NULL,
            ended_utc_ms INTEGER NOT NULL,
            reason TEXT NOT NULL CHECK (reason = 'collector_restart')
        ) STRICT;
    ",
    },
    Migration {
        version: 3,
        sql: "
        ALTER TABLE collector_batches ADD COLUMN batch_checksum INTEGER NOT NULL
            DEFAULT 0 CHECK (batch_checksum BETWEEN 0 AND 4294967295);
    ",
    },
    Migration {
        version: 4,
        sql: "
        ALTER TABLE data_availability RENAME TO data_availability_v3;

        CREATE TABLE data_availability (
            availability_id INTEGER PRIMARY KEY,
            data_class TEXT NOT NULL CHECK (data_class IN ('activity', 'input')),
            status TEXT NOT NULL CHECK (status IN ('monitoring_gap', 'cleaned')),
            started_utc_ms INTEGER NOT NULL,
            ended_utc_ms INTEGER NOT NULL,
            reason TEXT NOT NULL CHECK (reason IN (
                'collector_restart', 'buffer_overflow', 'input_overflow',
                'retention_time', 'retention_space'
            )),
            item_count INTEGER,
            released_bytes INTEGER
        ) STRICT;

        INSERT INTO data_availability (
            availability_id, data_class, status, started_utc_ms,
            ended_utc_ms, reason
        )
        SELECT availability_id, data_class, status, started_utc_ms,
               ended_utc_ms, reason
        FROM data_availability_v3;

        DROP TABLE data_availability_v3;
    ",
    },
    Migration {
        version: 5,
        sql: "
        CREATE TABLE input_minute_buckets (
            bucket_id INTEGER PRIMARY KEY,
            minute_started_utc_ms INTEGER NOT NULL,
            timezone_offset_minutes INTEGER NOT NULL
                CHECK (timezone_offset_minutes BETWEEN -1440 AND 1440),
            local_date TEXT NOT NULL CHECK (length(local_date) = 10),
            focused_application_identity TEXT REFERENCES applications(identity),
            keyboard_count INTEGER NOT NULL CHECK (keyboard_count >= 0),
            left_click_count INTEGER NOT NULL CHECK (left_click_count >= 0),
            middle_click_count INTEGER NOT NULL CHECK (middle_click_count >= 0),
            right_click_count INTEGER NOT NULL CHECK (right_click_count >= 0)
        ) STRICT;

        CREATE INDEX input_minute_time_app
            ON input_minute_buckets(minute_started_utc_ms, focused_application_identity);

        CREATE TABLE daily_physical_key_frequency (
            local_date TEXT NOT NULL CHECK (length(local_date) = 10),
            timezone_offset_minutes INTEGER NOT NULL
                CHECK (timezone_offset_minutes BETWEEN -1440 AND 1440),
            scan_code INTEGER NOT NULL CHECK (scan_code BETWEEN 0 AND 511),
            keyboard_layout INTEGER NOT NULL CHECK (keyboard_layout >= 0),
            key_count INTEGER NOT NULL CHECK (key_count > 0),
            PRIMARY KEY (
                local_date, timezone_offset_minutes, scan_code, keyboard_layout
            )
        ) STRICT;

        CREATE TABLE anonymous_daily_input_ledger (
            local_date TEXT NOT NULL CHECK (length(local_date) = 10),
            timezone_offset_minutes INTEGER NOT NULL
                CHECK (timezone_offset_minutes BETWEEN -1440 AND 1440),
            keyboard_count INTEGER NOT NULL CHECK (keyboard_count >= 0),
            left_click_count INTEGER NOT NULL CHECK (left_click_count >= 0),
            middle_click_count INTEGER NOT NULL CHECK (middle_click_count >= 0),
            right_click_count INTEGER NOT NULL CHECK (right_click_count >= 0),
            PRIMARY KEY (local_date, timezone_offset_minutes)
        ) STRICT;
    ",
    },
    Migration {
        version: 6,
        sql: "
        CREATE TABLE tray_background_intervals (
            interval_id INTEGER PRIMARY KEY,
            application_identity TEXT NOT NULL REFERENCES applications(identity),
            process_id INTEGER NOT NULL CHECK (process_id > 0),
            process_started_at_100ns INTEGER NOT NULL
                CHECK (process_started_at_100ns > 0),
            started_utc_ms INTEGER NOT NULL,
            started_monotonic_ms INTEGER NOT NULL CHECK (started_monotonic_ms >= 0),
            start_run_id BLOB NOT NULL REFERENCES collector_runs(run_id),
            start_sequence INTEGER NOT NULL,
            ended_utc_ms INTEGER,
            ended_monotonic_ms INTEGER,
            end_run_id BLOB REFERENCES collector_runs(run_id),
            end_sequence INTEGER,
            UNIQUE (start_run_id, start_sequence)
        ) STRICT;

        CREATE UNIQUE INDEX one_open_tray_interval_per_application_process
            ON tray_background_intervals(
                application_identity, process_id, process_started_at_100ns
            ) WHERE ended_utc_ms IS NULL;
    ",
    },
    Migration {
        version: 7,
        sql: "
        CREATE TABLE retention_policy (
            singleton_id INTEGER PRIMARY KEY CHECK (singleton_id = 1),
            retention_days INTEGER CHECK (
                retention_days IS NULL OR retention_days BETWEEN 1 AND 36500
            ),
            max_detail_bytes INTEGER NOT NULL CHECK (max_detail_bytes >= 1048576)
        ) STRICT;

        INSERT INTO retention_policy (
            singleton_id, retention_days, max_detail_bytes
        ) VALUES (1, 30, 104857600);
    ",
    },
    Migration {
        version: 8,
        sql: "
        CREATE UNIQUE INDEX one_open_state_per_window_instance
            ON window_state_intervals(window_instance_id)
            WHERE ended_utc_ms IS NULL;

        CREATE INDEX window_instances_timeline
            ON window_instances(opened_utc_ms, closed_utc_ms, application_identity);

        CREATE INDEX window_states_timeline
            ON window_state_intervals(
                started_utc_ms, ended_utc_ms, window_instance_id
            );

        CREATE INDEX tray_intervals_timeline
            ON tray_background_intervals(
                started_utc_ms, ended_utc_ms, application_identity
            );

        CREATE INDEX application_metadata_latest
            ON application_metadata_revisions(application_identity, revision_id DESC);
    ",
    },
    Migration {
        version: 9,
        sql: "
        CREATE TABLE snapshot_policy (
            singleton_id INTEGER PRIMARY KEY CHECK (singleton_id = 1),
            enabled INTEGER NOT NULL CHECK (enabled IN (0, 1)),
            interval_minutes INTEGER NOT NULL CHECK (
                interval_minutes IN (1, 3, 5, 10, 15, 30, 60)
            ),
            capture_all_displays INTEGER NOT NULL
                CHECK (capture_all_displays IN (0, 1)),
            cycle_started_utc_ms INTEGER NOT NULL,
            retention_days INTEGER CHECK (
                retention_days IS NULL OR retention_days BETWEEN 1 AND 36500
            ),
            max_snapshot_bytes INTEGER NOT NULL
                CHECK (max_snapshot_bytes >= 1048576)
        ) STRICT;

        INSERT INTO snapshot_policy (
            singleton_id, enabled, interval_minutes, capture_all_displays,
            cycle_started_utc_ms, retention_days, max_snapshot_bytes
        ) VALUES (1, 1, 5, 0, 0, 7, 1073741824);

        CREATE TABLE snapshot_exclusions (
            application_identity TEXT PRIMARY KEY,
            added_utc_ms INTEGER NOT NULL
        ) STRICT;

        CREATE TABLE snapshot_blobs (
            blob_id INTEGER PRIMARY KEY,
            file_name TEXT NOT NULL UNIQUE,
            content_sha256 BLOB NOT NULL UNIQUE CHECK (length(content_sha256) = 32),
            plaintext_bytes INTEGER NOT NULL CHECK (plaintext_bytes > 0),
            encrypted_bytes INTEGER NOT NULL CHECK (encrypted_bytes > 0),
            pixel_width INTEGER NOT NULL CHECK (pixel_width > 0),
            pixel_height INTEGER NOT NULL CHECK (pixel_height > 0),
            created_utc_ms INTEGER NOT NULL
        ) STRICT;

        CREATE TABLE snapshot_slots (
            slot_id INTEGER PRIMARY KEY,
            slot_started_utc_ms INTEGER NOT NULL,
            captured_at_utc_ms INTEGER,
            display_key TEXT NOT NULL,
            display_x INTEGER NOT NULL,
            display_y INTEGER NOT NULL,
            display_width INTEGER NOT NULL CHECK (display_width >= 0),
            display_height INTEGER NOT NULL CHECK (display_height >= 0),
            orientation_degrees INTEGER NOT NULL
                CHECK (orientation_degrees IN (0, 90, 180, 270)),
            pixel_width INTEGER NOT NULL CHECK (pixel_width >= 0),
            pixel_height INTEGER NOT NULL CHECK (pixel_height >= 0),
            blob_id INTEGER REFERENCES snapshot_blobs(blob_id),
            trigger TEXT NOT NULL CHECK (trigger IN ('scheduled', 'manual')),
            capture_method TEXT NOT NULL CHECK (
                capture_method IN ('desktop_duplication', 'none')
            ),
            result TEXT NOT NULL CHECK (result IN ('success', 'missing', 'cleaned')),
            missing_reason TEXT CHECK (missing_reason IN (
                'desktop_idle', 'global_pause', 'locked', 'sleep',
                'secure_desktop', 'session_disconnected', 'remote_session',
                'privacy_exclusion', 'capture_failed', 'low_disk',
                'retention_cleaned', 'user_deleted'
            )),
            timeline_started_utc_ms INTEGER NOT NULL,
            timeline_ended_utc_ms INTEGER NOT NULL,
            CHECK (timeline_ended_utc_ms >= timeline_started_utc_ms),
            CHECK (
                (result = 'success' AND blob_id IS NOT NULL
                    AND captured_at_utc_ms IS NOT NULL
                    AND pixel_width > 0 AND pixel_height > 0
                    AND missing_reason IS NULL)
                OR
                (result IN ('missing', 'cleaned') AND blob_id IS NULL
                    AND missing_reason IS NOT NULL)
            ),
            UNIQUE (slot_started_utc_ms, display_key, trigger)
        ) STRICT;

        CREATE INDEX snapshot_slots_range
            ON snapshot_slots(slot_started_utc_ms, result);
        CREATE INDEX snapshot_slots_blob
            ON snapshot_slots(blob_id) WHERE blob_id IS NOT NULL;

        CREATE TABLE local_reports (
            report_id INTEGER PRIMARY KEY,
            range_started_utc_ms INTEGER NOT NULL,
            range_ended_utc_ms INTEGER NOT NULL,
            generated_utc_ms INTEGER NOT NULL,
            rules_version INTEGER NOT NULL CHECK (rules_version > 0),
            covered_ms INTEGER NOT NULL CHECK (covered_ms >= 0),
            gap_count INTEGER NOT NULL CHECK (gap_count >= 0),
            application_count INTEGER NOT NULL CHECK (application_count >= 0),
            keyboard_count INTEGER NOT NULL CHECK (keyboard_count >= 0),
            left_click_count INTEGER NOT NULL CHECK (left_click_count >= 0),
            middle_click_count INTEGER NOT NULL CHECK (middle_click_count >= 0),
            right_click_count INTEGER NOT NULL CHECK (right_click_count >= 0),
            snapshot_success_count INTEGER NOT NULL CHECK (snapshot_success_count >= 0),
            snapshot_missing_count INTEGER NOT NULL CHECK (snapshot_missing_count >= 0),
            CHECK (range_ended_utc_ms > range_started_utc_ms)
        ) STRICT;

        CREATE INDEX local_reports_generated
            ON local_reports(generated_utc_ms);

        CREATE TABLE local_report_applications (
            report_id INTEGER NOT NULL REFERENCES local_reports(report_id) ON DELETE CASCADE,
            ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
            application_identity TEXT NOT NULL,
            display_name TEXT NOT NULL,
            opened_ms INTEGER NOT NULL CHECK (opened_ms >= 0),
            displayed_ms INTEGER NOT NULL CHECK (displayed_ms >= 0),
            focused_ms INTEGER NOT NULL CHECK (focused_ms >= 0),
            background_ms INTEGER NOT NULL CHECK (background_ms >= 0),
            window_count INTEGER NOT NULL CHECK (window_count >= 0),
            keyboard_count INTEGER NOT NULL CHECK (keyboard_count >= 0),
            left_click_count INTEGER NOT NULL CHECK (left_click_count >= 0),
            middle_click_count INTEGER NOT NULL CHECK (middle_click_count >= 0),
            right_click_count INTEGER NOT NULL CHECK (right_click_count >= 0),
            PRIMARY KEY (report_id, ordinal)
        ) STRICT;

        CREATE TABLE local_report_gaps (
            report_id INTEGER NOT NULL REFERENCES local_reports(report_id) ON DELETE CASCADE,
            ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
            data_class TEXT NOT NULL,
            started_utc_ms INTEGER NOT NULL,
            ended_utc_ms INTEGER NOT NULL,
            reason TEXT NOT NULL,
            PRIMARY KEY (report_id, ordinal)
        ) STRICT;

        CREATE TABLE local_report_snapshot_reasons (
            report_id INTEGER NOT NULL REFERENCES local_reports(report_id) ON DELETE CASCADE,
            reason TEXT NOT NULL,
            slot_count INTEGER NOT NULL CHECK (slot_count > 0),
            PRIMARY KEY (report_id, reason)
        ) STRICT;
    ",
    },
    Migration {
        version: 10,
        sql: include_str!("ai-schema.sql"),
    },
    Migration {
        version: 11,
        sql: include_str!("collection-schema.sql"),
    },
    Migration {
        version: 12,
        sql: include_str!("recovery-schema.sql"),
    },
];

fn validate_event_order(events: &[CollectorEvent]) -> Result<()> {
    for event in events {
        if event.observed_at_utc_ms <= 0 || event.monotonic_ms > i64::MAX as u64 {
            return Err(StorageError::InvalidBatch(
                "event timestamp is outside the supported range".to_owned(),
            ));
        }
    }
    if events.windows(2).any(|pair| {
        pair[1].observed_at_utc_ms < pair[0].observed_at_utc_ms
            || pair[1].monotonic_ms < pair[0].monotonic_ms
    }) {
        return Err(StorageError::InvalidBatch(
            "event timestamps are not monotonic within the batch".to_owned(),
        ));
    }
    Ok(())
}

fn close_stale_runs(
    transaction: &Transaction<'_>,
    resumed_at_utc_ms: i64,
    record_restart_gap: bool,
) -> Result<()> {
    let last_reliable = transaction.query_row(
        "SELECT MAX(last_observed_utc_ms) FROM collector_runs",
        [],
        |row| row.get::<_, Option<i64>>(0),
    )?;
    let Some(last_reliable) = last_reliable else {
        return Ok(());
    };

    transaction.execute(
        "UPDATE window_state_intervals AS states SET
            ended_utc_ms = (
                SELECT runs.last_observed_utc_ms
                FROM window_instances AS instances
                JOIN collector_runs AS runs ON runs.run_id = instances.run_id
                WHERE instances.instance_id = states.window_instance_id
            ),
            ended_monotonic_ms = (
                SELECT runs.last_monotonic_ms
                FROM window_instances AS instances
                JOIN collector_runs AS runs ON runs.run_id = instances.run_id
                WHERE instances.instance_id = states.window_instance_id
            )
         WHERE ended_utc_ms IS NULL",
        [],
    )?;
    transaction.execute(
        "UPDATE window_instances SET
            closed_utc_ms = (
                SELECT last_observed_utc_ms FROM collector_runs
                WHERE collector_runs.run_id = window_instances.run_id
            ),
            closed_monotonic_ms = (
                SELECT last_monotonic_ms FROM collector_runs
                WHERE collector_runs.run_id = window_instances.run_id
            )
         WHERE closed_utc_ms IS NULL",
        [],
    )?;
    transaction.execute(
        "UPDATE tray_background_intervals AS tray SET
            ended_utc_ms = (
                SELECT last_observed_utc_ms FROM collector_runs
                WHERE collector_runs.run_id = tray.start_run_id
            ),
            ended_monotonic_ms = (
                SELECT last_monotonic_ms FROM collector_runs
                WHERE collector_runs.run_id = tray.start_run_id
            )
         WHERE ended_utc_ms IS NULL",
        [],
    )?;
    if record_restart_gap && resumed_at_utc_ms > last_reliable {
        transaction.execute(
            "INSERT INTO data_availability (
                data_class, status, started_utc_ms, ended_utc_ms, reason
             ) VALUES ('activity', 'monitoring_gap', ?1, ?2, 'collector_restart')",
            params![last_reliable, resumed_at_utc_ms],
        )?;
    }
    Ok(())
}

fn ingest_collector_event(
    transaction: &Transaction<'_>,
    run_id: &[u8],
    sequence: u64,
    event: &CollectorEvent,
) -> Result<()> {
    let transition = match event.body.as_ref() {
        Some(collector_event::Body::SystemInterval(interval)) => {
            return collection::ingest_system_interval(transaction, event, interval);
        }
        Some(collector_event::Body::WindowTransition(transition)) => transition,
        Some(collector_event::Body::MonitoringGap(gap)) => {
            return ingest_monitoring_gap(transaction, event, gap);
        }
        Some(collector_event::Body::InputMinute(minute)) => {
            return ingest_input_minute(transaction, event, minute);
        }
        Some(collector_event::Body::TrayTransition(transition)) => {
            return ingest_tray_transition(transaction, run_id, sequence, event, transition);
        }
        None => {
            return Err(StorageError::InvalidBatch(
                "collector event body is missing".to_owned(),
            ));
        }
    };
    let kind = WindowTransitionKind::try_from(transition.kind)
        .map_err(|_| StorageError::InvalidBatch("window transition kind is invalid".to_owned()))?;
    if kind == WindowTransitionKind::Unspecified {
        return Err(StorageError::InvalidBatch(
            "window transition kind is unspecified".to_owned(),
        ));
    }
    let window = transition.window.as_ref().ok_or_else(|| {
        StorageError::InvalidBatch("window transition facts are missing".to_owned())
    })?;
    upsert_application(transaction, run_id, sequence, event, window)?;

    match kind {
        WindowTransitionKind::Opened => {
            open_window_instance(transaction, run_id, sequence, event, window)
        }
        WindowTransitionKind::Updated => {
            update_window_instance(transaction, run_id, sequence, event, window)
        }
        WindowTransitionKind::Closed => {
            close_window_instance(transaction, run_id, sequence, event, window)
        }
        WindowTransitionKind::Unspecified => unreachable!("checked above"),
    }
}

fn ingest_tray_transition(
    transaction: &Transaction<'_>,
    run_id: &[u8],
    sequence: u64,
    event: &CollectorEvent,
    transition: &TrayTransition,
) -> Result<()> {
    let kind = TrayTransitionKind::try_from(transition.kind)
        .map_err(|_| StorageError::InvalidBatch("tray transition kind is invalid".to_owned()))?;
    if kind == TrayTransitionKind::Unspecified
        || transition.process_id == 0
        || transition.process_started_at_100ns == 0
    {
        return Err(StorageError::InvalidBatch(
            "tray transition facts are invalid".to_owned(),
        ));
    }
    let application_exists = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM applications WHERE identity = ?1)",
        params![&transition.application_identity],
        |row| row.get::<_, bool>(0),
    )?;
    if !application_exists {
        let identity_source = if transition.application_identity.starts_with("aumid:") {
            IdentitySource::ProcessAppUserModelId
        } else if transition.application_identity.starts_with("package:") {
            IdentitySource::Package
        } else if transition.application_identity.starts_with("path:") {
            IdentitySource::ExecutablePath
        } else {
            return Err(StorageError::InvalidBatch(
                "tray transition refers to an unknown application identity type".to_owned(),
            ));
        };
        transaction.execute(
            "INSERT INTO applications (
                identity, identity_source, first_observed_utc_ms, last_observed_utc_ms
             ) VALUES (?1, ?2, ?3, ?3)",
            params![
                &transition.application_identity,
                identity_source as i32,
                event.observed_at_utc_ms
            ],
        )?;
    }
    let open_interval = transaction
        .query_row(
            "SELECT interval_id FROM tray_background_intervals
             WHERE application_identity = ?1 AND process_id = ?2
               AND process_started_at_100ns = ?3 AND ended_utc_ms IS NULL",
            params![
                &transition.application_identity,
                i64::from(transition.process_id),
                to_i64(transition.process_started_at_100ns, "process start time")?
            ],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    match (kind, open_interval) {
        (TrayTransitionKind::Started, None) => {
            transaction.execute(
                "INSERT INTO tray_background_intervals (
                    application_identity, process_id, process_started_at_100ns,
                    started_utc_ms, started_monotonic_ms, start_run_id, start_sequence
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    &transition.application_identity,
                    i64::from(transition.process_id),
                    to_i64(transition.process_started_at_100ns, "process start time")?,
                    event.observed_at_utc_ms,
                    to_i64(event.monotonic_ms, "monotonic timestamp")?,
                    run_id,
                    to_i64(sequence, "sequence")?
                ],
            )?;
        }
        (TrayTransitionKind::Ended, Some(interval_id)) => {
            transaction.execute(
                "UPDATE tray_background_intervals SET
                    ended_utc_ms = ?2, ended_monotonic_ms = ?3,
                    end_run_id = ?4, end_sequence = ?5
                 WHERE interval_id = ?1",
                params![
                    interval_id,
                    event.observed_at_utc_ms,
                    to_i64(event.monotonic_ms, "monotonic timestamp")?,
                    run_id,
                    to_i64(sequence, "sequence")?
                ],
            )?;
        }
        (TrayTransitionKind::Started, Some(_)) => {
            return Err(StorageError::InvalidBatch(
                "tray background started while already open".to_owned(),
            ));
        }
        (TrayTransitionKind::Ended, None) => {
            return Err(StorageError::InvalidBatch(
                "tray background ended without an open interval".to_owned(),
            ));
        }
        (TrayTransitionKind::Unspecified, _) => unreachable!("checked above"),
    }
    Ok(())
}

fn ingest_monitoring_gap(
    transaction: &Transaction<'_>,
    event: &CollectorEvent,
    gap: &MonitoringGap,
) -> Result<()> {
    if gap.started_at_utc_ms <= 0 || gap.started_at_utc_ms > event.observed_at_utc_ms {
        return Err(StorageError::InvalidBatch(
            "monitoring gap range is invalid".to_owned(),
        ));
    }
    let (reason, data_classes): (&str, &[&str]) = match MonitoringGapReason::try_from(gap.reason)
        .map_err(|_| StorageError::InvalidBatch("monitoring gap reason is invalid".to_owned()))?
    {
        MonitoringGapReason::BufferOverflow => ("buffer_overflow", &["activity", "input"]),
        MonitoringGapReason::InputOverflow => ("input_overflow", &["input"]),
        MonitoringGapReason::Unspecified => {
            return Err(StorageError::InvalidBatch(
                "monitoring gap reason is unspecified".to_owned(),
            ));
        }
    };
    for data_class in data_classes {
        transaction.execute(
            "INSERT INTO data_availability (
                data_class, status, started_utc_ms, ended_utc_ms, reason
             ) VALUES (?1, 'monitoring_gap', ?2, ?3, ?4)",
            params![
                data_class,
                gap.started_at_utc_ms,
                event.observed_at_utc_ms,
                reason
            ],
        )?;
    }
    Ok(())
}

fn ingest_input_minute(
    transaction: &Transaction<'_>,
    event: &CollectorEvent,
    minute: &InputMinute,
) -> Result<()> {
    validate_input_minute(event, minute)?;
    if !minute.anonymous_only {
        let existing_bucket = transaction
            .query_row(
                "SELECT bucket_id FROM input_minute_buckets
             WHERE minute_started_utc_ms = ?1
               AND timezone_offset_minutes = ?2
               AND focused_application_identity IS ?3",
                params![
                    minute.minute_started_at_utc_ms,
                    minute.timezone_offset_minutes,
                    &minute.focused_application_identity
                ],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        if let Some(bucket_id) = existing_bucket {
            transaction.execute(
                "UPDATE input_minute_buckets SET
                keyboard_count = keyboard_count + ?2,
                left_click_count = left_click_count + ?3,
                middle_click_count = middle_click_count + ?4,
                right_click_count = right_click_count + ?5
             WHERE bucket_id = ?1",
                params![
                    bucket_id,
                    i64::from(minute.keyboard_count),
                    i64::from(minute.left_click_count),
                    i64::from(minute.middle_click_count),
                    i64::from(minute.right_click_count)
                ],
            )?;
        } else {
            transaction.execute(
                "INSERT INTO input_minute_buckets (
                minute_started_utc_ms, timezone_offset_minutes, local_date,
                focused_application_identity, keyboard_count, left_click_count,
                middle_click_count, right_click_count
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    minute.minute_started_at_utc_ms,
                    minute.timezone_offset_minutes,
                    &minute.local_date,
                    &minute.focused_application_identity,
                    i64::from(minute.keyboard_count),
                    i64::from(minute.left_click_count),
                    i64::from(minute.middle_click_count),
                    i64::from(minute.right_click_count)
                ],
            )?;
        }

        for key in &minute.key_counts {
            transaction.execute(
                "INSERT INTO daily_physical_key_frequency (
                local_date, timezone_offset_minutes, scan_code,
                keyboard_layout, key_count
             ) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (
                local_date, timezone_offset_minutes, scan_code, keyboard_layout
             ) DO UPDATE SET key_count = key_count + excluded.key_count",
                params![
                    &minute.local_date,
                    minute.timezone_offset_minutes,
                    i64::from(key.scan_code),
                    to_i64(key.keyboard_layout, "keyboard layout")?,
                    i64::from(key.count)
                ],
            )?;
        }
    }
    transaction.execute(
        "INSERT INTO anonymous_daily_input_ledger (
            local_date, timezone_offset_minutes, keyboard_count,
            left_click_count, middle_click_count, right_click_count
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT (local_date, timezone_offset_minutes) DO UPDATE SET
            keyboard_count = keyboard_count + excluded.keyboard_count,
            left_click_count = left_click_count + excluded.left_click_count,
            middle_click_count = middle_click_count + excluded.middle_click_count,
            right_click_count = right_click_count + excluded.right_click_count",
        params![
            &minute.local_date,
            minute.timezone_offset_minutes,
            i64::from(minute.keyboard_count),
            i64::from(minute.left_click_count),
            i64::from(minute.middle_click_count),
            i64::from(minute.right_click_count)
        ],
    )?;
    Ok(())
}

fn validate_input_minute(event: &CollectorEvent, minute: &InputMinute) -> Result<()> {
    if minute.anonymous_only
        && (minute.focused_application_identity.is_some() || !minute.key_counts.is_empty())
    {
        return Err(StorageError::InvalidBatch(
            "anonymous input contains detail".into(),
        ));
    }
    let date = minute.local_date.as_bytes();
    let valid_date = date.len() == 10
        && date[4] == b'-'
        && date[7] == b'-'
        && date
            .iter()
            .enumerate()
            .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit());
    if (!minute.anonymous_only && minute.minute_started_at_utc_ms <= 0)
        || minute.minute_started_at_utc_ms % 60_000 != 0
        || minute.minute_started_at_utc_ms >= event.observed_at_utc_ms
        || !(-1_440..=1_440).contains(&minute.timezone_offset_minutes)
        || !valid_date
    {
        return Err(StorageError::InvalidBatch(
            "input minute boundaries are invalid".to_owned(),
        ));
    }
    let mut keys = std::collections::HashSet::with_capacity(minute.key_counts.len());
    let mut keyboard_total = 0_u64;
    for key in &minute.key_counts {
        if key.scan_code > 0x1ff
            || key.count == 0
            || !keys.insert((key.scan_code, key.keyboard_layout))
        {
            return Err(StorageError::InvalidBatch(
                "input minute key counts are invalid".to_owned(),
            ));
        }
        keyboard_total = keyboard_total
            .checked_add(u64::from(key.count))
            .ok_or_else(|| StorageError::InvalidBatch("input count overflow".to_owned()))?;
    }
    if (!minute.anonymous_only && keyboard_total != u64::from(minute.keyboard_count))
        || minute
            .keyboard_count
            .saturating_add(minute.left_click_count)
            .saturating_add(minute.middle_click_count)
            .saturating_add(minute.right_click_count)
            == 0
    {
        return Err(StorageError::InvalidBatch(
            "input minute totals are inconsistent".to_owned(),
        ));
    }
    Ok(())
}

fn upsert_application(
    transaction: &Transaction<'_>,
    run_id: &[u8],
    sequence: u64,
    event: &CollectorEvent,
    window: &WindowObservation,
) -> Result<()> {
    let identity_source = IdentitySource::try_from(window.identity_source).map_err(|_| {
        StorageError::InvalidBatch("application identity source is invalid".to_owned())
    })?;
    if identity_source == IdentitySource::Unspecified {
        return Err(StorageError::InvalidBatch(
            "application identity source is unspecified".to_owned(),
        ));
    }
    transaction.execute(
        "INSERT INTO applications (
            identity, identity_source, first_observed_utc_ms, last_observed_utc_ms
         ) VALUES (?1, ?2, ?3, ?3)
         ON CONFLICT(identity) DO UPDATE SET
            identity_source = MAX(identity_source, excluded.identity_source),
            last_observed_utc_ms = MAX(last_observed_utc_ms, excluded.last_observed_utc_ms)",
        params![
            &window.application_identity,
            identity_source as i32,
            event.observed_at_utc_ms
        ],
    )?;

    let latest = transaction
        .query_row(
            "SELECT executable_path, app_user_model_id, package_identity
             FROM application_metadata_revisions
             WHERE application_identity = ?1
             ORDER BY revision_id DESC LIMIT 1",
            params![&window.application_identity],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()?;
    let current = (
        window.executable_path.clone(),
        window.app_user_model_id.clone(),
        window.package_identity.clone(),
    );
    if latest.as_ref() != Some(&current) {
        transaction.execute(
            "INSERT INTO application_metadata_revisions (
                application_identity, effective_utc_ms, run_id, sequence,
                executable_path, app_user_model_id, package_identity
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                &window.application_identity,
                event.observed_at_utc_ms,
                run_id,
                to_i64(sequence, "sequence")?,
                &window.executable_path,
                &window.app_user_model_id,
                &window.package_identity
            ],
        )?;
    }
    Ok(())
}

struct OpenWindowInstance {
    instance_id: i64,
    process_id: i64,
    process_started_at_100ns: i64,
    application_identity: String,
}

fn find_open_window_instance(
    transaction: &Transaction<'_>,
    run_id: &[u8],
    sensor_window_id: u64,
) -> Result<Option<OpenWindowInstance>> {
    transaction
        .query_row(
            "SELECT instance_id, process_id, process_started_at_100ns, application_identity
             FROM window_instances
             WHERE run_id = ?1 AND sensor_window_id = ?2 AND closed_utc_ms IS NULL",
            params![run_id, to_i64(sensor_window_id, "window ID")?],
            |row| {
                Ok(OpenWindowInstance {
                    instance_id: row.get(0)?,
                    process_id: row.get(1)?,
                    process_started_at_100ns: row.get(2)?,
                    application_identity: row.get(3)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
}

fn validate_open_window(window: &WindowObservation, open: &OpenWindowInstance) -> Result<()> {
    if open.process_id != i64::from(window.process_id)
        || open.process_started_at_100ns
            != to_i64(window.process_started_at_100ns, "process start time")?
        || open.application_identity != window.application_identity
    {
        return Err(StorageError::InvalidBatch(
            "window transition does not match the open instance".to_owned(),
        ));
    }
    Ok(())
}

fn open_window_instance(
    transaction: &Transaction<'_>,
    run_id: &[u8],
    sequence: u64,
    event: &CollectorEvent,
    window: &WindowObservation,
) -> Result<()> {
    if find_open_window_instance(transaction, run_id, window.window_id)?.is_some() {
        return Err(StorageError::InvalidBatch(
            "window opened while the same sensor handle is already open".to_owned(),
        ));
    }
    transaction.execute(
        "INSERT INTO window_instances (
            run_id, sensor_window_id, process_id, process_started_at_100ns,
            application_identity, opened_utc_ms, opened_monotonic_ms, open_sequence
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            run_id,
            to_i64(window.window_id, "window ID")?,
            i64::from(window.process_id),
            to_i64(window.process_started_at_100ns, "process start time")?,
            &window.application_identity,
            event.observed_at_utc_ms,
            to_i64(event.monotonic_ms, "monotonic timestamp")?,
            to_i64(sequence, "sequence")?
        ],
    )?;
    insert_window_state(
        transaction,
        transaction.last_insert_rowid(),
        run_id,
        sequence,
        event,
        window,
    )
}

fn update_window_instance(
    transaction: &Transaction<'_>,
    run_id: &[u8],
    sequence: u64,
    event: &CollectorEvent,
    window: &WindowObservation,
) -> Result<()> {
    let open =
        find_open_window_instance(transaction, run_id, window.window_id)?.ok_or_else(|| {
            StorageError::InvalidBatch("window update has no open instance".to_owned())
        })?;
    validate_open_window(window, &open)?;
    let previous = transaction.query_row(
        "SELECT displayed, focused, on_current_virtual_desktop, virtual_desktop_id
         FROM window_state_intervals
         WHERE window_instance_id = ?1 AND ended_utc_ms IS NULL",
        params![open.instance_id],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        },
    )?;
    let current = (
        bool_i64(window.displayed),
        bool_i64(window.focused),
        window.on_current_virtual_desktop.map(bool_i64),
        window.virtual_desktop_id.clone(),
    );
    if previous != current {
        close_window_state(transaction, open.instance_id, event)?;
        insert_window_state(
            transaction,
            open.instance_id,
            run_id,
            sequence,
            event,
            window,
        )?;
    }
    Ok(())
}

fn close_window_instance(
    transaction: &Transaction<'_>,
    run_id: &[u8],
    sequence: u64,
    event: &CollectorEvent,
    window: &WindowObservation,
) -> Result<()> {
    let open =
        find_open_window_instance(transaction, run_id, window.window_id)?.ok_or_else(|| {
            StorageError::InvalidBatch("window close has no open instance".to_owned())
        })?;
    validate_open_window(window, &open)?;
    close_window_state(transaction, open.instance_id, event)?;
    let changed = transaction.execute(
        "UPDATE window_instances SET
            closed_utc_ms = ?2,
            closed_monotonic_ms = ?3,
            close_sequence = ?4
         WHERE instance_id = ?1 AND closed_utc_ms IS NULL",
        params![
            open.instance_id,
            event.observed_at_utc_ms,
            to_i64(event.monotonic_ms, "monotonic timestamp")?,
            to_i64(sequence, "sequence")?
        ],
    )?;
    if changed != 1 {
        return Err(StorageError::InvalidBatch(
            "window instance was already closed".to_owned(),
        ));
    }
    Ok(())
}

fn insert_window_state(
    transaction: &Transaction<'_>,
    instance_id: i64,
    run_id: &[u8],
    sequence: u64,
    event: &CollectorEvent,
    window: &WindowObservation,
) -> Result<()> {
    transaction.execute(
        "INSERT INTO window_state_intervals (
            window_instance_id, started_utc_ms, started_monotonic_ms,
            start_run_id, start_sequence, displayed, focused,
            on_current_virtual_desktop, virtual_desktop_id
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            instance_id,
            event.observed_at_utc_ms,
            to_i64(event.monotonic_ms, "monotonic timestamp")?,
            run_id,
            to_i64(sequence, "sequence")?,
            bool_i64(window.displayed),
            bool_i64(window.focused),
            window.on_current_virtual_desktop.map(bool_i64),
            &window.virtual_desktop_id
        ],
    )?;
    Ok(())
}

fn close_window_state(
    transaction: &Transaction<'_>,
    instance_id: i64,
    event: &CollectorEvent,
) -> Result<()> {
    let changed = transaction.execute(
        "UPDATE window_state_intervals SET
            ended_utc_ms = ?2,
            ended_monotonic_ms = ?3
         WHERE window_instance_id = ?1 AND ended_utc_ms IS NULL",
        params![
            instance_id,
            event.observed_at_utc_ms,
            to_i64(event.monotonic_ms, "monotonic timestamp")?
        ],
    )?;
    if changed != 1 {
        return Err(StorageError::InvalidBatch(
            "window has no open state interval".to_owned(),
        ));
    }
    Ok(())
}

fn bool_i64(value: bool) -> i64 {
    i64::from(value)
}

fn to_i64(value: u64, label: &str) -> Result<i64> {
    i64::try_from(value)
        .map_err(|_| StorageError::InvalidBatch(format!("{label} exceeds SQLite range")))
}

fn migrate_database(
    database_path: &Path,
    backup_path: &Path,
    key: &[u8],
    migrations: &[Migration],
) -> Result<Connection> {
    let had_existing_database = database_path
        .metadata()
        .is_ok_and(|metadata| metadata.len() > 0);
    let mut connection = open_encrypted(database_path, key, false)?;
    let current_version = schema_version(&connection)?;
    let target_version = migrations
        .last()
        .map_or(current_version, |migration| migration.version);
    if current_version > target_version.max(LATEST_SCHEMA_VERSION) {
        return Err(StorageError::SchemaTooNew {
            found: current_version,
            supported: target_version.max(LATEST_SCHEMA_VERSION),
        });
    }
    if current_version >= target_version {
        check_integrity(&connection)?;
        return Ok(connection);
    }

    if had_existing_database {
        check_integrity(&connection)?;
        connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        drop(connection);
        copy_and_sync(database_path, backup_path)?;
        connection = open_encrypted(database_path, key, false)?;
    }

    let migration_result = (|| -> Result<()> {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for migration in migrations
            .iter()
            .filter(|migration| migration.version > current_version)
        {
            transaction.execute_batch(migration.sql)?;
            transaction.pragma_update(None, "user_version", migration.version)?;
        }
        transaction.commit()?;
        check_integrity(&connection)?;
        Ok(())
    })();

    if let Err(error) = migration_result {
        drop(connection);
        if had_existing_database {
            restore_backup(database_path, backup_path)?;
        }
        return Err(StorageError::MigrationRolledBack(error.to_string()));
    }

    if had_existing_database {
        remove_file_if_present(backup_path)?;
    }
    Ok(connection)
}

fn recover_interrupted_migration(
    database_path: &Path,
    backup_path: &Path,
    key: &[u8],
) -> Result<()> {
    if !backup_path.exists() {
        return Ok(());
    }
    if !database_path.exists() {
        restore_backup(database_path, backup_path)?;
        return Ok(());
    }

    let backup_version = inspect_database(backup_path, key)?;
    match inspect_database(database_path, key) {
        Ok(current_version) if current_version >= backup_version => {
            remove_file_if_present(backup_path)?;
        }
        _ => restore_backup(database_path, backup_path)?,
    }
    Ok(())
}

fn inspect_database(path: &Path, key: &[u8]) -> Result<i64> {
    let connection = open_encrypted(path, key, true)?;
    check_integrity(&connection)?;
    schema_version(&connection)
}

fn restore_backup(database_path: &Path, backup_path: &Path) -> Result<()> {
    remove_file_if_present(&wal_path(database_path))?;
    remove_file_if_present(&shm_path(database_path))?;
    copy_and_sync(backup_path, database_path)?;
    remove_file_if_present(backup_path)?;
    Ok(())
}

fn open_encrypted(path: &Path, key: &[u8], read_only: bool) -> Result<Connection> {
    let flags = if read_only {
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX
    } else {
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
    };
    let connection = Connection::open_with_flags(path, flags)?;
    let key_hex = hex::encode(key);
    connection.execute_batch(&format!("PRAGMA key = \"x'{key_hex}'\";"))?;
    if cipher_version(&connection)?.is_empty() {
        return Err(StorageError::CipherUnavailable);
    }
    connection.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA temp_store = MEMORY;
         PRAGMA busy_timeout = 5000;",
    )?;
    if !read_only {
        connection.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;",
        )?;
    }
    Ok(connection)
}

fn cipher_version(connection: &Connection) -> Result<String> {
    connection
        .query_row("PRAGMA cipher_version", [], |row| row.get(0))
        .map_err(Into::into)
}

fn schema_version(connection: &Connection) -> Result<i64> {
    connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(Into::into)
}

fn check_integrity(connection: &Connection) -> Result<()> {
    let result: String = connection.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    if result == "ok" {
        if connection
            .prepare("PRAGMA foreign_key_check")?
            .query([])?
            .next()?
            .is_some()
        {
            return Err(StorageError::Integrity("foreign key violation".into()));
        }
        Ok(())
    } else {
        Err(StorageError::Integrity(result))
    }
}

fn load_or_create_key(path: &Path) -> Result<Zeroizing<Vec<u8>>> {
    if path.exists() {
        return load_key(path);
    }

    let key = random_key()?;
    let protected = protect_data(&key)?;
    let temporary = path.with_extension(format!("dpapi.new.{}", unsafe { GetCurrentProcessId() }));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    file.write_all(KEY_MAGIC)?;
    file.write_all(&(protected.len() as u32).to_le_bytes())?;
    file.write_all(&protected)?;
    file.sync_all()?;
    drop(file);
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    Ok(key)
}

fn load_key(path: &Path) -> Result<Zeroizing<Vec<u8>>> {
    let bytes = fs::read(path)?;
    if bytes.len() < KEY_MAGIC.len() + size_of::<u32>() || &bytes[..KEY_MAGIC.len()] != KEY_MAGIC {
        return Err(StorageError::InvalidKeyFile);
    }
    let length_offset = KEY_MAGIC.len();
    let protected_length = u32::from_le_bytes(
        bytes[length_offset..length_offset + 4]
            .try_into()
            .map_err(|_| StorageError::InvalidKeyFile)?,
    ) as usize;
    let protected = &bytes[length_offset + 4..];
    if protected.len() != protected_length {
        return Err(StorageError::InvalidKeyFile);
    }
    let key = unprotect_data(protected)?;
    if key.len() != KEY_BYTES {
        return Err(StorageError::InvalidKeyFile);
    }
    Ok(Zeroizing::new(key))
}

fn random_key() -> Result<Zeroizing<Vec<u8>>> {
    let mut key = Zeroizing::new(vec![0_u8; KEY_BYTES]);
    let status = unsafe {
        BCryptGenRandom(
            null_mut(),
            key.as_mut_ptr(),
            key.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status < 0 {
        return Err(StorageError::DataProtection(status));
    }
    Ok(key)
}

fn protect_data(plain: &[u8]) -> Result<Vec<u8>> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: plain.len() as u32,
        pbData: plain.as_ptr().cast_mut(),
    };
    let entropy = CRYPT_INTEGER_BLOB {
        cbData: DPAPI_ENTROPY.len() as u32,
        pbData: DPAPI_ENTROPY.as_ptr().cast_mut(),
    };
    let description = wide("Timelens local data key");
    let mut output = CRYPT_INTEGER_BLOB::default();
    if unsafe {
        CryptProtectData(
            &input,
            description.as_ptr(),
            &entropy,
            null(),
            null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    } == 0
    {
        return Err(StorageError::DataProtection(
            unsafe { GetLastError() } as i32
        ));
    }
    copy_local_blob(output)
}

fn unprotect_data(protected: &[u8]) -> Result<Vec<u8>> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: protected.len() as u32,
        pbData: protected.as_ptr().cast_mut(),
    };
    let entropy = CRYPT_INTEGER_BLOB {
        cbData: DPAPI_ENTROPY.len() as u32,
        pbData: DPAPI_ENTROPY.as_ptr().cast_mut(),
    };
    let mut description = null_mut();
    let mut output = CRYPT_INTEGER_BLOB::default();
    if unsafe {
        CryptUnprotectData(
            &input,
            &mut description,
            &entropy,
            null(),
            null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    } == 0
    {
        return Err(StorageError::DataProtection(
            unsafe { GetLastError() } as i32
        ));
    }
    if !description.is_null() {
        unsafe {
            LocalFree(description.cast());
        }
    }
    copy_local_blob(output)
}

fn copy_local_blob(blob: CRYPT_INTEGER_BLOB) -> Result<Vec<u8>> {
    if blob.pbData.is_null() || blob.cbData == 0 {
        return Err(StorageError::InvalidKeyFile);
    }
    let bytes = unsafe { std::slice::from_raw_parts(blob.pbData, blob.cbData as usize) }.to_vec();
    unsafe {
        LocalFree(blob.pbData.cast());
    }
    Ok(bytes)
}

fn copy_and_sync(source: &Path, destination: &Path) -> Result<()> {
    let mut source = File::open(source)?;
    let mut destination = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(destination)?;
    io::copy(&mut source, &mut destination)?;
    destination.sync_all()?;
    Ok(())
}

fn remove_file_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn wal_path(database_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}-wal", database_path.display()))
}

fn shm_path(database_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}-shm", database_path.display()))
}

fn unix_time_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
    value
        .as_ref()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use timelens_ipc::{PhysicalKeyCount, WindowTransition, collector_event};

    fn window_event(
        kind: WindowTransitionKind,
        observed_at_utc_ms: i64,
        monotonic_ms: u64,
        displayed: bool,
        focused: bool,
    ) -> CollectorEvent {
        CollectorEvent {
            observed_at_utc_ms,
            monotonic_ms,
            body: Some(collector_event::Body::WindowTransition(WindowTransition {
                kind: kind as i32,
                window: Some(WindowObservation {
                    window_id: 100,
                    process_id: 200,
                    process_started_at_100ns: 300,
                    application_identity: "path:c:\\apps\\private-marker.exe".to_owned(),
                    identity_source: IdentitySource::ExecutablePath as i32,
                    executable_path: Some(r"C:\Apps\private-marker.exe".to_owned()),
                    app_user_model_id: None,
                    package_identity: None,
                    displayed,
                    focused,
                    on_current_virtual_desktop: Some(true),
                    virtual_desktop_id: Some("desktop-one".to_owned()),
                }),
            })),
        }
    }

    fn event_batch(run_byte: u8, first_sequence: u64, events: Vec<CollectorEvent>) -> EventBatch {
        EventBatch {
            collector_run_id: vec![run_byte; 16],
            first_sequence,
            events,
        }
    }

    #[test]
    fn creates_reopens_and_encrypts_the_database() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        assert_eq!(storage.schema_version().unwrap(), LATEST_SCHEMA_VERSION);
        assert!(!storage.cipher_version().is_empty());
        storage.record_component_health("core", 1, None).unwrap();
        storage
            .connection
            .execute(
                "INSERT OR REPLACE INTO runtime_health (
                    component, last_healthy_utc_ms, protocol_version, peer_process_id
                 ) VALUES ('collector', 4242424242, 1, 123)",
                [],
            )
            .unwrap();
        storage.checkpoint().unwrap();
        let database_path = storage.database_path().to_owned();
        drop(storage);

        let bytes = fs::read(&database_path).unwrap();
        assert!(!bytes.starts_with(b"SQLite format 3"));
        assert!(!contains_bytes(&bytes, b"collector"));
        assert!(!contains_bytes(&bytes, b"4242424242"));

        let reopened = Storage::open(directory.path()).unwrap();
        let count: i64 = reopened
            .connection
            .query_row("SELECT COUNT(*) FROM runtime_health", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn upgrades_schema_two_with_batch_content_checksums() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join(DATABASE_FILE);
        let backup_path = directory.path().join(BACKUP_FILE);
        let key = load_or_create_key(&directory.path().join(KEY_FILE)).unwrap();
        let connection =
            migrate_database(&database_path, &backup_path, &key, &MIGRATIONS[..2]).unwrap();
        assert_eq!(schema_version(&connection).unwrap(), 2);
        drop(connection);

        let upgraded = Storage::open(directory.path()).unwrap();
        assert_eq!(upgraded.schema_version().unwrap(), LATEST_SCHEMA_VERSION);
        upgraded
            .connection
            .prepare("SELECT batch_checksum FROM collector_batches")
            .unwrap();
        upgraded
            .connection
            .prepare("SELECT item_count, released_bytes FROM data_availability")
            .unwrap();
        upgraded
            .connection
            .prepare("SELECT keyboard_count FROM input_minute_buckets")
            .unwrap();
        upgraded
            .connection
            .prepare("SELECT result, missing_reason FROM snapshot_slots")
            .unwrap();
        upgraded
            .connection
            .prepare("SELECT covered_ms, rules_version FROM local_reports")
            .unwrap();
        let timeline_indexes: i64 = upgraded
            .connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_schema
                 WHERE type = 'index' AND name IN (
                    'one_open_state_per_window_instance',
                    'window_instances_timeline', 'window_states_timeline',
                    'tray_intervals_timeline', 'application_metadata_latest'
                 )",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(timeline_indexes, 5);
    }

    #[test]
    fn dpapi_round_trip_does_not_embed_the_plain_key() {
        let key = [0x5a_u8; KEY_BYTES];
        let protected = protect_data(&key).unwrap();
        assert!(!contains_bytes(&protected, &key));
        assert_eq!(unprotect_data(&protected).unwrap(), key);
    }

    #[test]
    fn window_batches_are_idempotent_and_close_state_intervals() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        let opened = event_batch(
            1,
            1,
            vec![window_event(
                WindowTransitionKind::Opened,
                1_700_000_000_000,
                10,
                true,
                true,
            )],
        );
        assert_eq!(
            storage.ingest_event_batch(&opened).unwrap(),
            WindowBatchOutcome::Stored
        );
        assert_eq!(
            storage.ingest_event_batch(&opened).unwrap(),
            WindowBatchOutcome::Duplicate
        );
        let mut conflicting = opened.clone();
        let Some(collector_event::Body::WindowTransition(transition)) =
            conflicting.events[0].body.as_mut()
        else {
            unreachable!()
        };
        transition.window.as_mut().unwrap().focused = false;
        let error = storage.ingest_event_batch(&conflicting).unwrap_err();
        assert!(error.to_string().contains("different content"));
        storage
            .ingest_event_batch(&event_batch(
                1,
                2,
                vec![window_event(
                    WindowTransitionKind::Updated,
                    1_700_000_000_100,
                    110,
                    false,
                    false,
                )],
            ))
            .unwrap();
        storage
            .ingest_event_batch(&event_batch(
                1,
                3,
                vec![window_event(
                    WindowTransitionKind::Closed,
                    1_700_000_000_200,
                    210,
                    false,
                    false,
                )],
            ))
            .unwrap();

        let instances: i64 = storage
            .connection
            .query_row("SELECT COUNT(*) FROM window_instances", [], |row| {
                row.get(0)
            })
            .unwrap();
        let states: i64 = storage
            .connection
            .query_row("SELECT COUNT(*) FROM window_state_intervals", [], |row| {
                row.get(0)
            })
            .unwrap();
        let open_states: i64 = storage
            .connection
            .query_row(
                "SELECT COUNT(*) FROM window_state_intervals WHERE ended_utc_ms IS NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let batches: i64 = storage
            .connection
            .query_row("SELECT COUNT(*) FROM collector_batches", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(instances, 1);
        assert_eq!(states, 2);
        assert_eq!(open_states, 0);
        assert_eq!(batches, 3);

        storage.checkpoint().unwrap();
        let database_path = storage.database_path().to_owned();
        drop(storage);
        let bytes = fs::read(database_path).unwrap();
        assert!(!contains_bytes(&bytes, b"private-marker"));
        assert!(!contains_bytes(&bytes, b"desktop-one"));
    }

    #[test]
    fn current_window_state_lookup_uses_the_partial_index() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        let detail: String = storage
            .connection
            .query_row(
                "EXPLAIN QUERY PLAN
                 SELECT displayed, focused, on_current_virtual_desktop, virtual_desktop_id
                 FROM window_state_intervals
                 WHERE window_instance_id = ?1 AND ended_utc_ms IS NULL",
                params![1],
                |row| row.get(3),
            )
            .unwrap();
        assert!(
            detail.contains("one_open_state_per_window_instance"),
            "{detail}"
        );
    }

    #[test]
    fn rejects_sequence_gaps_without_partial_writes() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        storage
            .ingest_event_batch(&event_batch(
                2,
                1,
                vec![window_event(
                    WindowTransitionKind::Opened,
                    1_700_000_001_000,
                    10,
                    true,
                    false,
                )],
            ))
            .unwrap();
        let error = storage
            .ingest_event_batch(&event_batch(
                2,
                3,
                vec![window_event(
                    WindowTransitionKind::Updated,
                    1_700_000_001_100,
                    110,
                    false,
                    false,
                )],
            ))
            .unwrap_err();
        assert!(error.to_string().contains("contiguous"));
        let batches: i64 = storage
            .connection
            .query_row("SELECT COUNT(*) FROM collector_batches", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(batches, 1);
    }

    #[test]
    fn a_new_collector_run_closes_old_facts_and_records_a_gap() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        storage
            .ingest_event_batch(&event_batch(
                3,
                1,
                vec![window_event(
                    WindowTransitionKind::Opened,
                    1_700_000_002_000,
                    10,
                    true,
                    false,
                )],
            ))
            .unwrap();
        storage
            .ingest_event_batch(&event_batch(
                4,
                1,
                vec![window_event(
                    WindowTransitionKind::Opened,
                    1_700_000_003_000,
                    5,
                    true,
                    false,
                )],
            ))
            .unwrap();

        let old_close: i64 = storage
            .connection
            .query_row(
                "SELECT closed_utc_ms FROM window_instances WHERE run_id = ?1",
                params![vec![3_u8; 16]],
                |row| row.get(0),
            )
            .unwrap();
        let gap: (i64, i64) = storage
            .connection
            .query_row(
                "SELECT started_utc_ms, ended_utc_ms FROM data_availability",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(old_close, 1_700_000_002_000);
        assert_eq!(gap, (1_700_000_002_000, 1_700_000_003_000));
    }

    #[test]
    fn an_explicit_buffer_gap_replaces_the_generic_restart_gap() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        storage
            .ingest_event_batch(&event_batch(
                5,
                1,
                vec![window_event(
                    WindowTransitionKind::Opened,
                    1_700_000_004_000,
                    10,
                    true,
                    false,
                )],
            ))
            .unwrap();
        storage
            .ingest_event_batch(&event_batch(
                6,
                1,
                vec![CollectorEvent {
                    observed_at_utc_ms: 1_700_000_006_000,
                    monotonic_ms: 5,
                    body: Some(collector_event::Body::MonitoringGap(MonitoringGap {
                        started_at_utc_ms: 1_700_000_004_500,
                        reason: MonitoringGapReason::BufferOverflow as i32,
                    })),
                }],
            ))
            .unwrap();

        let gaps: i64 = storage
            .connection
            .query_row(
                "SELECT COUNT(*) FROM data_availability
                 WHERE reason = 'buffer_overflow'
                   AND started_utc_ms = 1700000004500
                   AND ended_utc_ms = 1700000006000",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(gaps, 2);
    }

    #[test]
    fn input_minutes_update_buckets_key_frequency_and_anonymous_ledger() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        storage
            .ingest_event_batch(&event_batch(
                7,
                1,
                vec![window_event(
                    WindowTransitionKind::Opened,
                    1_699_999_980_000,
                    10,
                    true,
                    true,
                )],
            ))
            .unwrap();
        let input = CollectorEvent {
            observed_at_utc_ms: 1_700_000_040_000,
            monotonic_ms: 60_010,
            body: Some(collector_event::Body::InputMinute(InputMinute {
                anonymous_only: false,
                minute_started_at_utc_ms: 1_699_999_980_000,
                timezone_offset_minutes: 480,
                local_date: "2023-11-15".to_owned(),
                focused_application_identity: Some("path:c:\\apps\\private-marker.exe".to_owned()),
                keyboard_count: 3,
                left_click_count: 2,
                middle_click_count: 1,
                right_click_count: 0,
                key_counts: vec![
                    PhysicalKeyCount {
                        scan_code: 0x1e,
                        keyboard_layout: 0x0804_0804,
                        count: 2,
                    },
                    PhysicalKeyCount {
                        scan_code: 0x11d,
                        keyboard_layout: 0x0804_0804,
                        count: 1,
                    },
                ],
            })),
        };
        storage
            .ingest_event_batch(&event_batch(7, 2, vec![input]))
            .unwrap();

        let bucket: (i64, i64, i64, i64) = storage
            .connection
            .query_row(
                "SELECT keyboard_count, left_click_count,
                        middle_click_count, right_click_count
                 FROM input_minute_buckets",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(bucket, (3, 2, 1, 0));
        let key_total: i64 = storage
            .connection
            .query_row(
                "SELECT SUM(key_count) FROM daily_physical_key_frequency",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let ledger: (i64, i64, i64, i64) = storage
            .connection
            .query_row(
                "SELECT keyboard_count, left_click_count,
                        middle_click_count, right_click_count
                 FROM anonymous_daily_input_ledger",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(key_total, 3);
        assert_eq!(ledger, bucket);
    }

    #[test]
    fn tray_background_is_an_application_interval_not_a_window_instance() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        storage
            .ingest_event_batch(&event_batch(
                8,
                1,
                vec![window_event(
                    WindowTransitionKind::Opened,
                    1_700_000_100_000,
                    10,
                    true,
                    true,
                )],
            ))
            .unwrap();
        let tray = |kind, utc_ms, monotonic_ms| CollectorEvent {
            observed_at_utc_ms: utc_ms,
            monotonic_ms,
            body: Some(collector_event::Body::TrayTransition(TrayTransition {
                kind: kind as i32,
                application_identity: "path:c:\\apps\\private-marker.exe".to_owned(),
                process_id: 200,
                process_started_at_100ns: 300,
            })),
        };
        storage
            .ingest_event_batch(&event_batch(
                8,
                2,
                vec![
                    window_event(
                        WindowTransitionKind::Closed,
                        1_700_000_100_100,
                        110,
                        false,
                        false,
                    ),
                    tray(TrayTransitionKind::Started, 1_700_000_100_100, 110),
                ],
            ))
            .unwrap();
        storage
            .ingest_event_batch(&event_batch(
                8,
                4,
                vec![tray(TrayTransitionKind::Ended, 1_700_000_100_500, 510)],
            ))
            .unwrap();

        let interval: (i64, i64) = storage
            .connection
            .query_row(
                "SELECT started_utc_ms, ended_utc_ms FROM tray_background_intervals",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let windows: i64 = storage
            .connection
            .query_row("SELECT COUNT(*) FROM window_instances", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(interval, (1_700_000_100_100, 1_700_000_100_500));
        assert_eq!(windows, 1);
    }

    #[test]
    fn timeline_uses_application_interval_unions_and_numbered_windows() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        let base = 1_700_000_200_000;
        let mut second_open =
            window_event(WindowTransitionKind::Opened, base + 100, 110, true, false);
        let Some(collector_event::Body::WindowTransition(transition)) = second_open.body.as_mut()
        else {
            unreachable!()
        };
        transition.window.as_mut().unwrap().window_id = 101;
        let mut second_close =
            window_event(WindowTransitionKind::Closed, base + 300, 310, false, false);
        let Some(collector_event::Body::WindowTransition(transition)) = second_close.body.as_mut()
        else {
            unreachable!()
        };
        transition.window.as_mut().unwrap().window_id = 101;
        let tray = |kind, utc_ms, monotonic_ms| CollectorEvent {
            observed_at_utc_ms: utc_ms,
            monotonic_ms,
            body: Some(collector_event::Body::TrayTransition(TrayTransition {
                kind: kind as i32,
                application_identity: "path:c:\\apps\\private-marker.exe".to_owned(),
                process_id: 200,
                process_started_at_100ns: 300,
            })),
        };
        storage
            .ingest_event_batch(&event_batch(
                9,
                1,
                vec![
                    window_event(WindowTransitionKind::Opened, base, 10, true, true),
                    second_open,
                    window_event(WindowTransitionKind::Updated, base + 200, 210, false, false),
                    second_close,
                    window_event(WindowTransitionKind::Closed, base + 400, 410, false, false),
                    tray(TrayTransitionKind::Started, base + 400, 410),
                    tray(TrayTransitionKind::Ended, base + 600, 610),
                ],
            ))
            .unwrap();

        let timeline = storage.timeline_snapshot(base, base + 700).unwrap();
        assert_eq!(timeline.applications.len(), 1);
        let application = &timeline.applications[0];
        assert_eq!(application.display_name, "private-marker");
        assert_eq!(application.window_count, 2);
        assert_eq!(application.opened_ms, 600);
        assert_eq!(application.displayed_ms, 300);
        assert_eq!(application.focused_ms, 200);
        assert_eq!(application.background_ms, 300);
        assert_eq!(
            application
                .windows
                .iter()
                .map(|window| window.number)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert!(
            application
                .segments
                .iter()
                .any(|segment| segment.inferred_tray)
        );
    }

    #[test]
    fn retention_deletes_detail_but_preserves_the_anonymous_ledger() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        storage
            .set_retention_policy(RetentionPolicy {
                days: Some(1),
                max_bytes: 100 * 1024 * 1024,
            })
            .unwrap();
        let base = 1_699_999_980_000;
        storage
            .ingest_event_batch(&event_batch(
                10,
                1,
                vec![
                    window_event(WindowTransitionKind::Opened, base, 10, true, true),
                    window_event(
                        WindowTransitionKind::Closed,
                        base + 1_000,
                        1_010,
                        false,
                        false,
                    ),
                    CollectorEvent {
                        observed_at_utc_ms: base + 60_000,
                        monotonic_ms: 60_010,
                        body: Some(collector_event::Body::InputMinute(InputMinute {
                            anonymous_only: false,
                            minute_started_at_utc_ms: base,
                            timezone_offset_minutes: 480,
                            local_date: "2023-11-15".to_owned(),
                            focused_application_identity: Some(
                                "path:c:\\apps\\private-marker.exe".to_owned(),
                            ),
                            keyboard_count: 1,
                            left_click_count: 0,
                            middle_click_count: 0,
                            right_click_count: 0,
                            key_counts: vec![PhysicalKeyCount {
                                scan_code: 0x1e,
                                keyboard_layout: 1,
                                count: 1,
                            }],
                        })),
                    },
                ],
            ))
            .unwrap();

        let report = storage.apply_retention(base + 2 * 86_400_000).unwrap();
        assert!(report.activity_items > 0);
        assert!(report.input_items > 0);
        let detailed_input: i64 = storage
            .connection
            .query_row("SELECT COUNT(*) FROM input_minute_buckets", [], |row| {
                row.get(0)
            })
            .unwrap();
        let ledger: i64 = storage
            .connection
            .query_row(
                "SELECT SUM(keyboard_count) FROM anonymous_daily_input_ledger",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let cleanup_records: i64 = storage
            .connection
            .query_row(
                "SELECT COUNT(*) FROM data_availability WHERE status = 'cleaned'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(detailed_input, 0);
        assert_eq!(ledger, 1);
        assert_eq!(cleanup_records, 2);
    }

    #[test]
    fn clear_all_rotates_the_key_and_preserves_retention_settings() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        let policy = RetentionPolicy {
            days: Some(7),
            max_bytes: 50 * 1024 * 1024,
        };
        storage.set_retention_policy(policy).unwrap();
        storage.record_component_health("core", 3, None).unwrap();
        let key_path = directory.path().join(KEY_FILE);
        let old_protected_key = fs::read(&key_path).unwrap();

        let report = storage.clear_all().unwrap();
        assert!(report.deleted_rows > 0);
        assert_eq!(storage.retention_policy().unwrap(), policy);
        assert_ne!(fs::read(&key_path).unwrap(), old_protected_key);
        drop(storage);

        let reopened = Storage::open(directory.path()).unwrap();
        assert_eq!(reopened.retention_policy().unwrap(), policy);
        let health_rows: i64 = reopened
            .connection
            .query_row("SELECT COUNT(*) FROM runtime_health", [], |row| row.get(0))
            .unwrap();
        assert_eq!(health_rows, 0);
    }

    #[test]
    fn coordinated_clear_waits_for_collector_and_resumes_it() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        storage.record_component_health("core", 3, None).unwrap();
        let key_path = directory.path().join(KEY_FILE);
        let old_protected_key = fs::read(&key_path).unwrap();
        let request_path = directory.path().join(COLLECTOR_RESET_REQUEST_FILE);
        let paused_path = directory.path().join(COLLECTOR_RESET_PAUSED_FILE);
        let collector_request_path = request_path.clone();
        let collector_paused_path = paused_path.clone();
        let collector = thread::spawn(move || {
            let started = Instant::now();
            while !collector_request_path.exists() && started.elapsed() < Duration::from_secs(2) {
                thread::sleep(Duration::from_millis(5));
            }
            assert!(collector_request_path.exists());
            fs::write(&collector_paused_path, b"paused").unwrap();
            let paused_at = Instant::now();
            while collector_request_path.exists() && paused_at.elapsed() < Duration::from_secs(2) {
                thread::sleep(Duration::from_millis(5));
            }
            assert!(!collector_request_path.exists());
            fs::remove_file(&collector_paused_path).unwrap();
        });

        let report = storage
            .clear_all_coordinated(Duration::from_secs(2))
            .unwrap();
        collector.join().unwrap();
        assert!(report.deleted_rows > 0);
        assert!(!request_path.exists());
        assert!(!paused_path.exists());
        assert_ne!(fs::read(key_path).unwrap(), old_protected_key);
    }

    #[test]
    fn coordinated_clear_removes_offline_collector_artifacts() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        storage.record_component_health("core", 3, None).unwrap();
        let collector_artifacts = [
            directory.path().join(COLLECTOR_SPOOL_FILE),
            directory.path().join(COLLECTOR_SPOOL_KEY_FILE),
            directory.path().join(COLLECTOR_TRAY_STATE_FILE),
        ];
        for artifact in &collector_artifacts {
            fs::write(artifact, b"offline-artifact").unwrap();
        }

        let report = storage
            .clear_all_coordinated(Duration::from_millis(10))
            .unwrap();
        assert!(report.deleted_rows > 0);
        assert!(
            collector_artifacts
                .iter()
                .all(|artifact| !artifact.exists())
        );
        assert!(!directory.path().join(COLLECTOR_RESET_REQUEST_FILE).exists());
    }

    #[test]
    fn tray_snapshot_can_recreate_an_application_after_clear() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        let base = 1_700_000_500_000;
        storage
            .ingest_event_batch(&event_batch(
                11,
                1,
                vec![CollectorEvent {
                    observed_at_utc_ms: base,
                    monotonic_ms: 10,
                    body: Some(collector_event::Body::TrayTransition(TrayTransition {
                        kind: TrayTransitionKind::Started as i32,
                        application_identity: "path:c:\\apps\\tray-only.exe".to_owned(),
                        process_id: 400,
                        process_started_at_100ns: 500,
                    })),
                }],
            ))
            .unwrap();

        let timeline = storage.timeline_snapshot(base, base + 1_000).unwrap();
        assert_eq!(timeline.applications.len(), 1);
        assert_eq!(timeline.applications[0].display_name, "tray-only");
        assert_eq!(timeline.applications[0].opened_ms, 1_000);
        assert_eq!(timeline.applications[0].background_ms, 1_000);
        assert!(timeline.applications[0].segments[0].inferred_tray);
    }

    #[test]
    fn failed_migration_restores_the_encrypted_backup() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        storage.record_component_health("core", 1, None).unwrap();
        storage.checkpoint().unwrap();
        drop(storage);

        let database_path = directory.path().join(DATABASE_FILE);
        let backup_path = directory.path().join(BACKUP_FILE);
        let key = load_key(&directory.path().join(KEY_FILE)).unwrap();
        let mut migrations = MIGRATIONS.to_vec();
        migrations.push(Migration {
            version: LATEST_SCHEMA_VERSION + 1,
            sql: "CREATE TABLE should_rollback (id INTEGER); INVALID SQL;",
        });
        let error = migrate_database(&database_path, &backup_path, &key, &migrations).unwrap_err();
        assert!(error.to_string().contains("restored"));
        assert!(!backup_path.exists());

        let connection = open_encrypted(&database_path, &key, false).unwrap();
        assert_eq!(schema_version(&connection).unwrap(), LATEST_SCHEMA_VERSION);
        let core_rows: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM runtime_health WHERE component = 'core'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(core_rows, 1);
        let rollback_table: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_schema WHERE name = 'should_rollback'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rollback_table, 0);
    }

    fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }
}
