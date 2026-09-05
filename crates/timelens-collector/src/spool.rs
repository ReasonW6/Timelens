use std::{
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    mem::size_of,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    ptr::{null, null_mut},
};

use anyhow::{Context, Result, bail};
use prost::Message;
use timelens_ipc::{
    COLLECTOR_SPOOL_FILE, COLLECTOR_SPOOL_KEY_FILE, COLLECTOR_TRAY_STATE_FILE, EventBatch,
    TrayTransition,
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

pub const SPOOL_FILE_NAME: &str = COLLECTOR_SPOOL_FILE;
pub const SPOOL_KEY_FILE_NAME: &str = COLLECTOR_SPOOL_KEY_FILE;
pub const TRAY_STATE_FILE_NAME: &str = COLLECTOR_TRAY_STATE_FILE;
pub const SPOOL_FILE_BYTES: u64 = 32 * 1024 * 1024;

const DATA_OFFSET: u64 = 4096;
const HEADER_SLOT_BYTES: usize = 512;
const HEADER_MAGIC: &[u8; 8] = b"TLSPOOL1";
const HEADER_VERSION: u32 = 1;
const HEADER_CRC_OFFSET: usize = 56;
const RECORD_MAGIC: u32 = 0x544C_5243;
const RECORD_HEADER_BYTES: usize = 12;
const KEY_MAGIC: &[u8; 8] = b"TLSPKEY1";
const KEY_BYTES: usize = 32;
const DPAPI_LABEL: &[u8] = b"Timelens collector spool v1";
const TRAY_STATE_MAGIC: &[u8; 8] = b"TLTRAY1\0";

pub struct PendingSpool {
    file: File,
    key: Zeroizing<Vec<u8>>,
    header: Header,
    active_header_slot: usize,
    data_capacity: u64,
    key_path: PathBuf,
    tray_state_path: PathBuf,
}

#[derive(Clone, PartialEq, Message)]
struct PersistedTrayState {
    #[prost(message, repeated, tag = "1")]
    transitions: Vec<TrayTransition>,
    #[prost(uint32, tag = "2")]
    version: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Header {
    generation: u64,
    head: u64,
    tail: u64,
    used: u64,
    count: u64,
}

impl PendingSpool {
    pub fn open(data_directory: &Path) -> Result<Self> {
        fs::create_dir_all(data_directory).with_context(|| {
            format!(
                "failed to create collector data directory {}",
                data_directory.display()
            )
        })?;
        Self::open_paths(
            &data_directory.join(SPOOL_FILE_NAME),
            &data_directory.join(SPOOL_KEY_FILE_NAME),
            SPOOL_FILE_BYTES,
        )
    }

    fn open_paths(spool_path: &Path, key_path: &Path, file_bytes: u64) -> Result<Self> {
        if file_bytes <= DATA_OFFSET + RECORD_HEADER_BYTES as u64 {
            bail!("collector spool capacity is too small");
        }
        let spool_existed = spool_path.exists();
        let tray_state_path = spool_path.with_file_name(TRAY_STATE_FILE_NAME);
        if spool_existed && !key_path.exists() {
            bail!(
                "collector spool key is missing for {}",
                spool_path.display()
            );
        }
        let key = load_or_create_key(key_path)?;
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(spool_path)
            .with_context(|| format!("failed to open collector spool {}", spool_path.display()))?;
        let existing_length = file.metadata()?.len();
        let data_capacity = file_bytes - DATA_OFFSET;

        if !spool_existed || existing_length == 0 {
            file.set_len(file_bytes)?;
            let header = Header {
                generation: 1,
                head: 0,
                tail: 0,
                used: 0,
                count: 0,
            };
            write_header_slot(&mut file, 0, &header)?;
            file.sync_data()?;
            return Ok(Self {
                file,
                key,
                header,
                active_header_slot: 0,
                data_capacity,
                key_path: key_path.to_owned(),
                tray_state_path,
            });
        }
        if existing_length != file_bytes {
            bail!(
                "collector spool has {} bytes; expected {file_bytes}",
                existing_length
            );
        }

        let candidates = [
            read_header_slot(&mut file, 0, data_capacity)?,
            read_header_slot(&mut file, 1, data_capacity)?,
        ];
        let (active_header_slot, header) = candidates
            .into_iter()
            .enumerate()
            .filter_map(|(slot, header)| header.map(|header| (slot, header)))
            .max_by_key(|(_, header)| header.generation)
            .context("collector spool has no valid header")?;

        let mut spool = Self {
            file,
            key,
            header,
            active_header_slot,
            data_capacity,
            key_path: key_path.to_owned(),
            tray_state_path,
        };
        spool.validate_records()?;
        Ok(spool)
    }

    #[cfg(test)]
    pub fn len(&self) -> u64 {
        self.header.count
    }

    pub fn is_empty(&self) -> bool {
        self.header.count == 0
    }

    pub fn push(&mut self, batch: &EventBatch) -> Result<bool> {
        batch
            .last_sequence()
            .context("collector spool rejected an empty or overflowing batch")?;
        let plain = Zeroizing::new(batch.encode_to_vec());
        let protected = protect_data(&plain, &self.key)?;
        let total = RECORD_HEADER_BYTES
            .checked_add(protected.len())
            .context("collector spool record length overflow")? as u64;
        if total > self.data_capacity {
            bail!("collector event batch is larger than the entire spool");
        }
        if self.data_capacity - self.header.used < total {
            return Ok(false);
        }

        let mut record = Vec::with_capacity(total as usize);
        record.extend_from_slice(&RECORD_MAGIC.to_le_bytes());
        record.extend_from_slice(&(protected.len() as u32).to_le_bytes());
        record.extend_from_slice(&crc32fast::hash(&protected).to_le_bytes());
        record.extend_from_slice(&protected);
        self.write_circular(self.header.tail, &record)?;
        self.file.sync_data()?;

        self.header.tail = (self.header.tail + total) % self.data_capacity;
        self.header.used += total;
        self.header.count += 1;
        self.commit_header()?;
        Ok(true)
    }

    pub fn front(&mut self) -> Result<Option<EventBatch>> {
        self.read_front().map(|front| front.map(|(batch, _)| batch))
    }

    pub fn oldest_observed_at_utc_ms(&mut self) -> Result<Option<i64>> {
        Ok(self
            .front()?
            .and_then(|batch| batch.events.first().map(|event| event.observed_at_utc_ms)))
    }

    pub fn pop_if_matches(&mut self, expected: &EventBatch) -> Result<bool> {
        let Some((front, total)) = self.read_front()? else {
            return Ok(false);
        };
        if &front != expected {
            return Ok(false);
        }
        self.header.head = (self.header.head + total) % self.data_capacity;
        self.header.used -= total;
        self.header.count -= 1;
        if self.header.count == 0 {
            self.header.head = self.header.tail;
            self.header.used = 0;
        }
        self.commit_header()?;
        Ok(true)
    }

    pub fn reset(&mut self) -> Result<()> {
        self.header.head = self.header.tail;
        self.header.used = 0;
        self.header.count = 0;
        self.commit_header()
    }

    pub fn load_tray_state(&self) -> Result<Vec<TrayTransition>> {
        if !self.tray_state_path.exists() {
            return Ok(Vec::new());
        }
        let bytes = fs::read(&self.tray_state_path)?;
        if bytes.len() < 16 || &bytes[..8] != TRAY_STATE_MAGIC {
            bail!("collector tray state file is invalid");
        }
        let protected_length = u32::from_le_bytes(bytes[8..12].try_into()?) as usize;
        let expected_crc = u32::from_le_bytes(bytes[12..16].try_into()?);
        let protected = &bytes[16..];
        if protected.len() != protected_length || crc32fast::hash(protected) != expected_crc {
            bail!("collector tray state file checksum is invalid");
        }
        let plain = Zeroizing::new(unprotect_data(protected, &self.key)?);
        let state = PersistedTrayState::decode(plain.as_slice())
            .context("collector tray state payload is invalid")?;
        if state.version != 1 {
            bail!("collector tray state version is unsupported");
        }
        Ok(state.transitions)
    }

    pub fn save_tray_state(&self, transitions: &[TrayTransition]) -> Result<()> {
        let plain = Zeroizing::new(
            PersistedTrayState {
                transitions: transitions.to_vec(),
                version: 1,
            }
            .encode_to_vec(),
        );
        let protected = protect_data(&plain, &self.key)?;
        let temporary = self
            .tray_state_path
            .with_extension(format!("dpapi.new.{}", unsafe { GetCurrentProcessId() }));
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temporary)?;
        file.write_all(TRAY_STATE_MAGIC)?;
        file.write_all(&(protected.len() as u32).to_le_bytes())?;
        file.write_all(&crc32fast::hash(&protected).to_le_bytes())?;
        file.write_all(&protected)?;
        file.sync_all()?;
        drop(file);

        let source = wide(&temporary);
        let destination = wide(&self.tray_state_path);
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
            bail!("collector tray state replacement failed with status {status}");
        }
        Ok(())
    }

    pub fn rotate_key(&mut self) -> Result<()> {
        self.reset()?;
        let new_key = random_key()?;
        replace_key_file(&self.key_path, &new_key)?;
        self.key = new_key;
        self.save_tray_state(&[])
    }

    fn read_front(&mut self) -> Result<Option<(EventBatch, u64)>> {
        if self.header.count == 0 {
            return Ok(None);
        }
        let (protected, total) = self.read_record(self.header.head, self.header.used)?;
        let plain = Zeroizing::new(unprotect_data(&protected, &self.key)?);
        let batch = EventBatch::decode(plain.as_slice())
            .context("collector spool contains an invalid event batch")?;
        batch
            .last_sequence()
            .context("collector spool contains an empty or overflowing batch")?;
        Ok(Some((batch, total)))
    }

    fn read_record(&mut self, position: u64, available: u64) -> Result<(Vec<u8>, u64)> {
        if available < RECORD_HEADER_BYTES as u64 {
            bail!("collector spool record header is truncated");
        }
        let header = self.read_circular(position, RECORD_HEADER_BYTES)?;
        let magic = u32::from_le_bytes(header[0..4].try_into()?);
        if magic != RECORD_MAGIC {
            bail!("collector spool record magic is invalid");
        }
        let protected_length = u32::from_le_bytes(header[4..8].try_into()?) as usize;
        let expected_crc = u32::from_le_bytes(header[8..12].try_into()?);
        let total = RECORD_HEADER_BYTES
            .checked_add(protected_length)
            .context("collector spool record length overflow")? as u64;
        if total > available || total > self.data_capacity {
            bail!("collector spool record length is invalid");
        }
        let protected = self.read_circular(
            (position + RECORD_HEADER_BYTES as u64) % self.data_capacity,
            protected_length,
        )?;
        if crc32fast::hash(&protected) != expected_crc {
            bail!("collector spool record checksum does not match");
        }
        Ok((protected, total))
    }

    fn validate_records(&mut self) -> Result<()> {
        let mut position = self.header.head;
        let mut remaining = self.header.used;
        for _ in 0..self.header.count {
            let (_, total) = self.read_record(position, remaining)?;
            position = (position + total) % self.data_capacity;
            remaining -= total;
        }
        if remaining != 0 || position != self.header.tail {
            bail!("collector spool header does not match its records");
        }
        Ok(())
    }

    fn read_circular(&mut self, position: u64, length: usize) -> Result<Vec<u8>> {
        let first = length.min((self.data_capacity - position) as usize);
        let mut bytes = vec![0_u8; length];
        read_exact_at(&mut self.file, DATA_OFFSET + position, &mut bytes[..first])?;
        if first < length {
            read_exact_at(&mut self.file, DATA_OFFSET, &mut bytes[first..])?;
        }
        Ok(bytes)
    }

    fn write_circular(&mut self, position: u64, bytes: &[u8]) -> Result<()> {
        let first = bytes.len().min((self.data_capacity - position) as usize);
        write_all_at(&mut self.file, DATA_OFFSET + position, &bytes[..first])?;
        if first < bytes.len() {
            write_all_at(&mut self.file, DATA_OFFSET, &bytes[first..])?;
        }
        Ok(())
    }

    fn commit_header(&mut self) -> Result<()> {
        self.header.generation = self
            .header
            .generation
            .checked_add(1)
            .context("collector spool header generation overflow")?;
        let next_slot = 1 - self.active_header_slot;
        write_header_slot(&mut self.file, next_slot, &self.header)?;
        self.file.sync_data()?;
        self.active_header_slot = next_slot;
        Ok(())
    }
}

