//! Where the collector keeps its control files, and keeping that place fixed.
//!
//! An elevated collector creates, replaces and deletes files in a directory the
//! signed-in user can modify. It therefore ignores the user's environment, never
//! creates the directory itself, refuses one reached through a reparse point the
//! user could have made, and holds it open so it cannot be renamed, deleted or
//! swapped for a junction while the collector runs.

use std::{
    env,
    ffi::{OsStr, OsString},
    fs, io,
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        fs::MetadataExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use windows_sys::Win32::{
    Foundation::{HANDLE, INVALID_HANDLE_VALUE, LocalFree},
    Security::{
        Authorization::{GetSecurityInfo, SE_FILE_OBJECT},
        GetTokenInformation, IsWellKnownSid, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
        PSID, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation, WinBuiltinAdministratorsSid,
        WinLocalSystemSid,
    },
    Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_ATTRIBUTE_DIRECTORY,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE,
        GetFileInformationByHandle, OPEN_EXISTING, READ_CONTROL, SYNCHRONIZE,
    },
    System::Threading::{GetCurrentProcess, OpenProcessToken},
    UI::Shell::GetUserProfileDirectoryW,
};

/// At logon the collector can start before the core has created the directory.
const ELEVATED_WAIT: Duration = Duration::from_secs(120);
const ELEVATED_POLL: Duration = Duration::from_secs(1);

pub struct DataDirectory {
    path: PathBuf,
    _pin: Option<OwnedHandle>,
}

impl DataDirectory {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// `explicit` is the `--data-dir` argument, which the scheduled task sets for an
/// isolated install. Without it an unelevated collector follows the same
/// environment as the core, and an elevated one uses the profile directory that
/// Windows records for this user.
pub fn resolve(explicit: Option<PathBuf>) -> Result<DataDirectory> {
    let token = ProcessToken::open()?;
    if !token.elevated()? {
        let path = match explicit {
            Some(path) => path,
            None => match env::var_os("TIMELENS_DATA_DIR") {
                Some(path) => PathBuf::from(path),
                None => PathBuf::from(
                    env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is unavailable")?,
                )
                .join("Timelens"),
            },
        };
        return Ok(DataDirectory { path, _pin: None });
    }
    let path = match explicit {
        Some(path) if path.is_absolute() => path,
        Some(path) => bail!("--data-dir must be absolute: {}", path.display()),
        None => token
            .profile_directory()?
            .join("AppData")
            .join("Local")
            .join("Timelens"),
    };
    let pin = wait_and_pin(&path)?;
    Ok(DataDirectory {
        path,
        _pin: Some(pin),
    })
}

fn wait_and_pin(path: &Path) -> Result<OwnedHandle> {
    let started = Instant::now();
    let pin = loop {
        match open_directory(path) {
            Ok(handle) => break handle,
            Err(error)
                if error.kind() == io::ErrorKind::NotFound && started.elapsed() < ELEVATED_WAIT =>
            {
                thread::sleep(ELEVATED_POLL);
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "collector data directory is unavailable: {}",
                        path.display()
                    )
                });
            }
        }
    };
    let attributes = file_attributes(&pin)?;
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        bail!(
            "collector data directory is a reparse point: {}",
            path.display()
        );
    }
    if attributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
        bail!(
            "collector data directory is not a directory: {}",
            path.display()
        );
    }
    // The pinned directory keeps every ancestor from being renamed, so these checks
    // stay true for as long as the collector runs.
    for ancestor in path.ancestors().skip(1) {
        let metadata = fs::symlink_metadata(ancestor).with_context(|| {
            format!(
                "collector data directory ancestor is unavailable: {}",
                ancestor.display()
            )
        })?;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            && !owned_by_system_or_administrators(ancestor)?
        {
            bail!(
                "collector data directory passes through a user-created link: {}",
                ancestor.display()
            );
        }
    }
    Ok(pin)
}

/// Open the directory itself, not what a link at that path points to. Like a
/// working-directory handle, it shares read and write but not delete, so the
/// directory cannot be renamed or removed while files inside work normally.
fn open_directory(path: &Path) -> io::Result<OwnedHandle> {
    open_handle(
        path,
        FILE_TRAVERSE | FILE_READ_ATTRIBUTES | READ_CONTROL | SYNCHRONIZE,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
    )
}

