use std::path::{Path, PathBuf};
use chrono::{DateTime, Datelike, Utc};
use crate::error::Result;

pub(crate) fn screenshot_path(base_dir: &Path, timestamp: i64, display_id: &str) -> PathBuf {
    let (year, month, day) = date_parts(timestamp);
    base_dir
        .join("screenshots")
        .join(format!("{}/{:02}/{:02}", year, month, day))
        .join(format!("{}_{}.heif", timestamp, display_id))
}

pub(crate) fn audio_path(base_dir: &Path, timestamp: i64, source: &str) -> PathBuf {
    let (year, month, day) = date_parts(timestamp);
    base_dir
        .join("audio")
        .join(format!("{}/{:02}/{:02}", year, month, day))
        .join(format!("{}_{}.opus", timestamp, source))
}

pub(crate) fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
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
}
