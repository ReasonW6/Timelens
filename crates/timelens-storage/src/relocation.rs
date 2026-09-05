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
    /// Copy authenticated ciphertext, verify every image and database, then publish
    /// the caller's atomic pointer. The collector control directory never migrates.
    pub fn relocate(
        &mut self,
        destination: &Path,
        publish: impl FnOnce(&Path) -> Result<()>,
    ) -> Result<u64> {
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
        if destination.exists() && fs::read_dir(&destination)?.next().is_some() {
            return Err(invalid("目标目录必须为空；V1 不合并数据集"));
        }
        let parent = destination
            .parent()
            .ok_or_else(|| invalid("目标父目录无效"))?;
        fs::create_dir_all(parent)?;
        self.checkpoint()?;
        self.verify_integrity()?;
        let stage = tempfile::Builder::new()
            .prefix("timelens-move-")
            .tempdir_in(parent)?;
        copy_and_sync(&self.database_path, &stage.path().join(DATABASE_FILE))?;
        copy_and_sync(&self.key_path, &stage.path().join(KEY_FILE))?;
        fs::create_dir_all(stage.path().join("snapshots"))?;
        let names = self
            .connection
            .prepare("SELECT file_name FROM snapshot_blobs")?
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for name in &names {
            if !owned_snapshot_name(name) {
                return Err(invalid("数据集快照路径无效"));
            }
            copy_and_sync(
                &self.data_directory.join("snapshots").join(name),
                &stage.path().join("snapshots").join(name),
            )?;
        }
        {
            let staged = Storage::open(stage.path())?;
            staged.verify_integrity()?;
            let ids=staged.connection.prepare("SELECT MIN(slot_id) FROM snapshot_slots WHERE blob_id IS NOT NULL GROUP BY blob_id")?.query_map([],|r|r.get::<_,i64>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            for id in ids {
                staged.load_snapshot_image(id)?;
            }
            staged.checkpoint()?;
        }
        if destination.exists() {
            fs::remove_dir(&destination)?;
        }
        fs::rename(stage.path(), &destination)?;
        let mut replacement = Storage::open(&destination)?;
        replacement.set_control_directory(&self.control_directory)?;
        replacement.checkpoint()?;
        if let Err(error) = publish(&destination) {
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
                .relocate(&target, |_| Err(invalid("publish failed")))
                .is_err()
        );
        assert_eq!(storage.data_directory(), source.path());
        assert_eq!(fs::read(source.path().join(KEY_FILE)).unwrap(), key);
        storage.verify_integrity().unwrap();
        Storage::open(&target).unwrap().verify_integrity().unwrap();
    }
    #[test]
    fn relocation_and_uninstall_leave_unrelated_exports_intact() {
        let (source, mut storage) = ai::tests::fixture();
        let parent = tempfile::tempdir().unwrap();
        let target = parent.path().join("new-dataset");
        fs::write(source.path().join("my-backup.zip"), b"user-owned").unwrap();
        assert_eq!(storage.relocate(&target, |_| Ok(())).unwrap(), 0);
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
