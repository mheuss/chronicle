use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::error::Result;

const FILE_MODE: u32 = 0o600;

/// Owns the base directory and all file lifecycle operations.
/// Permission policy is enforced here: files get 0o600, directories get 0o700.
pub(crate) struct MediaManager {
    base_dir: PathBuf,
}

impl MediaManager {
    pub(crate) fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    pub(crate) fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    /// Write bytes to path, then set file permissions to owner-only (0o600).
    pub(crate) fn write_file(&self, path: &Path, data: &[u8]) -> Result<()> {
        std::fs::write(path, data)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(FILE_MODE))?;
        Ok(())
    }

    /// Move (rename) a file from one path to another, then set 0o600.
    pub(crate) fn move_file(&self, from: &Path, to: &Path) -> Result<()> {
        std::fs::rename(from, to)?;
        std::fs::set_permissions(to, std::fs::Permissions::from_mode(FILE_MODE))?;
        Ok(())
    }

    /// Delete a file, returning bytes freed. Returns Ok(0) if the file is already gone.
    pub(crate) fn delete_file(&self, path: &Path) -> Result<u64> {
        let size = match std::fs::metadata(path) {
            Ok(meta) => meta.len(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e.into()),
        };
        std::fs::remove_file(path)?;
        Ok(size)
    }

    /// Set owner-only permissions (0o600) on an existing file.
    /// Use after external writes (e.g. encode_heif) that bypass MediaManager.
    ///
    /// Design deviation: the design specifies routing all writes through
    /// write_file(). However, encode_heif is an external function from the
    /// capture crate that writes directly to a path and cannot be wrapped.
    /// harden_file closes the permission window immediately after the write.
    pub(crate) fn harden_file(&self, path: &Path) -> Result<()> {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(FILE_MODE))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn write_file_sets_owner_only_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = MediaManager::new(dir.path().to_path_buf());
        let path = dir.path().join("test.dat");
        mgr.write_file(&path, b"secret data").unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "file should be owner-only (0o600), got {:#o}", mode);
        assert_eq!(std::fs::read(&path).unwrap(), b"secret data");
    }

    #[test]
    fn move_file_sets_owner_only_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = MediaManager::new(dir.path().to_path_buf());

        let src = dir.path().join("src.dat");
        std::fs::write(&src, b"audio data").unwrap();

        let dest = dir.path().join("dest.dat");
        mgr.move_file(&src, &dest).unwrap();

        assert!(!src.exists(), "source should be removed after move");
        let mode = std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "moved file should be 0o600, got {:#o}", mode);
        assert_eq!(std::fs::read(&dest).unwrap(), b"audio data");
    }

    #[test]
    fn delete_file_returns_bytes_freed() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = MediaManager::new(dir.path().to_path_buf());
        let path = dir.path().join("deleteme.dat");
        std::fs::write(&path, &[0u8; 256]).unwrap();

        let freed = mgr.delete_file(&path).unwrap();
        assert_eq!(freed, 256);
        assert!(!path.exists());
    }

    #[test]
    fn delete_file_returns_zero_for_missing() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = MediaManager::new(dir.path().to_path_buf());
        let freed = mgr.delete_file(&dir.path().join("nope.dat")).unwrap();
        assert_eq!(freed, 0);
    }

    #[test]
    fn harden_file_sets_permissions_on_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = MediaManager::new(dir.path().to_path_buf());
        let path = dir.path().join("existing.dat");
        std::fs::write(&path, b"content").unwrap();

        mgr.harden_file(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
