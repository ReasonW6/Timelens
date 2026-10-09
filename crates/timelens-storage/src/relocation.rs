use super::*;
use std::os::windows::fs::MetadataExt;
fn invalid(s: impl Into<String>) -> StorageError {
    StorageError::Integrity(s.into())
}
pub fn validate_fixed_directory(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(invalid("请选择本地固定磁盘的绝对路径"));
    }
    let mut existing = path.to_path_buf();
    while !existing.exists() {
        if !existing.pop() {
            return Err(invalid("目标父目录不可用"));
        }
    }
    for ancestor in existing.ancestors() {
        let metadata = fs::symlink_metadata(ancestor)?;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(invalid("数据位置不能经过符号链接、联接或云占位目录"));
        }
    }
    if !existing.is_dir() {
        return Err(invalid("数据位置必须是目录"));
    }
    let canonical = fs::canonicalize(&existing)?;
    let display = canonical.to_string_lossy();
    if display.starts_with(r"\\?\UNC\") {
        return Err(invalid("不支持网络数据目录"));
    }
    let plain = display.trim_start_matches(r"\\?\");
    let root = plain.get(..3).ok_or_else(|| invalid("磁盘根路径无效"))?;
    if unsafe { windows_sys::Win32::Storage::FileSystem::GetDriveTypeW(wide(root).as_ptr()) } != 3 {
        return Err(invalid("数据目录必须位于本地固定磁盘"));
    }
    for component in path.components() {
        let name = component.as_os_str().to_string_lossy().to_lowercase();
        if name.starts_with("onedrive")
            || name == "dropbox"
            || name == "icloud drive"
            || name == "google drive"
        {
            return Err(invalid("数据目录不能放在云同步目录"));
        }
    }
    let mut info = [0_u8; 1024];
    let mut returned = 0;
    let status = unsafe {
        windows_sys::Win32::Storage::CloudFilters::CfGetSyncRootInfoByPath(
            wide(&existing).as_ptr(),
            windows_sys::Win32::Storage::CloudFilters::CF_SYNC_ROOT_INFO_BASIC,
            info.as_mut_ptr().cast(),
            info.len() as u32,
            &mut returned,
        )
    };
    if status >= 0 || returned > 0 {
        return Err(invalid("目标位于已注册的云同步根目录"));
    }
    Ok(PathBuf::from(plain).join(
        path.strip_prefix(&existing)
            .map_err(|_| invalid("数据路径无法规范化"))?,
    ))
}
/// Move the verified copy out of its staging folder into the destination. A
/// failed move puts back what had moved so the stage cleans up as one piece.
fn adopt_stage(stage: &Path, destination: &Path) -> Result<()> {
    let names = fs::read_dir(stage)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<std::io::Result<Vec<_>>>()?;
    for (index, name) in names.iter().enumerate() {
        if let Err(error) = fs::rename(stage.join(name), destination.join(name)) {
            for moved in names[..index].iter().rev() {
                let _ = fs::rename(destination.join(moved), stage.join(moved));
            }
            return Err(error.into());
        }
    }
    Ok(())
}
/// What a relocation is doing, for a progress display.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RelocationStep {
    /// Checking the current dataset before anything is copied.
    Checking,
    /// Copying files; the counts are bytes.
    Copying,
    /// Opening the copy and reading every image back; the counts are images.
    Verifying,
    /// Pointing Timelens at the copy and removing the original.
    Switching,
}
/// Copy in chunks so a large database still reports progress as it goes.
fn copy_counted(source: &Path, destination: &Path, copied: &mut dyn FnMut(u64)) -> Result<()> {
    use std::io::{Read, Write};
    let mut source = File::open(source)?;
    let mut target = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(destination)?;
    let mut buffer = vec![0_u8; 1 << 20];
    loop {
        let read = source.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        target.write_all(&buffer[..read])?;
        copied(read as u64);
    }
    target.sync_all()?;
    Ok(())
}
pub(crate) fn owned_snapshot_name(name: &str) -> bool {
    name.strip_suffix(".tlsnap")
        .is_some_and(|s| s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()))
}
impl Storage {
    /// Read only the credential identifiers owned by this dataset. This permits
    /// scoped maintenance without enumerating other profiles in Credential Manager.
    pub fn dataset_credential_ids(directory: &Path) -> Result<Vec<String>> {
        let directory = validate_fixed_directory(directory)?;
        let database = directory.join(DATABASE_FILE);
        if !database.exists() {
            return Ok(vec![]);
        }
        let key = load_key(&directory.join(KEY_FILE))?;
        let connection = open_encrypted(&database, &key, true)?;
        let exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='ai_profiles')",
            [],
            |r| r.get(0),
        )?;
        if !exists {
            return Ok(vec![]);
        }
        Ok(connection
            .prepare("SELECT id FROM ai_profiles")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }
    pub fn control_directory(&self) -> &Path {
        &self.control_directory
    }
    pub fn set_control_directory(&mut self, directory: &Path) -> Result<()> {
        self.control_directory = directory.to_path_buf();
        self.sync_collection_policy()
    }
    pub fn validate_data_destination(path: &Path) -> Result<PathBuf> {
        validate_fixed_directory(path)
    }
    pub fn holds_dataset(directory: &Path) -> bool {
        directory.join(DATABASE_FILE).exists()
    }
    /// Copy authenticated ciphertext, verify every image and database, then publish
    /// the caller's atomic pointer. The collector control directory never migrates.
    pub fn relocate(
        &mut self,
        destination: &Path,
        publish: impl FnOnce(&Path) -> Result<()>,
        mut progress: impl FnMut(RelocationStep, u64, u64),
    ) -> Result<u64> {
        progress(RelocationStep::Checking, 0, 1);
        self.ensure_writable()?;
        let destination = validate_fixed_directory(destination)?;
        let source = fs::canonicalize(&self.data_directory)?;
        let source_text = source
            .to_string_lossy()
            .trim_start_matches(r"\\?\")
            .to_lowercase();
        let target_text = destination
            .to_string_lossy()
            .trim_end_matches(['\\', '/'])
            .to_lowercase();
        if target_text == source_text
            || target_text.starts_with(&(source_text.clone() + "\\"))
            || source_text.starts_with(&(target_text.clone() + "\\"))
        {
            return Err(invalid("新旧数据目录不能相同或互相包含"));
        }
        let created = !destination.exists();
        if !created && fs::read_dir(&destination)?.next().is_some() {
            return Err(invalid("目标目录必须为空；V1 不合并数据集"));
        }
        match fs::create_dir_all(&destination) {
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                return Err(invalid(
                    "无法在这个位置创建目录。请选择有写入权限的位置，或先建好一个空目录再迁移",
                ));
            }
            result => result?,
        }
        // Staging inside the destination needs write access only to it, so an
        // empty directory prepared under a read-only parent (such as the
        // installer's Data folder) works too.
        let result = self.relocate_into(&destination, publish, &mut progress);
        if result.is_err() && created {
            let _ = fs::remove_dir(&destination);
        }
        result
    }
    fn relocate_into(
        &mut self,
        destination: &Path,
        publish: impl FnOnce(&Path) -> Result<()>,
        progress: &mut dyn FnMut(RelocationStep, u64, u64),
    ) -> Result<u64> {
        self.checkpoint()?;
        self.verify_integrity()?;
        let stage = tempfile::Builder::new()
            .prefix("timelens-move-")
            .tempdir_in(destination)?;
        let names = self
            .connection
            .prepare("SELECT file_name FROM snapshot_blobs")?
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if !names.iter().all(|name| owned_snapshot_name(name)) {
            return Err(invalid("数据集快照路径无效"));
        }
        let mut files = vec![
            (self.database_path.clone(), stage.path().join(DATABASE_FILE)),
            (self.key_path.clone(), stage.path().join(KEY_FILE)),
        ];
        files.extend(names.iter().map(|name| {
            (
                self.data_directory.join("snapshots").join(name),
                stage.path().join("snapshots").join(name),
            )
        }));
        let total = files
            .iter()
            .map(|(source, _)| fs::metadata(source).map(|m| m.len()))
            .sum::<std::io::Result<u64>>()?
            .max(1);
        fs::create_dir_all(stage.path().join("snapshots"))?;
        let mut copied = 0_u64;
        progress(RelocationStep::Copying, 0, total);
        for (source, target) in &files {
            copy_counted(source, target, &mut |bytes| {
                copied += bytes;
                progress(RelocationStep::Copying, copied.min(total), total);
            })?;
        }
        {
            let staged = Storage::open(stage.path())?;
            staged.verify_integrity()?;
            let ids=staged.connection.prepare("SELECT MIN(slot_id) FROM snapshot_slots WHERE blob_id IS NOT NULL GROUP BY blob_id")?.query_map([],|r|r.get::<_,i64>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            let count = ids.len() as u64;
            progress(RelocationStep::Verifying, 0, count.max(1));
            for (index, id) in ids.into_iter().enumerate() {
                staged.load_snapshot_image(id)?;
                progress(RelocationStep::Verifying, index as u64 + 1, count);
            }
            staged.checkpoint()?;
        }
        progress(RelocationStep::Switching, 0, 1);
        adopt_stage(stage.path(), destination)?;
        let mut replacement = Storage::open(destination)?;
        replacement.set_control_directory(&self.control_directory)?;
        replacement.checkpoint()?;
        if let Err(error) = publish(destination) {
            return Err(invalid(format!(
                "切换未完成，原数据仍使用；已验证的副本位于 {}。{error}",
                destination.display()
            )));
        }
        let old = std::mem::replace(self, replacement);
        let old_database = old.database_path.clone();
        let old_key = old.key_path.clone();
        let old_directory = old.data_directory.clone();
        drop(old);
        // Only known product-owned files are removed. Other files, including user
        // exports in the directory, remain outside Timelens cleanup control.
        let mut residual_files = 0;
        for file in [
            &old_database,
            &wal_path(&old_database),
            &shm_path(&old_database),
            &old_key,
        ] {
            if remove_file_if_present(file).is_err() {
                residual_files += 1;
            }
        }
        for name in names {
            if remove_file_if_present(&old_directory.join("snapshots").join(name)).is_err() {
                residual_files += 1;
            }
        }
        let _ = fs::remove_dir(old_directory.join("snapshots"));
        if old_directory != self.control_directory {
            if remove_file_if_present(&old_directory.join(timelens_ipc::privacy::POLICY_FILE))
                .is_err()
            {
                residual_files += 1;
            }
            let _ = fs::remove_dir(&old_directory);
        }
        progress(RelocationStep::Switching, 1, 1);
        Ok(residual_files)
    }
    pub fn delete_owned_dataset(directory: &Path) -> Result<()> {
        let directory = validate_fixed_directory(directory)?;
        if !directory.exists() {
            return Ok(());
        }
        // Never recurse into a user-selected directory. Fixed owned files only.
        let snapshots = directory.join("snapshots");
        if snapshots.is_dir() {
            validate_fixed_directory(&snapshots)?;
            for entry in fs::read_dir(&snapshots)? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                if entry.file_type()?.is_file() && owned_snapshot_name(&name) {
                    let mut file = File::open(entry.path())?;
                    let mut header = [0; 8];
                    use std::io::Read;
                    if file.read_exact(&mut header).is_ok() && &header == b"TLSNAP\0\x01" {
                        drop(file);
                        fs::remove_file(entry.path())?;
                    }
                }
            }
            let _ = fs::remove_dir(&snapshots);
        }
        for name in [
            DATABASE_FILE,
            KEY_FILE,
            BACKUP_FILE,
            "timelens.sqlite3-wal",
            "timelens.sqlite3-shm",
            "timelens.sqlite3-journal",
            timelens_ipc::privacy::POLICY_FILE,
            "collection-policy.next",
            COLLECTOR_SPOOL_FILE,
            COLLECTOR_SPOOL_KEY_FILE,
            COLLECTOR_TRAY_STATE_FILE,
            COLLECTOR_RESET_REQUEST_FILE,
            COLLECTOR_RESET_PAUSED_FILE,
            "collector-stop.request",
        ] {
            remove_file_if_present(&directory.join(name))?;
        }
        remove_quarantined_collector_state(&directory)?;
        let _ = fs::remove_dir(directory);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failed_pointer_publish_preserves_source_and_verified_destination() {
        let (source, mut storage) = ai::tests::fixture();
        let parent = tempfile::tempdir().unwrap();
        let target = parent.path().join("new-dataset");
        storage.checkpoint().unwrap();
        let key = fs::read(source.path().join(KEY_FILE)).unwrap();
        assert!(
            storage
                .relocate(&target, |_| Err(invalid("publish failed")), |_, _, _| {})
                .is_err()
        );
        assert_eq!(storage.data_directory(), source.path());
        assert_eq!(fs::read(source.path().join(KEY_FILE)).unwrap(), key);
        storage.verify_integrity().unwrap();
        Storage::open(&target).unwrap().verify_integrity().unwrap();
    }
    #[test]
    fn relocation_fills_an_existing_empty_directory_without_leaving_its_stage() {
        let (_source, mut storage) = ai::tests::fixture();
        let parent = tempfile::tempdir().unwrap();
        let target = parent.path().join("prepared");
        fs::create_dir(&target).unwrap();
        let mut steps = Vec::new();
        assert_eq!(
            storage
                .relocate(
                    &target,
                    |_| Ok(()),
                    |step, done, total| steps.push((step, done, total))
                )
                .unwrap(),
            0
        );
        assert_eq!(storage.data_directory(), target);
        storage.verify_integrity().unwrap();
        // Progress starts with the check, copies every byte, and ends switched.
        assert_eq!(steps.first(), Some(&(RelocationStep::Checking, 0, 1)));
        assert!(
            steps
                .iter()
                .any(|(step, done, total)| *step == RelocationStep::Copying
                    && done == total
                    && *total > 0)
        );
        assert_eq!(steps.last(), Some(&(RelocationStep::Switching, 1, 1)));
        let names = fs::read_dir(&target)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(names.iter().all(|name| !name.starts_with("timelens-move-")));
        assert!(names.iter().any(|name| name == DATABASE_FILE));
        assert!(Storage::holds_dataset(&target));
    }
    #[test]
    fn relocation_works_in_a_prepared_directory_under_a_read_only_parent() {
        let (_source, mut storage) = ai::tests::fixture();
        let parent = tempfile::tempdir().unwrap();
        let target = parent.path().join("Data");
        fs::create_dir(&target).unwrap();
        // Like an installation directory: nothing new can be added to the parent.
        let user = format!(
            "{}\\{}",
            std::env::var("USERDOMAIN").unwrap(),
            std::env::var("USERNAME").unwrap()
        );
        let denied = std::process::Command::new("icacls")
            .arg(parent.path())
            .args(["/deny", &format!("{user}:(AD,WD)")])
            .output()
            .unwrap();
        assert!(denied.status.success());
        assert!(fs::create_dir(parent.path().join("probe")).is_err());

        assert_eq!(
            storage.relocate(&target, |_| Ok(()), |_, _, _| {}).unwrap(),
            0
        );
        storage.verify_integrity().unwrap();
        let error = storage
            .relocate(&parent.path().join("Other"), |_| Ok(()), |_, _, _| {})
            .unwrap_err();
        assert!(error.to_string().contains("无法在这个位置创建目录"));
        assert_eq!(storage.data_directory(), target);
    }
    #[test]
    fn relocation_and_uninstall_leave_unrelated_exports_intact() {
        let (source, mut storage) = ai::tests::fixture();
        let parent = tempfile::tempdir().unwrap();
        let target = parent.path().join("new-dataset");
        fs::write(source.path().join("my-backup.zip"), b"user-owned").unwrap();
        assert_eq!(
            storage.relocate(&target, |_| Ok(()), |_, _, _| {}).unwrap(),
            0
        );
        assert_eq!(storage.data_directory(), target);
        assert!(!source.path().join(DATABASE_FILE).exists());
        assert_eq!(
            fs::read(source.path().join("my-backup.zip")).unwrap(),
            b"user-owned"
        );
        fs::write(target.join("my-report.html"), b"external export").unwrap();
        drop(storage);
        Storage::delete_owned_dataset(&target).unwrap();
        assert!(!target.join(DATABASE_FILE).exists());
        assert_eq!(
            fs::read(target.join("my-report.html")).unwrap(),
            b"external export"
        );
        assert!(validate_fixed_directory(&target.join("..\\other")).is_err());
    }
}
