//! Portable archives never contain a DPAPI key, credential, runtime spool or log.
//! Plain SQLite exists only in memory. Replacement commits in one SQLite transaction;
//! newly encrypted blobs are installed first and unreferenced blobs are reconciled later.
use super::*;
use milestone3::{encrypt_snapshot, random_snapshot_file_name, sha256, snapshot_pixel_hash};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Seek},
};
use zip::{AesMode, CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

const FORMAT: u32 = 1;
const MAX_DB: u64 = 512 * 1024 * 1024;
const MAX_IMAGE: u64 = 4 * 1024 * 1024;
const MAX_TOTAL: u64 = 16 * 1024 * 1024 * 1024;
const MAX_FILES: usize = 100_000;

pub struct BackupOptions<'a> {
    pub include_snapshots: bool,
    pub password: Option<&'a str>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupInfo {
    pub format_version: u32,
    pub schema_version: i64,
    pub created_utc_ms: i64,
    pub includes_snapshots: bool,
    pub files: Vec<BackupEntry>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupEntry {
    pub name: String,
    pub bytes: u64,
    pub sha256: String,
}
pub struct PreparedRestore {
    storage: Storage,
    _directory: tempfile::TempDir,
    pub info: BackupInfo,
}
pub enum ExportFormat {
    Csv,
    Json,
}

fn invalid(message: impl Into<String>) -> StorageError {
    StorageError::Integrity(message.into())
}
fn json<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|e| invalid(e.to_string()))
}
fn schema_objects(c: &Connection) -> Result<Vec<(String, String, Option<String>)>> {
    Ok(c.prepare(
        "SELECT type,name,sql FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%' ORDER BY type,name",
    )?
    .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
    .collect::<rusqlite::Result<_>>()?)
}
pub(crate) fn table_names(c: &Connection) -> Result<Vec<String>> {
    Ok(c.prepare("SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")?.query_map([],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?)
}
pub(crate) fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}
fn full_check(c: &Connection) -> Result<()> {
    let errors = c
        .prepare("PRAGMA integrity_check")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if errors != ["ok"] {
        return Err(invalid("数据库完整性检查失败"));
    }
    if c.prepare("PRAGMA foreign_key_check")?.exists([])? {
        return Err(invalid("数据库引用完整性检查失败"));
    }
    Ok(())
}

fn validate_schema(c: &Connection, version: i64) -> Result<()> {
    if version > LATEST_SCHEMA_VERSION {
        return Err(StorageError::SchemaTooNew {
            found: version,
            supported: LATEST_SCHEMA_VERSION,
        });
    }
    if version < 1 {
        return Err(invalid("备份数据库版本无效"));
    }
    let expected = Connection::open_in_memory()?;
    for migration in MIGRATIONS.iter().filter(|m| m.version <= version) {
        expected.execute_batch(migration.sql)?;
    }
    // Reject virtual tables, views, triggers and unexpected schema SQL before reading
    // product rows. Imported SQL is never executed as migration or configuration.
    let normalize = |rows: Vec<(String, String, Option<String>)>| {
        rows.into_iter()
            .map(|(t, n, s)| {
                (
                    t,
                    n,
                    s.map(|s| s.split_whitespace().collect::<Vec<_>>().join(" ")),
                )
            })
            .collect::<Vec<_>>()
    };
    if normalize(schema_objects(c)?) != normalize(schema_objects(&expected)?) {
        return Err(invalid("备份包含未知或被修改的数据库结构"));
    }
    full_check(c)
}

