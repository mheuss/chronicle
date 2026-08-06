use std::path::Path;

use rusqlite::{Connection, params};

use crate::error::Result;
use crate::media::MediaManager;
use crate::models::CleanupStats;

const CLEANUP_BATCH_SIZE: usize = 500;

/// Descriptor for a media table's columns. Parameterizes generic cleanup/sweep.
struct MediaTable {
    table: &'static str,
    timestamp_col: &'static str,
    path_col: &'static str,
    subdir: &'static str,
}

const SCREENSHOT_TABLE: MediaTable = MediaTable {
    table: "screenshots",
    timestamp_col: "timestamp",
    path_col: "image_path",
    subdir: "screenshots",
};

const AUDIO_TABLE: MediaTable = MediaTable {
    table: "audio_segments",
    timestamp_col: "start_timestamp",
    path_col: "audio_path",
    subdir: "audio",
};

/// Delete expired records and their media files in batches.
/// Order: delete files first, then DB rows (crash-safe — see design doc).
pub(crate) fn run_cleanup(
    conn: &Connection,
    media_mgr: &MediaManager,
    retention_days: i64,
) -> Result<CleanupStats> {
    if retention_days <= 0 {
        return Ok(CleanupStats::default());
    }

    let now_millis = chrono::Utc::now().timestamp_millis();
    let cutoff = now_millis - retention_days * 86_400 * 1000;

    let mut stats = CleanupStats::default();

    let (s_deleted, s_freed) = cleanup_media(conn, &SCREENSHOT_TABLE, media_mgr, cutoff)?;
    stats.screenshots_deleted += s_deleted;
    stats.bytes_freed += s_freed;

    let (a_deleted, a_freed) = cleanup_media(conn, &AUDIO_TABLE, media_mgr, cutoff)?;
    stats.audio_segments_deleted += a_deleted;
    stats.bytes_freed += a_freed;

    Ok(stats)
}

