use std::path::{Path, PathBuf};

use rusqlite::{params, Connection};

use crate::error::Result;
use crate::models::CleanupStats;

const BATCH_SIZE: usize = 500;

/// Delete expired screenshots and audio segments in batches.
/// Deletes DB rows first, then removes associated files.
pub(crate) fn run_cleanup(conn: &Connection, retention_days: i64) -> Result<CleanupStats> {
    let now_millis = chrono::Utc::now().timestamp_millis();
    let cutoff = now_millis - retention_days * 86_400 * 1000;

    let mut stats = CleanupStats::default();

    // Delete expired screenshots in batches
    loop {
        let paths: Vec<String> = {
            let mut stmt = conn.prepare(
                "SELECT image_path FROM screenshots WHERE timestamp < ?1 LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![cutoff, BATCH_SIZE as i64], |row| {
                row.get::<_, String>(0)
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };

        if paths.is_empty() {
            break;
        }

        let count = paths.len();
        conn.execute(
            "DELETE FROM screenshots WHERE id IN (
                SELECT id FROM screenshots WHERE timestamp < ?1 LIMIT ?2
            )",
            params![cutoff, BATCH_SIZE as i64],
        )?;

        for path in &paths {
            let bytes = delete_file_if_exists(Path::new(path));
            stats.bytes_freed += bytes;
        }

        stats.screenshots_deleted += count;

        if count < BATCH_SIZE {
            break;
        }
    }

    // Delete expired audio segments in batches
    loop {
        let paths: Vec<String> = {
            let mut stmt = conn.prepare(
                "SELECT audio_path FROM audio_segments WHERE start_timestamp < ?1 LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![cutoff, BATCH_SIZE as i64], |row| {
                row.get::<_, String>(0)
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };

        if paths.is_empty() {
            break;
        }

        let count = paths.len();
        conn.execute(
            "DELETE FROM audio_segments WHERE id IN (
                SELECT id FROM audio_segments WHERE start_timestamp < ?1 LIMIT ?2
            )",
            params![cutoff, BATCH_SIZE as i64],
        )?;

        for path in &paths {
            let bytes = delete_file_if_exists(Path::new(path));
            stats.bytes_freed += bytes;
        }

        stats.audio_segments_deleted += count;

        if count < BATCH_SIZE {
            break;
        }
    }

    Ok(stats)
}

/// Walk screenshots/ and audio/ directories, delete any files not tracked in the DB.
pub(crate) fn sweep_orphans(conn: &Connection, base_dir: &Path) -> Result<u64> {
    let mut bytes_freed: u64 = 0;

    let screenshots_dir = base_dir.join("screenshots");
    if screenshots_dir.exists() {
        bytes_freed += sweep_directory(conn, &screenshots_dir, "screenshots", "image_path")?;
    }

    let audio_dir = base_dir.join("audio");
    if audio_dir.exists() {
        bytes_freed += sweep_directory(conn, &audio_dir, "audio_segments", "audio_path")?;
    }

    Ok(bytes_freed)
}

fn sweep_directory(
    conn: &Connection,
    dir: &Path,
    table: &str,
    path_column: &str,
) -> Result<u64> {
    let mut bytes_freed: u64 = 0;
    let files = walkdir(dir);

    for file_path in files {
        let path_str = file_path.to_string_lossy().to_string();
        let query = format!("SELECT COUNT(*) FROM {} WHERE {} = ?1", table, path_column);
        let count: i64 = conn.query_row(&query, params![path_str], |row| row.get(0))?;

        if count == 0 {
            bytes_freed += delete_file_if_exists(&file_path);
        }
    }

    Ok(bytes_freed)
}

fn walkdir(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    walk_recursive(dir, &mut files);
    files
}

fn walk_recursive(dir: &Path, files: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        if path.is_dir() {
            walk_recursive(&path, files);
        } else {
            files.push(path);
        }
    }
}

