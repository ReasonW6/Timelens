//! Credentials exist only in the current Windows user's Credential Manager.
use crate::valid_id;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, mem::size_of, ptr::null_mut};
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_NOT_FOUND, GetLastError},
    Security::{Credentials::*, GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation},
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};
use zeroize::{Zeroize, Zeroizing};

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Secrets {
    pub api_key: String,
    pub headers: BTreeMap<String, String>,
}
impl Drop for Secrets {
    fn drop(&mut self) {
        self.api_key.zeroize();
        for value in self.headers.values_mut() {
            value.zeroize();
        }
    }
}
impl Secrets {
    pub fn validate(&self) -> Result<(), String> {
        if self.api_key.chars().any(char::is_control) {
            return Err("API 密钥包含无效控制字符".into());
        }
        for (name, value) in &self.headers {
            if name.is_empty()
                || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                || value.chars().any(char::is_control)
                || matches!(
                    name.to_ascii_lowercase().as_str(),
                    "host"
                        | "content-length"
                        | "transfer-encoding"
                        | "connection"
                        | "proxy-authorization"
                        | "content-type"
                        | "accept"
                        | "authorization"
                        | "x-api-key"
                        | "x-goog-api-key"
                        | "api-key"
                )
            {
                return Err("高级请求头名称无效、保留或包含换行".into());
            }
        }
        Ok(())
    }
    pub fn values(&self) -> Vec<&str> {
        std::iter::once(self.api_key.as_str())
            .chain(self.headers.values().map(String::as_str))
            .collect()
    }
}
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
fn target(id: &str) -> Result<Vec<u16>, String> {
    if !valid_id(id) {
        return Err("凭据标识无效".into());
    }
    Ok(wide(&format!("Timelens/ai/{id}")))
}

pub fn save(id: &str, secrets: &Secrets) -> Result<(), String> {
    require_ordinary_privilege()?;
    secrets.validate()?;
    let mut bytes = Zeroizing::new(serde_json::to_vec(secrets).map_err(|_| "无法编码凭据")?);
    if bytes.len() > CRED_MAX_CREDENTIAL_BLOB_SIZE as usize {
        return Err("密钥与秘密请求头超过 Windows 凭据大小限制".into());
    }
    let mut target = target(id)?;
    let mut user = wide("Timelens");
    let credential = CREDENTIALW {
        Type: CRED_TYPE_GENERIC,
        TargetName: target.as_mut_ptr(),
        CredentialBlobSize: bytes.len() as u32,
        CredentialBlob: bytes.as_mut_ptr(),
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        UserName: user.as_mut_ptr(),
        ..Default::default()
    };
    if unsafe { CredWriteW(&credential, 0) } == 0 {
        return Err(format!("Windows 凭据保存失败（{}）", unsafe {
            GetLastError()
        }));
    }
    Ok(())
}
pub fn load(id: &str) -> Result<Secrets, String> {
    require_ordinary_privilege()?;
    let name = target(id)?;
    let mut credential = null_mut();
    if unsafe { CredReadW(name.as_ptr(), CRED_TYPE_GENERIC, 0, &mut credential) } == 0 {
        return Err(format!(
            "请在提供商设置中填写凭据（Windows {}）",
            unsafe { GetLastError() }
        ));
    }
    let result = unsafe {
        let data = std::slice::from_raw_parts(
            (*credential).CredentialBlob,
            (*credential).CredentialBlobSize as usize,
        );
        let parsed =
            serde_json::from_slice::<Secrets>(data).map_err(|_| "Windows 凭据格式无效".to_owned());
        std::slice::from_raw_parts_mut(
            (*credential).CredentialBlob,
            (*credential).CredentialBlobSize as usize,
        )
        .zeroize();
        CredFree(credential.cast());
        parsed
    };
    let secrets = result?;
    secrets.validate()?;
    Ok(secrets)
}
pub fn delete(id: &str) -> Result<(), String> {
    let name = target(id)?;
    if unsafe { CredDeleteW(name.as_ptr(), CRED_TYPE_GENERIC, 0) } == 0
        && unsafe { GetLastError() } != ERROR_NOT_FOUND
    {
        return Err(format!("Windows 凭据删除失败（{}）", unsafe {
            GetLastError()
        }));
    }
    Ok(())
}
pub fn delete_all() -> Result<usize, String> {
    let filter = wide("Timelens/ai/*");
    let mut count = 0;
    let mut entries = null_mut();
    if unsafe { CredEnumerateW(filter.as_ptr(), 0, &mut count, &mut entries) } == 0 {
        return if unsafe { GetLastError() } == ERROR_NOT_FOUND {
            Ok(0)
        } else {
            Err("无法列出 Timelens 凭据".into())
        };
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
        return Err("部分 Timelens 凭据未能删除".into());
    }
    Ok(deleted)
}

pub fn require_ordinary_privilege() -> Result<(), String> {
    let mut token = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err("无法检查 AI 进程权限".into());
    }
    let mut elevation = TOKEN_ELEVATION::default();
    let mut returned = 0;
    let success = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
    };
    unsafe {
        CloseHandle(token);
    }
    if success == 0 || elevation.TokenIsElevated != 0 {
        return Err("AI 网络功能只允许普通权限进程运行，请正常启动 Timelens".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_secret_header_cannot_rewrite_the_request_target_or_framing() {
        let mut secret = Secrets::default();
        secret.headers.insert("Host".into(), "elsewhere".into());
        assert!(secret.validate().is_err());
        secret.headers.clear();
        secret
            .headers
            .insert("X-Tenant".into(), "tenant\r\nAuthorization: leaked".into());
        assert!(secret.validate().is_err());
        assert!(target("../unrelated").is_err());
    }
}