pub(crate) fn sanitize(c: &Connection, schema: &str, has_snapshots: bool) -> Result<()> {
    let prefix = quoted(schema);
    c.execute_batch(&format!(
        "UPDATE {prefix}.storage_checks SET last_full_check_utc_ms=0;"
    ))?;
    c.execute_batch(&format!("DELETE FROM {prefix}.runtime_health; DELETE FROM {prefix}.ai_image_authorizations; UPDATE {prefix}.ai_jobs SET state='canceled',error_json=NULL WHERE state IN ('queued','running');"))?;
    let profiles = c
        .prepare(&format!("SELECT id,config_json FROM {prefix}.ai_profiles"))?
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (id, raw) in profiles {
        let mut p: timelens_ai::ProviderProfile =
            serde_json::from_str(&raw).map_err(|_| invalid("备份提供商设置无效"))?;
        p.tested_revision = None;
        p.credential_revision = 0;
        c.execute(
            &format!("UPDATE {prefix}.ai_profiles SET config_json=? WHERE id=?"),
            params![
                String::from_utf8(json(&p)?).map_err(|_| invalid("配置编码无效"))?,
                id
            ],
        )?;
    }
    let schedules = c
        .prepare(&format!(
            "SELECT id,schedule_json FROM {prefix}.ai_schedules"
        ))?
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (id, raw) in schedules {
        let mut p: timelens_ai::schedule::Schedule =
            serde_json::from_str(&raw).map_err(|_| invalid("备份计划设置无效"))?;
        p.enabled = false;
        p.paused_reason = Some("恢复后请重新配置凭据、测试连接并启用计划".into());
        c.execute(
            &format!("UPDATE {prefix}.ai_schedules SET schedule_json=? WHERE id=?"),
            params![
                String::from_utf8(json(&p)?).map_err(|_| invalid("配置编码无效"))?,
                id
            ],
        )?;
    }
    // Device-local credential revision stamps in historical public snapshots carry no
    // credentials, but must never authorize a restored request against a local vault.
    for (table, column, key) in [
        ("ai_jobs", "spec_json", "job_id"),
        ("ai_versions", "snapshot_json", "version_id"),
    ] {
        let values = c
            .prepare(&format!("SELECT {key},{column} FROM {prefix}.{table}"))?
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (id, raw) in values {
            let mut value: serde_json::Value =
                serde_json::from_str(&raw).map_err(|_| invalid("AI 快照格式无效"))?;
            if let Some(provider) = value.get_mut("provider") {
                provider["tested_revision"] = serde_json::Value::Null;
                provider["credential_revision"] = 0.into();
            }
            c.execute(
                &format!("UPDATE {prefix}.{table} SET {column}=? WHERE {key}=?"),
                params![value.to_string(), id],
            )?;
        }
    }
    if !has_snapshots {
        c.execute_batch(&format!("UPDATE {prefix}.snapshot_slots SET blob_id=NULL,result='missing',missing_reason='backup_omitted' WHERE blob_id IS NOT NULL; DELETE FROM {prefix}.snapshot_blobs;"))?;
    }
    Ok(())
}

