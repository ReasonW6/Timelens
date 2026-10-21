//! Provider keys that earlier versions kept for AI summaries in Credential Manager.
use anyhow::{Result, bail};
use std::ptr::null_mut;
use windows_sys::Win32::{
    Foundation::{ERROR_NOT_FOUND, GetLastError},
    Security::Credentials::{CRED_TYPE_GENERIC, CredDeleteW, CredEnumerateW, CredFree},
};

/// Delete every credential stored under `Timelens/ai/`; none is ever read again.
pub fn remove_ai_credentials() -> Result<usize> {
    let filter = "Timelens/ai/*"
        .encode_utf16()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let mut count = 0;
    let mut entries = null_mut();
    if unsafe { CredEnumerateW(filter.as_ptr(), 0, &mut count, &mut entries) } == 0 {
        if unsafe { GetLastError() } == ERROR_NOT_FOUND {
            return Ok(0);
        }
        bail!("无法列出 Timelens 凭据");
    }
    let mut deleted = 0;
    for entry in unsafe { std::slice::from_raw_parts(entries, count as usize) } {
        if unsafe { CredDeleteW((**entry).TargetName, CRED_TYPE_GENERIC, 0) } != 0 {
            deleted += 1;
        }
    }
    unsafe {
        CredFree(entries.cast());
    }
    if deleted != count as usize {
        bail!("部分 Timelens 凭据未能删除");
    }
    Ok(deleted)
}