fn write_header_slot(file: &mut File, slot: usize, header: &Header) -> Result<()> {
    let mut bytes = [0_u8; HEADER_SLOT_BYTES];
    bytes[0..8].copy_from_slice(HEADER_MAGIC);
    bytes[8..12].copy_from_slice(&HEADER_VERSION.to_le_bytes());
    bytes[16..24].copy_from_slice(&header.generation.to_le_bytes());
    bytes[24..32].copy_from_slice(&header.head.to_le_bytes());
    bytes[32..40].copy_from_slice(&header.tail.to_le_bytes());
    bytes[40..48].copy_from_slice(&header.used.to_le_bytes());
    bytes[48..56].copy_from_slice(&header.count.to_le_bytes());
    let checksum = crc32fast::hash(&bytes[..HEADER_CRC_OFFSET]);
    bytes[HEADER_CRC_OFFSET..HEADER_CRC_OFFSET + 4].copy_from_slice(&checksum.to_le_bytes());
    write_all_at(file, (slot * HEADER_SLOT_BYTES) as u64, &bytes)
}

fn read_header_slot(file: &mut File, slot: usize, data_capacity: u64) -> Result<Option<Header>> {
    let mut bytes = [0_u8; HEADER_SLOT_BYTES];
    read_exact_at(file, (slot * HEADER_SLOT_BYTES) as u64, &mut bytes)?;
    if bytes.iter().all(|byte| *byte == 0) {
        return Ok(None);
    }
    if &bytes[0..8] != HEADER_MAGIC
        || u32::from_le_bytes(bytes[8..12].try_into()?) != HEADER_VERSION
    {
        return Ok(None);
    }
    let stored_crc =
        u32::from_le_bytes(bytes[HEADER_CRC_OFFSET..HEADER_CRC_OFFSET + 4].try_into()?);
    if crc32fast::hash(&bytes[..HEADER_CRC_OFFSET]) != stored_crc {
        return Ok(None);
    }
    let header = Header {
        generation: u64::from_le_bytes(bytes[16..24].try_into()?),
        head: u64::from_le_bytes(bytes[24..32].try_into()?),
        tail: u64::from_le_bytes(bytes[32..40].try_into()?),
        used: u64::from_le_bytes(bytes[40..48].try_into()?),
        count: u64::from_le_bytes(bytes[48..56].try_into()?),
    };
    if header.head >= data_capacity
        || header.tail >= data_capacity
        || header.used > data_capacity
        || header.count > data_capacity / RECORD_HEADER_BYTES as u64
        || (header.count == 0 && header.used != 0)
    {
        return Ok(None);
    }
    Ok(Some(header))
}