impl Storage {
    pub fn data_directory(&self) -> &Path {
        &self.data_directory
    }
    pub fn verify_integrity(&self) -> Result<()> {
        let result = full_check(&self.connection);
        if result.is_err() {
            self.connection.quarantine();
        }
        result
    }
    pub fn is_quarantined(&self) -> bool {
        self.connection.is_quarantined()
    }
    pub fn periodic_integrity_check(&self, now: i64) -> Result<()> {
        self.ensure_writable()?;
        let previous: i64 = self.connection.query_row(
            "SELECT last_full_check_utc_ms FROM storage_checks WHERE id=1",
            [],
            |r| r.get(0),
        )?;
        if now.saturating_sub(previous) >= 86_400_000 {
            self.verify_integrity()?;
            self.connection.execute(
                "UPDATE storage_checks SET last_full_check_utc_ms=? WHERE id=1",
                [now],
            )?;
        }
        Ok(())
    }
    pub(crate) fn ensure_writable(&self) -> Result<()> {
        if self.is_quarantined() {
            return Err(invalid(
                "检测到数据损坏，已停止写入。请进入恢复界面，保留原件后恢复到新目录。",
            ));
        }
        Ok(())
    }
    pub fn export_backup(
        &self,
        destination: &Path,
        options: BackupOptions<'_>,
    ) -> Result<BackupInfo> {
        if options
            .password
            .is_some_and(|p| p.is_empty() || p.len() > 1024)
        {
            return Err(invalid("独立密码不能为空或超过 1024 字节"));
        }
        if destination.exists() {
            return Err(invalid("目标文件已存在，请选择新文件名"));
        }
        let parent = destination
            .parent()
            .ok_or_else(|| invalid("备份路径无效"))?;
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        let mut archive = ZipWriter::new(temp.as_file_mut());
        let mut info = BackupInfo {
            format_version: FORMAT,
            schema_version: LATEST_SCHEMA_VERSION,
            created_utc_ms: unix_time_ms(),
            includes_snapshots: options.include_snapshots,
            files: vec![],
        };
        let mut add = |name: &str, bytes: &[u8]| -> Result<()> {
            let mut entry =
                SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
            if let Some(password) = options.password {
                entry = entry.with_aes_encryption(AesMode::Aes256, password);
            }
            archive.start_file(name, entry)?;
            archive.write_all(bytes)?;
            info.files.push(BackupEntry {
                name: name.into(),
                bytes: bytes.len() as u64,
                sha256: hex::encode(sha256(bytes)?),
            });
            Ok(())
        };
        self.connection.execute_batch(
            "ATTACH DATABASE ':memory:' AS portable KEY ''; PRAGMA portable.journal_mode=MEMORY;",
        )?;
        let build: Result<()> = (|| {
            self.connection
                .query_row("SELECT sqlcipher_export('portable')", [], |_| Ok(()))?;
            self.connection.pragma_update(
                Some("portable"),
                "user_version",
                LATEST_SCHEMA_VERSION,
            )?;
            sanitize(&self.connection, "portable", options.include_snapshots)?;
            if options.include_snapshots {
                let blobs=self.connection.prepare("SELECT blob_id,MIN(slot_id) FROM snapshot_slots WHERE blob_id IS NOT NULL GROUP BY blob_id")?.query_map([],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
                for (blob, slot) in blobs {
                    let bytes = Zeroizing::new(self.load_snapshot_image(slot)?.webp);
                    add(&format!("snapshots/{blob}.webp"), &bytes)?;
                }
            }
            let data = self.connection.serialize("portable")?;
            add("database.sqlite3", &data)?;
            Ok(())
        })();
        let detach = self.connection.execute_batch("DETACH DATABASE portable");
        build?;
        detach?;
        let manifest = json(&info)?;
        let mut entry =
            SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        if let Some(password) = options.password {
            entry = entry.with_aes_encryption(AesMode::Aes256, password);
        }
        archive.start_file("manifest.json", entry)?;
        archive.write_all(&manifest)?;
        archive.finish()?;
        temp.as_file().sync_all()?;
        temp.persist_noclobber(destination)
            .map_err(|e| StorageError::Io(e.error))?;
        Ok(info)
    }

    /// All validation happens before this returns. Stage contents are encrypted with
    /// a new DPAPI-protected key; the plaintext archive database stays in RAM.
    pub fn prepare_restore(
        archive_path: &Path,
        password: Option<&str>,
        stage_parent: &Path,
    ) -> Result<PreparedRestore> {
        let file = File::open(archive_path)?;
        let mut archive = ZipArchive::new(file)?;
        if archive.len() > MAX_FILES {
            return Err(invalid("ZIP 文件数量超出限制"));
        }
        let mut names = BTreeSet::new();
        let mut total = 0_u64;
        for index in 0..archive.len() {
            let entry = archive.by_index_raw(index)?;
            let name = entry.name();
            let allowed = name == "manifest.json"
                || name == "database.sqlite3"
                || name
                    .strip_prefix("snapshots/")
                    .and_then(|s| s.strip_suffix(".webp"))
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()));
            if !allowed || entry.is_dir() || entry.is_symlink() || !names.insert(name.to_string()) {
                return Err(invalid("ZIP 包含重复、越界或未知路径"));
            }
            let cap = if name == "database.sqlite3" {
                MAX_DB
            } else if name == "manifest.json" {
                32 * 1024 * 1024
            } else {
                MAX_IMAGE
            };
            total = total
                .checked_add(entry.size())
                .ok_or_else(|| invalid("ZIP 容量溢出"))?;
            if entry.size() > cap || total > MAX_TOTAL {
                return Err(invalid("ZIP 解压大小超出限制"));
            }
        }
        let manifest = read_entry(&mut archive, "manifest.json", password, 32 * 1024 * 1024)?;
        let info: BackupInfo =
            serde_json::from_slice(&manifest).map_err(|_| invalid("备份清单无效或密码错误"))?;
        if info.format_version != FORMAT {
            return Err(invalid("不兼容的备份格式版本"));
        }
        if info.schema_version > LATEST_SCHEMA_VERSION {
            return Err(StorageError::SchemaTooNew {
                found: info.schema_version,
                supported: LATEST_SCHEMA_VERSION,
            });
        }
        let expected: BTreeSet<_> = info
            .files
            .iter()
            .map(|f| f.name.clone())
            .chain(std::iter::once("manifest.json".into()))
            .collect();
        if expected != names || info.files.len() + 1 != names.len() {
            return Err(invalid("清单与 ZIP 内容不一致"));
        }
        let entries: BTreeMap<_, _> = info.files.iter().map(|e| (e.name.clone(), e)).collect();
        let db = verified_entry(&mut archive, "database.sqlite3", password, MAX_DB, &entries)?;
        let mut plain = Connection::open_in_memory()?;
        plain.deserialize_read_exact("main", db.as_slice(), db.len(), false)?;
        plain.execute_batch("PRAGMA trusted_schema=OFF; PRAGMA temp_store=MEMORY;")?;
        if schema_version(&plain)? != info.schema_version {
            return Err(invalid("数据库版本与备份清单不一致"));
        }
        validate_schema(&plain, info.schema_version)?;
        // Migrate only our known SQL in memory, then validate the complete result.
        for migration in MIGRATIONS
            .iter()
            .filter(|m| m.version > info.schema_version)
        {
            plain.execute_batch(migration.sql)?;
        }
        plain.pragma_update(None, "user_version", LATEST_SCHEMA_VERSION)?;
        sanitize(&plain, "main", info.includes_snapshots)?;
        let directory = tempfile::Builder::new()
            .prefix("timelens-restore-")
            .tempdir_in(stage_parent)?;
        let key = load_or_create_key(&directory.path().join(KEY_FILE))?;
        let database = directory.path().join(DATABASE_FILE);
        plain.execute(
            "ATTACH DATABASE ? AS staged KEY ?",
            params![
                database.to_string_lossy(),
                format!("x'{}'", hex::encode(&*key))
            ],
        )?;
        let export = plain.query_row("SELECT sqlcipher_export('staged')", [], |_| Ok(()));
        if export.is_ok() {
            plain.pragma_update(Some("staged"), "user_version", LATEST_SCHEMA_VERSION)?;
        }
        plain.execute_batch("DETACH DATABASE staged")?;
        export?;
        let storage = Storage {
            connection: open_encrypted(&database, &key, false)?.into(),
            data_directory: directory.path().into(),
            control_directory: directory.path().into(),
            key_path: directory.path().join(KEY_FILE),
            database_path: database,
            cipher_version: cipher_version(&plain)?,
        };
        let blobs=storage.connection.prepare("SELECT blob_id,content_sha256,plaintext_bytes,pixel_width,pixel_height FROM snapshot_blobs")?.query_map([],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,Vec<u8>>(1)?,r.get::<_,i64>(2)?,r.get::<_,u32>(3)?,r.get::<_,u32>(4)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        if entries.len() != blobs.len() + 1 {
            return Err(invalid("图片数量与数据库不一致"));
        }
        fs::create_dir_all(directory.path().join("snapshots"))?;
        for (id, hash, size, width, height) in blobs {
            if width > 16384 || height > 16384 || u64::from(width) * u64::from(height) > 33_554_432
            {
                return Err(invalid("快照像素大小超出限制"));
            }
            let bytes = verified_entry(
                &mut archive,
                &format!("snapshots/{id}.webp"),
                password,
                MAX_IMAGE,
                &entries,
            )?;
            if bytes.len() as i64 != size
                || snapshot_pixel_hash(&bytes, width, height)?.as_slice() != hash
            {
                return Err(invalid("快照像素校验失败"));
            }
            let hash: [u8; 32] = hash.try_into().map_err(|_| invalid("图片哈希无效"))?;
            let encrypted = encrypt_snapshot(&key, &bytes, &hash)?;
            let name = random_snapshot_file_name()?;
            milestone3::write_new_file(
                &directory.path().join("snapshots").join(&name),
                &encrypted,
            )?;
            storage.connection.execute(
                "UPDATE snapshot_blobs SET file_name=?,encrypted_bytes=? WHERE blob_id=?",
                params![name, encrypted.len() as i64, id],
            )?;
        }
        full_check(&storage.connection)?;
        storage.checkpoint()?;
        Ok(PreparedRestore {
            storage,
            _directory: directory,
            info,
        })
    }

