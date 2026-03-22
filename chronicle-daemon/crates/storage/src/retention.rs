use std::path::Path;

use rusqlite::{params, Connection};

use crate::error::Result;
use crate::files;
use crate::models::CleanupStats;

// Must stay below SQLite's SQLITE_MAX_VARIABLE_NUMBER (default 999).
const CLEANUP_BATCH_SIZE: usize = 500;

/// Delete expired screenshots and audio segments in batches.
/// Deletes DB rows first, then removes associated files.
pub(crate) fn run_cleanup(conn: &Connection, retention_days: i64) -> Result<CleanupStats> {
    // A retention of 0 or negative days is invalid — treat it as a no-op.
    if retention_days <= 0 {
        return Ok(CleanupStats::default());
    }

    let now_millis = chrono::Utc::now().timestamp_millis();
    let cutoff = now_millis - retention_days * 86_400 * 1000;

    let mut stats = CleanupStats::default();

    // Delete expired screenshots in batches
    loop {
        let tx = conn.unchecked_transaction()?;

        let batch: Vec<(i64, String)> = {
            let mut stmt = tx.prepare(
                "SELECT id, image_path FROM screenshots WHERE timestamp < ?1 LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![cutoff, CLEANUP_BATCH_SIZE as i64], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };

        if batch.is_empty() {
            tx.commit()?;
            break;
        }

        let count = batch.len();
        let ids: Vec<i64> = batch.iter().map(|(id, _)| *id).collect();
        let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!("DELETE FROM screenshots WHERE id IN ({})", placeholders);
        let id_params: Vec<&dyn rusqlite::ToSql> =
            ids.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
        tx.execute(&sql, id_params.as_slice())?;

        tx.commit()?;

        for (_, path) in &batch {
            let bytes = delete_file_if_exists(Path::new(path));
            stats.bytes_freed += bytes;
        }

        stats.screenshots_deleted += count;

        if count < CLEANUP_BATCH_SIZE {
            break;
        }
    }

    // Delete expired audio segments in batches
    loop {
        let tx = conn.unchecked_transaction()?;

        let batch: Vec<(i64, String)> = {
            let mut stmt = tx.prepare(
                "SELECT id, audio_path FROM audio_segments WHERE start_timestamp < ?1 LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![cutoff, CLEANUP_BATCH_SIZE as i64], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };

        if batch.is_empty() {
            tx.commit()?;
            break;
        }

        let count = batch.len();
        let ids: Vec<i64> = batch.iter().map(|(id, _)| *id).collect();
        let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "DELETE FROM audio_segments WHERE id IN ({})",
            placeholders
        );
        let id_params: Vec<&dyn rusqlite::ToSql> =
            ids.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
        tx.execute(&sql, id_params.as_slice())?;

        tx.commit()?;

        for (_, path) in &batch {
            let bytes = delete_file_if_exists(Path::new(path));
            stats.bytes_freed += bytes;
        }

        stats.audio_segments_deleted += count;

        if count < CLEANUP_BATCH_SIZE {
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
        bytes_freed += sweep_screenshots(conn, &screenshots_dir)?;
    }

    let audio_dir = base_dir.join("audio");
    if audio_dir.exists() {
        bytes_freed += sweep_audio(conn, &audio_dir)?;
    }

    Ok(bytes_freed)
}

/// Sweep the screenshots directory for orphan files not tracked in the DB.
fn sweep_screenshots(conn: &Connection, dir: &Path) -> Result<u64> {
    let mut bytes_freed: u64 = 0;
    let file_list = files::walk_files(dir)?;

    for file_path in file_list {
        let canonical = std::fs::canonicalize(&file_path).unwrap_or_else(|_| file_path.clone());
        let path_str = canonical.to_string_lossy().to_string();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM screenshots WHERE image_path = ?1",
            params![path_str],
            |row| row.get(0),
        )?;

        if count == 0 {
            bytes_freed += delete_file_if_exists(&file_path);
        }
    }

    Ok(bytes_freed)
}

/// Sweep the audio directory for orphan files not tracked in the DB.
fn sweep_audio(conn: &Connection, dir: &Path) -> Result<u64> {
    let mut bytes_freed: u64 = 0;
    let file_list = files::walk_files(dir)?;

    for file_path in file_list {
        let canonical = std::fs::canonicalize(&file_path).unwrap_or_else(|_| file_path.clone());
        let path_str = canonical.to_string_lossy().to_string();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM audio_segments WHERE audio_path = ?1",
            params![path_str],
            |row| row.get(0),
        )?;

        if count == 0 {
            bytes_freed += delete_file_if_exists(&file_path);
        }
    }

    Ok(bytes_freed)
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
        let new_ts = now - 86_400 * 1000; // 1 day ago

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
        let new_ts = now - 86_400 * 1000;

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