fn read_exact_at(file: &mut File, offset: u64, bytes: &mut [u8]) -> Result<()> {
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(bytes)?;
    Ok(())
}

fn write_all_at(file: &mut File, offset: u64, bytes: &[u8]) -> Result<()> {
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(bytes)?;
    Ok(())
}

fn load_or_create_key(path: &Path) -> Result<Zeroizing<Vec<u8>>> {
    if path.exists() {
        return load_key(path);
    }
    let key = random_key()?;
    let protected = protect_data(&key, DPAPI_LABEL)?;
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

fn replace_key_file(path: &Path, key: &[u8]) -> Result<()> {
    let protected = protect_data(key, DPAPI_LABEL)?;
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
        bail!("collector spool key replacement failed with status {status}");
    }
    Ok(())
}

fn load_key(path: &Path) -> Result<Zeroizing<Vec<u8>>> {
    let bytes = fs::read(path)?;
    if bytes.len() < KEY_MAGIC.len() + size_of::<u32>() || &bytes[..KEY_MAGIC.len()] != KEY_MAGIC {
        bail!("collector spool key file is invalid");
    }
    let length_offset = KEY_MAGIC.len();
    let protected_length =
        u32::from_le_bytes(bytes[length_offset..length_offset + 4].try_into()?) as usize;
    let protected = &bytes[length_offset + 4..];
    if protected.len() != protected_length {
        bail!("collector spool key file length is invalid");
    }
    let key = unprotect_data(protected, DPAPI_LABEL)?;
    if key.len() != KEY_BYTES {
        bail!("collector spool key has an invalid length");
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
        bail!("collector spool key generation failed with status {status}");
    }
    Ok(key)
}

