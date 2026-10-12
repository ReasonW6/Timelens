//! One append-only encrypted image container per local calendar day. SQLite is
//! the commit journal: file bytes are durable before their encrypted index commits.
//! Replacements use fresh file names, so an interrupted operation has one authority.
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::windows::fs::MetadataExt,
    path::{Path, PathBuf},
};

use chrono::{Days, Local, NaiveDate, TimeZone};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use zeroize::Zeroizing;

use super::milestone3::{
    SnapshotRetentionOutcome, decrypt_snapshot, encrypt_snapshot, random_snapshot_file_name,
    row_u64, sha256, snapshot_pixel_hash,
};
use super::{
    Result, SnapshotMissingReason, SnapshotStoreRequest, Storage, StorageError, load_key,
    random_key, to_sql_u64,
};

pub(crate) const MAGIC: &[u8; 8] = b"TLSDAY\0\x01";
const RECORD_OVERHEAD: u64 = 36;

fn invalid(message: &str) -> StorageError {
    StorageError::Integrity(message.to_owned())
}

fn local_day(utc_ms: i64) -> Result<NaiveDate> {
    calendar_day(utc_ms, &Local)
}

fn calendar_day<T: TimeZone>(utc_ms: i64, zone: &T) -> Result<NaiveDate> {
    zone.timestamp_millis_opt(utc_ms)
        .single()
        .map(|d| d.date_naive())
        .ok_or_else(|| invalid("snapshot timestamp has no local calendar date"))
}

fn container_name() -> Result<String> {
    Ok(random_snapshot_file_name()?.replace(".tlsnap", ".tlsday"))
}

pub(crate) fn owned_container_name(name: &str) -> bool {
    name.strip_suffix(".tlsday")
        .is_some_and(|s| s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn image_path(directory: &Path, name: &str) -> Result<PathBuf> {
    if !owned_container_name(name) && !super::relocation::owned_snapshot_name(name) {
        return Err(invalid("invalid snapshot file name"));
    }
    let path = directory.join(name);
    match fs::symlink_metadata(&path) {
        Ok(m) if !m.is_file() || m.file_attributes() & 0x400 != 0 => {
            return Err(invalid("snapshot file must be a regular file"));
        }
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    Ok(path)
}

// FULL applies only to snapshot commits. Other collection writes retain their
// existing settings. secure_delete and a truncated WAL scrub retired image keys.
pub(crate) struct SnapshotWrite<'a> {
    connection: &'a Connection,
    synchronous: i64,
    secure_delete: i64,
}

impl<'a> SnapshotWrite<'a> {
    pub(crate) fn new(connection: &'a Connection) -> Result<Self> {
        let synchronous = connection.query_row("PRAGMA synchronous", [], |r| r.get(0))?;
        let secure_delete = connection.query_row("PRAGMA secure_delete", [], |r| r.get(0))?;
        let guard = Self {
            connection,
            synchronous,
            secure_delete,
        };
        connection.execute_batch("PRAGMA synchronous=FULL; PRAGMA secure_delete=ON;")?;
        Ok(guard)
    }
}

impl Drop for SnapshotWrite<'_> {
    fn drop(&mut self) {
        let _ = self
            .connection
            .pragma_update(None, "synchronous", self.synchronous);
        let _ = self
            .connection
            .pragma_update(None, "secure_delete", self.secure_delete);
    }
}

struct ContainerAppend {
    file: File,
    path: PathBuf,
    original_bytes: Option<u64>,
    bytes: u64,
    committed: bool,
}

impl ContainerAppend {
    fn open(storage: &Storage, tx: &Transaction<'_>, day: &str) -> Result<Self> {
        fs::create_dir_all(storage.snapshot_directory())?;
        let existing = tx
            .query_row(
                "SELECT file_name,committed_bytes FROM snapshot_containers WHERE local_day=?",
                [day],
                |r| Ok((r.get::<_, String>(0)?, row_u64(r, 1)?)),
            )
            .optional()?;
        if let Some((name, bytes)) = existing {
            let path = image_path(&storage.snapshot_directory(), &name)?;
            let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
            check_header(&mut file, bytes)?;
            let append = Self {
                file,
                path,
                original_bytes: Some(bytes),
                bytes,
                committed: false,
            };
            append.file.set_len(bytes)?; // Discard only an uncommitted tail.
            Ok(append)
        } else {
            let name = container_name()?;
            let path = image_path(&storage.snapshot_directory(), &name)?;
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)?;
            let mut append = Self {
                file,
                path,
                original_bytes: None,
                bytes: 8,
                committed: false,
            };
            append.file.write_all(MAGIC)?;
            append.file.sync_all()?;
            tx.execute(
                "INSERT INTO snapshot_containers VALUES(?,?,8)",
                params![day, name],
            )?;
            Ok(append)
        }
    }

    fn append(
        &mut self,
        tx: &Transaction<'_>,
        day: &str,
        blob_id: i64,
        plain: &[u8],
        hash: &[u8; 32],
    ) -> Result<()> {
        let key = random_key()?;
        let encrypted = encrypt_snapshot(&key, plain, hash)?;
        let webp_hash = sha256(plain)?;
        let offset = self.bytes;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(&encrypted)?;
        self.file.sync_all()?;
        fault("after_append")?;
        // Read back the actual file, authenticate it and compare encoded bytes.
        let mut verified = vec![0; encrypted.len()];
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.read_exact(&mut verified)?;
        let restored = Zeroizing::new(decrypt_snapshot(&key, &verified, hash)?);
        if restored.as_slice() != plain {
            return Err(invalid("snapshot append read-back failed"));
        }
        tx.execute(
            "INSERT INTO snapshot_container_records VALUES(?,?,?,?,?,?)",
            params![
                day,
                blob_id,
                to_sql_u64(offset, "snapshot offset")?,
                encrypted.len() as i64,
                key.as_slice(),
                webp_hash.as_slice()
            ],
        )?;
        self.bytes = offset
            .checked_add(encrypted.len() as u64)
            .ok_or_else(|| invalid("snapshot container is too large"))?;
        tx.execute(
            "UPDATE snapshot_containers SET committed_bytes=? WHERE local_day=?",
            params![to_sql_u64(self.bytes, "snapshot container size")?, day],
        )?;
        Ok(())
    }
}

impl Drop for ContainerAppend {
    fn drop(&mut self) {
        if !self.committed {
            if let Some(bytes) = self.original_bytes {
                let _ = self.file.set_len(bytes);
                let _ = self.file.sync_all();
            } else {
                // Windows permits deleting this handle's file after it closes;
                // orphan reconciliation also handles a failed best-effort unlink.
                let _ = fs::remove_file(&self.path);
            }
        }
    }
}

fn check_header(file: &mut File, committed_bytes: u64) -> Result<()> {
    if committed_bytes < 8 || file.metadata()?.len() < committed_bytes {
        return Err(invalid("snapshot container is truncated"));
    }
    let mut magic = [0; 8];
    file.seek(SeekFrom::Start(0))?;
    file.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(invalid("snapshot container header is invalid"));
    }
    Ok(())
}

struct Record {
    name: String,
    committed_bytes: u64,
    offset: u64,
    bytes: u64,
    key: Zeroizing<Vec<u8>>,
    webp_hash: Vec<u8>,
}

fn record(connection: &Connection, blob_id: i64, day: Option<&str>) -> Result<Option<Record>> {
    Ok(connection.query_row(
        "SELECT c.file_name,c.committed_bytes,r.record_offset,r.record_bytes,r.record_key,r.webp_sha256
         FROM snapshot_container_records r JOIN snapshot_containers c USING(local_day)
         WHERE r.blob_id=?1 AND (?2 IS NULL OR r.local_day=?2) ORDER BY r.local_day LIMIT 1",
        params![blob_id,day], |r| Ok(Record { name:r.get(0)?,committed_bytes:row_u64(r,1)?,offset:row_u64(r,2)?,
            bytes:row_u64(r,3)?,key:Zeroizing::new(r.get(4)?),webp_hash:r.get(5)? }),
    ).optional()?)
}

