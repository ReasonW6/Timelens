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

use rusqlite::{Connection, OpenFlags, TransactionBehavior, params};
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
const LATEST_SCHEMA_VERSION: i64 = 1;

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
}

pub type Result<T> = std::result::Result<T, StorageError>;

pub struct Storage {
    connection: Connection,
    database_path: PathBuf,
    cipher_version: String,
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

const MIGRATIONS: &[Migration] = &[Migration {
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
}];

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
    fn dpapi_round_trip_does_not_embed_the_plain_key() {
        let key = [0x5a_u8; KEY_BYTES];
        let protected = protect_data(&key).unwrap();
        assert!(!contains_bytes(&protected, &key));
        assert_eq!(unprotect_data(&protected).unwrap(), key);
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
            Migration {
                version: 2,
                sql: "CREATE TABLE should_rollback (id INTEGER); INVALID SQL;",
            },
        ];
        let error = migrate_database(&database_path, &backup_path, &key, &migrations).unwrap_err();
        assert!(error.to_string().contains("restored"));
        assert!(!backup_path.exists());

        let connection = open_encrypted(&database_path, &key, false).unwrap();
        assert_eq!(schema_version(&connection).unwrap(), 1);
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