/// Generic cleanup for one media table. Returns (rows_deleted, bytes_freed).
///
/// **Single-caller assumption:** This function is not safe for concurrent
/// execution. The SELECT runs outside the transaction, so a concurrent call
/// could select the same batch. The daemon calls `run_cleanup` from a single
/// async task — if that changes, wrap SELECT + file deletion + DB DELETE in
/// a broader transaction or add row-level locking.
fn cleanup_media(
    conn: &Connection,
    media: &MediaTable,
    media_mgr: &MediaManager,
    cutoff: i64,
) -> Result<(usize, u64)> {
    let mut total_deleted = 0usize;
    let mut total_freed = 0u64;

    loop {
        // 1. Select batch of expired rows
        let select_sql = format!(
            "SELECT id, {} FROM {} WHERE {} < ?1 LIMIT ?2",
            media.path_col, media.table, media.timestamp_col
        );
        let batch: Vec<(i64, String)> = {
            let mut stmt = conn.prepare(&select_sql)?;
            let rows = stmt.query_map(params![cutoff, CLEANUP_BATCH_SIZE as i64], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };

        if batch.is_empty() {
            break;
        }

        let count = batch.len();

        // 2. Delete files FIRST (crash-safe: orphan DB rows are easy to detect)
        for (_, path) in &batch {
            match media_mgr.delete_file(Path::new(path)) {
                Ok(freed) => total_freed += freed,
                Err(e) => {
                    log::warn!("cleanup: failed to delete {}: {}", path, e);
                }
            }
        }

        // 3. Delete DB rows in a transaction
        let tx = conn.unchecked_transaction()?;
        let ids: Vec<i64> = batch.iter().map(|(id, _)| *id).collect();
        let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let delete_sql = format!("DELETE FROM {} WHERE id IN ({})", media.table, placeholders);
        let id_params: Vec<&dyn rusqlite::ToSql> =
            ids.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
        tx.execute(&delete_sql, id_params.as_slice())?;
        tx.commit()?;

        total_deleted += count;

        if count < CLEANUP_BATCH_SIZE {
            break;
        }
    }

    Ok((total_deleted, total_freed))
}

/// Walk media directories and delete files not tracked in the database.
pub(crate) fn sweep_orphans(conn: &Connection, media_mgr: &MediaManager) -> Result<u64> {
    let mut bytes_freed: u64 = 0;
    bytes_freed += sweep_media_orphans(conn, &SCREENSHOT_TABLE, media_mgr)?;
    bytes_freed += sweep_media_orphans(conn, &AUDIO_TABLE, media_mgr)?;
    Ok(bytes_freed)
}

fn sweep_media_orphans(
    conn: &Connection,
    media: &MediaTable,
    media_mgr: &MediaManager,
) -> Result<u64> {
    let file_list = media_mgr.walk_files(media.subdir);
    let mut bytes_freed: u64 = 0;

    let count_sql = format!(
        "SELECT COUNT(*) FROM {} WHERE {} = ?1",
        media.table, media.path_col
    );

    for file_path in file_list {
        let canonical = std::fs::canonicalize(&file_path).unwrap_or_else(|_| file_path.clone());
        let path_str = canonical.to_string_lossy().to_string();
        let count: i64 = conn.query_row(&count_sql, params![path_str], |row| row.get(0))?;

        if count == 0 {
            match media_mgr.delete_file(&file_path) {
                Ok(freed) => bytes_freed += freed,
                Err(e) => {
                    log::warn!(
                        "orphan sweep: failed to delete {}: {}",
                        file_path.display(),
                        e
                    );
                }
            }
        }
    }

    Ok(bytes_freed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AudioSegmentMetadata, ScreenshotMetadata};
    use crate::schema;
    use crate::{audio, screenshots};
    use std::path::PathBuf;

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::setup_connection(&conn).unwrap();
        schema::migrate(&conn).unwrap();
        conn
    }

    fn now_millis() -> i64 {
        chrono::Utc::now().timestamp_millis()
    }

    fn dummy_media_mgr() -> crate::media::MediaManager {
        let path = std::path::PathBuf::from("/tmp/chronicle-test-dummy");
        std::fs::create_dir_all(&path).unwrap();
        crate::media::MediaManager::new(path).unwrap()
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

        let media_mgr = dummy_media_mgr();
        let stats = run_cleanup(&conn, &media_mgr, 30).unwrap();
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

        let media_mgr = dummy_media_mgr();
        let stats = run_cleanup(&conn, &media_mgr, 30).unwrap();
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

        let media_mgr = crate::media::MediaManager::new(dir.path().to_path_buf()).unwrap();
        let stats = run_cleanup(&conn, &media_mgr, 30).unwrap();
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
        let media_mgr = dummy_media_mgr();
        let stats = run_cleanup(&conn, &media_mgr, 30).unwrap();
        assert_eq!(stats.screenshots_deleted, 1);
        assert_eq!(stats.bytes_freed, 0);
    }

    #[test]
    fn cleanup_deletes_file_before_db_row() {
        let conn = setup_db();
        let now = now_millis();
        let old_ts = now - 31 * 86_400 * 1000;

        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("old_shot.heif");
        std::fs::write(&file_path, b"image data").unwrap();

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

        let media_mgr = crate::media::MediaManager::new(dir.path().to_path_buf()).unwrap();
        let stats = run_cleanup(&conn, &media_mgr, 30).unwrap();

        assert_eq!(stats.screenshots_deleted, 1);
        assert!(!file_path.exists(), "file should be deleted");
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM screenshots", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0, "DB row should be deleted");
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

        let media_mgr = crate::media::MediaManager::new(dir.path().to_path_buf()).unwrap();
        let bytes_freed = sweep_orphans(&conn, &media_mgr).unwrap();
        assert!(bytes_freed > 0);
        assert!(!orphan_file.exists());
    }

    /// Insert a screenshots row pointing at `path`'s canonical form.
    ///
    /// Storing the CANONICAL path mirrors `MediaManager::allocate_path`, and it
    /// is load-bearing: `walk_files` joins from the base_dir as passed
    /// (`media.rs:176`), not the canonical one, so walked paths keep the
    /// unresolved form while the DB holds the resolved one. The sweep
    /// canonicalizes each walked file to bridge that. Storing the unresolved
    /// path here would make the sweep treat every live file as an orphan and
    /// delete it. See docs/use-cases/storage.md.
    fn track_screenshot(conn: &Connection, path: &Path) {
        let canonical = std::fs::canonicalize(path).unwrap();
        screenshots::insert(
            conn,
            &ScreenshotMetadata {
                timestamp: now_millis(),
                display_id: "d1".into(),
                app_name: None,
                app_bundle_id: None,
                window_title: None,
                image_path: canonical.to_string_lossy().into_owned(),
                ocr_text: None,
                phash: None,
                resolution: None,
            },
        )
        .unwrap();
    }

    fn track_audio(conn: &Connection, path: &Path) {
        let canonical = std::fs::canonicalize(path).unwrap();
        audio::insert(
            conn,
            &AudioSegmentMetadata {
                start_timestamp: now_millis(),
                end_timestamp: now_millis() + 30_000,
                source: "mic".into(),
                audio_path: canonical.to_string_lossy().into_owned(),
                transcript: None,
                whisper_model: None,
                language: None,
            },
        )
        .unwrap();
    }

    #[test]
    fn sweep_orphans_keeps_tracked_files_and_deletes_orphans() {
        let conn = setup_db();
        let dir = tempfile::tempdir().unwrap();

        // Reach the storage root through a symlink so the sweep's
        // canonicalization is genuinely exercised no matter where TMPDIR points.
        // Without this the test leans on macOS happening to put temp dirs under
        // /var (a symlink to /private/var); on a non-symlinked TMPDIR
        // canonicalize becomes a no-op and every canonicalization assertion
        // below silently degrades to trivially true.
        let real_root = dir.path().join("real_root");
        std::fs::create_dir_all(&real_root).unwrap();
        let root = dir.path().join("link_root");
        std::os::unix::fs::symlink(&real_root, &root).unwrap();

        let screenshots_dir = root.join("screenshots");
        std::fs::create_dir_all(&screenshots_dir).unwrap();
        let audio_dir = root.join("audio");
        std::fs::create_dir_all(&audio_dir).unwrap();

        // More tracked rows than CLEANUP_BATCH_SIZE. A set-building SELECT that
        // borrowed `LIMIT CLEANUP_BATCH_SIZE` from cleanup_media (40 lines above
        // in this file, a natural pattern to mirror) would drop this fixture's
        // tail from the tracked set and delete real captures. A single tracked
        // row cannot see that class of bug at all.
        let tracked_shots: Vec<PathBuf> = (0..=CLEANUP_BATCH_SIZE)
            .map(|i| {
                let path = screenshots_dir.join(format!("tracked_{i:04}.heif"));
                std::fs::write(&path, b"s").unwrap();
                track_screenshot(&conn, &path);
                path
            })
            .collect();

        let tracked_audio = audio_dir.join("tracked.opus");
        std::fs::write(&tracked_audio, b"aa").unwrap();
        track_audio(&conn, &tracked_audio);

        // Orphan payloads are distinct powers of two, so a short total names
        // exactly which orphans were missed. This is a diagnostic aid, not a
        // classification check on its own — the fixture also holds 501 one-byte
        // tracked files, so other wrong classifications can still sum to 60.
        // The per-file existence assertions below are what catch those, and
        // they fire before the byte check.
        let orphan_shot_a = screenshots_dir.join("orphan_a.heif");
        std::fs::write(&orphan_shot_a, [b'x'; 4]).unwrap();
        let orphan_shot_b = screenshots_dir.join("orphan_b.heif");
        std::fs::write(&orphan_shot_b, [b'x'; 8]).unwrap();
        let orphan_audio = audio_dir.join("orphan.opus");
        std::fs::write(&orphan_audio, [b'x'; 16]).unwrap();

        // A file under audio/ whose path is recorded in the SCREENSHOTS table.
        // The audio sweep must still delete it: membership is per-table, and a
        // single set merged across both tables would wrongly spare it. This is
        // what proves SCREENSHOT_TABLE and AUDIO_TABLE stay separate descriptors
        // for membership, not just for the directory walk.
        let cross_table = audio_dir.join("cross_table.opus");
        std::fs::write(&cross_table, [b'x'; 32]).unwrap();
        track_screenshot(&conn, &cross_table);

        let media_mgr = crate::media::MediaManager::new(root.clone()).unwrap();
        let bytes_freed = sweep_orphans(&conn, &media_mgr).unwrap();

        for path in &tracked_shots {
            assert!(
                path.exists(),
                "a screenshot with a matching DB row must survive the sweep: {}",
                path.display()
            );
        }
        assert!(
            tracked_audio.exists(),
            "an audio segment with a matching DB row must survive the sweep"
        );
        for orphan in [&orphan_shot_a, &orphan_shot_b] {
            assert!(
                !orphan.exists(),
                "a screenshot with no DB row must be deleted: {}",
                orphan.display()
            );
        }
        assert!(
            !orphan_audio.exists(),
            "an audio file with no DB row must be deleted"
        );
        assert!(
            !cross_table.exists(),
            "an audio-dir file tracked only in the screenshots table is an \
             orphan to the audio sweep and must be deleted"
        );
        assert_eq!(
            bytes_freed, 60,
            "only the orphans' bytes should be counted as freed (4 + 8 + 16 + 32)"
        );
    }

    #[test]
    fn cleanup_unified_handles_both_tables() {
        let conn = setup_db();
        let now = now_millis();
        let old_ts = now - 31 * 86_400 * 1000;

        let dir = tempfile::tempdir().unwrap();

        let shot_path = dir.path().join("old_shot.heif");
        std::fs::write(&shot_path, b"image").unwrap();
        let audio_path = dir.path().join("old_audio.opus");
        std::fs::write(&audio_path, b"audio").unwrap();

        screenshots::insert(
            &conn,
            &ScreenshotMetadata {
                timestamp: old_ts,
                display_id: "d1".into(),
                app_name: None,
                app_bundle_id: None,
                window_title: None,
                image_path: shot_path.to_string_lossy().into_owned(),
                ocr_text: None,
                phash: None,
                resolution: None,
            },
        )
        .unwrap();

        audio::insert(
            &conn,
            &AudioSegmentMetadata {
                start_timestamp: old_ts,
                end_timestamp: old_ts + 30_000,
                source: "mic".into(),
                audio_path: audio_path.to_string_lossy().into_owned(),
                transcript: None,
                whisper_model: None,
                language: None,
            },
        )
        .unwrap();

        let media_mgr = crate::media::MediaManager::new(dir.path().to_path_buf()).unwrap();
        let stats = run_cleanup(&conn, &media_mgr, 30).unwrap();

        assert_eq!(
            stats.screenshots_deleted, 1,
            "should delete expired screenshot"
        );
        assert_eq!(
            stats.audio_segments_deleted, 1,
            "should delete expired audio"
        );
        assert!(!shot_path.exists(), "screenshot file should be deleted");
        assert!(!audio_path.exists(), "audio file should be deleted");
    }
}
