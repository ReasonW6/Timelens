use std::{
    collections::HashSet,
    ffi::c_void,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    ptr::{null, null_mut},
};

use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use windows_sys::Win32::Security::Cryptography::{
    BCRYPT_AES_ALGORITHM, BCRYPT_ALG_HANDLE, BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO,
    BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO_VERSION, BCRYPT_CHAINING_MODE, BCRYPT_KEY_HANDLE,
    BCRYPT_SHA256_ALGORITHM, BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptCloseAlgorithmProvider,
    BCryptDecrypt, BCryptDestroyKey, BCryptEncrypt, BCryptGenRandom, BCryptGenerateSymmetricKey,
    BCryptHash, BCryptOpenAlgorithmProvider, BCryptSetProperty,
};

use super::{Result, Storage, StorageError, TimelineApplication, load_key, to_sql_u64};

const SNAPSHOT_DIRECTORY: &str = "snapshots";
const SNAPSHOT_MAGIC: &[u8; 8] = b"TLSNAP\0\x01";
const SNAPSHOT_NONCE_BYTES: usize = 12;
const SNAPSHOT_TAG_BYTES: usize = 16;
const LOCAL_REPORT_RULES_VERSION: u32 = 2;
const VALID_INTERVALS: &[u32] = &[1, 3, 5, 10, 15, 30, 60];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SnapshotPolicy {
    pub enabled: bool,
    pub interval_minutes: u32,
    pub capture_all_displays: bool,
    pub cycle_started_utc_ms: i64,
    pub retention_days: Option<u32>,
    pub max_bytes: u64,
}

impl Default for SnapshotPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_minutes: 5,
            capture_all_displays: false,
            cycle_started_utc_ms: 0,
            retention_days: Some(7),
            max_bytes: 1024 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotDisplay {
    pub key: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub orientation_degrees: u16,
}