fn open_handle(path: &Path, access: u32, share: u32) -> io::Result<OwnedHandle> {
    let wide = wide(path.as_os_str());
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            access,
            share,
            null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE || handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

fn file_attributes(handle: &OwnedHandle) -> Result<u32> {
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(handle.as_raw_handle() as HANDLE, &mut information) }
        == 0
    {
        return Err(io::Error::last_os_error()).context("could not read directory attributes");
    }
    Ok(information.dwFileAttributes)
}

/// Links that Windows or an administrator created, such as a profile moved to
/// another drive, are trusted; a link the user created is not.
fn owned_by_system_or_administrators(path: &Path) -> Result<bool> {
    let handle = open_handle(
        path,
        READ_CONTROL,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
    )
    .with_context(|| format!("could not inspect link {}", path.display()))?;
    let mut owner: PSID = null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
    let status = unsafe {
        GetSecurityInfo(
            handle.as_raw_handle() as HANDLE,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            null_mut(),
            null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        bail!(
            "could not read the owner of {} (status {status})",
            path.display()
        );
    }
    let trusted = unsafe {
        IsWellKnownSid(owner, WinBuiltinAdministratorsSid) != 0
            || IsWellKnownSid(owner, WinLocalSystemSid) != 0
    };
    unsafe { LocalFree(descriptor) };
    Ok(trusted)
}

struct ProcessToken(OwnedHandle);

impl ProcessToken {
    fn open() -> Result<Self> {
        let mut token: HANDLE = null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error()).context("could not open the process token");
        }
        Ok(Self(unsafe { OwnedHandle::from_raw_handle(token) }))
    }

    fn elevated(&self) -> Result<bool> {
        let mut elevation = TOKEN_ELEVATION::default();
        let mut returned = 0;
        if unsafe {
            GetTokenInformation(
                self.0.as_raw_handle() as HANDLE,
                TokenElevation,
                (&mut elevation as *mut TOKEN_ELEVATION).cast(),
                size_of::<TOKEN_ELEVATION>() as u32,
                &mut returned,
            )
        } == 0
        {
            return Err(io::Error::last_os_error()).context("could not read token elevation");
        }
        Ok(elevation.TokenIsElevated != 0)
    }

    /// The profile path comes from the machine's profile list, which only
    /// administrators can change, unlike the user's environment.
    fn profile_directory(&self) -> Result<PathBuf> {
        let mut length = 0_u32;
        unsafe {
            GetUserProfileDirectoryW(self.0.as_raw_handle() as HANDLE, null_mut(), &mut length)
        };
        if length == 0 {
            return Err(io::Error::last_os_error()).context("could not size the profile path");
        }
        let mut buffer = vec![0_u16; length as usize];
        if unsafe {
            GetUserProfileDirectoryW(
                self.0.as_raw_handle() as HANDLE,
                buffer.as_mut_ptr(),
                &mut length,
            )
        } == 0
        {
            return Err(io::Error::last_os_error()).context("could not read the profile path");
        }
        let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
        let profile = PathBuf::from(OsString::from_wide(&buffer[..end]));
        if !profile.is_absolute() {
            bail!("profile path is not absolute: {}", profile.display());
        }
        Ok(profile)
    }
}

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pinned_directory_cannot_be_renamed_while_its_files_still_work() {
        let parent = tempfile::tempdir().unwrap();
        let directory = parent.path().join("data");
        fs::create_dir(&directory).unwrap();
        let pin = wait_and_pin(&directory).unwrap();

        fs::write(directory.join("a.tmp"), b"x").unwrap();
        fs::rename(directory.join("a.tmp"), directory.join("b.tmp")).unwrap();
        fs::remove_file(directory.join("b.tmp")).unwrap();
        assert!(fs::rename(&directory, parent.path().join("moved")).is_err());

        drop(pin);
        fs::rename(&directory, parent.path().join("moved")).unwrap();
    }

    #[test]
    fn a_missing_directory_is_not_created() {
        let parent = tempfile::tempdir().unwrap();
        assert!(open_directory(&parent.path().join("absent")).is_err());
        assert!(!parent.path().join("absent").exists());
    }
}
