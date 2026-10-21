//! Repair operates on an encrypted working copy and only publishes a new directory.
//! Original database, WAL, keys and images are never rewritten or deleted here.
use super::*;
use portable::{quoted, table_names};
use rusqlite::{params_from_iter, types::Value};

#[derive(Debug, Default)]
pub struct RecoveryReport {
    pub copied_rows: u64,
    pub discarded_rows: u64,
    pub unreadable_tables: u64,
    pub unavailable_images: u64,
}

fn invalid(text: impl Into<String>) -> StorageError {
    StorageError::Integrity(text.into())
}

impl Storage {
    pub fn migration_notice(directory: &Path) -> Result<Option<String>> {
        let database = directory.join(DATABASE_FILE);
        let backup = directory.join(BACKUP_FILE);
        if !database.exists() && !backup.exists() {
            return Ok(None);
        }
        let key = load_key(&directory.join(KEY_FILE))?;
        // A previous interrupted upgrade must roll back before deciding whether
        // another schema upgrade needs consent in the recovery window.
        recover_interrupted_migration(&database, &backup, &key)?;
        let current = inspect_database(&database, &key)?;
        if current > LATEST_SCHEMA_VERSION {
            return Err(StorageError::SchemaTooNew {
                found: current,
                supported: LATEST_SCHEMA_VERSION,
            });
        }
        Ok((9..12).contains(&current).then(|| format!(
            "需要将数据结构从版本 {current} 升级到 {LATEST_SCHEMA_VERSION}。这会重建快照与缺失记录的约束，保留全部记录。升级期间会锁定数据库，并建立临时加密安全副本；检查成功后删除副本，失败自动回滚。")))
    }