impl SnapshotDisplay {
    pub fn session() -> Self {
        Self {
            key: "session".to_owned(),
            x: 0,
            y: 0,
            width: 0,
            height: 0,
            orientation_degrees: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotTrigger {
    Scheduled,
    Manual,
}

impl SnapshotTrigger {
    fn as_str(self) -> &'static str {
        match self {
            Self::Scheduled => "scheduled",
            Self::Manual => "manual",
        }
    }

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "scheduled" => Ok(Self::Scheduled),
            "manual" => Ok(Self::Manual),
            _ => Err(StorageError::Integrity(format!(
                "unknown snapshot trigger: {value}"
            ))),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotMissingReason {
    DesktopIdle,
    GlobalPause,
    Locked,
    Sleep,
    SecureDesktop,
    SessionDisconnected,
    RemoteSession,
    PrivacyExclusion,
    CaptureFailed,
    LowDisk,
    RetentionCleaned,
    UserDeleted,
    BackupOmitted,
    RecoveryCorruption,
}

impl SnapshotMissingReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DesktopIdle => "desktop_idle",
            Self::GlobalPause => "global_pause",
            Self::Locked => "locked",
            Self::Sleep => "sleep",
            Self::SecureDesktop => "secure_desktop",
            Self::SessionDisconnected => "session_disconnected",
            Self::RemoteSession => "remote_session",
            Self::PrivacyExclusion => "privacy_exclusion",
            Self::CaptureFailed => "capture_failed",
            Self::LowDisk => "low_disk",
            Self::RetentionCleaned => "retention_cleaned",
            Self::UserDeleted => "user_deleted",
            Self::BackupOmitted => "backup_omitted",
            Self::RecoveryCorruption => "recovery_corruption",
        }
    }

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "desktop_idle" => Ok(Self::DesktopIdle),
            "global_pause" => Ok(Self::GlobalPause),
            "locked" => Ok(Self::Locked),
            "sleep" => Ok(Self::Sleep),
            "secure_desktop" => Ok(Self::SecureDesktop),
            "session_disconnected" => Ok(Self::SessionDisconnected),
            "remote_session" => Ok(Self::RemoteSession),
            "privacy_exclusion" => Ok(Self::PrivacyExclusion),
            "capture_failed" => Ok(Self::CaptureFailed),
            "low_disk" => Ok(Self::LowDisk),
            "retention_cleaned" => Ok(Self::RetentionCleaned),
            "user_deleted" => Ok(Self::UserDeleted),
            "backup_omitted" => Ok(Self::BackupOmitted),
            "recovery_corruption" => Ok(Self::RecoveryCorruption),
            _ => Err(StorageError::Integrity(format!(
                "unknown snapshot missing reason: {value}"
            ))),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotStoreRequest {
    pub slot_started_utc_ms: i64,
    pub captured_at_utc_ms: i64,
    pub display: SnapshotDisplay,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub trigger: SnapshotTrigger,
    pub webp: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotSlot {
    pub id: i64,
    pub slot_started_utc_ms: i64,
    pub captured_at_utc_ms: Option<i64>,
    pub display: SnapshotDisplay,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub trigger: SnapshotTrigger,
    pub success: bool,
    pub missing_reason: Option<SnapshotMissingReason>,
    pub plaintext_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotImage {
    pub slot: SnapshotSlot,
    pub webp: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalReport {
    pub id: i64,
    pub range_started_utc_ms: i64,
    pub range_ended_utc_ms: i64,
    pub generated_utc_ms: i64,
    pub rules_version: u32,
    pub covered_ms: u64,
    pub keyboard_count: u64,
    pub left_click_count: u64,
    pub middle_click_count: u64,
    pub right_click_count: u64,
    pub snapshot_success_count: u64,
    pub snapshot_missing_count: u64,
    pub applications: Vec<LocalReportApplication>,
    pub gaps: Vec<LocalReportGap>,
    pub snapshot_reasons: Vec<LocalReportSnapshotReason>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalReportApplication {
    pub identity: String,
    pub display_name: String,
    pub opened_ms: u64,
    pub displayed_ms: u64,
    pub focused_ms: u64,
    pub background_ms: u64,
    pub window_count: u64,
    pub keyboard_count: u64,
    pub left_click_count: u64,
    pub middle_click_count: u64,
    pub right_click_count: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalReportGap {
    pub data_class: String,
    pub started_utc_ms: i64,
    pub ended_utc_ms: i64,
    pub reason: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalReportSnapshotReason {
    pub reason: String,
    pub slot_count: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct SnapshotRetentionOutcome {
    pub snapshot_items: u64,
    pub report_items: u64,
    pub released_bytes: u64,
}

impl Storage {
    pub fn snapshot_policy(&self) -> Result<SnapshotPolicy> {
        self.connection
            .query_row(
                "SELECT enabled, interval_minutes, capture_all_displays,
                        cycle_started_utc_ms, retention_days, max_snapshot_bytes
                 FROM snapshot_policy WHERE singleton_id = 1",
                [],
                |row| {
                    Ok(SnapshotPolicy {
                        enabled: row.get::<_, i64>(0)? != 0,
                        interval_minutes: row.get::<_, u32>(1)?,
                        capture_all_displays: row.get::<_, i64>(2)? != 0,
                        cycle_started_utc_ms: row.get(3)?,
                        retention_days: row.get(4)?,
                        max_bytes: row_u64(row, 5)?,
                    })
                },
            )
            .map_err(Into::into)
    }

    pub fn set_snapshot_policy(&self, mut policy: SnapshotPolicy, now_utc_ms: i64) -> Result<()> {
        validate_policy(policy)?;
        let current = self.snapshot_policy()?;
        if policy.enabled != current.enabled
            || policy.interval_minutes != current.interval_minutes
            || policy.capture_all_displays != current.capture_all_displays
        {
            policy.cycle_started_utc_ms = now_utc_ms;
        } else {
            policy.cycle_started_utc_ms = current.cycle_started_utc_ms;
        }
        self.connection.execute(
            "UPDATE snapshot_policy SET
                enabled = ?1,
                interval_minutes = ?2,
                capture_all_displays = ?3,
                cycle_started_utc_ms = ?4,
                retention_days = ?5,
                max_snapshot_bytes = ?6
             WHERE singleton_id = 1",
            params![
                i64::from(policy.enabled),
                policy.interval_minutes,
                i64::from(policy.capture_all_displays),
                policy.cycle_started_utc_ms,
                policy.retention_days,
                to_sql_u64(policy.max_bytes, "snapshot retention bytes")?,
            ],
        )?;
        Ok(())
    }

    pub fn snapshot_exclusions(&self) -> Result<Vec<String>> {
        let mut statement = self.connection.prepare(
            "SELECT application_identity FROM snapshot_exclusions
             ORDER BY application_identity",
        )?;
        statement
            .query_map([], |row| row.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn set_snapshot_excluded(
        &self,
        application_identity: &str,
        excluded: bool,
        now_utc_ms: i64,
    ) -> Result<()> {
        if application_identity.is_empty() || application_identity.len() > 4096 {
            return Err(StorageError::Integrity(
                "snapshot exclusion identity is outside supported bounds".to_owned(),
            ));
        }
        if excluded {
            self.connection.execute(
                "INSERT INTO snapshot_exclusions (application_identity, added_utc_ms)
                 VALUES (?1, ?2)
                 ON CONFLICT(application_identity) DO NOTHING",
                params![application_identity, now_utc_ms],
            )?;
        } else {
            self.connection.execute(
                "DELETE FROM snapshot_exclusions WHERE application_identity = ?1",
                params![application_identity],
            )?;
        }
        Ok(())
    }

    pub fn record_snapshot_missing(
        &self,
        slot_started_utc_ms: i64,
        display: &SnapshotDisplay,
        trigger: SnapshotTrigger,
        reason: SnapshotMissingReason,
    ) -> Result<Option<i64>> {
        validate_slot(slot_started_utc_ms, display, trigger)?;
        let inserted = self.connection.execute(
            "INSERT INTO snapshot_slots (
                slot_started_utc_ms, captured_at_utc_ms, display_key,
                display_x, display_y, display_width, display_height,
                orientation_degrees, pixel_width, pixel_height, blob_id,
                trigger, capture_method, result, missing_reason,
                timeline_started_utc_ms, timeline_ended_utc_ms
             ) VALUES (
                ?1, NULL, ?2, ?3, ?4, ?5, ?6, ?7, 0, 0, NULL,
                ?8, 'none', 'missing', ?9, ?1, ?10
             )
             ON CONFLICT(slot_started_utc_ms, display_key, trigger) DO NOTHING",
            params![
                slot_started_utc_ms,
                display.key,
                display.x,
                display.y,
                display.width,
                display.height,
                display.orientation_degrees,
                trigger.as_str(),
                reason.as_str(),
                slot_started_utc_ms.saturating_add(1),
            ],
        )?;
        Ok((inserted == 1).then(|| self.connection.last_insert_rowid()))
    }

    pub fn store_snapshot(&self, request: &SnapshotStoreRequest) -> Result<i64> {
        validate_slot(
            request.slot_started_utc_ms,
            &request.display,
            request.trigger,
        )?;
        if request.captured_at_utc_ms < request.slot_started_utc_ms
            || request.pixel_width == 0
            || request.pixel_height == 0
            || request.webp.len() > u32::MAX as usize
            || !is_webp(&request.webp)
        {
            return Err(StorageError::Integrity(
                "snapshot image request is invalid".to_owned(),
            ));
        }
        if let Some(existing) = self.existing_slot_id(
            request.slot_started_utc_ms,
            &request.display.key,
            request.trigger,
        )? {
            return Ok(existing);
        }

        let content_sha256 =
            snapshot_pixel_hash(&request.webp, request.pixel_width, request.pixel_height)?;
        let existing_blob = self
            .connection
            .query_row(
                "SELECT blob_id, pixel_width, pixel_height
                 FROM snapshot_blobs WHERE content_sha256 = ?1",
                params![content_sha256.as_slice()],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, u32>(1)?,
                        row.get::<_, u32>(2)?,
                    ))
                },
            )
            .optional()?;
        let mut new_file: Option<PathBuf> = None;

        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let blob_id = if let Some((blob_id, width, height)) = existing_blob {
            if width != request.pixel_width || height != request.pixel_height {
                return Err(StorageError::Integrity(
                    "snapshot content hash has inconsistent dimensions".to_owned(),
                ));
            }
            blob_id
        } else {
            let key = load_key(&self.key_path)?;
            let encrypted = encrypt_snapshot(&key, &request.webp, &content_sha256)?;
            let directory = self.snapshot_directory();
            fs::create_dir_all(&directory)?;
            let file_name = random_snapshot_file_name()?;
            let final_path = directory.join(&file_name);
            write_new_file(&final_path, &encrypted)?;
            let inserted = transaction.execute(
                "INSERT INTO snapshot_blobs (
                    file_name, content_sha256, plaintext_bytes, encrypted_bytes,
                    pixel_width, pixel_height, created_utc_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    file_name,
                    content_sha256.as_slice(),
                    to_sql_u64(request.webp.len() as u64, "snapshot plaintext bytes")?,
                    to_sql_u64(encrypted.len() as u64, "snapshot encrypted bytes")?,
                    request.pixel_width,
                    request.pixel_height,
                    request.captured_at_utc_ms,
                ],
            );
            if let Err(error) = inserted {
                let _ = fs::remove_file(&final_path);
                return Err(error.into());
            }
            new_file = Some(final_path);
            transaction.last_insert_rowid()
        };

        let inserted = transaction.execute(
            "INSERT INTO snapshot_slots (
                slot_started_utc_ms, captured_at_utc_ms, display_key,
                display_x, display_y, display_width, display_height,
                orientation_degrees, pixel_width, pixel_height, blob_id,
                trigger, capture_method, result, missing_reason,
                timeline_started_utc_ms, timeline_ended_utc_ms
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                ?12, 'desktop_duplication', 'success', NULL, ?1, ?13
             )",
            params![
                request.slot_started_utc_ms,
                request.captured_at_utc_ms,
                request.display.key,
                request.display.x,
                request.display.y,
                request.display.width,
                request.display.height,
                request.display.orientation_degrees,
                request.pixel_width,
                request.pixel_height,
                blob_id,
                request.trigger.as_str(),
                request.slot_started_utc_ms.saturating_add(1),
            ],
        );
        let slot_id = match inserted {
            Ok(1) => transaction.last_insert_rowid(),
            Ok(_) => {
                if let Some(path) = new_file {
                    let _ = fs::remove_file(path);
                }
                return Err(StorageError::Integrity(
                    "snapshot slot insert did not affect one row".to_owned(),
                ));
            }
            Err(error) => {
                if let Some(path) = new_file {
                    let _ = fs::remove_file(path);
                }
                return Err(error.into());
            }
        };
        if let Err(error) = transaction.commit() {
            if let Some(path) = new_file {
                let _ = fs::remove_file(path);
            }
            return Err(error.into());
        }
        Ok(slot_id)
    }

    pub fn list_snapshot_slots(
        &self,
        range_started_utc_ms: i64,
        range_ended_utc_ms: i64,
        limit: usize,
    ) -> Result<Vec<SnapshotSlot>> {
        if range_ended_utc_ms <= range_started_utc_ms || limit == 0 || limit > 10_000 {
            return Err(StorageError::Integrity(
                "snapshot list range or limit is invalid".to_owned(),
            ));
        }
        let mut statement = self.connection.prepare(
            "SELECT slots.slot_id, slots.slot_started_utc_ms, slots.captured_at_utc_ms,
                    slots.display_key, slots.display_x, slots.display_y,
                    slots.display_width, slots.display_height, slots.orientation_degrees,
                    slots.pixel_width, slots.pixel_height, slots.trigger,
                    slots.result, slots.missing_reason,
                    COALESCE(blobs.plaintext_bytes, 0)
             FROM snapshot_slots AS slots
             LEFT JOIN snapshot_blobs AS blobs ON blobs.blob_id = slots.blob_id
             WHERE slots.slot_started_utc_ms >= ?1
               AND slots.slot_started_utc_ms < ?2
             ORDER BY slots.slot_started_utc_ms DESC, slots.display_key
             LIMIT ?3",
        )?;
        statement
            .query_map(
                params![range_started_utc_ms, range_ended_utc_ms, limit as i64],
                snapshot_slot_from_row,
            )?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn load_snapshot_image(&self, slot_id: i64) -> Result<SnapshotImage> {
        let (slot, file_name, expected_hash, expected_bytes) = self.connection.query_row(
            "SELECT slots.slot_id, slots.slot_started_utc_ms, slots.captured_at_utc_ms,
                    slots.display_key, slots.display_x, slots.display_y,
                    slots.display_width, slots.display_height, slots.orientation_degrees,
                    slots.pixel_width, slots.pixel_height, slots.trigger,
                    slots.result, slots.missing_reason, blobs.plaintext_bytes,
                    blobs.file_name, blobs.content_sha256, blobs.plaintext_bytes
             FROM snapshot_slots AS slots
             JOIN snapshot_blobs AS blobs ON blobs.blob_id = slots.blob_id
             WHERE slots.slot_id = ?1 AND slots.result = 'success'",
            params![slot_id],
            |row| {
                Ok((
                    snapshot_slot_from_row(row)?,
                    row.get::<_, String>(15)?,
                    row.get::<_, Vec<u8>>(16)?,
                    row_u64(row, 17)?,
                ))
            },
        )?;
        if expected_hash.len() != 32 {
            return Err(StorageError::Integrity(
                "snapshot hash has an invalid length".to_owned(),
            ));
        }
        let mut hash = [0_u8; 32];
        hash.copy_from_slice(&expected_hash);
        let encrypted = fs::read(self.snapshot_directory().join(file_name))?;
        let key = load_key(&self.key_path)?;
        let webp = decrypt_snapshot(&key, &encrypted, &hash)?;
        if webp.len() as u64 != expected_bytes
            || snapshot_pixel_hash(&webp, slot.pixel_width, slot.pixel_height)? != hash
        {
            return Err(StorageError::Integrity(
                "snapshot image failed integrity verification".to_owned(),
            ));
        }
        Ok(SnapshotImage { slot, webp })
    }

    pub fn delete_snapshot(&self, slot_id: i64) -> Result<bool> {
        self.clean_snapshot_slot(slot_id, SnapshotMissingReason::UserDeleted)
            .map(|outcome| outcome.is_some())
    }

    pub fn generate_local_report(
        &self,
        range_started_utc_ms: i64,
        range_ended_utc_ms: i64,
        generated_utc_ms: i64,
    ) -> Result<LocalReport> {
        if range_ended_utc_ms <= range_started_utc_ms || generated_utc_ms <= 0 {
            return Err(StorageError::Integrity(
                "local report range is invalid".to_owned(),
            ));
        }
        let timeline = self.timeline_snapshot(range_started_utc_ms, range_ended_utc_ms)?;
        let mut snapshot_statement = self.connection.prepare(
            "SELECT result, COALESCE(missing_reason, 'success'), COUNT(*)
             FROM snapshot_slots
             WHERE slot_started_utc_ms >= ?1 AND slot_started_utc_ms < ?2
             GROUP BY result, COALESCE(missing_reason, 'success')",
        )?;
        let snapshot_counts = snapshot_statement
            .query_map(params![range_started_utc_ms, range_ended_utc_ms], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row_u64(row, 2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let snapshot_success_count = snapshot_counts
            .iter()
            .filter(|(result, _, _)| result == "success")
            .map(|(_, _, count)| *count)
            .sum::<u64>();
        let snapshot_missing_count = snapshot_counts
            .iter()
            .filter(|(result, _, _)| result != "success")
            .map(|(_, _, count)| *count)
            .sum::<u64>();
        let snapshot_reasons = snapshot_counts
            .into_iter()
            .filter(|(result, _, _)| result != "success")
            .map(|(_, reason, slot_count)| LocalReportSnapshotReason { reason, slot_count })
            .collect::<Vec<_>>();

        let (covered_ms, availability) =
            self.data_coverage(range_started_utc_ms, range_ended_utc_ms)?;
        let applications = timeline
            .applications
            .iter()
            .map(local_report_application)
            .collect::<Vec<_>>();
        let gaps = availability
            .into_iter()
            .map(|gap| LocalReportGap {
                data_class: gap.category,
                started_utc_ms: gap.started_utc_ms,
                ended_utc_ms: gap.ended_utc_ms,
                reason: gap.reason,
            })
            .collect::<Vec<_>>();

        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO local_reports (
                range_started_utc_ms, range_ended_utc_ms, generated_utc_ms,
                rules_version, covered_ms, gap_count, application_count,
                keyboard_count, left_click_count, middle_click_count,
                right_click_count, snapshot_success_count, snapshot_missing_count
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                range_started_utc_ms,
                range_ended_utc_ms,
                generated_utc_ms,
                LOCAL_REPORT_RULES_VERSION,
                to_sql_u64(covered_ms, "report covered milliseconds")?,
                gaps.len() as i64,
                applications.len() as i64,
                to_sql_u64(timeline.keyboard_count, "report keyboard count")?,
                to_sql_u64(timeline.left_click_count, "report left clicks")?,
                to_sql_u64(timeline.middle_click_count, "report middle clicks")?,
                to_sql_u64(timeline.right_click_count, "report right clicks")?,
                to_sql_u64(snapshot_success_count, "report snapshot success count")?,
                to_sql_u64(snapshot_missing_count, "report snapshot missing count")?,
            ],
        )?;
        let report_id = transaction.last_insert_rowid();
        for (ordinal, application) in applications.iter().enumerate() {
            transaction.execute(
                "INSERT INTO local_report_applications (
                    report_id, ordinal, application_identity, display_name,
                    opened_ms, displayed_ms, focused_ms, background_ms,
                    window_count, keyboard_count, left_click_count,
                    middle_click_count, right_click_count
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    report_id,
                    ordinal as i64,
                    application.identity,
                    application.display_name,
                    to_sql_u64(
                        application.opened_ms,
                        "report application opened milliseconds"
                    )?,
                    to_sql_u64(
                        application.displayed_ms,
                        "report application displayed milliseconds"
                    )?,
                    to_sql_u64(
                        application.focused_ms,
                        "report application focused milliseconds"
                    )?,
                    to_sql_u64(
                        application.background_ms,
                        "report application background milliseconds"
                    )?,
                    to_sql_u64(application.window_count, "report application window count")?,
                    to_sql_u64(
                        application.keyboard_count,
                        "report application keyboard count"
                    )?,
                    to_sql_u64(
                        application.left_click_count,
                        "report application left clicks"
                    )?,
                    to_sql_u64(
                        application.middle_click_count,
                        "report application middle clicks"
                    )?,
                    to_sql_u64(
                        application.right_click_count,
                        "report application right clicks"
                    )?,
                ],
            )?;
        }
        for (ordinal, gap) in gaps.iter().enumerate() {
            transaction.execute(
                "INSERT INTO local_report_gaps (
                    report_id, ordinal, data_class, started_utc_ms,
                    ended_utc_ms, reason
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    report_id,
                    ordinal as i64,
                    gap.data_class,
                    gap.started_utc_ms,
                    gap.ended_utc_ms,
                    gap.reason,
                ],
            )?;
        }
        for reason in &snapshot_reasons {
            transaction.execute(
                "INSERT INTO local_report_snapshot_reasons (
                    report_id, reason, slot_count
                 ) VALUES (?1, ?2, ?3)",
                params![
                    report_id,
                    reason.reason,
                    to_sql_u64(reason.slot_count, "report snapshot reason count")?,
                ],
            )?;
        }
        transaction.commit()?;
        Ok(LocalReport {
            id: report_id,
            range_started_utc_ms,
            range_ended_utc_ms,
            generated_utc_ms,
            rules_version: LOCAL_REPORT_RULES_VERSION,
            covered_ms,
            keyboard_count: timeline.keyboard_count,
            left_click_count: timeline.left_click_count,
            middle_click_count: timeline.middle_click_count,
            right_click_count: timeline.right_click_count,
            snapshot_success_count,
            snapshot_missing_count,
            applications,
            gaps,
            snapshot_reasons,
        })
    }

    pub fn load_local_report(&self, report_id: i64) -> Result<LocalReport> {
        let header = self.connection.query_row(
            "SELECT range_started_utc_ms, range_ended_utc_ms, generated_utc_ms,
                    rules_version, covered_ms, keyboard_count, left_click_count,
                    middle_click_count, right_click_count, snapshot_success_count,
                    snapshot_missing_count
             FROM local_reports WHERE report_id = ?1",
            params![report_id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, u32>(3)?,
                    row_u64(row, 4)?,
                    row_u64(row, 5)?,
                    row_u64(row, 6)?,
                    row_u64(row, 7)?,
                    row_u64(row, 8)?,
                    row_u64(row, 9)?,
                    row_u64(row, 10)?,
                ))
            },
        )?;
        let mut application_statement = self.connection.prepare(
            "SELECT application_identity, display_name, opened_ms, displayed_ms,
                    focused_ms, background_ms, window_count, keyboard_count,
                    left_click_count, middle_click_count, right_click_count
             FROM local_report_applications WHERE report_id = ?1 ORDER BY ordinal",
        )?;
        let applications = application_statement
            .query_map(params![report_id], |row| {
                Ok(LocalReportApplication {
                    identity: row.get(0)?,
                    display_name: row.get(1)?,
                    opened_ms: row_u64(row, 2)?,
                    displayed_ms: row_u64(row, 3)?,
                    focused_ms: row_u64(row, 4)?,
                    background_ms: row_u64(row, 5)?,
                    window_count: row_u64(row, 6)?,
                    keyboard_count: row_u64(row, 7)?,
                    left_click_count: row_u64(row, 8)?,
                    middle_click_count: row_u64(row, 9)?,
                    right_click_count: row_u64(row, 10)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut gap_statement = self.connection.prepare(
            "SELECT data_class, started_utc_ms, ended_utc_ms, reason
             FROM local_report_gaps WHERE report_id = ?1 ORDER BY ordinal",
        )?;
        let gaps = gap_statement
            .query_map(params![report_id], |row| {
                Ok(LocalReportGap {
                    data_class: row.get(0)?,
                    started_utc_ms: row.get(1)?,
                    ended_utc_ms: row.get(2)?,
                    reason: row.get(3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut reason_statement = self.connection.prepare(
            "SELECT reason, slot_count FROM local_report_snapshot_reasons
             WHERE report_id = ?1 ORDER BY reason",
        )?;
        let snapshot_reasons = reason_statement
            .query_map(params![report_id], |row| {
                Ok(LocalReportSnapshotReason {
                    reason: row.get(0)?,
                    slot_count: row_u64(row, 1)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(LocalReport {
            id: report_id,
            range_started_utc_ms: header.0,
            range_ended_utc_ms: header.1,
            generated_utc_ms: header.2,
            rules_version: header.3,
            covered_ms: header.4,
            keyboard_count: header.5,
            left_click_count: header.6,
            middle_click_count: header.7,
            right_click_count: header.8,
            snapshot_success_count: header.9,
            snapshot_missing_count: header.10,
            applications,
            gaps,
            snapshot_reasons,
        })
    }

    pub fn latest_local_report(
        &self,
        range_started_utc_ms: i64,
        range_ended_utc_ms: i64,
    ) -> Result<Option<LocalReport>> {
        let report_id = self
            .connection
            .query_row(
                "SELECT report_id FROM local_reports
                 WHERE range_started_utc_ms = ?1 AND range_ended_utc_ms = ?2
                 ORDER BY generated_utc_ms DESC, report_id DESC LIMIT 1",
                params![range_started_utc_ms, range_ended_utc_ms],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        report_id.map(|id| self.load_local_report(id)).transpose()
    }

    pub fn delete_local_report(&self, report_id: i64) -> Result<bool> {
        Ok(self.connection.execute(
            "DELETE FROM local_reports WHERE report_id = ?1",
            params![report_id],
        )? == 1)
    }

    pub(crate) fn apply_milestone3_retention(
        &self,
        now_utc_ms: i64,
        activity_cutoff_utc_ms: Option<i64>,
    ) -> Result<SnapshotRetentionOutcome> {
        let mut outcome = SnapshotRetentionOutcome::default();
        if let Some(cutoff) = activity_cutoff_utc_ms {
            outcome.report_items = self.connection.execute(
                "DELETE FROM local_reports WHERE generated_utc_ms < ?1",
                params![cutoff],
            )? as u64;
        }
        let policy = self.snapshot_policy()?;
        if let Some(days) = policy.retention_days {
            let cutoff = now_utc_ms.saturating_sub(i64::from(days) * 86_400_000);
            for slot_id in self.successful_snapshot_ids_before(cutoff)? {
                if let Some(released) =
                    self.clean_snapshot_slot(slot_id, SnapshotMissingReason::RetentionCleaned)?
                {
                    outcome.snapshot_items += 1;
                    outcome.released_bytes = outcome.released_bytes.saturating_add(released);
                }
            }
        }
        while self.snapshot_live_bytes()? > policy.max_bytes {
            let Some(slot_id) = self.oldest_successful_snapshot_id()? else {
                break;
            };
            if let Some(released) =
                self.clean_snapshot_slot(slot_id, SnapshotMissingReason::RetentionCleaned)?
            {
                outcome.snapshot_items += 1;
                outcome.released_bytes = outcome.released_bytes.saturating_add(released);
            }
        }
        Ok(outcome)
    }

    pub(crate) fn remove_all_snapshot_files(&self) -> Result<()> {
        let directory = self.snapshot_directory();
        let Ok(entries) = fs::read_dir(&directory) else {
            return Ok(());
        };
        for entry in entries {
            let path = entry?.path();
            if path.is_file() {
                fs::remove_file(path)?;
            }
        }
        Ok(())
    }

    pub(crate) fn reconcile_snapshot_files(&self) -> Result<()> {
        let directory = self.snapshot_directory();
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let entries = entries.collect::<std::result::Result<Vec<_>, _>>()?;
        for entry in &entries {
            let path = entry.path();
            let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Some(original_name) = file_name.strip_suffix(".deleting") else {
                continue;
            };
            let referenced = self.connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM snapshot_blobs WHERE file_name = ?1)",
                params![original_name],
                |row| row.get::<_, i64>(0),
            )? != 0;
            if referenced {
                let original_path = directory.join(original_name);
                if original_path.exists() {
                    fs::remove_file(path)?;
                } else {
                    fs::rename(path, original_path)?;
                }
            } else {
                fs::remove_file(path)?;
            }
        }

        let referenced = {
            let mut statement = self
                .connection
                .prepare("SELECT file_name FROM snapshot_blobs")?;
            statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<HashSet<_>, _>>()?
        };
        for entry in fs::read_dir(&directory)? {
            let path = entry?.path();
            let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if file_name.ends_with(".tlsnap") && !referenced.contains(file_name) {
                fs::remove_file(path)?;
            }
        }

        let missing = referenced
            .iter()
            .filter(|file_name| !directory.join(file_name).is_file())
            .cloned()
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            let transaction =
                Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
            for file_name in missing {
                let blob_id = transaction.query_row(
                    "SELECT blob_id FROM snapshot_blobs WHERE file_name = ?1",
                    params![file_name],
                    |row| row.get::<_, i64>(0),
                )?;
                transaction.execute(
                    "UPDATE snapshot_slots SET
                        captured_at_utc_ms = NULL,
                        pixel_width = 0,
                        pixel_height = 0,
                        blob_id = NULL,
                        capture_method = 'none',
                        result = 'cleaned',
                        missing_reason = 'capture_failed'
                     WHERE blob_id = ?1",
                    params![blob_id],
                )?;
                transaction.execute(
                    "DELETE FROM snapshot_blobs WHERE blob_id = ?1",
                    params![blob_id],
                )?;
            }
            transaction.commit()?;
        }
        Ok(())
    }

    pub(crate) fn clean_oldest_local_report(&self) -> Result<bool> {
        let report_id = self
            .connection
            .query_row(
                "SELECT report_id FROM local_reports
                 ORDER BY generated_utc_ms, report_id LIMIT 1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        report_id
            .map(|report_id| self.delete_local_report(report_id))
            .transpose()
            .map(|deleted| deleted.unwrap_or(false))
    }

    fn snapshot_directory(&self) -> PathBuf {
        self.data_directory.join(SNAPSHOT_DIRECTORY)
    }

    fn existing_slot_id(
        &self,
        slot_started_utc_ms: i64,
        display_key: &str,
        trigger: SnapshotTrigger,
    ) -> Result<Option<i64>> {
        self.connection
            .query_row(
                "SELECT slot_id FROM snapshot_slots
                 WHERE slot_started_utc_ms = ?1 AND display_key = ?2 AND trigger = ?3",
                params![slot_started_utc_ms, display_key, trigger.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    fn successful_snapshot_ids_before(&self, cutoff_utc_ms: i64) -> Result<Vec<i64>> {
        let mut statement = self.connection.prepare(
            "SELECT slot_id FROM snapshot_slots
             WHERE result = 'success' AND slot_started_utc_ms < ?1
             ORDER BY slot_started_utc_ms, slot_id",
        )?;
        statement
            .query_map(params![cutoff_utc_ms], |row| row.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    fn oldest_successful_snapshot_id(&self) -> Result<Option<i64>> {
        self.connection
            .query_row(
                "SELECT slot_id FROM snapshot_slots WHERE result = 'success'
                 ORDER BY slot_started_utc_ms, slot_id LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    fn snapshot_live_bytes(&self) -> Result<u64> {
        self.connection
            .query_row(
                "SELECT COALESCE(SUM(encrypted_bytes), 0) FROM snapshot_blobs",
                [],
                |row| row_u64(row, 0),
            )
            .map_err(Into::into)
    }

    fn clean_snapshot_slot(
        &self,
        slot_id: i64,
        reason: SnapshotMissingReason,
    ) -> Result<Option<u64>> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let blob = transaction
            .query_row(
                "SELECT blobs.blob_id, blobs.file_name, blobs.encrypted_bytes
                 FROM snapshot_slots AS slots
                 JOIN snapshot_blobs AS blobs ON blobs.blob_id = slots.blob_id
                 WHERE slots.slot_id = ?1 AND slots.result = 'success'",
                params![slot_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row_u64(row, 2)?,
                    ))
                },
            )
            .optional()?;
        let Some((blob_id, file_name, encrypted_bytes)) = blob else {
            return Ok(None);
        };
        transaction.execute(
            "UPDATE snapshot_slots SET
                captured_at_utc_ms = NULL,
                pixel_width = 0,
                pixel_height = 0,
                blob_id = NULL,
                capture_method = 'none',
                result = 'cleaned',
                missing_reason = ?2
             WHERE slot_id = ?1",
            params![slot_id, reason.as_str()],
        )?;
        let remaining: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM snapshot_slots WHERE blob_id = ?1",
            params![blob_id],
            |row| row.get(0),
        )?;
        if remaining == 0 {
            transaction.execute(
                "DELETE FROM snapshot_blobs WHERE blob_id = ?1",
                params![blob_id],
            )?;
        }
        let staged_delete = if remaining == 0 {
            let path = self.snapshot_directory().join(&file_name);
            if path.exists() {
                let staged = self
                    .snapshot_directory()
                    .join(format!("{file_name}.deleting"));
                fs::rename(&path, &staged)?;
                Some((path, staged))
            } else {
                None
            }
        } else {
            None
        };
        if let Err(error) = transaction.commit() {
            if let Some((path, staged)) = &staged_delete {
                let _ = fs::rename(staged, path);
            }
            return Err(error.into());
        }
        if remaining == 0 {
            if let Some((_, staged)) = staged_delete {
                match fs::remove_file(staged) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
            Ok(Some(encrypted_bytes))
        } else {
            Ok(Some(0))
        }
    }
}

fn validate_policy(policy: SnapshotPolicy) -> Result<()> {
    if !VALID_INTERVALS.contains(&policy.interval_minutes)
        || policy
            .retention_days
            .is_some_and(|days| days == 0 || days > 36_500)
        || policy.max_bytes < 1024 * 1024
        || policy.max_bytes > i64::MAX as u64
    {
        return Err(StorageError::Integrity(
            "snapshot policy is outside supported bounds".to_owned(),
        ));
    }
    Ok(())
}

fn validate_slot(
    slot_started_utc_ms: i64,
    display: &SnapshotDisplay,
    _trigger: SnapshotTrigger,
) -> Result<()> {
    if slot_started_utc_ms <= 0
        || display.key.is_empty()
        || display.key.len() > 128
        || !matches!(display.orientation_degrees, 0 | 90 | 180 | 270)
        || display.width > i32::MAX as u32
        || display.height > i32::MAX as u32
    {
        return Err(StorageError::Integrity(
            "snapshot slot is outside supported bounds".to_owned(),
        ));
    }
    Ok(())
}

fn row_u64(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value = row.get::<_, i64>(index)?;
    u64::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

fn snapshot_slot_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SnapshotSlot> {
    let trigger = row.get::<_, String>(11)?;
    let result = row.get::<_, String>(12)?;
    let missing_reason = row.get::<_, Option<String>>(13)?;
    let parse_error = |message: String| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(StorageError::Integrity(message)),
        )
    };
    Ok(SnapshotSlot {
        id: row.get(0)?,
        slot_started_utc_ms: row.get(1)?,
        captured_at_utc_ms: row.get(2)?,
        display: SnapshotDisplay {
            key: row.get(3)?,
            x: row.get(4)?,
            y: row.get(5)?,
            width: row.get(6)?,
            height: row.get(7)?,
            orientation_degrees: row.get(8)?,
        },
        pixel_width: row.get(9)?,
        pixel_height: row.get(10)?,
        trigger: SnapshotTrigger::from_str(&trigger)
            .map_err(|error| parse_error(error.to_string()))?,
        success: result == "success",
        missing_reason: missing_reason
            .map(|reason| {
                SnapshotMissingReason::from_str(&reason)
                    .map_err(|error| parse_error(error.to_string()))
            })
            .transpose()?,
        plaintext_bytes: row_u64(row, 14)?,
    })
}

fn local_report_application(application: &TimelineApplication) -> LocalReportApplication {
    LocalReportApplication {
        identity: application.identity.clone(),
        display_name: application.display_name.clone(),
        opened_ms: application.opened_ms,
        displayed_ms: application.displayed_ms,
        focused_ms: application.focused_ms,
        background_ms: application.background_ms,
        window_count: application.window_count as u64,
        keyboard_count: application.keyboard_count,
        left_click_count: application.left_click_count,
        middle_click_count: application.middle_click_count,
        right_click_count: application.right_click_count,
    }
}

fn is_webp(bytes: &[u8]) -> bool {
    bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP"
}

pub(crate) fn snapshot_pixel_hash(webp: &[u8], width: u32, height: u32) -> Result<[u8; 32]> {
    let pixels = image::load_from_memory_with_format(webp, image::ImageFormat::WebP)
        .map_err(|error| {
            StorageError::Integrity(format!("snapshot WebP decoding failed: {error}"))
        })?
        .to_rgba8();
    if pixels.dimensions() != (width, height) {
        return Err(StorageError::Integrity(
            "snapshot WebP dimensions do not match its metadata".to_owned(),
        ));
    }
    sha256(pixels.as_raw())
}

pub(crate) fn random_snapshot_file_name() -> Result<String> {
    let mut random = [0_u8; 16];
    let status = unsafe {
        BCryptGenRandom(
            null_mut(),
            random.as_mut_ptr(),
            random.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    nt_success(status, "snapshot file-name generation")?;
    Ok(format!("{}.tlsnap", hex::encode(random)))
}

pub(crate) fn write_new_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

pub(crate) fn sha256(bytes: &[u8]) -> Result<[u8; 32]> {
    let mut algorithm: BCRYPT_ALG_HANDLE = null_mut();
    let status =
        unsafe { BCryptOpenAlgorithmProvider(&mut algorithm, BCRYPT_SHA256_ALGORITHM, null(), 0) };
    nt_success(status, "snapshot SHA-256 provider open")?;
    let mut output = [0_u8; 32];
    let status = unsafe {
        BCryptHash(
            algorithm,
            null(),
            0,
            bytes.as_ptr(),
            u32::try_from(bytes.len())
                .map_err(|_| StorageError::Integrity("snapshot is too large to hash".to_owned()))?,
            output.as_mut_ptr(),
            output.len() as u32,
        )
    };
    unsafe {
        BCryptCloseAlgorithmProvider(algorithm, 0);
    }
    nt_success(status, "snapshot SHA-256")?;
    Ok(output)
}

pub(crate) fn encrypt_snapshot(key: &[u8], plain: &[u8], hash: &[u8; 32]) -> Result<Vec<u8>> {
    let mut nonce = [0_u8; SNAPSHOT_NONCE_BYTES];
    let status = unsafe {
        BCryptGenRandom(
            null_mut(),
            nonce.as_mut_ptr(),
            nonce.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    nt_success(status, "snapshot nonce generation")?;
    let mut tag = [0_u8; SNAPSHOT_TAG_BYTES];
    let cipher = aes_gcm(key, plain, hash, &mut nonce, &mut tag, true)?;
    let mut output = Vec::with_capacity(
        SNAPSHOT_MAGIC.len() + SNAPSHOT_NONCE_BYTES + SNAPSHOT_TAG_BYTES + cipher.len(),
    );
    output.extend_from_slice(SNAPSHOT_MAGIC);
    output.extend_from_slice(&nonce);
    output.extend_from_slice(&tag);
    output.extend_from_slice(&cipher);
    Ok(output)
}

pub(crate) fn decrypt_snapshot(key: &[u8], encrypted: &[u8], hash: &[u8; 32]) -> Result<Vec<u8>> {
    let header = SNAPSHOT_MAGIC.len() + SNAPSHOT_NONCE_BYTES + SNAPSHOT_TAG_BYTES;
    if encrypted.len() <= header || &encrypted[..SNAPSHOT_MAGIC.len()] != SNAPSHOT_MAGIC {
        return Err(StorageError::Integrity(
            "snapshot encrypted file header is invalid".to_owned(),
        ));
    }
    let mut nonce = [0_u8; SNAPSHOT_NONCE_BYTES];
    nonce.copy_from_slice(
        &encrypted[SNAPSHOT_MAGIC.len()..SNAPSHOT_MAGIC.len() + SNAPSHOT_NONCE_BYTES],
    );
    let mut tag = [0_u8; SNAPSHOT_TAG_BYTES];
    tag.copy_from_slice(&encrypted[SNAPSHOT_MAGIC.len() + SNAPSHOT_NONCE_BYTES..header]);
    aes_gcm(key, &encrypted[header..], hash, &mut nonce, &mut tag, false)
}

fn aes_gcm(
    key_bytes: &[u8],
    input: &[u8],
    hash: &[u8; 32],
    nonce: &mut [u8; SNAPSHOT_NONCE_BYTES],
    tag: &mut [u8; SNAPSHOT_TAG_BYTES],
    encrypting: bool,
) -> Result<Vec<u8>> {
    if key_bytes.len() != 32 || input.len() > u32::MAX as usize {
        return Err(StorageError::Integrity(
            "snapshot encryption input is invalid".to_owned(),
        ));
    }
    let mut algorithm: BCRYPT_ALG_HANDLE = null_mut();
    let status =
        unsafe { BCryptOpenAlgorithmProvider(&mut algorithm, BCRYPT_AES_ALGORITHM, null(), 0) };
    nt_success(status, "snapshot AES provider open")?;
    let mode = "ChainingModeGCM\0".encode_utf16().collect::<Vec<_>>();
    let status = unsafe {
        BCryptSetProperty(
            algorithm.cast(),
            BCRYPT_CHAINING_MODE,
            mode.as_ptr().cast(),
            (mode.len() * 2) as u32,
            0,
        )
    };
    if let Err(error) = nt_success(status, "snapshot AES-GCM mode") {
        unsafe {
            BCryptCloseAlgorithmProvider(algorithm, 0);
        }
        return Err(error);
    }
    let mut key: BCRYPT_KEY_HANDLE = null_mut();
    let status = unsafe {
        BCryptGenerateSymmetricKey(
            algorithm,
            &mut key,
            null_mut(),
            0,
            key_bytes.as_ptr(),
            key_bytes.len() as u32,
            0,
        )
    };
    if let Err(error) = nt_success(status, "snapshot AES key import") {
        unsafe {
            BCryptCloseAlgorithmProvider(algorithm, 0);
        }
        return Err(error);
    }
    let mut aad = Vec::with_capacity(SNAPSHOT_MAGIC.len() + hash.len());
    aad.extend_from_slice(SNAPSHOT_MAGIC);
    aad.extend_from_slice(hash);
    let mut auth = BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO {
        cbSize: std::mem::size_of::<BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO>() as u32,
        dwInfoVersion: BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO_VERSION,
        pbNonce: nonce.as_mut_ptr(),
        cbNonce: nonce.len() as u32,
        pbAuthData: aad.as_mut_ptr(),
        cbAuthData: aad.len() as u32,
        pbTag: tag.as_mut_ptr(),
        cbTag: tag.len() as u32,
        ..Default::default()
    };
    let mut output = vec![0_u8; input.len()];
    let mut output_bytes = 0_u32;
    let status = if encrypting {
        unsafe {
            BCryptEncrypt(
                key,
                input.as_ptr(),
                input.len() as u32,
                (&mut auth as *mut BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO).cast::<c_void>(),
                null_mut(),
                0,
                output.as_mut_ptr(),
                output.len() as u32,
                &mut output_bytes,
                0,
            )
        }
    } else {
        unsafe {
            BCryptDecrypt(
                key,
                input.as_ptr(),
                input.len() as u32,
                (&mut auth as *mut BCRYPT_AUTHENTICATED_CIPHER_MODE_INFO).cast::<c_void>(),
                null_mut(),
                0,
                output.as_mut_ptr(),
                output.len() as u32,
                &mut output_bytes,
                0,
            )
        }
    };
    unsafe {
        BCryptDestroyKey(key);
        BCryptCloseAlgorithmProvider(algorithm, 0);
    }
    nt_success(
        status,
        if encrypting {
            "snapshot encryption"
        } else {
            "snapshot decryption"
        },
    )?;
    output.truncate(output_bytes as usize);
    Ok(output)
}

fn nt_success(status: i32, operation: &str) -> Result<()> {
    if status < 0 {
        Err(StorageError::Integrity(format!(
            "{operation} failed with NTSTATUS {status:#010x}"
        )))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::ImageEncoder;

    fn test_webp(width: u32, height: u32, marker: u8) -> Vec<u8> {
        let pixels = (0..width * height)
            .flat_map(|index| {
                let value = marker.wrapping_add(index as u8);
                [value, value.wrapping_mul(3), value.wrapping_mul(7), 255]
            })
            .collect::<Vec<_>>();
        let mut webp = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut webp)
            .write_image(&pixels, width, height, image::ExtendedColorType::Rgba8)
            .unwrap();
        webp
    }

    fn noisy_webp(width: u32, height: u32) -> Vec<u8> {
        let mut state = 0x7a31_9d2b_u32;
        let pixels = (0..width * height)
            .flat_map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                [state as u8, (state >> 8) as u8, (state >> 16) as u8, 255]
            })
            .collect::<Vec<_>>();
        let mut webp = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut webp)
            .write_image(&pixels, width, height, image::ExtendedColorType::Rgba8)
            .unwrap();
        webp
    }

    fn display() -> SnapshotDisplay {
        SnapshotDisplay {
            key: r"\\.\DISPLAY1".to_owned(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            orientation_degrees: 0,
        }
    }

    #[test]
    fn snapshot_defaults_match_the_five_minute_active_display_contract() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        let policy = storage.snapshot_policy().unwrap();
        assert_eq!(policy, SnapshotPolicy::default());
        assert!(policy.enabled);
        assert_eq!(policy.interval_minutes, 5);
        assert!(!policy.capture_all_displays);
        assert_eq!(policy.retention_days, Some(7));
        assert_eq!(policy.max_bytes, 1024 * 1024 * 1024);
    }

    #[test]
    fn snapshot_images_are_encrypted_deduplicated_previewed_and_deleted() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        let webp = test_webp(32, 24, 19);
        let first = storage
            .store_snapshot(&SnapshotStoreRequest {
                slot_started_utc_ms: 1_700_000_000_000,
                captured_at_utc_ms: 1_700_000_000_010,
                display: display(),
                pixel_width: 32,
                pixel_height: 24,
                trigger: SnapshotTrigger::Manual,
                webp: webp.clone(),
            })
            .unwrap();
        let second = storage
            .store_snapshot(&SnapshotStoreRequest {
                slot_started_utc_ms: 1_700_000_001_000,
                captured_at_utc_ms: 1_700_000_001_010,
                display: display(),
                pixel_width: 32,
                pixel_height: 24,
                trigger: SnapshotTrigger::Manual,
                webp: webp.clone(),
            })
            .unwrap();
        let blob_count: i64 = storage
            .connection
            .query_row("SELECT COUNT(*) FROM snapshot_blobs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(blob_count, 1);
        let encrypted_path = storage
            .connection
            .query_row("SELECT file_name FROM snapshot_blobs", [], |row| {
                row.get::<_, String>(0)
            })
            .map(|name| storage.snapshot_directory().join(name))
            .unwrap();
        let encrypted = fs::read(&encrypted_path).unwrap();
        assert!(!is_webp(&encrypted));
        assert!(!contains_bytes(&encrypted, &webp));
        assert_eq!(storage.load_snapshot_image(first).unwrap().webp, webp);
        assert!(storage.delete_snapshot(first).unwrap());
        assert!(encrypted_path.exists());
        assert!(storage.delete_snapshot(second).unwrap());
        assert!(!encrypted_path.exists());
    }

    #[test]
    fn snapshot_retention_marks_slots_and_keeps_settings_and_exclusions() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        storage
            .set_snapshot_excluded("path:c:\\private.exe", true, 1_700_000_000_000)
            .unwrap();
        let mut policy = storage.snapshot_policy().unwrap();
        policy.enabled = true;
        policy.retention_days = Some(1);
        storage
            .set_snapshot_policy(policy, 1_700_000_000_000)
            .unwrap();
        storage
            .store_snapshot(&SnapshotStoreRequest {
                slot_started_utc_ms: 1_700_000_000_000,
                captured_at_utc_ms: 1_700_000_000_010,
                display: display(),
                pixel_width: 10,
                pixel_height: 10,
                trigger: SnapshotTrigger::Scheduled,
                webp: test_webp(10, 10, 7),
            })
            .unwrap();
        let outcome = storage
            .apply_milestone3_retention(1_700_172_800_000, None)
            .unwrap();
        assert_eq!(outcome.snapshot_items, 1);
        assert_eq!(
            storage.snapshot_exclusions().unwrap(),
            vec!["path:c:\\private.exe"]
        );
        assert!(storage.snapshot_policy().unwrap().enabled);
        let slots = storage
            .list_snapshot_slots(1_699_999_000_000, 1_700_001_000_000, 10)
            .unwrap();
        assert_eq!(
            slots[0].missing_reason,
            Some(SnapshotMissingReason::RetentionCleaned)
        );
    }

    #[test]
    fn snapshot_byte_cap_cleans_the_oldest_live_blob_automatically() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        let mut policy = storage.snapshot_policy().unwrap();
        policy.retention_days = None;
        policy.max_bytes = 1024 * 1024;
        storage
            .set_snapshot_policy(policy, 1_700_000_000_000)
            .unwrap();
        storage
            .store_snapshot(&SnapshotStoreRequest {
                slot_started_utc_ms: 1_700_000_000_000,
                captured_at_utc_ms: 1_700_000_000_010,
                display: display(),
                pixel_width: 1024,
                pixel_height: 768,
                trigger: SnapshotTrigger::Scheduled,
                webp: noisy_webp(1024, 768),
            })
            .unwrap();
        assert!(storage.snapshot_live_bytes().unwrap() > policy.max_bytes);

        let outcome = storage
            .apply_milestone3_retention(1_700_000_000_020, None)
            .unwrap();
        assert_eq!(outcome.snapshot_items, 1);
        assert!(storage.snapshot_live_bytes().unwrap() <= policy.max_bytes);
        let slots = storage
            .list_snapshot_slots(1_699_999_000_000, 1_700_001_000_000, 10)
            .unwrap();
        assert_eq!(
            slots[0].missing_reason,
            Some(SnapshotMissingReason::RetentionCleaned)
        );
    }

    #[test]
    fn snapshot_file_reconciliation_recovers_staged_deletes_and_marks_missing_blobs() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        let slot_id = storage
            .store_snapshot(&SnapshotStoreRequest {
                slot_started_utc_ms: 1_700_000_000_000,
                captured_at_utc_ms: 1_700_000_000_010,
                display: display(),
                pixel_width: 8,
                pixel_height: 8,
                trigger: SnapshotTrigger::Manual,
                webp: test_webp(8, 8, 31),
            })
            .unwrap();
        let file_name = storage
            .connection
            .query_row("SELECT file_name FROM snapshot_blobs", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap();
        let original = storage.snapshot_directory().join(&file_name);
        let staged = storage
            .snapshot_directory()
            .join(format!("{file_name}.deleting"));
        fs::rename(&original, &staged).unwrap();
        fs::write(
            storage.snapshot_directory().join("unreferenced.tlsnap"),
            b"orphan",
        )
        .unwrap();
        drop(storage);

        let recovered = Storage::open(directory.path()).unwrap();
        assert!(original.exists());
        assert!(!staged.exists());
        assert!(recovered.load_snapshot_image(slot_id).unwrap().slot.success);
        assert!(
            !recovered
                .snapshot_directory()
                .join("unreferenced.tlsnap")
                .exists()
        );
        fs::remove_file(&original).unwrap();
        drop(recovered);

        let repaired = Storage::open(directory.path()).unwrap();
        let slots = repaired
            .list_snapshot_slots(1_699_999_000_000, 1_700_001_000_000, 10)
            .unwrap();
        assert_eq!(slots.len(), 1);
        assert_eq!(
            slots[0].missing_reason,
            Some(SnapshotMissingReason::CaptureFailed)
        );
        assert_eq!(
            repaired
                .connection
                .query_row("SELECT COUNT(*) FROM snapshot_blobs", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            0
        );
    }

    #[test]
    fn local_report_persists_only_aggregated_results() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        storage
            .record_snapshot_missing(
                1_700_000_000_000,
                &SnapshotDisplay::session(),
                SnapshotTrigger::Scheduled,
                SnapshotMissingReason::Locked,
            )
            .unwrap();
        let report = storage
            .generate_local_report(1_699_999_000_000, 1_700_001_000_000, 1_700_001_000_010)
            .unwrap();
        assert_eq!(report.snapshot_missing_count, 1);
        assert_eq!(report.snapshot_reasons[0].reason, "locked");
        assert_eq!(storage.load_local_report(report.id).unwrap(), report);
        assert!(storage.delete_local_report(report.id).unwrap());
    }

    fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }
}
