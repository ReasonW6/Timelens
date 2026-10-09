use anyhow::{Context, Result, bail};
use std::{
    fs,
    io::{Read, Write},
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
};
const LOCATION: &str = "data-location.txt";
pub fn control_directory() -> Result<PathBuf> {
    Ok(
        PathBuf::from(std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is unavailable")?)
            .join("Timelens"),
    )
}
pub fn resolve(control: &Path) -> Result<PathBuf> {
    let path = control.join(LOCATION);
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(control.to_path_buf()),
        Err(e) => return Err(e.into()),
    };
    let mut text = String::new();
    Read::by_ref(&mut file)
        .take(65537)
        .read_to_string(&mut text)?;
    if text.len() > 65536 {
        bail!("数据位置设置过大");
    }
    let target = text
        .strip_prefix("Timelens data location v1\n")
        .context("数据位置设置格式无效")?;
    let target = timelens_storage::Storage::validate_data_destination(Path::new(target))?;
    if !target.is_dir() {
        bail!(
            "已设置的数据位置不可用：{}。请连接原磁盘或进入恢复界面。",
            target.display()
        );
    }
    Ok(target)
}
pub fn publish(control: &Path, destination: &Path) -> timelens_storage::Result<()> {
    fs::create_dir_all(control)?;
    let mut file = fs::File::create(control.join("data-location.next"))?;
    file.write_all(format!("Timelens data location v1\n{}", destination.display()).as_bytes())?;
    file.sync_all()?;
    drop(file);
    let wide = |p: PathBuf| {
        p.as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>()
    };
    let a = wide(control.join("data-location.next"));
    let b = wide(control.join(LOCATION));
    if unsafe {
        windows_sys::Win32::Storage::FileSystem::MoveFileExW(
            a.as_ptr(),
            b.as_ptr(),
            windows_sys::Win32::Storage::FileSystem::MOVEFILE_REPLACE_EXISTING
                | windows_sys::Win32::Storage::FileSystem::MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
/// Point a new user's dataset at the empty directory the installer prepared. A
/// user who already has a dataset or a chosen location keeps it.
pub fn adopt_initial(control: &Path, destination: &Path) -> Result<bool> {
    if control.join(LOCATION).exists() || timelens_storage::Storage::holds_dataset(control) {
        return Ok(false);
    }
    let target = timelens_storage::Storage::validate_data_destination(destination)?;
    if !target.is_dir() || fs::read_dir(&target)?.next().is_some() {
        bail!("安装器准备的数据目录不存在或不为空：{}", target.display());
    }
    publish(control, &target)?;
    Ok(true)
}
pub fn delete_pointer(control: &Path) -> Result<()> {
    for name in [LOCATION, "data-location.next"] {
        match fs::remove_file(control.join(name)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    let _ = fs::remove_dir(control);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_user_adopts_the_prepared_directory_and_an_existing_one_keeps_theirs() {
        let root = tempfile::tempdir().unwrap();
        let control = root.path().join("control");
        let prepared = root.path().join("Data");
        fs::create_dir(&prepared).unwrap();
        assert!(adopt_initial(&control, &prepared).unwrap());
        assert_eq!(resolve(&control).unwrap(), prepared);

        let other = root.path().join("Other");
        fs::create_dir(&other).unwrap();
        assert!(!adopt_initial(&control, &other).unwrap());
        assert_eq!(resolve(&control).unwrap(), prepared);
    }

    #[test]
    fn a_non_empty_directory_is_not_adopted() {
        let root = tempfile::tempdir().unwrap();
        let control = root.path().join("control");
        let prepared = root.path().join("Data");
        fs::create_dir(&prepared).unwrap();
        fs::write(prepared.join("notes.txt"), b"x").unwrap();
        assert!(adopt_initial(&control, &prepared).is_err());
        assert!(!control.join(LOCATION).exists());
    }
}
