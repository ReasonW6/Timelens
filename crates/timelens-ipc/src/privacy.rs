//! Typed local collection policy. Fixed-name, DPAPI-protected files contain no
//! commands or arbitrary output paths and grant the collector no new capability.
use crate::{IpcError, MAX_IDENTITY_BYTES, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Write},
    path::Path,
    ptr::null_mut,
};
use windows_sys::Win32::{
    Foundation::{GetLastError, LocalFree},
    Security::Cryptography::*,
    Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW},
};
pub const POLICY_FILE: &str = "collection-policy.dpapi";
pub const SYSTEM_KINDS: &[&str] = &[
    "active",
    "desktop_idle",
    "locked",
    "sleep",
    "global_pause",
    "privacy_exclusion",
    "session_disconnected",
    "secure_desktop",
    "clock_discontinuity",
    "system_end",
];
const ENTROPY: &[u8] = b"Timelens collection policy v1";
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub revision: u64,
    pub paused: bool,
    pub activity: BTreeSet<String>,
    pub input: BTreeSet<String>,
}
impl Policy {
    pub fn validate(&self) -> Result<()> {
        if self.activity.len() + self.input.len() > 4096
            || self
                .activity
                .iter()
                .chain(&self.input)
                .any(|s| s.is_empty() || s.len() > MAX_IDENTITY_BYTES)
        {
            return Err(IpcError::InvalidMessage("invalid collection policy".into()));
        }
        Ok(())
    }
    pub fn load(directory: &Path) -> Result<Self> {
        let path = directory.join(POLICY_FILE);
        let file = match fs::File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e.into()),
        };
        let mut protected = vec![];
        file.take(2 * 1024 * 1024 + 1).read_to_end(&mut protected)?;
        if protected.len() > 2 * 1024 * 1024 {
            return Err(IpcError::InvalidMessage("policy too large".into()));
        }
        let bytes = protect(&protected, false)?;
        let p: Self = serde_json::from_slice(&bytes)
            .map_err(|_| IpcError::InvalidMessage("invalid encrypted collection policy".into()))?;
        p.validate()?;
        Ok(p)
    }
    pub fn save(&self, directory: &Path) -> Result<()> {
        self.validate()?;
        let plain = zeroize::Zeroizing::new(
            serde_json::to_vec(self)
                .map_err(|_| IpcError::InvalidMessage("invalid policy".into()))?,
        );
        let encrypted = protect(&plain, true)?;
        let destination = directory.join(POLICY_FILE);
        let staged = directory.join("collection-policy.next");
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&staged)?;
        file.write_all(&encrypted)?;
        file.sync_all()?;
        drop(file);
        use std::os::windows::ffi::OsStrExt;
        let a: Vec<u16> = staged.as_os_str().encode_wide().chain(Some(0)).collect();
        let b: Vec<u16> = destination
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        if unsafe {
            MoveFileExW(
                a.as_ptr(),
                b.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
}
fn protect(bytes: &[u8], encrypt: bool) -> Result<zeroize::Zeroizing<Vec<u8>>> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: bytes.len() as u32,
        pbData: bytes.as_ptr().cast_mut(),
    };
    let entropy = CRYPT_INTEGER_BLOB {
        cbData: ENTROPY.len() as u32,
        pbData: ENTROPY.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    let okay = unsafe {
        if encrypt {
            CryptProtectData(
                &input,
                std::ptr::null(),
                &entropy,
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptUnprotectData(
                &input,
                null_mut(),
                &entropy,
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        }
    };
    if okay == 0 {
        return Err(std::io::Error::from_raw_os_error(unsafe { GetLastError() } as i32).into());
    }
    let result =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
    unsafe {
        std::ptr::write_bytes(output.pbData, 0, output.cbData as usize);
        LocalFree(output.pbData.cast());
    }
    Ok(zeroize::Zeroizing::new(result))
}