    /// Caller must suspend the AI worker and collector, and hold the shared storage
    /// lock. No original database copy is made or retained.
    pub fn replace_from_backup(
        &self,
        prepared: PreparedRestore,
        confirm_replace: bool,
    ) -> Result<()> {
        self.ensure_writable()?;
        let populated:bool=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM collector_runs UNION ALL SELECT 1 FROM ai_versions UNION ALL SELECT 1 FROM snapshot_slots)",[],|r|r.get(0))?;
        if populated && !confirm_replace {
            return Err(invalid("替换非空数据集需要确认；可先主动导出安全备份"));
        }
        let key = load_key(&self.key_path)?;
        let stage_key = load_key(&prepared.storage.key_path)?;
        let blob_slots=prepared.storage.connection.prepare("SELECT blob_id,MIN(slot_id) FROM snapshot_slots WHERE blob_id IS NOT NULL GROUP BY blob_id")?.query_map([],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        fs::create_dir_all(self.data_directory.join("snapshots"))?;
        for (id, slot) in blob_slots {
            let image = prepared.storage.load_snapshot_image(slot)?;
            let bytes = Zeroizing::new(image.webp);
            let hash =
                snapshot_pixel_hash(&bytes, image.slot.pixel_width, image.slot.pixel_height)?;
            let encrypted = encrypt_snapshot(&key, &bytes, &hash)?;
            let name = random_snapshot_file_name()?;
            milestone3::write_new_file(
                &self.data_directory.join("snapshots").join(&name),
                &encrypted,
            )?;
            prepared.storage.connection.execute(
                "UPDATE snapshot_blobs SET file_name=?,encrypted_bytes=? WHERE blob_id=?",
                params![name, encrypted.len() as i64, id],
            )?;
        }
        prepared.storage.checkpoint()?;
        self.connection.execute(
            "ATTACH DATABASE ? AS restore_source KEY ?",
            params![
                prepared.storage.database_path.to_string_lossy(),
                format!("x'{}'", hex::encode(&*stage_key))
            ],
        )?;
        self.connection.execute_batch("PRAGMA foreign_keys=OFF;")?;
        let replace: Result<()> = (|| {
            let tx = Transaction::new_unchecked(&self.connection, TransactionBehavior::Exclusive)?;
            let tables = table_names(&tx)?;
            // Cascades are off only while the exclusive, all-table replacement runs.
            // Validate every foreign key explicitly before commit, including cycles.
            for name in &tables {
                tx.execute(&format!("DELETE FROM main.{}", quoted(name)), [])?;
            }
            for name in &tables {
                tx.execute(
                    &format!(
                        "INSERT INTO main.{0} SELECT * FROM restore_source.{0}",
                        quoted(name)
                    ),
                    [],
                )?;
            }
            full_check(&tx)?;
            tx.commit()?;
            Ok(())
        })();
        let detach = self
            .connection
            .execute_batch("PRAGMA foreign_keys=ON; DETACH DATABASE restore_source");
        if replace.is_err() {
            let _ = self.reconcile_snapshot_files();
        }
        replace?;
        detach?;
        self.checkpoint()?;
        self.reconcile_snapshot_files()?;
        self.sync_collection_policy()?;
        Ok(())
    }

