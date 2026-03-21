use std::path::{Path, PathBuf};
use chrono::{DateTime, Datelike, Utc};
use crate::error::Result;

/// Sanitize a user-supplied identifier (display_id, source) so it cannot
/// escape the intended directory. Replaces `/`, `\`, `..`, and null bytes
/// with `_`.
fn sanitize_id(input: &str) -> String {
    input
        .replace("..", "_")
        .replace(['/', '\\', '\0'], "_")
}

pub(crate) fn screenshot_path(base_dir: &Path, timestamp: i64, display_id: &str) -> PathBuf {
    let safe_id = sanitize_id(display_id);
    let (year, month, day) = date_parts(timestamp);
    base_dir
        .join("screenshots")
        .join(format!("{}/{:02}/{:02}", year, month, day))
        .join(format!("{}_{}.heif", timestamp, safe_id))
}

pub(crate) fn audio_path(base_dir: &Path, timestamp: i64, source: &str) -> PathBuf {
    let safe_source = sanitize_id(source);
    let (year, month, day) = date_parts(timestamp);
    base_dir
        .join("audio")
        .join(format!("{}/{:02}/{:02}", year, month, day))
        .join(format!("{}_{}.opus", timestamp, safe_source))
}

pub(crate) fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

/// Recursively collect all file paths under `dir`.
pub(crate) fn walk_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    walk_files_recursive(dir, &mut files)?;
    Ok(files)
}

fn walk_files_recursive(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    let entries = std::fs::read_dir(dir)?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            walk_files_recursive(&path, files)?;
        } else {
            files.push(path);
        }
    }
    Ok(())
}

/// Sum the size (in bytes) of all files under `path`, recursively.
pub(crate) fn dir_size(path: &Path) -> u64 {
    if !path.exists() {
        return 0;
    }
    let mut total: u64 = 0;
    dir_size_recursive(path, &mut total);
    total
}

fn dir_size_recursive(path: &Path, total: &mut u64) {
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let p = entry.path();
        if p.is_dir() {
            dir_size_recursive(&p, total);
        } else if let Ok(meta) = std::fs::metadata(&p) {
            *total += meta.len();
        }
    }
}

fn date_parts(timestamp_millis: i64) -> (i32, u32, u32) {
    let dt = DateTime::<Utc>::from_timestamp_millis(timestamp_millis)
        .unwrap_or_else(|| DateTime::<Utc>::from_timestamp(0, 0).unwrap());
    (dt.year(), dt.month(), dt.day())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn screenshot_path_has_correct_structure() {
        let base = PathBuf::from("/data");
        // 2026-03-21 12:00:00.000 UTC = 1774094400000 ms
        let ts: i64 = 1774094400000;
        let path = screenshot_path(&base, ts, "display1");

        assert_eq!(
            path,
            PathBuf::from("/data/screenshots/2026/03/21/1774094400000_display1.heif")
        );
    }

    #[test]
    fn audio_path_has_correct_structure() {
        let base = PathBuf::from("/data");
        let ts: i64 = 1774094400000;
        let path = audio_path(&base, ts, "mic");

        assert_eq!(
            path,
            PathBuf::from("/data/audio/2026/03/21/1774094400000_mic.opus")
        );
    }

    #[test]
    fn ensure_parent_dir_creates_nested_directories() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("a/b/c/file.txt");
        ensure_parent_dir(&file_path).unwrap();
        assert!(file_path.parent().unwrap().is_dir());
    }

    #[test]
    fn sanitize_id_replaces_path_separators() {
        assert_eq!(sanitize_id("../etc/passwd"), "__etc_passwd");
        assert_eq!(sanitize_id("display/1"), "display_1");
        assert_eq!(sanitize_id("foo\\bar"), "foo_bar");
        assert_eq!(sanitize_id("ok\0bad"), "ok_bad");
    }

    #[test]
    fn screenshot_path_sanitizes_display_id() {
        let base = PathBuf::from("/data");
        let ts: i64 = 1774094400000;
        let path = screenshot_path(&base, ts, "../evil");
        // Should not escape the screenshots directory
        assert!(!path.to_string_lossy().contains(".."));
    }

    #[test]
    fn walk_files_collects_recursively() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("a/b");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(dir.path().join("top.txt"), b"x").unwrap();
        std::fs::write(sub.join("nested.txt"), b"y").unwrap();

        let files = walk_files(dir.path()).unwrap();
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn dir_size_sums_file_sizes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.bin"), &[0u8; 100]).unwrap();
        std::fs::write(dir.path().join("b.bin"), &[0u8; 200]).unwrap();

        let size = dir_size(dir.path());
        assert_eq!(size, 300);
    }
}