fn delete_file_if_exists(path: &Path) -> u64 {
    let size = match std::fs::metadata(path) {
        Ok(meta) => meta.len(),
        Err(_) => return 0,
    };
    match std::fs::remove_file(path) {
        Ok(()) => size,
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AudioSegmentMetadata, ScreenshotMetadata};
    use crate::schema;
    use crate::{audio, screenshots};

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::setup_connection(&conn).unwrap();
        schema::migrate(&conn).unwrap();
        conn
    }

    fn now_millis() -> i64 {
        chrono::Utc::now().timestamp_millis()
    }

    #[test]
    fn cleanup_deletes_expired_screenshots() {
        let conn = setup_db();
        let now = now_millis();
        let old_ts = now - 31 * 86_400 * 1000; // 31 days ago
        let new_ts = now - 1 * 86_400 * 1000; // 1 day ago

        let old_meta = ScreenshotMetadata {
            timestamp: old_ts,
            display_id: "display1".into(),
            app_name: None,
            app_bundle_id: None,
            window_title: None,
            image_path: "/tmp/old_shot.heif".into(),
            ocr_text: None,
            phash: None,
            resolution: None,
        };
        let new_meta = ScreenshotMetadata {
            timestamp: new_ts,
            display_id: "display1".into(),
            app_name: None,
            app_bundle_id: None,
            window_title: None,
            image_path: "/tmp/new_shot.heif".into(),
            ocr_text: None,
            phash: None,
            resolution: None,
        };

        screenshots::insert(&conn, &old_meta).unwrap();
        let new_id = screenshots::insert(&conn, &new_meta).unwrap();

        let stats = run_cleanup(&conn, 30).unwrap();
        assert_eq!(stats.screenshots_deleted, 1);

        // New screenshot should still exist
        let remaining = screenshots::get(&conn, new_id).unwrap();
        assert_eq!(remaining.timestamp, new_ts);

        // Total should be 1
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM screenshots", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn cleanup_deletes_expired_audio() {
        let conn = setup_db();
        let now = now_millis();
        let old_ts = now - 31 * 86_400 * 1000;
        let new_ts = now - 1 * 86_400 * 1000;

        let old_meta = AudioSegmentMetadata {
            start_timestamp: old_ts,
            end_timestamp: old_ts + 30_000,
            source: "mic".into(),
            audio_path: "/tmp/old_audio.opus".into(),
            transcript: None,
            whisper_model: None,
            language: None,
        };
        let new_meta = AudioSegmentMetadata {
            start_timestamp: new_ts,
            end_timestamp: new_ts + 30_000,
            source: "mic".into(),
            audio_path: "/tmp/new_audio.opus".into(),
            transcript: None,
            whisper_model: None,
            language: None,
        };

        audio::insert(&conn, &old_meta).unwrap();
        let new_id = audio::insert(&conn, &new_meta).unwrap();

        let stats = run_cleanup(&conn, 30).unwrap();
        assert_eq!(stats.audio_segments_deleted, 1);

        let remaining = audio::get(&conn, new_id).unwrap();
        assert_eq!(remaining.start_timestamp, new_ts);

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM audio_segments", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn cleanup_deletes_associated_files() {
        let conn = setup_db();
        let now = now_millis();
        let old_ts = now - 31 * 86_400 * 1000;

        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("old_shot.heif");
        std::fs::write(&file_path, b"fake image data").unwrap();
        assert!(file_path.exists());

        let meta = ScreenshotMetadata {
            timestamp: old_ts,
            display_id: "display1".into(),
            app_name: None,
            app_bundle_id: None,
            window_title: None,
            image_path: file_path.to_string_lossy().into_owned(),
            ocr_text: None,
            phash: None,
            resolution: None,
        };
        screenshots::insert(&conn, &meta).unwrap();

        let stats = run_cleanup(&conn, 30).unwrap();
        assert_eq!(stats.screenshots_deleted, 1);
        assert!(stats.bytes_freed > 0);
        assert!(!file_path.exists());
    }

    #[test]
    fn cleanup_handles_missing_files_gracefully() {
        let conn = setup_db();
        let now = now_millis();
        let old_ts = now - 31 * 86_400 * 1000;

        let meta = ScreenshotMetadata {
            timestamp: old_ts,
            display_id: "display1".into(),
            app_name: None,
            app_bundle_id: None,
            window_title: None,
            image_path: "/tmp/nonexistent_file_12345.heif".into(),
            ocr_text: None,
            phash: None,
            resolution: None,
        };
        screenshots::insert(&conn, &meta).unwrap();

        // Should not error even though the file doesn't exist
        let stats = run_cleanup(&conn, 30).unwrap();
        assert_eq!(stats.screenshots_deleted, 1);
        assert_eq!(stats.bytes_freed, 0);
    }

    #[test]
    fn sweep_orphans_deletes_untracked_files() {
        let conn = setup_db();
        let dir = tempfile::tempdir().unwrap();

        // Create a screenshots subdirectory with an orphan file
        let screenshots_dir = dir.path().join("screenshots");
        std::fs::create_dir_all(&screenshots_dir).unwrap();
        let orphan_file = screenshots_dir.join("orphan.heif");
        std::fs::write(&orphan_file, b"orphan data").unwrap();
        assert!(orphan_file.exists());

        let bytes_freed = sweep_orphans(&conn, dir.path()).unwrap();
        assert!(bytes_freed > 0);
        assert!(!orphan_file.exists());
    }
}