    pub fn export_report(
        &self,
        report_id: i64,
        destination: &Path,
        format: ExportFormat,
        include_paths: bool,
    ) -> Result<()> {
        let report = self.load_local_report(report_id)?;
        let apps=report.applications.iter().map(|app|{
            let mut item=serde_json::json!({"name":app.display_name,"opened_ms":app.opened_ms,"displayed_ms":app.displayed_ms,"focused_ms":app.focused_ms,"background_ms":app.background_ms,"windows":app.window_count,"keyboard":app.keyboard_count,"left_clicks":app.left_click_count,"middle_clicks":app.middle_click_count,"right_clicks":app.right_click_count});
            if include_paths {let path=self.connection.query_row("SELECT executable_path FROM application_metadata_revisions WHERE application_identity=? ORDER BY effective_utc_ms DESC LIMIT 1",params![app.identity],|r|r.get::<_,String>(0)).optional()?.unwrap_or_default();item["executable_path"]=path.into();}Ok(item)
        }).collect::<Result<Vec<_>>>()?;
        let value = serde_json::json!({"format_version":1,"rules_version":report.rules_version,"started_utc_ms":report.range_started_utc_ms,"ended_utc_ms":report.range_ended_utc_ms,"generated_utc_ms":report.generated_utc_ms,"covered_ms":report.covered_ms,"applications":apps,"gaps":report.gaps.iter().map(|g|serde_json::json!({"category":g.data_class,"reason":g.reason,"started_utc_ms":g.started_utc_ms,"ended_utc_ms":g.ended_utc_ms})).collect::<Vec<_>>()});
        let bytes = match format {
            ExportFormat::Json => json(&value)?,
            ExportFormat::Csv => {
                let mut keys = vec![
                    "name",
                    "opened_ms",
                    "displayed_ms",
                    "focused_ms",
                    "background_ms",
                    "windows",
                    "keyboard",
                    "left_clicks",
                    "middle_clicks",
                    "right_clicks",
                ];
                if include_paths {
                    keys.push("executable_path");
                }
                let mut out = String::from(
                    "\u{feff}record_type,rules_version,started_utc_ms,ended_utc_ms,covered_ms,",
                );
                out.push_str(&keys.join(","));
                out.push_str("\r\n");
                for app in &apps {
                    out.push_str(&format!(
                        "application,{},{},{},{},",
                        report.rules_version,
                        report.range_started_utc_ms,
                        report.range_ended_utc_ms,
                        report.covered_ms
                    ));
                    out.push_str(
                        &keys
                            .iter()
                            .map(|key| {
                                csv_cell(
                                    app[*key]
                                        .as_str()
                                        .map_or_else(|| app[*key].to_string(), str::to_owned),
                                )
                            })
                            .collect::<Vec<_>>()
                            .join(","),
                    );
                    out.push_str("\r\n");
                }
                out.into_bytes()
            }
        };
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(destination)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        Ok(())
    }
}