fn read_record(directory: &Path, record: &Record, hash: &[u8; 32]) -> Result<Zeroizing<Vec<u8>>> {
    if record.offset < 8
        || record.bytes <= RECORD_OVERHEAD
        || record.bytes > u64::from(u32::MAX) + RECORD_OVERHEAD
        || record
            .offset
            .checked_add(record.bytes)
            .is_none_or(|end| end > record.committed_bytes)
    {
        return Err(invalid("invalid snapshot record bounds"));
    }
    let mut file = File::open(image_path(directory, &record.name)?)?;
    check_header(&mut file, record.committed_bytes)?;
    file.seek(SeekFrom::Start(record.offset))?;
    let mut bytes = vec![
        0;
        usize::try_from(record.bytes)
            .map_err(|_| invalid("snapshot record is too large"))?
    ];
    file.read_exact(&mut bytes)?;
    let plain = Zeroizing::new(decrypt_snapshot(&record.key, &bytes, hash)?);
    if sha256(&plain)?.as_slice() != record.webp_hash {
        return Err(invalid("snapshot encoded bytes failed verification"));
    }
    Ok(plain)
}

impl Storage {
    pub(crate) fn append_snapshot(
        &self,
        request: &SnapshotStoreRequest,
        hash: &[u8; 32],
    ) -> Result<i64> {
        self.append_snapshot_on_day(
            request,
            hash,
            &local_day(request.captured_at_utc_ms)?.to_string(),
        )
    }

