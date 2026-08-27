//! Deciding whether a media file backing a database row is gone.
//!
//! Nothing here deletes or modifies anything. It answers one question for the
//! counters exposed over Status IPC: was this path genuinely absent?

use std::path::Path;

/// Is this stat result evidence the file is gone?
///
/// Only `NotFound` counts. Every other error — permissions, I/O, an
/// interrupted syscall — means "we could not tell", and a metric that counts
/// "could not tell" as "missing" manufactures the very signal it exists to
/// measure.
///
/// Split from [`media_is_absent`] so error classes that cannot be produced
/// from a real filesystem (`Interrupted`, `Other`) are still testable. A real
/// unreadable-parent fixture is not reliable — `chmod 000` does not
/// consistently make `metadata` fail — which is why the discipline is pinned
/// here against constructed errors instead.
pub(crate) fn classify_absent<T>(result: &std::io::Result<T>) -> bool {
    matches!(result, Err(e) if e.kind() == std::io::ErrorKind::NotFound)
}

/// Does the file at `path` fail to exist?
///
/// Follows symlinks, matching how the UI reads the file. Never mutates.
pub(crate) fn media_is_absent(path: &Path) -> bool {
    classify_absent(&std::fs::metadata(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error, ErrorKind};

    /// Build an `io::Result<()>` carrying a specific kind, so error classes
    /// that cannot be produced from a real filesystem are still testable.
    fn err(kind: ErrorKind) -> std::io::Result<()> {
        Err(Error::new(kind, "test"))
    }

    #[test]
    fn a_present_file_is_not_absent() {
        assert!(!classify_absent(&Ok(())));
    }

    #[test]
    fn not_found_is_absent() {
        assert!(classify_absent(&err(ErrorKind::NotFound)));
    }

    #[test]
    fn permission_denied_is_not_absent() {
        // THE regression. A blanket `Err(_) => true` passes every other test
        // in this module and fails only this one.
        assert!(!classify_absent(&err(ErrorKind::PermissionDenied)));
    }

    #[test]
    fn an_io_error_is_not_absent() {
        assert!(!classify_absent(&err(ErrorKind::Other)));
    }

    #[test]
    fn an_interrupted_call_is_not_absent() {
        assert!(!classify_absent(&err(ErrorKind::Interrupted)));
    }

    #[test]
    fn a_real_missing_path_is_absent() {
        // Inside a TempDir rather than a hard-coded /nonexistent path, so the
        // test cannot be defeated by something existing at that location.
        let dir = tempfile::tempdir().unwrap();
        assert!(media_is_absent(&dir.path().join("never-created.heif")));
    }

    #[test]
    fn a_real_present_path_is_not_absent() {
        let f = tempfile::NamedTempFile::new().unwrap();
        assert!(!media_is_absent(f.path()));
    }

    #[test]
    fn a_dangling_symlink_is_absent() {
        // Pins `metadata` over `symlink_metadata`. Every other case in this
        // module is symlink-blind, so without this one, swapping line 28 to
        // `symlink_metadata` silently stops counting dangling links and the
        // whole suite stays green. The link resolves to nothing, so the UI
        // cannot read it either — which is the alignment the doc claims.
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("link.heif");
        std::os::unix::fs::symlink(dir.path().join("never-created.heif"), &link).unwrap();
        assert!(media_is_absent(&link));
    }

    #[test]
    fn a_directory_is_not_absent() {
        // `metadata` succeeds on a directory. Present is present — this must
        // never count as a missing media file.
        let dir = tempfile::tempdir().unwrap();
        assert!(!media_is_absent(dir.path()));
    }
}