fn csv_cell(mut value: String) -> String {
    if value.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        value.insert(0, '\'');
    }
    format!("\"{}\"", value.replace('"', "\"\""))
}
fn read_entry<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    name: &str,
    password: Option<&str>,
    cap: u64,
) -> Result<Zeroizing<Vec<u8>>> {
    let entry = archive.by_name_decrypt(name, password.unwrap_or("").as_bytes())?;
    if entry.size() > cap {
        return Err(invalid("备份文件大小超出限制"));
    }
    let mut bytes = Zeroizing::new(Vec::with_capacity(entry.size() as usize));
    entry.take(cap + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > cap {
        return Err(invalid("备份文件解压超出限制"));
    }
    Ok(bytes)
}
fn verified_entry<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    name: &str,
    password: Option<&str>,
    cap: u64,
    entries: &BTreeMap<String, &BackupEntry>,
) -> Result<Zeroizing<Vec<u8>>> {
    let expected = entries
        .get(name)
        .ok_or_else(|| invalid("备份缺少必要文件"))?;
    let bytes = read_entry(archive, name, password, cap)?;
    if bytes.len() as u64 != expected.bytes || hex::encode(sha256(&bytes)?) != expected.sha256 {
        return Err(invalid("备份文件校验失败"));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn portable_roundtrip_uses_new_key_and_preserves_ai_and_filtered_exports() {
        let (_source, source) = ai::tests::fixture();
        let version = ai::tests::summary(&source, 7000);
        let folder = tempfile::tempdir().unwrap();
        let archive = folder.path().join("portable.zip");
        source
            .export_backup(
                &archive,
                BackupOptions {
                    include_snapshots: false,
                    password: None,
                },
            )
            .unwrap();
        let report = source.generate_local_report(1000, 6000, 7000).unwrap();
        let exported = folder.path().join("report.json");
        source
            .export_report(report.id, &exported, ExportFormat::Json, false)
            .unwrap();
        let text = fs::read_to_string(exported).unwrap();
        assert!(!text.contains("identity"));
        assert!(!text.contains("private"));
        let mut zip = ZipArchive::new(File::open(&archive).unwrap()).unwrap();
        let plain = read_entry(&mut zip, "database.sqlite3", None, MAX_DB).unwrap();
        assert_eq!(&plain[..16], b"SQLite format 3\0");
        let target = tempfile::tempdir().unwrap();
        let target = Storage::open(target.path()).unwrap();
        let prepared = Storage::prepare_restore(&archive, None, folder.path()).unwrap();
        target.replace_from_backup(prepared, false).unwrap();
        assert_eq!(
            target.ai_version(version).unwrap().answer,
            "answer-private-marker"
        );
        assert!(!target.ai_profile("fixture").unwrap().is_tested());
        assert_eq!(
            target.timeline_snapshot(1000, 6000).unwrap().applications,
            source.timeline_snapshot(1000, 6000).unwrap().applications
        );
        assert_ne!(
            *load_key(&target.key_path).unwrap(),
            *load_key(&source.key_path).unwrap()
        );
        assert!(
            !fs::read(target.database_path())
                .unwrap()
                .windows(21)
                .any(|s| s == b"answer-private-marker")
        );
        let before = target.ai_versions().unwrap().len();
        let prepared = Storage::prepare_restore(&archive, None, folder.path()).unwrap();
        assert!(target.replace_from_backup(prepared, false).is_err());
        assert_eq!(before, target.ai_versions().unwrap().len());
        let prepared = Storage::prepare_restore(&archive, None, folder.path()).unwrap();
        target.replace_from_backup(prepared, true).unwrap();
        target.verify_integrity().unwrap();
    }
    #[test]
    fn password_and_tampering_fail_before_replacement() {
        let (_source, source) = ai::tests::fixture();
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("locked.zip");
        source
            .export_backup(
                &path,
                BackupOptions {
                    include_snapshots: true,
                    password: Some("independent-passphrase"),
                },
            )
            .unwrap();
        assert!(Storage::prepare_restore(&path, Some("wrong"), folder.path()).is_err());
        assert!(Storage::prepare_restore(&path, None, folder.path()).is_err());
        assert!(
            Storage::prepare_restore(&path, Some("independent-passphrase"), folder.path()).is_ok()
        );
        let traversal = folder.path().join("traversal.zip");
        let mut zip = ZipWriter::new(File::create(&traversal).unwrap());
        zip.start_file("../escape", SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"x").unwrap();
        zip.finish().unwrap();
        assert!(Storage::prepare_restore(&traversal, None, folder.path()).is_err());
        assert!(!folder.path().join("escape").exists());
        let c = Connection::open_in_memory().unwrap();
        for m in MIGRATIONS {
            c.execute_batch(m.sql).unwrap();
        }
        c.pragma_update(None, "user_version", LATEST_SCHEMA_VERSION)
            .unwrap();
        c.execute_batch("CREATE TRIGGER malicious AFTER INSERT ON applications BEGIN DELETE FROM ai_versions; END;").unwrap();
        assert!(validate_schema(&c, LATEST_SCHEMA_VERSION).is_err());
        assert_eq!(csv_cell("=RUN()".into()), "\"'=RUN()\"");
    }
    #[test]
    fn snapshot_pixels_are_portable_and_authenticated() {
        let (_source, source) = ai::tests::fixture();
        let image = image::RgbaImage::from_pixel(8, 8, image::Rgba([40, 80, 120, 255]));
        let mut bytes = std::io::Cursor::new(Vec::new());
        image
            .write_to(&mut bytes, image::ImageFormat::WebP)
            .unwrap();
        let display = SnapshotDisplay {
            key: "synthetic-display".into(),
            x: 0,
            y: 0,
            width: 8,
            height: 8,
            orientation_degrees: 0,
        };
        let id = source
            .store_snapshot(&SnapshotStoreRequest {
                slot_started_utc_ms: 1000,
                captured_at_utc_ms: 1000,
                display,
                trigger: SnapshotTrigger::Manual,
                webp: bytes.into_inner(),
                pixel_width: 8,
                pixel_height: 8,
            })
            .unwrap();
        let folder = tempfile::tempdir().unwrap();
        let archive = folder.path().join("images.zip");
        source
            .export_backup(
                &archive,
                BackupOptions {
                    include_snapshots: true,
                    password: None,
                },
            )
            .unwrap();
        let target = tempfile::tempdir().unwrap();
        let target = Storage::open(target.path()).unwrap();
        let prepared = Storage::prepare_restore(&archive, None, folder.path()).unwrap();
        target.replace_from_backup(prepared, false).unwrap();
        assert_eq!(
            source.load_snapshot_image(id).unwrap().webp,
            target.load_snapshot_image(id).unwrap().webp
        );
    }
}