fn protect_data(plain: &[u8], entropy_bytes: &[u8]) -> Result<Vec<u8>> {
    let input = blob(plain);
    let entropy = blob(entropy_bytes);
    let description = wide("Timelens collector spool record");
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
        bail!("collector spool encryption failed with status {}", unsafe {
            GetLastError()
        });
    }
    copy_local_blob(output)
}

fn unprotect_data(protected: &[u8], entropy_bytes: &[u8]) -> Result<Vec<u8>> {
    let input = blob(protected);
    let entropy = blob(entropy_bytes);
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
        bail!("collector spool decryption failed with status {}", unsafe {
            GetLastError()
        });
    }
    if !description.is_null() {
        unsafe {
            LocalFree(description.cast());
        }
    }
    copy_local_blob(output)
}

fn blob(bytes: &[u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB {
        cbData: bytes.len() as u32,
        pbData: bytes.as_ptr().cast_mut(),
    }
}

fn copy_local_blob(blob: CRYPT_INTEGER_BLOB) -> Result<Vec<u8>> {
    if blob.pbData.is_null() || blob.cbData == 0 {
        bail!("collector spool data-protection output is empty");
    }
    let bytes = unsafe { std::slice::from_raw_parts(blob.pbData, blob.cbData as usize) }.to_vec();
    unsafe {
        LocalFree(blob.pbData.cast());
    }
    Ok(bytes)
}

fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use timelens_ipc::{
        CollectorEvent, IdentitySource, TrayTransitionKind, WindowObservation, WindowTransition,
        WindowTransitionKind, collector_event,
    };

    fn batch(sequence: u64, marker: &str) -> EventBatch {
        EventBatch {
            collector_run_id: vec![7; 16],
            first_sequence: sequence,
            events: vec![CollectorEvent {
                observed_at_utc_ms: 1_700_000_000_000 + sequence as i64,
                monotonic_ms: sequence,
                body: Some(collector_event::Body::WindowTransition(WindowTransition {
                    kind: WindowTransitionKind::Opened as i32,
                    window: Some(WindowObservation {
                        window_id: sequence,
                        process_id: 2,
                        process_started_at_100ns: 3,
                        application_identity: format!("path:c:\\{marker}.exe"),
                        identity_source: IdentitySource::ExecutablePath as i32,
                        executable_path: Some(format!(r"C:\{marker}.exe")),
                        app_user_model_id: None,
                        package_identity: None,
                        displayed: true,
                        focused: false,
                        on_current_virtual_desktop: Some(true),
                        virtual_desktop_id: None,
                    }),
                })),
            }],
        }
    }

    #[test]
    fn survives_reopen_and_never_writes_plain_facts() {
        let directory = tempfile::tempdir().unwrap();
        let spool_path = directory.path().join(SPOOL_FILE_NAME);
        let key_path = directory.path().join(SPOOL_KEY_FILE_NAME);
        let first = batch(1, "private-spool-marker");
        let second = batch(2, "second-marker");
        let mut spool = PendingSpool::open_paths(&spool_path, &key_path, 64 * 1024).unwrap();
        assert!(spool.push(&first).unwrap());
        assert!(spool.push(&second).unwrap());
        assert_eq!(spool.len(), 2);
        drop(spool);

        let bytes = fs::read(&spool_path).unwrap();
        assert!(!contains_bytes(&bytes, b"private-spool-marker"));
        let mut reopened = PendingSpool::open_paths(&spool_path, &key_path, 64 * 1024).unwrap();
        assert_eq!(reopened.front().unwrap(), Some(first.clone()));
        assert!(reopened.pop_if_matches(&first).unwrap());
        assert_eq!(reopened.front().unwrap(), Some(second.clone()));
        assert!(reopened.pop_if_matches(&second).unwrap());
        assert!(reopened.is_empty());
    }

    #[test]
    fn reports_full_without_discarding_the_oldest_batch() {
        let directory = tempfile::tempdir().unwrap();
        let spool_path = directory.path().join(SPOOL_FILE_NAME);
        let key_path = directory.path().join(SPOOL_KEY_FILE_NAME);
        let first = batch(1, &"a".repeat(900));
        let second = batch(2, &"b".repeat(900));
        let mut spool =
            PendingSpool::open_paths(&spool_path, &key_path, DATA_OFFSET + 4096).unwrap();
        assert!(spool.push(&first).unwrap());
        assert!(!spool.push(&second).unwrap());
        assert_eq!(spool.len(), 1);
        assert_eq!(spool.front().unwrap(), Some(first));
    }

    #[test]
    fn tray_restart_seed_is_encrypted_and_survives_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let spool_path = directory.path().join(SPOOL_FILE_NAME);
        let key_path = directory.path().join(SPOOL_KEY_FILE_NAME);
        let marker = "path:c:\\private-tray-marker.exe";
        let transition = TrayTransition {
            kind: TrayTransitionKind::Started as i32,
            application_identity: marker.to_owned(),
            process_id: 20,
            process_started_at_100ns: 30,
        };
        let spool = PendingSpool::open_paths(&spool_path, &key_path, 64 * 1024).unwrap();
        spool
            .save_tray_state(std::slice::from_ref(&transition))
            .unwrap();
        let state_bytes = fs::read(directory.path().join(TRAY_STATE_FILE_NAME)).unwrap();
        assert!(!contains_bytes(&state_bytes, marker.as_bytes()));
        drop(spool);

        let reopened = PendingSpool::open_paths(&spool_path, &key_path, 64 * 1024).unwrap();
        assert_eq!(reopened.load_tray_state().unwrap(), vec![transition]);
    }

    #[test]
    fn key_rotation_cryptographically_invalidates_pending_records_and_tray_state() {
        let directory = tempfile::tempdir().unwrap();
        let spool_path = directory.path().join(SPOOL_FILE_NAME);
        let key_path = directory.path().join(SPOOL_KEY_FILE_NAME);
        let mut spool = PendingSpool::open_paths(&spool_path, &key_path, 64 * 1024).unwrap();
        spool.push(&batch(1, "clear-marker")).unwrap();
        let old_key = fs::read(&key_path).unwrap();
        spool
            .save_tray_state(&[TrayTransition {
                kind: TrayTransitionKind::Started as i32,
                application_identity: "path:c:\\clear-tray-marker.exe".to_owned(),
                process_id: 20,
                process_started_at_100ns: 30,
            }])
            .unwrap();

        spool.rotate_key().unwrap();
        assert!(spool.is_empty());
        assert!(spool.load_tray_state().unwrap().is_empty());
        assert_ne!(fs::read(&key_path).unwrap(), old_key);
        drop(spool);
        let reopened = PendingSpool::open_paths(&spool_path, &key_path, 64 * 1024).unwrap();
        assert!(reopened.is_empty());
    }

    fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }
}