    pub fn recover_to_new_directory(
        source: &Path,
        destination: &Path,
    ) -> Result<(Self, RecoveryReport)> {
        let source = relocation::validate_fixed_directory(source)?;
        let destination = relocation::validate_fixed_directory(destination)?;
        if source == destination
            || destination.starts_with(&source)
            || source.starts_with(&destination)
        {
            return Err(invalid("恢复目录必须与原数据目录分开"));
        }
        if destination.exists() && fs::read_dir(&destination)?.next().is_some() {
            return Err(invalid("请选择一个新的空恢复目录"));
        }
        let parent = destination
            .parent()
            .ok_or_else(|| invalid("恢复目录无效"))?;
        fs::create_dir_all(parent)?;
        let working = tempfile::Builder::new()
            .prefix("timelens-recovery-read-")
            .tempdir_in(parent)?;
        for name in [DATABASE_FILE, KEY_FILE, "timelens.sqlite3-wal"] {
            let original = source.join(name);
            if original.exists() {
                copy_and_sync(&original, &working.path().join(name))?;
            }
        }
        let original_key = load_key(&working.path().join(KEY_FILE))?;
        let original = open_encrypted(&working.path().join(DATABASE_FILE), &original_key, false)?;
        original.execute_batch("PRAGMA trusted_schema=OFF; PRAGMA query_only=ON;")?;
        let version = schema_version(&original)?;
        if version < 1 {
            return Err(invalid("无法读取原数据库的数据结构"));
        }
        if version > LATEST_SCHEMA_VERSION {
            return Err(StorageError::SchemaTooNew {
                found: version,
                supported: LATEST_SCHEMA_VERSION,
            });
        }
        let stage = tempfile::Builder::new()
            .prefix("timelens-recovered-")
            .tempdir_in(parent)?;
        let key_path = stage.path().join(KEY_FILE);
        let key = load_or_create_key(&key_path)?;
        let database = stage.path().join(DATABASE_FILE);
        let migrations = MIGRATIONS
            .iter()
            .take_while(|m| m.version <= version)
            .count();
        let mut recovered = migrate_database(
            &database,
            &stage.path().join(BACKUP_FILE),
            &key,
            &MIGRATIONS[..migrations],
        )?;
        recovered.execute_batch("PRAGMA foreign_keys=OFF;")?;
        let mut report = RecoveryReport::default();
        {
            let tx = recovered.transaction_with_behavior(TransactionBehavior::Exclusive)?;
            let tables = table_names(&tx)?;
            for name in &tables {
                // Column names and tables come exclusively from our known schema.
                // No SQL stored in a damaged or foreign database is executed.
                let columns = tx
                    .prepare(&format!("PRAGMA table_info({})", quoted(name)))?
                    .query_map([], |r| r.get::<_, String>(1))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let select = format!(
                    "SELECT {} FROM {}",
                    columns
                        .iter()
                        .map(|c| quoted(c))
                        .collect::<Vec<_>>()
                        .join(","),
                    quoted(name)
                );
                let mut source_rows = match original.prepare(&select) {
                    Ok(s) => s,
                    Err(_) => {
                        report.unreadable_tables += 1;
                        continue;
                    }
                };
                let mut rows = match source_rows.query([]) {
                    Ok(r) => r,
                    Err(_) => {
                        report.unreadable_tables += 1;
                        continue;
                    }
                };
                let insert = format!(
                    "INSERT OR REPLACE INTO {} ({}) VALUES({})",
                    quoted(name),
                    columns
                        .iter()
                        .map(|c| quoted(c))
                        .collect::<Vec<_>>()
                        .join(","),
                    vec!["?"; columns.len()].join(",")
                );
                let mut insert = tx.prepare(&insert)?;
                loop {
                    let row = match rows.next() {
                        Ok(Some(row)) => row,
                        Ok(None) => break,
                        Err(_) => {
                            report.unreadable_tables += 1;
                            break;
                        }
                    };
                    let values = (0..columns.len())
                        .map(|i| row.get::<_, Value>(i))
                        .collect::<rusqlite::Result<Vec<_>>>();
                    match values.and_then(|v| insert.execute(params_from_iter(v))) {
                        Ok(n) => report.copied_rows += n as u64,
                        Err(_) => report.discarded_rows += 1,
                    }
                }
            }
            // A recovered child row without its parent is not a valid fact. Remove
            // only such rows, iterating through cascades without inventing parents.
            for _ in 0..=tables.len() {
                let broken = tx
                    .prepare("PRAGMA foreign_key_check")?
                    .query_map([], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                if broken.is_empty() {
                    break;
                }
                let mut removed = 0;
                for (table, row) in broken {
                    if !tables.contains(&table) {
                        return Err(invalid("恢复引用包含未知表"));
                    }
                    let row = row.ok_or_else(|| invalid("无法定位损坏的引用记录"))?;
                    removed += tx.execute(
                        &format!("DELETE FROM {} WHERE rowid=?", quoted(&table)),
                        [row],
                    )?;
                }
                report.discarded_rows += removed as u64;
                if removed == 0 {
                    return Err(invalid("恢复引用无法修复"));
                }
            }
            tx.commit()?;
        }
        recovered.execute_batch("PRAGMA foreign_keys=ON; PRAGMA wal_checkpoint(TRUNCATE);")?;
        drop(recovered);
        let connection =
            migrate_database(&database, &stage.path().join(BACKUP_FILE), &key, MIGRATIONS)?;
        let repaired = Self {
            timeline_cache: Default::default(),
            snapshot_files_settled: Default::default(),
            cipher_version: cipher_version(&connection)?,
            connection: connection.into(),
            data_directory: stage.path().into(),
            control_directory: stage.path().into(),
            key_path,
            database_path: database,
        };
        fs::create_dir_all(stage.path().join("snapshots"))?;
        let blobs = repaired.connection.prepare("SELECT blob_id,file_name,content_sha256,plaintext_bytes,pixel_width,pixel_height FROM snapshot_blobs")?
            .query_map([],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,Vec<u8>>(2)?,r.get::<_,i64>(3)? as u64,r.get::<_,u32>(4)?,r.get::<_,u32>(5)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (id, name, hash, size, width, height) in blobs {
            let restore_image: Result<()> = (|| {
                if !relocation::owned_snapshot_name(&name)
                    || size > 4 * 1024 * 1024
                    || u64::from(width) * u64::from(height) > 33_554_432
                {
                    return Err(invalid("快照记录无效"));
                }
                let directory = relocation::validate_fixed_directory(&source.join("snapshots"))?;
                let hash: [u8; 32] = hash.try_into().map_err(|_| invalid("快照哈希无效"))?;
                // Each day keeps its own copy of a shared image, and any authenticated
                // copy restores it. The blob-wide read stays as the final attempt.
                let days = repaired.connection.prepare("SELECT MIN(s.slot_id) FROM snapshot_slots s JOIN snapshot_slot_days d USING(slot_id) WHERE s.blob_id=? GROUP BY d.local_day ORDER BY d.local_day")?
                    .query_map([id], |r| r.get::<_, i64>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let mut plain = Err(invalid("快照记录无效"));
                for slot in days.into_iter().map(Some).chain([None]) {
                    plain = snapshot_container::read_payload(
                        &repaired.connection,
                        &directory,
                        &source.join(KEY_FILE),
                        id,
                        slot,
                    );
                    if plain.is_ok() {
                        break;
                    }
                }
                // A stopped format upgrade may still keep the image in its old file.
                if plain.is_err() {
                    plain = snapshot_container::read_legacy_payload(
                        &repaired.connection,
                        &directory,
                        &source.join(KEY_FILE),
                        id,
                    )
                    .or(plain);
                }
                let plain = plain?;
                if plain.len() as u64 != size
                    || milestone3::snapshot_pixel_hash(&plain, width, height)? != hash
                {
                    return Err(invalid("快照像素无法校验"));
                }
                let name = milestone3::random_snapshot_file_name()?;
                let bytes = milestone3::encrypt_snapshot(&key, &plain, &hash)?;
                milestone3::write_new_file(&stage.path().join("snapshots").join(&name), &bytes)?;
                repaired.connection.execute(
                    "UPDATE snapshot_blobs SET file_name=?,encrypted_bytes=? WHERE blob_id=?",
                    params![name, bytes.len() as i64, id],
                )?;
                Ok(())
            })();
            if restore_image.is_err() {
                repaired.connection.execute("UPDATE snapshot_slots SET blob_id=NULL,result='missing',missing_reason='recovery_corruption' WHERE blob_id=?",[id])?;
                repaired
                    .connection
                    .execute("DELETE FROM snapshot_blobs WHERE blob_id=?", [id])?;
                report.unavailable_images += 1;
            }
        }
        repaired.connection.execute_batch("DELETE FROM snapshot_container_records; DELETE FROM snapshot_containers; DELETE FROM snapshot_slot_days WHERE slot_id IN(SELECT slot_id FROM snapshot_slots WHERE result<>'success');")?;
        repaired.reconcile_snapshot_files()?;
        repaired.verify_snapshot_containers()?;
        portable::sanitize(&repaired.connection, "main", true)?;
        let now = unix_time_ms();
        let earliest: Option<i64> = repaired.connection.query_row(
            "SELECT MIN(first_observed_utc_ms) FROM collector_runs",
            [],
            |r| r.get(0),
        )?;
        for category in ["activity", "input"] {
            repaired.connection.execute("INSERT INTO data_availability(data_class,status,started_utc_ms,ended_utc_ms,reason,item_count) VALUES(?,'monitoring_gap',?,?,'recovery_corruption',?)",params![category,earliest.unwrap_or(now).min(now),now,report.discarded_rows as i64])?;
        }
        // Exclusions may have been on an unreadable page. Review them before any
        // collection resumes.
        let mut policy = repaired.collection_policy()?;
        policy.paused = true;
        repaired.set_collection_policy(policy)?;
        let mut snapshots = repaired.snapshot_policy()?;
        snapshots.enabled = false;
        repaired.set_snapshot_policy(snapshots, now)?;
        repaired.verify_integrity()?;
        repaired.checkpoint()?;
        drop(repaired);
        if destination.exists() {
            fs::remove_dir(&destination)?;
        }
        fs::rename(stage.path(), &destination)?;
        Ok((Storage::open(&destination)?, report))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn any_sqlite_corruption_error_latches_read_only_before_next_operation() {
        let (source, s) = fixtures::storage();
        let damaged = source.path().join("synthetic-damaged.sqlite3");
        fs::write(&damaged, b"not a database").unwrap();
        assert!(
            s.connection
                .execute(
                    "ATTACH DATABASE ?1 AS damaged",
                    [damaged.to_string_lossy().as_ref()]
                )
                .is_err()
        );
        assert!(s.is_quarantined());
        assert!(
            s.connection
                .execute("DELETE FROM local_reports", [])
                .is_err()
        );
        assert!(s.ensure_writable().is_err());
        s.timeline_snapshot(1000, 6000).unwrap();
    }
    #[test]
    fn recovery_keeps_original_and_removes_orphans_without_inventing_parents() {
        let (source, s) = fixtures::storage();
        s.connection
            .execute_batch(
                "PRAGMA foreign_keys=OFF; DELETE FROM window_instances; PRAGMA foreign_keys=ON;",
            )
            .unwrap();
        s.checkpoint().unwrap();
        let before = fs::read(s.database_path()).unwrap();
        let key = fs::read(source.path().join(KEY_FILE)).unwrap();
        drop(s);
        let parent = tempfile::tempdir().unwrap();
        let dest = parent.path().join("recovered");
        let (repaired, report) = Storage::recover_to_new_directory(source.path(), &dest).unwrap();
        assert!(report.discarded_rows > 0);
        repaired.verify_integrity().unwrap();
        assert!(repaired.collection_policy().unwrap().paused);
        assert!(!repaired.snapshot_policy().unwrap().enabled);
        assert_eq!(fs::read(source.path().join(DATABASE_FILE)).unwrap(), before);
        assert_eq!(fs::read(source.path().join(KEY_FILE)).unwrap(), key);
        assert_ne!(fs::read(dest.join(KEY_FILE)).unwrap(), key);
        assert_eq!(
            repaired
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM data_availability WHERE reason='recovery_corruption'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            2
        );
    }
    #[test]
    fn recovery_restores_an_image_from_a_later_day_when_the_first_copy_is_damaged() {
        use snapshot_container::tests::{request, store_on_day};
        let (source, s) = fixtures::storage();
        let a = request(1_700_000_000_000, 10);
        let first = store_on_day(&s, &a, "2023-11-14");
        let mut later = a.clone();
        later.slot_started_utc_ms += 86_400_000;
        later.captured_at_utc_ms += 86_400_000;
        let second = store_on_day(&s, &later, "2023-11-15");
        let name: String = s
            .connection
            .query_row(
                "SELECT file_name FROM snapshot_containers WHERE local_day='2023-11-14'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        s.checkpoint().unwrap();
        drop(s);
        let damaged = source.path().join("snapshots").join(name);
        let mut bytes = fs::read(&damaged).unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        fs::write(&damaged, bytes).unwrap();
        let parent = tempfile::tempdir().unwrap();
        let (repaired, report) =
            Storage::recover_to_new_directory(source.path(), &parent.path().join("recovered"))
                .unwrap();
        assert_eq!(report.unavailable_images, 0);
        assert_eq!(repaired.load_snapshot_image(first).unwrap().webp, a.webp);
        assert_eq!(repaired.load_snapshot_image(second).unwrap().webp, a.webp);
    }
    #[test]
    fn unreadable_header_does_not_replace_original_or_publish_target() {
        let (source, s) = fixtures::storage();
        drop(s);
        let database = source.path().join(DATABASE_FILE);
        fs::write(&database, b"damaged encrypted database").unwrap();
        let parent = tempfile::tempdir().unwrap();
        let dest = parent.path().join("new");
        assert!(Storage::recover_to_new_directory(source.path(), &dest).is_err());
        assert_eq!(fs::read(&database).unwrap(), b"damaged encrypted database");
        assert!(!dest.exists());
    }

    #[test]
    fn startup_recovers_interrupted_upgrade_before_inspecting_migration_notice() {
        let (source, s) = fixtures::storage();
        s.checkpoint().unwrap();
        drop(s);
        let db = source.path().join(DATABASE_FILE);
        let backup = source.path().join(BACKUP_FILE);
        fs::copy(&db, &backup).unwrap();
        fs::write(&db, b"interrupted upgrade").unwrap();
        assert!(Storage::migration_notice(source.path()).unwrap().is_none());
        assert!(!backup.exists());
        Storage::open(source.path())
            .unwrap()
            .verify_integrity()
            .unwrap();
    }
}