    fn append_snapshot_on_day(
        &self,
        request: &SnapshotStoreRequest,
        hash: &[u8; 32],
        day: &str,
    ) -> Result<i64> {
        self.ensure_writable()?;
        let _durable = SnapshotWrite::new(&self.connection)?;
        let tx = Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let existing = tx.query_row("SELECT blob_id,pixel_width,pixel_height,plaintext_bytes FROM snapshot_blobs WHERE content_sha256=?",
            [hash.as_slice()], |r| Ok((r.get::<_,i64>(0)?,r.get::<_,u32>(1)?,r.get::<_,u32>(2)?,row_u64(r,3)?))).optional()?;
        let blob_id = if let Some((id, width, height, _)) = existing {
            if (width, height) != (request.pixel_width, request.pixel_height) {
                return Err(invalid("snapshot dimensions are inconsistent"));
            }
            id
        } else {
            tx.execute("INSERT INTO snapshot_blobs(file_name,content_sha256,plaintext_bytes,encrypted_bytes,pixel_width,pixel_height,created_utc_ms) VALUES(?,?,?,?,?,?,?)",
                params![random_snapshot_file_name()?,hash.as_slice(),request.webp.len() as i64,request.webp.len() as i64+RECORD_OVERHEAD as i64,request.pixel_width,request.pixel_height,request.captured_at_utc_ms])?;
            tx.last_insert_rowid()
        };
        let mut append = ContainerAppend::open(self, &tx, day)?;
        if record(&tx, blob_id, Some(day))?.is_none() {
            // Global pixel deduplication remains unchanged. A copy per day lets
            // an old day expire without breaking a later day's identical image.
            // Every copy keeps the blob's recorded size; this capture has the same
            // pixels, so when its size matches a damaged older day is never read.
            let canonical = match existing {
                Some((.., size)) if size != request.webp.len() as u64 => {
                    Some(Zeroizing::new(self.read_snapshot_payload(blob_id, None)?))
                }
                _ => None,
            };
            append.append(
                &tx,
                day,
                blob_id,
                canonical
                    .as_deref()
                    .map_or(request.webp.as_slice(), Vec::as_slice),
                hash,
            )?;
        }
        tx.execute("INSERT INTO snapshot_slots (
            slot_started_utc_ms,captured_at_utc_ms,display_key,display_x,display_y,display_width,display_height,
            orientation_degrees,pixel_width,pixel_height,blob_id,trigger,capture_method,result,missing_reason,timeline_started_utc_ms,timeline_ended_utc_ms)
            VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,'desktop_duplication','success',NULL,?1,?13)",
            params![request.slot_started_utc_ms,request.captured_at_utc_ms,request.display.key,request.display.x,request.display.y,request.display.width,request.display.height,
                request.display.orientation_degrees,request.pixel_width,request.pixel_height,blob_id,request.trigger.as_str(),request.slot_started_utc_ms.saturating_add(1)])?;
        let slot_id = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO snapshot_slot_days VALUES(?,?)",
            params![slot_id, day],
        )?;
        append.committed = true;
        // If COMMIT itself reports an I/O error, its durable outcome can be
        // ambiguous. Keep the bytes; reconciliation consults SQLite on retry.
        tx.commit()?;
        Ok(slot_id)
    }

    pub(crate) fn read_snapshot_payload(
        &self,
        blob_id: i64,
        slot_id: Option<i64>,
    ) -> Result<Vec<u8>> {
        read_payload(
            &self.connection,
            &self.snapshot_directory(),
            &self.key_path,
            blob_id,
            slot_id,
        )
        .map(|bytes| bytes.to_vec())
    }

    /// Only the image-container layer changes. Historical local-day assignments
    /// from a current backup are retained; old snapshots use OS timezone rules.
    pub(crate) fn migrate_snapshot_containers(&self) -> Result<()> {
        let pending = self.connection.prepare(
            "SELECT s.slot_id,s.blob_id,s.captured_at_utc_ms,d.local_day FROM snapshot_slots s
             LEFT JOIN snapshot_slot_days d USING(slot_id)
             LEFT JOIN snapshot_container_records r ON r.blob_id=s.blob_id AND r.local_day=d.local_day
             WHERE s.result='success' AND r.blob_id IS NULL ORDER BY s.captured_at_utc_ms,s.slot_id")?
            .query_map([],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,i64>(2)?,r.get::<_,Option<String>>(3)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut days = BTreeMap::<String, Vec<(i64, i64)>>::new();
        for (slot, blob, utc, day) in pending {
            let day = day
                .map(Ok)
                .unwrap_or_else(|| local_day(utc).map(|d| d.to_string()))?;
            NaiveDate::parse_from_str(&day, "%Y-%m-%d")
                .map_err(|_| invalid("invalid snapshot calendar day"))?;
            days.entry(day).or_default().push((slot, blob));
        }
        if !days.is_empty() {
            // Freeze the old slots' day assignments before copying anything. A
            // timezone change during an interrupted migration cannot regroup them.
            let _durable = SnapshotWrite::new(&self.connection)?;
            let tx = Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
            for (day, slots) in &days {
                for (slot, _) in slots {
                    tx.execute(
                        "INSERT OR IGNORE INTO snapshot_slot_days VALUES(?,?)",
                        params![slot, day],
                    )?;
                }
            }
            tx.commit()?;
            fault("after_migration_dates")?;
        }
        for (day, slots) in days {
            let _durable = SnapshotWrite::new(&self.connection)?;
            let tx = Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
            let mut append = ContainerAppend::open(self, &tx, &day)?;
            for (slot, blob) in slots {
                if record(&tx, blob, Some(&day))?.is_none() {
                    let plain =
                        read_payload(&tx, &self.snapshot_directory(), &self.key_path, blob, None)?;
                    let hash = tx.query_row(
                        "SELECT content_sha256 FROM snapshot_blobs WHERE blob_id=?",
                        [blob],
                        |r| r.get::<_, Vec<u8>>(0),
                    )?;
                    let hash: [u8; 32] = hash
                        .try_into()
                        .map_err(|_| invalid("invalid snapshot pixel hash"))?;
                    append.append(&tx, &day, blob, &plain, &hash)?;
                }
                tx.execute("INSERT INTO snapshot_slot_days VALUES(?,?) ON CONFLICT(slot_id) DO UPDATE SET local_day=excluded.local_day",params![slot,day])?;
            }
            append.committed = true;
            tx.commit()?;
            fault("after_migration_day")?;
        }
        // Retain all old files until every original reference has a verified
        // replacement. Reopening after any interruption retries idempotently.
        let has_legacy = if self.snapshot_directory().exists() {
            fs::read_dir(self.snapshot_directory())?.try_fold(false, |found, entry| {
                let entry = entry?;
                Ok::<_, std::io::Error>(
                    found
                        || (entry.file_type()?.is_file()
                            && super::relocation::owned_snapshot_name(
                                &entry.file_name().to_string_lossy(),
                            )),
                )
            })?
        } else {
            false
        };
        if has_legacy {
            self.verify_snapshot_containers()?;
            self.checkpoint()?;
        }
        Ok(())
    }

    pub(crate) fn verify_snapshot_containers(&self) -> Result<()> {
        let ids = self.connection.prepare("SELECT MIN(s.slot_id) FROM snapshot_slots s JOIN snapshot_slot_days d USING(slot_id) WHERE s.result='success' GROUP BY d.local_day,s.blob_id")?
            .query_map([],|r|r.get::<_,i64>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let successes: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM snapshot_slots WHERE result='success'",
            [],
            |r| r.get(0),
        )?;
        let indexed: i64 = self.connection.query_row("SELECT COUNT(*) FROM snapshot_slots s JOIN snapshot_slot_days d USING(slot_id) JOIN snapshot_container_records r ON r.local_day=d.local_day AND r.blob_id=s.blob_id WHERE s.result='success'",[],|r|r.get(0))?;
        if successes != indexed {
            return Err(invalid("snapshot migration has unindexed images"));
        }
        for id in ids {
            self.load_snapshot_image(id)?;
        }
        Ok(())
    }

    pub(crate) fn snapshot_container_files(&self) -> Result<Vec<String>> {
        Ok(self
            .connection
            .prepare("SELECT file_name FROM snapshot_containers ORDER BY local_day")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub(crate) fn reconcile_snapshot_containers(&self) -> Result<()> {
        let directory = self.snapshot_directory();
        // Legacy staged deletes are still understood during the format upgrade.
        if directory.exists() {
            for entry in fs::read_dir(&directory)? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                if let Some(original) = name.strip_suffix(".deleting")
                    && (owned_container_name(original)
                        || super::relocation::owned_snapshot_name(original))
                    && entry.file_type()?.is_file()
                {
                    let referenced: bool = self.connection.query_row("SELECT EXISTS(SELECT 1 FROM snapshot_blobs WHERE file_name=?1 UNION ALL SELECT 1 FROM snapshot_containers WHERE file_name=?1)",[original],|r|r.get(0))?;
                    let path = image_path(&directory, original)?;
                    if referenced && !path.exists() {
                        fs::rename(entry.path(), path)?;
                    } else {
                        fs::remove_file(entry.path())?;
                    }
                }
            }
        }
        let containers = self
            .connection
            .prepare("SELECT local_day,file_name,committed_bytes FROM snapshot_containers")?
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    row_u64(r, 2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (day, name, bytes) in containers {
            let path = image_path(&directory, &name)?;
            if !path.exists() {
                let legacy = self.connection.prepare("SELECT b.file_name FROM snapshot_container_records r JOIN snapshot_blobs b USING(blob_id) WHERE r.local_day=?")?
                    .query_map([&day],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
                if !legacy.is_empty() && legacy.iter().all(|n| directory.join(n).is_file()) {
                    self.connection
                        .execute("DELETE FROM snapshot_containers WHERE local_day=?", [&day])?;
                } else {
                    self.clean_snapshot_day(&day, SnapshotMissingReason::CaptureFailed)?;
                }
                continue;
            }
            let ranges = self.connection.prepare("SELECT record_offset,record_bytes FROM snapshot_container_records WHERE local_day=? ORDER BY record_offset")?
                .query_map([&day],|r|Ok((row_u64(r,0)?,row_u64(r,1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
            let mut end = 8_u64;
            for (offset, length) in ranges {
                if offset != end {
                    return Err(invalid("snapshot container index overlaps or has a gap"));
                }
                end = end
                    .checked_add(length)
                    .ok_or_else(|| invalid("snapshot container index overflow"))?;
            }
            if end != bytes {
                return Err(invalid("snapshot container index size is inconsistent"));
            }
            let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
            check_header(&mut file, bytes)?;
            if file.metadata()?.len() > bytes {
                file.set_len(bytes)?;
                file.sync_all()?;
            }
        }
        // Preserve the existing explicit capture_failed behavior for missing old files.
        let missing = self.connection.prepare("SELECT b.blob_id,b.file_name FROM snapshot_blobs b WHERE NOT EXISTS(SELECT 1 FROM snapshot_container_records r WHERE r.blob_id=b.blob_id)")?
            .query_map([],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        for (blob, name) in missing {
            if !image_path(&directory, &name)?.exists() {
                let _durable = SnapshotWrite::new(&self.connection)?;
                let tx =
                    Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
                tx.execute("UPDATE snapshot_slots SET captured_at_utc_ms=NULL,pixel_width=0,pixel_height=0,blob_id=NULL,capture_method='none',result='cleaned',missing_reason='capture_failed' WHERE blob_id=?",[blob])?;
                tx.execute("DELETE FROM snapshot_blobs WHERE blob_id=?", [blob])?;
                tx.commit()?;
                self.checkpoint()?;
            }
        }
        self.migrate_snapshot_containers()?;
        // Reconciliation always sweeps, including after a backup replaced every row.
        self.snapshot_files_settled.set(false);
        self.settle_snapshot_files()
    }

    /// Finish the key scrubbing and file removal that deletions left behind.
    pub(crate) fn settle_snapshot_files(&self) -> Result<()> {
        if !self.snapshot_files_settled.get() {
            self.finish_snapshot_key_cleanup()?;
            self.purge_unreferenced_snapshot_files()?;
            self.snapshot_files_settled.set(true);
        }
        Ok(())
    }

    fn purge_unreferenced_snapshot_files(&self) -> Result<()> {
        let directory = self.snapshot_directory();
        let pending: bool = self.connection.query_row("SELECT EXISTS(SELECT 1 FROM snapshot_slots s LEFT JOIN snapshot_slot_days d USING(slot_id) LEFT JOIN snapshot_container_records r ON r.blob_id=s.blob_id AND r.local_day=d.local_day WHERE s.result='success' AND r.blob_id IS NULL)",[],|r|r.get(0))?;
        // An incomplete old-format migration keeps every file until a later
        // open finishes it; the remaining retention work must still proceed.
        if pending {
            return Ok(());
        }
        let referenced = self
            .snapshot_container_files()?
            .into_iter()
            .collect::<BTreeSet<_>>();
        if directory.exists() {
            for entry in fs::read_dir(&directory)? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                if entry.file_type()?.is_file()
                    && !referenced.contains(&name)
                    && (owned_container_name(&name)
                        || super::relocation::owned_snapshot_name(&name))
                {
                    fs::remove_file(image_path(&directory, &name)?)?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn snapshot_live_bytes(&self) -> Result<u64> {
        Ok(self.connection.query_row(
            "SELECT COALESCE(SUM(committed_bytes),0) FROM snapshot_containers",
            [],
            |r| row_u64(r, 0),
        )?)
    }

    pub(crate) fn apply_snapshot_day_retention(
        &self,
        now_utc_ms: i64,
    ) -> Result<SnapshotRetentionOutcome> {
        // Finish what an earlier deletion or open left behind. A reader holding
        // the log only postpones it; the rest of retention still runs.
        if let Err(error) = self.settle_snapshot_files() {
            eprintln!("snapshot cleanup postponed: {error}");
        }
        let policy = self.snapshot_policy()?;
        let today = local_day(now_utc_ms)?;
        let cutoff = policy
            .retention_days
            .map(|days| {
                today
                    .checked_sub_days(Days::new(u64::from(days - 1)))
                    .ok_or_else(|| invalid("snapshot retention date is out of range"))
            })
            .transpose()?;
        let today = today.to_string();
        let days = self
            .connection
            .prepare("SELECT local_day FROM snapshot_containers ORDER BY local_day")?
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut outcome = SnapshotRetentionOutcome::default();
        for day in days {
            let expired = cutoff.is_some_and(|cutoff| day < cutoff.to_string());
            // Capacity never removes the current day, or each new capture would
            // wipe the day it was just stored in; it becomes eligible tomorrow.
            if expired || (day < today && self.snapshot_live_bytes()? > policy.max_bytes) {
                let removed =
                    self.clean_snapshot_day(&day, SnapshotMissingReason::RetentionCleaned)?;
                outcome.snapshot_items += removed.snapshot_items;
                outcome.released_bytes += removed.released_bytes;
            }
        }
        Ok(outcome)
    }

    fn clean_snapshot_day(
        &self,
        day: &str,
        reason: SnapshotMissingReason,
    ) -> Result<SnapshotRetentionOutcome> {
        self.ensure_writable()?;
        let _durable = SnapshotWrite::new(&self.connection)?;
        let tx = Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let container = tx
            .query_row(
                "SELECT file_name,committed_bytes FROM snapshot_containers WHERE local_day=?",
                [day],
                |r| Ok((r.get::<_, String>(0)?, row_u64(r, 1)?)),
            )
            .optional()?;
        let count = tx.execute("UPDATE snapshot_slots SET captured_at_utc_ms=NULL,pixel_width=0,pixel_height=0,blob_id=NULL,capture_method='none',result='cleaned',missing_reason=?
            WHERE result='success' AND slot_id IN(SELECT slot_id FROM snapshot_slot_days WHERE local_day=?)",params![reason.as_str(),day])?;
        tx.execute("DELETE FROM snapshot_slot_days WHERE local_day=?", [day])?;
        tx.execute("DELETE FROM snapshot_containers WHERE local_day=?", [day])?;
        tx.execute("DELETE FROM snapshot_blobs WHERE NOT EXISTS(SELECT 1 FROM snapshot_slots s WHERE s.blob_id=snapshot_blobs.blob_id)",[])?;
        tx.commit()?;
        self.snapshot_files_settled.set(false);
        let mut released = 0;
        let cleanup = self.finish_snapshot_key_cleanup().and_then(|()| {
            if let Some((name, _)) = container {
                let path = image_path(&self.snapshot_directory(), &name)?;
                if path.exists() {
                    let bytes = fs::metadata(&path)?.len();
                    fs::remove_file(path)?;
                    released = bytes;
                }
            }
            Ok(())
        });
        // The day is already gone from the index; the next cleanup pass removes
        // the rest, so its result stands.
        if let Err(error) = cleanup {
            eprintln!("snapshot day cleanup postponed: {error}");
        }
        Ok(SnapshotRetentionOutcome {
            snapshot_items: count as u64,
            released_bytes: released,
            ..Default::default()
        })
    }

    pub(crate) fn delete_snapshot_from_container(
        &self,
        slot_id: i64,
        reason: SnapshotMissingReason,
    ) -> Result<Option<u64>> {
        self.ensure_writable()?;
        let _durable = SnapshotWrite::new(&self.connection)?;
        let tx = Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let target = tx.query_row("SELECT s.blob_id,d.local_day,c.file_name,c.committed_bytes FROM snapshot_slots s JOIN snapshot_slot_days d USING(slot_id)
            JOIN snapshot_containers c USING(local_day) WHERE s.slot_id=? AND s.result='success'",[slot_id],
            |r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,row_u64(r,3)?))).optional()?;
        let Some((blob, day, old_name, old_bytes)) = target else {
            return Ok(None);
        };
        tx.execute("UPDATE snapshot_slots SET captured_at_utc_ms=NULL,pixel_width=0,pixel_height=0,blob_id=NULL,capture_method='none',result='cleaned',missing_reason=? WHERE slot_id=?",params![reason.as_str(),slot_id])?;
        tx.execute("DELETE FROM snapshot_slot_days WHERE slot_id=?", [slot_id])?;
        let remaining: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM snapshot_slots s JOIN snapshot_slot_days d USING(slot_id) WHERE s.blob_id=? AND d.local_day=?)",params![blob,day],|r|r.get(0))?;
        if remaining {
            tx.commit()?;
            self.snapshot_files_settled.set(false);
            self.finish_snapshot_key_cleanup()
                .map_err(cleanup_pending)?;
            return Ok(Some(0));
        }
        let blobs = tx.prepare("SELECT blob_id FROM snapshot_container_records WHERE local_day=? AND blob_id<>? ORDER BY record_offset")?
            .query_map(params![day,blob],|r|r.get::<_,i64>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        tx.execute(
            "DELETE FROM snapshot_container_records WHERE local_day=? AND blob_id=?",
            params![day, blob],
        )?;
        let mut replacement = None;
        if blobs.is_empty() {
            tx.execute("DELETE FROM snapshot_containers WHERE local_day=?", [&day])?;
        } else {
            let name = container_name()?;
            let path = image_path(&self.snapshot_directory(), &name)?;
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)?;
            let mut staged = ContainerAppend {
                file,
                path,
                original_bytes: None,
                bytes: 8,
                committed: false,
            };
            staged.file.write_all(MAGIC)?;
            for id in blobs {
                let r = record(&tx, id, Some(&day))?
                    .ok_or_else(|| invalid("snapshot record disappeared"))?;
                let hash = tx.query_row(
                    "SELECT content_sha256 FROM snapshot_blobs WHERE blob_id=?",
                    [id],
                    |r| r.get::<_, Vec<u8>>(0),
                )?;
                let hash: [u8; 32] = hash
                    .try_into()
                    .map_err(|_| invalid("snapshot hash is invalid"))?;
                let _verified = read_record(&self.snapshot_directory(), &r, &hash)?;
                let mut source = File::open(image_path(&self.snapshot_directory(), &old_name)?)?;
                source.seek(SeekFrom::Start(r.offset))?;
                let offset = staged.bytes;
                staged.file.seek(SeekFrom::Start(offset))?;
                let copied = std::io::copy(&mut source.take(r.bytes), &mut staged.file)?;
                if copied != r.bytes {
                    return Err(invalid("snapshot compaction was truncated"));
                }
                staged.bytes += copied;
                tx.execute("UPDATE snapshot_container_records SET record_offset=? WHERE local_day=? AND blob_id=?",params![to_sql_u64(offset,"snapshot offset")?,day,id])?;
            }
            staged.file.sync_all()?;
            fault("after_compaction_file")?;
            tx.execute(
                "UPDATE snapshot_containers SET file_name=?,committed_bytes=? WHERE local_day=?",
                params![
                    name,
                    to_sql_u64(staged.bytes, "snapshot container size")?,
                    day
                ],
            )?;
            // Authenticate every retained record at its new on-disk position.
            let ids = tx.prepare("SELECT blob_id FROM snapshot_container_records WHERE local_day=? AND blob_id<>?")?
                .query_map(params![day,blob],|r|r.get::<_,i64>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            for id in ids {
                let r = record(&tx, id, Some(&day))?
                    .ok_or_else(|| invalid("snapshot record disappeared"))?;
                let hash = tx.query_row(
                    "SELECT content_sha256 FROM snapshot_blobs WHERE blob_id=?",
                    [id],
                    |r| r.get::<_, Vec<u8>>(0),
                )?;
                let hash: [u8; 32] = hash
                    .try_into()
                    .map_err(|_| invalid("snapshot hash is invalid"))?;
                read_record(&self.snapshot_directory(), &r, &hash)?;
            }
            replacement = Some(staged);
        }
        tx.execute("DELETE FROM snapshot_blobs WHERE blob_id=? AND NOT EXISTS(SELECT 1 FROM snapshot_slots WHERE blob_id=?)",params![blob,blob])?;
        if let Some(file) = &mut replacement {
            file.committed = true;
        }
        tx.commit()?;
        self.snapshot_files_settled.set(false);
        let new_bytes = if let Some(file) = replacement {
            file.bytes
        } else {
            0
        };
        self.finish_snapshot_key_cleanup()
            .and_then(|()| fault("after_delete_commit"))
            .and_then(|()| {
                Ok(fs::remove_file(image_path(
                    &self.snapshot_directory(),
                    &old_name,
                )?)?)
            })
            .map_err(cleanup_pending)?;
        Ok(Some(old_bytes.saturating_sub(new_bytes)))
    }

    fn finish_snapshot_key_cleanup(&self) -> Result<()> {
        let (busy, remaining): (i64, i64) =
            self.connection
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })?;
        if busy != 0 || remaining > 0 {
            return Err(invalid(
                "snapshot key cleanup is waiting for database readers",
            ));
        }
        Ok(())
    }
}

/// After a deletion commits, a failure only delays scrubbing; the slot stays deleted.
fn cleanup_pending(error: StorageError) -> StorageError {
    StorageError::SnapshotCleanupPending(error.to_string())
}

pub(crate) fn read_payload(
    connection: &Connection,
    directory: &Path,
    key_path: &Path,
    blob_id: i64,
    slot_id: Option<i64>,
) -> Result<Zeroizing<Vec<u8>>> {
    let (name,hash,size,width,height) = connection.query_row("SELECT file_name,content_sha256,plaintext_bytes,pixel_width,pixel_height FROM snapshot_blobs WHERE blob_id=?",[blob_id],
        |r|Ok((r.get::<_,String>(0)?,r.get::<_,Vec<u8>>(1)?,row_u64(r,2)?,r.get::<_,u32>(3)?,r.get::<_,u32>(4)?)))?;
    let hash: [u8; 32] = hash
        .try_into()
        .map_err(|_| invalid("snapshot hash is invalid"))?;
    let day = slot_id
        .map(|id| {
            connection
                .query_row(
                    "SELECT local_day FROM snapshot_slot_days WHERE slot_id=?",
                    [id],
                    |r| r.get::<_, String>(0),
                )
                .optional()
        })
        .transpose()?
        .flatten();
    let plain = if let Some(record) = record(connection, blob_id, day.as_deref())? {
        read_record(directory, &record, &hash)?
    } else {
        let bytes = fs::read(image_path(directory, &name)?)?;
        Zeroizing::new(decrypt_snapshot(&load_key(key_path)?, &bytes, &hash)?)
    };
    if plain.len() as u64 != size || snapshot_pixel_hash(&plain, width, height)? != hash {
        return Err(invalid("snapshot image failed integrity verification"));
    }
    Ok(plain)
}

#[cfg(not(test))]
fn fault(_: &'static str) -> Result<()> {
    Ok(())
}

#[cfg(test)]
thread_local! { static FAILURE: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) }; }

#[cfg(test)]
fn fault(point: &'static str) -> Result<()> {
    if FAILURE.with(|f| f.get() == Some(point)) {
        FAILURE.with(|f| f.set(None));
        return Err(std::io::Error::other("injected snapshot storage interruption").into());
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{BackupOptions, SnapshotDisplay, SnapshotTrigger};
    use chrono::{FixedOffset, Utc};
    use image::ImageEncoder;

    pub(crate) fn request(utc: i64, color: u8) -> SnapshotStoreRequest {
        let mut webp = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut webp)
            .write_image(
                &[color; 16 * 12 * 3],
                16,
                12,
                image::ExtendedColorType::Rgb8,
            )
            .unwrap();
        SnapshotStoreRequest {
            slot_started_utc_ms: utc,
            captured_at_utc_ms: utc + 10,
            display: SnapshotDisplay::session(),
            pixel_width: 16,
            pixel_height: 12,
            trigger: SnapshotTrigger::Manual,
            webp,
        }
    }

    fn store_on_day(storage: &Storage, request: &SnapshotStoreRequest, day: &str) -> i64 {
        let hash =
            snapshot_pixel_hash(&request.webp, request.pixel_width, request.pixel_height).unwrap();
        storage.append_snapshot_on_day(request, &hash, day).unwrap()
    }

    fn files(storage: &Storage) -> Vec<PathBuf> {
        fs::read_dir(storage.snapshot_directory())
            .unwrap()
            .map(|f| f.unwrap().path())
            .collect()
    }

    fn make_legacy(storage: &Storage) -> Vec<(PathBuf, Vec<u8>)> {
        let blobs = storage
            .connection
            .prepare("SELECT blob_id,file_name,content_sha256 FROM snapshot_blobs")
            .unwrap()
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        let mut originals = Vec::new();
        for (id, name, hash) in blobs {
            let hash: [u8; 32] = hash.try_into().unwrap();
            let plain = storage.read_snapshot_payload(id, None).unwrap();
            let bytes =
                encrypt_snapshot(&load_key(&storage.key_path).unwrap(), &plain, &hash).unwrap();
            let path = storage.snapshot_directory().join(name);
            super::super::milestone3::write_new_file(&path, &bytes).unwrap();
            originals.push((path, bytes));
        }
        for name in storage.snapshot_container_files().unwrap() {
            fs::remove_file(storage.snapshot_directory().join(name)).unwrap();
        }
        storage.connection.execute_batch("DROP TABLE snapshot_slot_days; DROP TABLE snapshot_container_records; DROP TABLE snapshot_containers; PRAGMA user_version=12;").unwrap();
        storage.checkpoint().unwrap();
        originals
    }

    #[test]
    fn distinct_images_append_to_one_encrypted_container_without_reencoding() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        let a = request(1_700_000_000_000, 17);
        let first = storage.store_snapshot(&a).unwrap();
        let path = files(&storage)[0].clone();
        let prefix = fs::read(&path).unwrap();
        let b = request(a.slot_started_utc_ms + 1000, 89);
        let second = storage.store_snapshot(&b).unwrap();
        assert_eq!(files(&storage), vec![path.clone()]);
        let content = fs::read(path).unwrap();
        assert_eq!(&content[..prefix.len()], prefix);
        assert!(!content.windows(a.webp.len()).any(|w| w == a.webp));
        assert_eq!(storage.load_snapshot_image(first).unwrap().webp, a.webp);
        assert_eq!(storage.load_snapshot_image(second).unwrap().webp, b.webp);
        assert_eq!(storage.snapshot_live_bytes().unwrap(), content.len() as u64);
        let keys = storage
            .connection
            .prepare("SELECT record_key FROM snapshot_container_records")
            .unwrap()
            .query_map([], |r| r.get::<_, Vec<u8>>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_ne!(keys[0], keys[1]);
        assert_ne!(keys[0], load_key(&storage.key_path).unwrap().as_slice());
    }

    #[test]
    fn local_midnight_and_short_or_long_days_use_calendar_dates() {
        let japan = FixedOffset::east_opt(9 * 3600).unwrap();
        let utc = Utc
            .with_ymd_and_hms(2026, 10, 10, 15, 0, 0)
            .unwrap()
            .timestamp_millis();
        assert_eq!(
            calendar_day(utc - 1, &japan).unwrap().to_string(),
            "2026-10-10"
        );
        assert_eq!(calendar_day(utc, &japan).unwrap().to_string(), "2026-10-11");
        // Both DST transitions yield adjacent dates although midnight separation
        // is 23 or 25 hours. Retention subtracts dates, never 24-hour durations.
        for (year, month, day, before, after, hours) in
            [(2026, 3, 8, -5, -4, 23), (2026, 11, 1, -4, -5, 25)]
        {
            let before = FixedOffset::east_opt(before * 3600).unwrap();
            let after = FixedOffset::east_opt(after * 3600).unwrap();
            let start = before.with_ymd_and_hms(year, month, day, 0, 0, 0).unwrap();
            let end = after
                .with_ymd_and_hms(year, month, day + 1, 0, 0, 0)
                .unwrap();
            assert_eq!(
                end.timestamp_millis() - start.timestamp_millis(),
                hours * 3_600_000
            );
            assert_eq!(
                end.date_naive().checked_sub_days(Days::new(1)).unwrap(),
                start.date_naive()
            );
        }
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        let local_midnight = Local
            .with_ymd_and_hms(2026, 10, 11, 0, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis();
        let a = request(local_midnight - 20, 7);
        let b = request(local_midnight, 8);
        storage.store_snapshot(&a).unwrap();
        storage.store_snapshot(&b).unwrap();
        assert_eq!(storage.snapshot_container_files().unwrap().len(), 2);
    }

    #[test]
    fn time_retention_removes_complete_days_and_keeps_today_in_the_day_count() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        let now = Local
            .with_ymd_and_hms(2026, 10, 11, 0, 1, 0)
            .single()
            .unwrap()
            .timestamp_millis();
        let first = store_on_day(&storage, &request(now - 1000, 10), "2026-10-09");
        let last = store_on_day(&storage, &request(now - 900, 11), "2026-10-09");
        let yesterday = store_on_day(&storage, &request(now - 800, 12), "2026-10-10");
        let today = store_on_day(&storage, &request(now - 700, 13), "2026-10-11");
        let mut policy = storage.snapshot_policy().unwrap();
        policy.retention_days = Some(2);
        storage.set_snapshot_policy(policy, now).unwrap();
        let result = storage.apply_snapshot_day_retention(now).unwrap();
        assert_eq!(result.snapshot_items, 2);
        assert!(storage.load_snapshot_image(first).is_err());
        assert!(storage.load_snapshot_image(last).is_err());
        assert!(storage.load_snapshot_image(yesterday).is_ok());
        assert!(storage.load_snapshot_image(today).is_ok());
        assert_eq!(files(&storage).len(), 2);
    }

    #[test]
    fn capacity_removes_the_whole_oldest_day_and_shared_pixels_survive_in_a_newer_day() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        let mut a = request(1_700_000_000_000, 10);
        let pixels = (0..512 * 512 * 3)
            .map(|n| ((n * 997 + n / 379) ^ ((n * 37) >> 11)) as u8)
            .collect::<Vec<_>>();
        let mut webp = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut webp)
            .write_image(&pixels, 512, 512, image::ExtendedColorType::Rgb8)
            .unwrap();
        a.webp = webp;
        a.pixel_width = 512;
        a.pixel_height = 512;
        assert!(a.webp.len() > 512 * 1024 && a.webp.len() < 1024 * 1024);
        let old = store_on_day(&storage, &a, "2023-11-14");
        let b = request(a.slot_started_utc_ms + 1000, 21);
        let old_last = store_on_day(&storage, &b, "2023-11-14");
        let mut duplicate = a.clone();
        duplicate.slot_started_utc_ms += 2000;
        duplicate.captured_at_utc_ms += 2000;
        let new = store_on_day(&storage, &duplicate, "2023-11-15");
        let mut policy = storage.snapshot_policy().unwrap();
        policy.retention_days = None;
        policy.max_bytes = 1024 * 1024;
        storage
            .set_snapshot_policy(policy, a.captured_at_utc_ms)
            .unwrap();
        // Both days must be in the past in every time zone; today is never cleaned by capacity.
        let now = Local
            .with_ymd_and_hms(2023, 11, 16, 12, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis();
        let outcome = storage.apply_snapshot_day_retention(now).unwrap();
        assert_eq!(outcome.snapshot_items, 2);
        assert!(storage.load_snapshot_image(old).is_err());
        assert!(storage.load_snapshot_image(old_last).is_err());
        assert_eq!(storage.load_snapshot_image(new).unwrap().webp, a.webp);
        assert_eq!(files(&storage).len(), 1);
        assert!(storage.snapshot_live_bytes().unwrap() <= policy.max_bytes);
    }

    #[test]
    fn a_damaged_older_copy_does_not_block_the_same_image_on_a_later_day() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        let a = request(1_700_000_000_000, 10);
        store_on_day(&storage, &a, "2023-11-14");
        let old = files(&storage)[0].clone();
        let mut bytes = fs::read(&old).unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        fs::write(&old, bytes).unwrap();
        let mut later = a.clone();
        later.slot_started_utc_ms += 86_400_000;
        later.captured_at_utc_ms += 86_400_000;
        let id = store_on_day(&storage, &later, "2023-11-15");
        assert_eq!(storage.load_snapshot_image(id).unwrap().webp, a.webp);
    }

    #[test]
    fn retention_skips_settled_files_and_retries_cleanup_a_reader_delayed() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        let a = request(1_700_000_000_000, 10);
        let b = request(a.slot_started_utc_ms + 1000, 20);
        let first = storage.store_snapshot(&a).unwrap();
        let second = storage.store_snapshot(&b).unwrap();
        let now = b.captured_at_utc_ms + 1000;
        // Only a cleanup pass looks for unreferenced files, so this one marks when a pass ran.
        let stray = storage.snapshot_directory().join(container_name().unwrap());
        fs::write(&stray, MAGIC).unwrap();
        storage.apply_retention(now).unwrap();
        assert!(stray.exists());

        storage
            .connection
            .execute_batch("PRAGMA busy_timeout=0")
            .unwrap();
        let key = load_key(&storage.key_path).unwrap();
        let reader = super::super::open_encrypted(&storage.database_path, &key, true).unwrap();
        reader
            .execute_batch("BEGIN; SELECT COUNT(*) FROM snapshot_slots;")
            .unwrap();
        // The deletion commits; its key cleanup and old-file removal wait for the reader.
        assert!(matches!(
            storage.delete_snapshot(first),
            Err(StorageError::SnapshotCleanupPending(_))
        ));
        assert_eq!(files(&storage).len(), 3);
        storage.apply_retention(now).unwrap();
        assert_eq!(files(&storage).len(), 3);

        drop(reader);
        storage.apply_retention(now).unwrap();
        assert!(!stray.exists());
        assert_eq!(files(&storage).len(), 1);
        assert!(storage.load_snapshot_image(first).is_err());
        assert_eq!(storage.load_snapshot_image(second).unwrap().webp, b.webp);
    }

    #[test]
    fn a_delayed_day_cleanup_still_reports_retention_and_finishes_later() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        let now = Local
            .with_ymd_and_hms(2026, 10, 11, 12, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis();
        let old = store_on_day(&storage, &request(now - 2000, 10), "2026-10-09");
        let today = store_on_day(&storage, &request(now - 1000, 11), "2026-10-11");
        let mut policy = storage.snapshot_policy().unwrap();
        policy.retention_days = Some(1);
        storage.set_snapshot_policy(policy, now).unwrap();
        storage
            .connection
            .execute_batch("PRAGMA busy_timeout=0")
            .unwrap();
        let key = load_key(&storage.key_path).unwrap();
        let reader = super::super::open_encrypted(&storage.database_path, &key, true).unwrap();
        reader
            .execute_batch("BEGIN; SELECT COUNT(*) FROM snapshot_slots;")
            .unwrap();
        let report = storage.apply_retention(now).unwrap();
        assert_eq!(report.snapshot_items, 1);
        assert!(storage.load_snapshot_image(old).is_err());
        assert_eq!(files(&storage).len(), 2);

        drop(reader);
        storage.apply_retention(now).unwrap();
        assert_eq!(files(&storage).len(), 1);
        assert!(storage.load_snapshot_image(today).is_ok());
    }

    fn large_request(utc: i64, seed: usize) -> SnapshotStoreRequest {
        let pixels = (0..512 * 512 * 3)
            .map(|n| ((n * 997 + n / 379 + seed) ^ ((n * 37) >> 11)) as u8)
            .collect::<Vec<_>>();
        let mut webp = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut webp)
            .write_image(&pixels, 512, 512, image::ExtendedColorType::Rgb8)
            .unwrap();
        SnapshotStoreRequest {
            pixel_width: 512,
            pixel_height: 512,
            webp,
            ..request(utc, 0)
        }
    }

    #[test]
    fn capacity_cleanup_keeps_the_current_day_even_when_it_alone_exceeds_the_limit() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        let now = Local
            .with_ymd_and_hms(2026, 10, 11, 12, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis();
        let older = store_on_day(&storage, &request(now - 86_400_000, 10), "2026-10-10");
        let morning = large_request(now - 2000, 1);
        let noon = large_request(now - 1000, 2);
        let today = [
            store_on_day(&storage, &morning, "2026-10-11"),
            store_on_day(&storage, &noon, "2026-10-11"),
        ];
        let mut policy = storage.snapshot_policy().unwrap();
        policy.retention_days = None;
        policy.max_bytes = 1024 * 1024;
        storage.set_snapshot_policy(policy, now).unwrap();
        let outcome = storage.apply_snapshot_day_retention(now).unwrap();
        assert_eq!(outcome.snapshot_items, 1);
        assert!(storage.load_snapshot_image(older).is_err());
        for (id, image) in today.into_iter().zip([morning, noon]) {
            assert_eq!(storage.load_snapshot_image(id).unwrap().webp, image.webp);
        }
        assert!(storage.snapshot_live_bytes().unwrap() > policy.max_bytes);
    }

    #[test]
    fn a_damaged_day_container_is_preserved_without_blocking_open_or_other_days() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        let a = request(1_700_000_000_000, 10);
        let b = request(a.slot_started_utc_ms + 1000, 20);
        let damaged = store_on_day(&storage, &a, "2023-11-13");
        let intact = store_on_day(&storage, &b, "2023-11-14");
        let mut policy = storage.snapshot_policy().unwrap();
        policy.retention_days = None;
        storage
            .set_snapshot_policy(policy, a.captured_at_utc_ms)
            .unwrap();
        let blob = storage
            .connection
            .query_row(
                "SELECT blob_id FROM snapshot_slots WHERE slot_id=?",
                [damaged],
                |r| r.get::<_, i64>(0),
            )
            .unwrap();
        let r = record(&storage.connection, blob, None).unwrap().unwrap();
        let path = storage.snapshot_directory().join(&r.name);
        drop(storage);
        let truncated = fs::read(&path).unwrap()[..r.committed_bytes as usize - 1].to_vec();
        fs::write(&path, &truncated).unwrap();

        let reopened = Storage::open(directory.path()).unwrap();
        assert!(reopened.load_snapshot_image(damaged).is_err());
        assert_eq!(reopened.load_snapshot_image(intact).unwrap().webp, b.webp);
        let c = request(a.slot_started_utc_ms + 2000, 30);
        let hash = snapshot_pixel_hash(&c.webp, c.pixel_width, c.pixel_height).unwrap();
        assert!(
            reopened
                .append_snapshot_on_day(&c, &hash, "2023-11-13")
                .is_err()
        );
        reopened
            .apply_snapshot_day_retention(a.captured_at_utc_ms + 3000)
            .unwrap();
        assert_eq!(fs::read(&path).unwrap(), truncated);
    }

    #[test]
    fn single_delete_compacts_the_day_scrubs_its_key_and_preserves_other_images() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        let a = request(1_700_000_000_000, 17);
        let b = request(a.slot_started_utc_ms + 1000, 89);
        let c = request(a.slot_started_utc_ms + 2000, 53);
        let first = storage.store_snapshot(&a).unwrap();
        let middle = storage.store_snapshot(&b).unwrap();
        let last = storage.store_snapshot(&c).unwrap();
        let blob = storage
            .connection
            .query_row(
                "SELECT blob_id FROM snapshot_slots WHERE slot_id=?",
                [middle],
                |r| r.get::<_, i64>(0),
            )
            .unwrap();
        let r = record(&storage.connection, blob, None).unwrap().unwrap();
        let old = storage.snapshot_directory().join(&r.name);
        let old_bytes = fs::read(&old).unwrap();
        let removed_cipher = &old_bytes[r.offset as usize..(r.offset + r.bytes) as usize];
        let size = storage.snapshot_live_bytes().unwrap();
        assert!(storage.delete_snapshot(middle).unwrap());
        assert!(!old.exists());
        assert_eq!(files(&storage).len(), 1);
        assert_eq!(storage.snapshot_live_bytes().unwrap(), size - r.bytes);
        let current = fs::read(&files(&storage)[0]).unwrap();
        assert!(
            !current
                .windows(removed_cipher.len())
                .any(|w| w == removed_cipher)
        );
        let pages = storage.connection.serialize("main").unwrap();
        assert!(!pages.windows(r.key.len()).any(|w| w == r.key.as_slice()));
        assert_eq!(
            fs::metadata(super::super::wal_path(&storage.database_path)).map_or(0, |m| m.len()),
            0
        );
        assert_eq!(storage.load_snapshot_image(first).unwrap().webp, a.webp);
        assert_eq!(storage.load_snapshot_image(last).unwrap().webp, c.webp);
        assert!(!storage.delete_snapshot(middle).unwrap());
        drop(storage);
        let reopened = Storage::open(directory.path()).unwrap();
        assert_eq!(reopened.load_snapshot_image(last).unwrap().webp, c.webp);
    }

    #[test]
    fn failed_append_rolls_back_metadata_and_file_tail() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        let a = request(1_700_000_000_000, 10);
        let id = storage.store_snapshot(&a).unwrap();
        let path = files(&storage)[0].clone();
        let before = fs::read(&path).unwrap();
        FAILURE.with(|f| f.set(Some("after_append")));
        assert!(
            storage
                .store_snapshot(&request(a.slot_started_utc_ms + 1000, 20))
                .is_err()
        );
        assert_eq!(fs::read(path).unwrap(), before);
        assert_eq!(
            storage
                .list_snapshot_slots(a.slot_started_utc_ms, a.slot_started_utc_ms + 2000, 10)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(storage.load_snapshot_image(id).unwrap().webp, a.webp);
    }

    #[test]
    fn interrupted_append_tail_and_precommit_replacement_are_removed_on_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        let a = request(1_700_000_000_000, 10);
        let id = storage.store_snapshot(&a).unwrap();
        let path = files(&storage)[0].clone();
        let before = fs::read(&path).unwrap();
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"incomplete encrypted append")
            .unwrap();
        let orphan = storage.snapshot_directory().join(container_name().unwrap());
        fs::write(&orphan, &before).unwrap();
        let unrelated = storage.snapshot_directory().join("notes.txt");
        fs::write(&unrelated, b"keep").unwrap();
        drop(storage);
        let reopened = Storage::open(directory.path()).unwrap();
        assert_eq!(fs::read(path).unwrap(), before);
        assert!(!orphan.exists());
        assert!(unrelated.exists());
        assert_eq!(reopened.load_snapshot_image(id).unwrap().webp, a.webp);
    }

    #[test]
    fn compaction_failure_preserves_original_image_and_postcommit_interruption_finishes_cleanup() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        let a = request(1_700_000_000_000, 10);
        let b = request(a.slot_started_utc_ms + 1000, 20);
        let first = storage.store_snapshot(&a).unwrap();
        let last = storage.store_snapshot(&b).unwrap();
        let old = files(&storage)[0].clone();
        let before = fs::read(&old).unwrap();
        FAILURE.with(|f| f.set(Some("after_compaction_file")));
        assert!(storage.delete_snapshot(first).is_err());
        assert_eq!(fs::read(&old).unwrap(), before);
        assert_eq!(storage.load_snapshot_image(first).unwrap().webp, a.webp);
        FAILURE.with(|f| f.set(Some("after_delete_commit")));
        assert!(matches!(
            storage.delete_snapshot(first),
            Err(StorageError::SnapshotCleanupPending(_))
        ));
        assert!(storage.load_snapshot_image(first).is_err());
        assert!(old.exists());
        drop(storage);
        let reopened = Storage::open(directory.path()).unwrap();
        assert!(!old.exists());
        assert_eq!(files(&reopened).len(), 1);
        assert_eq!(reopened.load_snapshot_image(last).unwrap().webp, b.webp);
    }

    #[test]
    fn legacy_migration_preserves_ids_encoded_bytes_settings_and_cross_day_references() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        let midnight = Local
            .with_ymd_and_hms(2026, 10, 11, 0, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis();
        let a = request(midnight - 1000, 10);
        let b = request(midnight + 1000, 10);
        let c = request(midnight + 2000, 20);
        let ids = [
            storage.store_snapshot(&a).unwrap(),
            storage.store_snapshot(&b).unwrap(),
            storage.store_snapshot(&c).unwrap(),
        ];
        let mut policy = storage.snapshot_policy().unwrap();
        policy.enabled = false;
        policy.interval_minutes = 15;
        policy.retention_days = None;
        storage.set_snapshot_policy(policy, midnight).unwrap();
        let policy = storage.snapshot_policy().unwrap();
        let originals = make_legacy(&storage);
        drop(storage);
        let migrated = Storage::open(directory.path()).unwrap();
        assert_eq!(migrated.schema_version().unwrap(), 13);
        assert_eq!(migrated.snapshot_policy().unwrap(), policy);
        assert_eq!(files(&migrated).len(), 2);
        assert!(originals.iter().all(|(p, _)| !p.exists()));
        for (id, image) in ids.into_iter().zip([a, b, c]) {
            assert_eq!(migrated.load_snapshot_image(id).unwrap().webp, image.webp);
        }
        migrated.verify_integrity().unwrap();
        migrated.verify_snapshot_containers().unwrap();
    }

    #[test]
    fn interrupted_legacy_migration_keeps_every_original_and_retries_without_duplicates() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        let a = request(1_700_000_000_000, 10);
        let b = request(a.slot_started_utc_ms + 86_400_000, 20);
        let ids = [
            storage.store_snapshot(&a).unwrap(),
            storage.store_snapshot(&b).unwrap(),
        ];
        let originals = make_legacy(&storage);
        drop(storage);
        FAILURE.with(|f| f.set(Some("after_migration_day")));
        drop(Storage::open(directory.path()).unwrap());
        for (path, bytes) in &originals {
            assert_eq!(fs::read(path).unwrap(), *bytes);
        }
        let recovered = Storage::open(directory.path()).unwrap();
        assert_eq!(files(&recovered).len(), 2);
        for (id, image) in ids.into_iter().zip([a, b]) {
            assert_eq!(recovered.load_snapshot_image(id).unwrap().webp, image.webp);
        }
    }

    #[test]
    fn corrupt_legacy_ciphertext_stops_migration_without_blocking_open() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        let a = request(1_700_000_000_000, 10);
        let id = storage.store_snapshot(&a).unwrap();
        let originals = make_legacy(&storage);
        drop(storage);
        let mut corrupt = originals[0].1.clone();
        corrupt[25] ^= 1;
        fs::write(&originals[0].0, &corrupt).unwrap();
        let opened = Storage::open(directory.path()).unwrap();
        assert!(opened.load_snapshot_image(id).is_err());
        opened
            .apply_snapshot_day_retention(a.captured_at_utc_ms)
            .unwrap();
        assert_eq!(fs::read(&originals[0].0).unwrap(), corrupt);
        drop(opened);
        fs::write(&originals[0].0, &originals[0].1).unwrap();
        let repaired = Storage::open(directory.path()).unwrap();
        assert_eq!(repaired.load_snapshot_image(id).unwrap().webp, a.webp);
    }

    #[test]
    fn portable_restore_keeps_local_days_strips_container_keys_and_replaces_old_containers() {
        let source_dir = tempfile::tempdir().unwrap();
        let source = Storage::open(source_dir.path()).unwrap();
        let a = request(1_700_000_000_000, 10);
        let b = request(a.slot_started_utc_ms + 1000, 20);
        let first = store_on_day(&source, &a, "2023-11-13");
        let second = store_on_day(&source, &b, "2023-11-14");
        let backup_dir = tempfile::tempdir().unwrap();
        let backup = backup_dir.path().join("snapshot.zip");
        source
            .export_backup(
                &backup,
                BackupOptions {
                    include_snapshots: true,
                    password: Some("test password"),
                },
            )
            .unwrap();
        let mut archive = zip::ZipArchive::new(File::open(&backup).unwrap()).unwrap();
        let mut db = Vec::new();
        archive
            .by_name_decrypt("database.sqlite3", b"test password")
            .unwrap()
            .read_to_end(&mut db)
            .unwrap();
        let mut plain = Connection::open_in_memory().unwrap();
        plain
            .deserialize_read_exact("main", db.as_slice(), db.len(), false)
            .unwrap();
        assert_eq!(
            plain
                .query_row("SELECT COUNT(*) FROM snapshot_container_records", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        let target_dir = tempfile::tempdir().unwrap();
        let target = Storage::open(target_dir.path()).unwrap();
        target
            .store_snapshot(&request(a.slot_started_utc_ms - 1000, 3))
            .unwrap();
        let old = files(&target);
        let prepared =
            Storage::prepare_restore(&backup, Some("test password"), target_dir.path()).unwrap();
        target.replace_from_backup(prepared, true).unwrap();
        assert!(old.iter().all(|p| !p.exists()));
        assert_eq!(files(&target).len(), 2);
        assert_eq!(target.load_snapshot_image(first).unwrap().webp, a.webp);
        assert_eq!(target.load_snapshot_image(second).unwrap().webp, b.webp);
        assert_eq!(
            target
                .connection
                .query_row(
                    "SELECT local_day FROM snapshot_slot_days WHERE slot_id=?",
                    [first],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
            "2023-11-13"
        );
        let without = backup_dir.path().join("without.zip");
        source
            .export_backup(
                &without,
                BackupOptions {
                    include_snapshots: false,
                    password: None,
                },
            )
            .unwrap();
        let prepared = Storage::prepare_restore(&without, None, target_dir.path()).unwrap();
        target.replace_from_backup(prepared, true).unwrap();
        assert!(files(&target).is_empty());
        assert!(target.load_snapshot_image(first).is_err());
    }

    #[test]
    fn directory_relocation_and_recovery_authenticate_every_day_container() {
        let directory = tempfile::tempdir().unwrap();
        let mut storage = Storage::open(directory.path()).unwrap();
        let a = request(1_700_000_000_000, 10);
        let b = request(a.slot_started_utc_ms + 86_400_000, 20);
        let first = storage.store_snapshot(&a).unwrap();
        let second = storage.store_snapshot(&b).unwrap();
        storage.checkpoint().unwrap();
        let originals = files(&storage)
            .into_iter()
            .map(|p| {
                let bytes = fs::read(&p).unwrap();
                (p, bytes)
            })
            .collect::<Vec<_>>();
        let recovered_dir = tempfile::tempdir().unwrap();
        let (recovered, report) =
            Storage::recover_to_new_directory(directory.path(), recovered_dir.path()).unwrap();
        assert_eq!(report.unavailable_images, 0);
        assert_eq!(files(&recovered).len(), 2);
        assert_eq!(recovered.load_snapshot_image(first).unwrap().webp, a.webp);
        assert_eq!(recovered.load_snapshot_image(second).unwrap().webp, b.webp);
        for (path, bytes) in originals {
            assert_eq!(fs::read(path).unwrap(), bytes);
        }
        let moved = tempfile::tempdir().unwrap();
        storage
            .relocate(moved.path(), |_| Ok(()), |_, _, _| {})
            .unwrap();
        assert_eq!(files(&storage).len(), 2);
        assert_eq!(storage.load_snapshot_image(second).unwrap().webp, b.webp);
        assert!(fs::read_dir(directory.path().join("snapshots")).is_err());
    }

    #[test]
    fn interrupted_migration_preserves_all_day_assignments_before_copying() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        let a = request(1_700_000_000_000, 10);
        let b = request(a.slot_started_utc_ms + 86_400_000, 20);
        let first = storage.store_snapshot(&a).unwrap();
        let second = storage.store_snapshot(&b).unwrap();
        let originals = make_legacy(&storage);
        drop(storage);
        FAILURE.with(|f| f.set(Some("after_migration_dates")));
        drop(Storage::open(directory.path()).unwrap());
        for (path, bytes) in &originals {
            assert_eq!(fs::read(path).unwrap(), *bytes);
        }
        let key = load_key(&directory.path().join(super::super::KEY_FILE)).unwrap();
        let db = super::super::open_encrypted(
            &directory.path().join(super::super::DATABASE_FILE),
            &key,
            true,
        )
        .unwrap();
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM snapshot_slot_days", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM snapshot_containers", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        drop(db);
        let recovered = Storage::open(directory.path()).unwrap();
        assert_eq!(recovered.load_snapshot_image(first).unwrap().webp, a.webp);
        assert_eq!(recovered.load_snapshot_image(second).unwrap().webp, b.webp);
    }

    #[test]
    fn authenticated_records_detect_ciphertext_tampering_without_hiding_other_images() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Storage::open(directory.path()).unwrap();
        let a = request(1_700_000_000_000, 10);
        let b = request(a.slot_started_utc_ms + 1000, 20);
        let first = storage.store_snapshot(&a).unwrap();
        let second = storage.store_snapshot(&b).unwrap();
        let blob = storage
            .connection
            .query_row(
                "SELECT blob_id FROM snapshot_slots WHERE slot_id=?",
                [first],
                |r| r.get::<_, i64>(0),
            )
            .unwrap();
        let r = record(&storage.connection, blob, None).unwrap().unwrap();
        let path = storage.snapshot_directory().join(r.name);
        let mut content = fs::read(&path).unwrap();
        content[(r.offset + 25) as usize] ^= 1;
        fs::write(path, content).unwrap();
        assert!(storage.load_snapshot_image(first).is_err());
        assert!(storage.verify_snapshot_containers().is_err());
        assert_eq!(storage.load_snapshot_image(second).unwrap().webp, b.webp);
    }

    #[test]
    fn schema_twelve_portable_snapshots_restore_into_daily_containers() {
        let directory = tempfile::tempdir().unwrap();
        let source = Storage::open(directory.path()).unwrap();
        let a = request(1_700_000_000_000, 10);
        let b = request(a.slot_started_utc_ms + 86_400_000, 20);
        let first = source.store_snapshot(&a).unwrap();
        let second = source.store_snapshot(&b).unwrap();
        let blobs = [first, second].map(|id| {
            source
                .connection
                .query_row(
                    "SELECT blob_id FROM snapshot_slots WHERE slot_id=?",
                    [id],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap()
        });
        make_legacy(&source);
        source
            .connection
            .execute_batch("ATTACH DATABASE ':memory:' AS legacy KEY '';")
            .unwrap();
        source
            .connection
            .query_row("SELECT sqlcipher_export('legacy')", [], |_| Ok(()))
            .unwrap();
        source
            .connection
            .pragma_update(Some("legacy"), "user_version", 12)
            .unwrap();
        super::super::portable::sanitize(&source.connection, "legacy", true).unwrap();
        let db = source.connection.serialize("legacy").unwrap().to_vec();
        source
            .connection
            .execute_batch("DETACH DATABASE legacy")
            .unwrap();
        let backup_directory = tempfile::tempdir().unwrap();
        let backup = backup_directory.path().join("old.zip");
        let mut zip = zip::ZipWriter::new(File::create(&backup).unwrap());
        let mut manifest = crate::BackupInfo {
            format_version: 1,
            schema_version: 12,
            created_utc_ms: a.captured_at_utc_ms,
            includes_snapshots: true,
            files: Vec::new(),
        };
        for (name, bytes) in [
            ("database.sqlite3".to_owned(), db),
            (format!("snapshots/{}.webp", blobs[0]), a.webp.clone()),
            (format!("snapshots/{}.webp", blobs[1]), b.webp.clone()),
        ] {
            zip.start_file(&name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(&bytes).unwrap();
            manifest.files.push(super::super::portable::BackupEntry {
                name,
                bytes: bytes.len() as u64,
                sha256: hex::encode(sha256(&bytes).unwrap()),
            });
        }
        zip.start_file("manifest.json", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(&serde_json::to_vec(&manifest).unwrap())
            .unwrap();
        zip.finish().unwrap().sync_all().unwrap();
        let target_directory = tempfile::tempdir().unwrap();
        let target = Storage::open(target_directory.path()).unwrap();
        let prepared = Storage::prepare_restore(&backup, None, target_directory.path()).unwrap();
        target.replace_from_backup(prepared, false).unwrap();
        assert_eq!(target.schema_version().unwrap(), 13);
        assert_eq!(files(&target).len(), 2);
        assert_eq!(target.load_snapshot_image(first).unwrap().webp, a.webp);
        assert_eq!(target.load_snapshot_image(second).unwrap().webp, b.webp);
    }
}
