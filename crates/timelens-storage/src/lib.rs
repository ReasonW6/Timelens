#![cfg(windows)]

use std::{
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    mem::size_of,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    time::{SystemTime, UNIX_EPOCH},
};

use prost::Message;
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use timelens_ipc::{
    CollectorEvent, EventBatch, IdentitySource, WindowObservation, WindowTransitionKind,
    collector_event,
};
use windows_sys::Win32::{
    Foundation::{GetLastError, LocalFree},
    Security::Cryptography::{
        BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom, CRYPT_INTEGER_BLOB,
        CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    },
    System::Threading::GetCurrentProcessId,
};
use zeroize::Zeroizing;

const DATABASE_FILE: &str = "timelens.sqlite3";
const KEY_FILE: &str = "data-key.dpapi";
const BACKUP_FILE: &str = "timelens.sqlite3.migration-backup";
const KEY_MAGIC: &[u8; 8] = b"TLKEY\0\0\x01";
const KEY_BYTES: usize = 32;
const DPAPI_ENTROPY: &[u8] = b"Timelens local data key v1";
const LATEST_SCHEMA_VERSION: i64 = 3;

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
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

pub struct Storage {
    connection: Connection,
    database_path: PathBuf,
    cipher_version: String,
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
        let key = load_or_create_key(&key_path)?;

        recover_interrupted_migration(&database_path, &backup_path, &key)?;
        let connection = migrate_database(&database_path, &backup_path, &key, MIGRATIONS)?;
        let cipher_version = cipher_version(&connection)?;

        Ok(Self {
            connection,
            database_path,
            cipher_version,
        })
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
                close_stale_runs(&transaction, first_event.observed_at_utc_ms)?;
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

    pub fn checkpoint(&self) -> Result<()> {
        self.connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        Ok(())
    }
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

fn close_stale_runs(transaction: &Transaction<'_>, resumed_at_utc_ms: i64) -> Result<()> {
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
    if resumed_at_utc_ms > last_reliable {
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
        Some(collector_event::Body::WindowTransition(transition)) => transition,
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
    use timelens_ipc::{WindowTransition, collector_event};

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
        assert_eq!(upgraded.schema_version().unwrap(), 3);
        upgraded
            .connection
            .prepare("SELECT batch_checksum FROM collector_batches")
            .unwrap();
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
    fn failed_migration_restores_the_encrypted_backup() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        storage.record_component_health("core", 1, None).unwrap();
        storage.checkpoint().unwrap();
        drop(storage);

        let database_path = directory.path().join(DATABASE_FILE);
        let backup_path = directory.path().join(BACKUP_FILE);
        let key = load_key(&directory.path().join(KEY_FILE)).unwrap();
        let migrations = [
            MIGRATIONS[0],
            MIGRATIONS[1],
            MIGRATIONS[2],
            Migration {
                version: 4,
                sql: "CREATE TABLE should_rollback (id INTEGER); INVALID SQL;",
            },
        ];
        let error = migrate_database(&database_path, &backup_path, &key, &migrations).unwrap_err();
        assert!(error.to_string().contains("restored"));
        assert!(!backup_path.exists());

        let connection = open_encrypted(&database_path, &key, false).unwrap();
        assert_eq!(schema_version(&connection).unwrap(), 3);
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
