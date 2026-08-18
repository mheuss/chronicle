use std::collections::HashSet;
use std::path::Path;

use rusqlite::{Connection, params};

use crate::error::{Result, StorageError};
use crate::media::MediaManager;
use crate::models::{CleanupOutcome, CleanupStats};

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

/// Largest accepted retention window: 100 years (`100 * 365`).
///
/// Past this a value is a configuration error rather than a policy — and left
/// unchecked, a large enough one wraps the cutoff into the *future* and expires
/// the entire database. Note that `0` already means "keep forever", so the
/// intuitive way to ask for that is not a large number.
///
/// The exact figure is a policy ceiling, not a numeric limit: 36,500 days is
/// ~3.15e12 ms against an `i64` ceiling of ~9.2e18, so it leaves six orders of
/// magnitude of headroom. It is set where it is because a century is already
/// past any real retention policy, not because larger values stop fitting.
pub const MAX_RETENTION_DAYS: i64 = 36_500;

/// Timestamp before which records are expired, or an error if the window does
/// not fit in an `i64`.
///
/// Its error branch is unreachable from its only production caller:
/// `run_cleanup` rejects everything that could overflow before calling here. The arithmetic stays
/// checked anyway, because a future caller that skips the bound must not be
/// able to turn a wrap into data loss. It is a separate `fn` so that guard can
/// be called — and therefore pinned — directly; see
/// `compute_cutoff_rejects_a_window_that_overflows`.
fn compute_cutoff(now_millis: i64, retention_days: i64) -> Result<i64> {
    retention_days
        .checked_mul(86_400 * 1000)
        .and_then(|window| now_millis.checked_sub(window))
        .ok_or_else(|| {
            StorageError::Other(format!(
                "retention cutoff overflows i64: now_millis {now_millis}, \
                 retention_days {retention_days}"
            ))
        })
}

/// Delete expired records and their media files in batches.
/// Order: delete files first, then DB rows (crash-safe — see design doc).
///
/// `retention_days` of `0` or less is "keep forever" and returns an empty
/// result; above [`MAX_RETENTION_DAYS`] is an error. Note the asymmetry with
/// `Storage::run_cleanup`, which rejects a *negative* value outright: `0` is a
/// legitimate setting, so it cannot be an error here, while a negative one can
/// only arrive from a caller that skipped the public boundary's validation.
/// Both layers refuse to delete; they differ only in how loudly.
pub(crate) fn run_cleanup(
    conn: &Connection,
    media_mgr: &MediaManager,
    retention_days: i64,
) -> Result<CleanupStats> {
    if retention_days <= 0 {
        return Ok(CleanupStats {
            outcome: CleanupOutcome::Disabled,
            ..CleanupStats::default()
        });
    }
    if retention_days > MAX_RETENTION_DAYS {
        // This log line is defence in depth; the `Err` below is the primary
        // signal. Kept because a caller that swallows the `Err` would otherwise
        // leave retention silently never running while disk grows. HEU-629 has
        // the scheduled tick log the error itself, so once that lands the
        // condition is reported twice per period rather than once.
        //
        // Note `main.rs` calls `env_logger::init()` with no default filter, so
        // with `RUST_LOG` unset this line does not print — same as every other
        // warn in this crate.
        log::warn!(
            "retention_days {retention_days} exceeds maximum {MAX_RETENTION_DAYS}; \
             skipping cleanup"
        );
        return Err(StorageError::Other(format!(
            "retention_days {retention_days} exceeds maximum {MAX_RETENTION_DAYS}"
        )));
    }

    let cutoff = compute_cutoff(chrono::Utc::now().timestamp_millis(), retention_days)?;

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
/// could select the same batch. Nothing in the daemon calls `run_cleanup` at
/// all today — HEU-629 adds a single scheduled task, and the assumption holds
/// only so long as that stays the sole caller. If it changes, wrap SELECT +
/// file deletion + DB DELETE in a broader transaction or add row-level locking.
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
        // `ORDER BY` so an interrupted run removes the oldest data first; the
        // `id` tiebreak makes the order total, so rows sharing a timestamp
        // cannot alternate between batches. Every identifier here is a
        // compile-time constant from `MediaTable` — no user input reaches this.
        let select_sql = format!(
            "SELECT id, {} FROM {} WHERE {} < ?1 ORDER BY {}, id LIMIT ?2",
            media.path_col, media.table, media.timestamp_col, media.timestamp_col
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

/// Was `path` modified at or after `cutoff`?
///
/// Fails CLOSED: if the metadata or the timestamp is unreadable, report `true`
/// so the caller keeps the file. The cost of a wrong `true` is one retained
/// orphan until the next sweep; the cost of a wrong `false` is a deleted
/// capture.
fn modified_since(path: &Path, cutoff: std::time::SystemTime) -> bool {
    match std::fs::metadata(path).and_then(|m| m.modified()) {
        Ok(modified) => modified >= cutoff,
        Err(_) => true,
    }
}

fn sweep_media_orphans(
    conn: &Connection,
    media: &MediaTable,
    media_mgr: &MediaManager,
) -> Result<u64> {
    // Walk BEFORE reading the tracked set, never after. Both media writers go
    // file-then-row — screenshots at pipeline.rs:76 then :121, audio at :391 then
    // :395 — so "file on disk, row not yet committed" is a real transient state,
    // and a file is deleted here iff it is in the walk list AND absent from the
    // set.
    //
    // Reading the set first loses any capture whose file lands between the SELECT
    // and the walk reaching that file's directory: its row commits after its file
    // by construction, so it can never be in the set. Walking first removes that
    // window.
    //
    // It does NOT close the race. A residual window remains when a writer's
    // file-then-row gap spans the walk-to-SELECT interval: the file makes the
    // list and the row misses the set. Note this is also less forgiving than the
    // per-file COUNT(*) this replaced, where each row had until its own file's
    // turn in the loop — minutes, which was the HEU-547 bug.
    //
    // Tolerability rests on two assumptions, and only the first holds today:
    //   1. HOLDS, in-process: the sole production caller is main.rs:89, awaited
    //      before the capture runtime spawns, so this process writes no media
    //      while the sweep runs. A periodic or IPC-triggered sweep breaks this.
    //   2. DOES NOT HOLD, cross-process: the only single-instance guard is the
    //      socket probe in IpcServer::start (main.rs:215), which runs long after
    //      the sweep, so a second daemon sweeps the shared media directory while
    //      the first is capturing. Tracked as HEU-591. The symptom is worse than
    //      a lost file — neither insert checks that the file still exists, so the
    //      row still commits and leaves a row pointing at nothing, plus an OCR or
    //      transcription job that can never succeed.
    //
    // `sweep_walks_before_reading_tracked_set` pins the ordering.
    //
    // The age guard below NARROWS that window. It does not close it, and
    // HEU-591 stays open. A genuine orphan is left behind by a crash, so it
    // predates the sweep that finds it; a capture still being written does not.
    //
    // With the guard, deletion requires BOTH: the file's last write predates
    // `sweep_start`, AND its row has not committed by the SELECT. Since
    // `sweep_start` is taken before the walk, everything written during the walk
    // is now safe — the walk dominates the walk-to-SELECT window, so that is
    // most of the old exposure. What remains at risk is a file written
    // before the sweep began whose row commits after the SELECT: the writer's
    // file-then-row gap has to span the entire walk. That gap is normally an
    // encode plus an insert, but nothing bounds it tightly — `busy_timeout` is
    // 5000 ms, so a contended insert can block for seconds.
    //
    // Two caveats on what the mtime actually measures. Audio is encoded in a
    // staging directory and `rename`d into place (main.rs:219, media.rs:107),
    // and rename preserves mtime — so for audio this is the staging-encode time,
    // milliseconds before the file appeared at the swept path, not the moment it
    // landed there. And `SystemTime::now()` is wall-clock: a backward step only
    // spares files, but a forward step (an NTP correction at boot, which is
    // exactly when this runs) can make an in-flight file look older than it is.
    // Both only bite under the cross-process case above.
    //
    // Reclaiming a true orphan created in that same instant is deferred to the
    // next sweep — which, with no periodic sweep, means the next daemon start.
    // Worth it: the other direction deletes real captures.
    let sweep_start = std::time::SystemTime::now();

    let files = media_mgr.walk_files(media.subdir);
    if files.is_empty() {
        // Nothing to classify — skip the table read entirely. A fresh install or
        // a relocated media directory would otherwise pay a full scan for nothing.
        return Ok(0);
    }

    // One query for every tracked path in this table. Covered by
    // idx_<table>_<path_col> (migration 002), so this reads the index rather
    // than scanning the table. Previously this function ran one
    // `SELECT COUNT(*)` per file on disk, which is O(files x rows) — see
    // HEU-547.
    let tracked: HashSet<String> = {
        let select_sql = format!("SELECT {} FROM {}", media.path_col, media.table);
        let mut stmt = conn.prepare(&select_sql)?;
        stmt.query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<HashSet<_>, _>>()?
    };

    let mut bytes_freed: u64 = 0;

    for file_path in files {
        // Canonicalize before comparing: the DB stores canonical paths (see
        // MediaManager::allocate_path). Fall back to the walked path if
        // canonicalize fails — the file may have been deleted between the walk
        // and this call.
        let canonical = std::fs::canonicalize(&file_path).unwrap_or_else(|_| file_path.clone());

        if !tracked.contains(canonical.to_string_lossy().as_ref()) {
            if modified_since(&file_path, sweep_start) {
                // Untracked but freshly written — treat as a capture in flight,
                // not an orphan. See the age-guard note above.
                continue;
            }
            // Pass the ORIGINAL walked path, not the canonical one. Either form
            // validates (MediaManager::validate_path canonicalizes before
            // comparing against base_dir), but the walked path is what the rest
            // of this loop reports and what walk_files handed us.
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
    use rusqlite::trace::{TraceEvent, TraceEventCodes};
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    /// Serializes every test that drives a `trace_v2` hook. The hooks below are
    /// bare `fn` pointers (rusqlite cannot take a closure), so they must reach
    /// their state through process-global statics. `trace_v2` binds
    /// per-connection, so the hooks themselves never collide — but the statics
    /// they read do, and two such tests running in parallel would both be flaky
    /// with no obvious cause. Take this lock for the whole hook lifetime.
    ///
    /// `std::sync::Mutex` is NOT reentrant, so taking it twice on one thread
    /// hangs with no timeout — far worse to diagnose than a failure. Helpers that
    /// need it therefore take `&HookGuard` rather than locking internally, which
    /// documents at the signature that the caller already holds it. That is a
    /// convention, not a capability token — nothing stops a test body, or a
    /// helper that takes `&HookGuard`, from calling `lock_hooks()` anyway.
    ///
    /// Poisoning is ignored: a panicking test has already failed, and letting it
    /// cascade into unrelated failures only obscures which test broke.
    static HOOK_LOCK: Mutex<()> = Mutex::new(());

    type HookGuard = std::sync::MutexGuard<'static, ()>;

    fn lock_hooks() -> HookGuard {
        HOOK_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Every statement executed while `capture_stmt` is registered. Used by
    /// `the_batch_select_orders_oldest_first` to assert on the SQL that really
    /// ran rather than on a copy rebuilt in the test.
    static CAPTURED_SQL: Mutex<Vec<String>> = Mutex::new(Vec::new());

    fn capture_stmt(evt: TraceEvent<'_>) {
        if let TraceEvent::Stmt(_, sql) = evt {
            CAPTURED_SQL
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(sql.to_string());
        }
    }

    static SWEEP_QUERY_COUNT: AtomicUsize = AtomicUsize::new(0);

    fn count_stmt(evt: TraceEvent<'_>) {
        if matches!(evt, TraceEvent::Stmt(..)) {
            SWEEP_QUERY_COUNT.fetch_add(1, AtomicOrdering::Relaxed);
        }
    }

    /// Set while `create_file_on_stmt` is registered: the path it should create.
    static LATE_WRITE_TARGET: Mutex<Option<PathBuf>> = Mutex::new(None);

    /// How many times `create_file_on_stmt` actually created a file. The tests
    /// assert on this because the hook cannot fail loudly — it runs inside an
    /// `extern "C"` SQLite callback, where a panic aborts the process instead of
    /// failing the test, so every error inside it must be swallowed.
    static LATE_WRITE_FIRES: AtomicUsize = AtomicUsize::new(0);

    /// Stands in for a capture written concurrently with the sweep. Fires when a
    /// statement executes — i.e. when the sweep reads its tracked set — and drops
    /// a file on disk with no matching row, the transient state the pipeline
    /// produces between writing a file and inserting its row.
    fn create_file_on_stmt(evt: TraceEvent<'_>) {
        if matches!(evt, TraceEvent::Stmt(..))
            && let Some(path) = LATE_WRITE_TARGET
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
        {
            // One-shot: `take()` clears the target so only the FIRST statement
            // creates the file. Firing on every statement would let the second
            // media table's SELECT re-create a file the first table's sweep had
            // correctly deleted, which silently defeats the assertion below.
            let _ = std::fs::write(&path, b"late capture");
            // Backdate past the age guard. A file created here is newer than
            // `sweep_start` either way, so without this the guard spares it
            // regardless of ordering and `sweep_walks_before_reading_tracked_set`
            // stops discriminating — verified: with the guard and a live mtime,
            // the select-first mutant passes every test.
            if let Ok(f) = std::fs::File::options().write(true).open(&path) {
                let _ = f.set_modified(std::time::UNIX_EPOCH);
            }
            LATE_WRITE_FIRES.fetch_add(1, AtomicOrdering::Relaxed);
        }
    }

    /// Run a full `sweep_orphans` over a fixture of `shots` screenshot files and
    /// `audio_files` audio files, all orphans, and return the number of SQL
    /// statements it executed.
    ///
    /// Takes `&HookGuard` rather than locking: the caller already holds
    /// `HOOK_LOCK`, and re-taking a non-reentrant mutex would hang.
    ///
    /// Both counts must be **> 0**. An empty media directory short-circuits
    /// before its `SELECT` (the early return in `sweep_media_orphans`), so a zero
    /// here yields one statement, not two, and the caller's assertion fails with
    /// a message about scaling that has nothing to do with the real cause.
    fn sweep_query_count(_hooks: &HookGuard, shots: usize, audio_files: usize) -> usize {
        assert!(
            shots > 0 && audio_files > 0,
            "fixture must be non-empty in both media dirs, got {shots} and {audio_files}"
        );
        let conn = setup_db();
        let dir = tempfile::tempdir().unwrap();

        let screenshots_dir = dir.path().join("screenshots");
        std::fs::create_dir_all(&screenshots_dir).unwrap();
        for i in 0..shots {
            std::fs::write(screenshots_dir.join(format!("orphan{i}.heif")), b"x").unwrap();
        }
        let audio_dir = dir.path().join("audio");
        std::fs::create_dir_all(&audio_dir).unwrap();
        for i in 0..audio_files {
            std::fs::write(audio_dir.join(format!("orphan{i}.opus")), b"x").unwrap();
        }

        let media_mgr = crate::media::MediaManager::new(dir.path().to_path_buf()).unwrap();

        SWEEP_QUERY_COUNT.store(0, AtomicOrdering::Relaxed);
        conn.trace_v2(TraceEventCodes::SQLITE_TRACE_STMT, Some(count_stmt));
        let swept = sweep_orphans(&conn, &media_mgr);
        conn.trace_v2(TraceEventCodes::SQLITE_TRACE_STMT, None);
        swept.unwrap();

        SWEEP_QUERY_COUNT.load(AtomicOrdering::Relaxed)
    }

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

    /// The sweep must not scale its query count with the number of files on disk.
    /// Classification tests cannot catch a regression to the per-file `COUNT(*)`
    /// loop — that implementation passes all of them while taking minutes on a real
    /// database. This is the guard that actually fails.
    ///
    /// Drives the full `sweep_orphans` so both `SCREENSHOT_TABLE` and `AUDIO_TABLE`
    /// are exercised: they are separate descriptors, and a bad one would be
    /// invisible to a screenshots-only test.
    #[test]
    fn sweep_issues_one_query_per_media_table() {
        // Two fixtures an order of magnitude apart. Asserting a constant against
        // one fixture would not actually prove independence from file count —
        // this compares the two, which is the property that matters.
        let hooks = lock_hooks();
        let small = sweep_query_count(&hooks, 5, 3);
        let large = sweep_query_count(&hooks, 50, 30);

        assert_eq!(
            small, 2,
            "sweep must issue exactly one SELECT per media table (2 total)"
        );
        assert_eq!(
            large, small,
            "sweep query count must not scale with the number of files on disk: \
             8 files issued {small} queries, 80 files issued {large}"
        );
    }

    /// The walk must run BEFORE the tracked-set SELECT. Moving it back below the
    /// SELECT keeps the query count at 2 and leaves every other test in this file
    /// green, so without this the ordering is protected by a comment alone — and
    /// the ordering is the whole fix.
    ///
    /// Drives the race deterministically with the trace hook: when the sweep's
    /// SELECT executes, create a file with no DB row. Walk-first, that file
    /// postdates the walk, never enters the list, and survives. Select-first, it
    /// predates the walk, lands in the list, is absent from the set, and is
    /// deleted — which is exactly the concurrent-capture loss this ordering
    /// exists to prevent.
    #[test]
    fn sweep_walks_before_reading_tracked_set() {
        let conn = setup_db();
        let dir = tempfile::tempdir().unwrap();
        let screenshots_dir = dir.path().join("screenshots");
        std::fs::create_dir_all(&screenshots_dir).unwrap();

        // A tracked file, so the walk is non-empty and the SELECT actually runs.
        let tracked = screenshots_dir.join("tracked.heif");
        std::fs::write(&tracked, b"tracked").unwrap();
        track_screenshot(&conn, &tracked);

        let late_file = screenshots_dir.join("late_capture.heif");
        let media_mgr = crate::media::MediaManager::new(dir.path().to_path_buf()).unwrap();

        let _hooks = lock_hooks();
        LATE_WRITE_FIRES.store(0, AtomicOrdering::Relaxed);
        *LATE_WRITE_TARGET.lock().unwrap_or_else(|e| e.into_inner()) = Some(late_file.clone());
        conn.trace_v2(
            TraceEventCodes::SQLITE_TRACE_STMT,
            Some(create_file_on_stmt),
        );
        let swept = sweep_orphans(&conn, &media_mgr);
        conn.trace_v2(TraceEventCodes::SQLITE_TRACE_STMT, None);
        *LATE_WRITE_TARGET.lock().unwrap_or_else(|e| e.into_inner()) = None;
        swept.unwrap();

        // Both preconditions, asserted rather than assumed. The hook cannot fail
        // loudly — it runs inside an extern "C" callback where a panic aborts —
        // so if either of these silently stopped holding, the assertion below
        // would pass for the wrong reason and this test would go vacuous.
        assert_eq!(
            LATE_WRITE_FIRES.load(AtomicOrdering::Relaxed),
            1,
            "the hook must create the file exactly once; a second firing would \
             re-create a file an earlier table's sweep had correctly deleted"
        );
        assert_eq!(
            std::fs::metadata(&late_file).unwrap().modified().unwrap(),
            std::time::UNIX_EPOCH,
            "the hook must have backdated the file, or the age guard spares it \
             regardless of ordering and this test proves nothing"
        );

        assert!(
            late_file.exists(),
            "a file created while the sweep was reading its tracked set must \
             survive — it postdates the walk, so the sweep never classified it. \
             Its deletion means the SELECT now runs before the walk."
        );
        assert!(tracked.exists(), "the tracked file must still survive");
    }

    #[test]
    fn modified_since_reports_age_against_the_cutoff() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        std::fs::write(&file, b"x").unwrap();

        let future = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(60);

        assert!(
            !modified_since(&file, future),
            "a file written before the cutoff is older than it"
        );
        assert!(
            modified_since(&file, past),
            "a file written after the cutoff is newer than it"
        );
        assert!(
            modified_since(&dir.path().join("does_not_exist"), past),
            "unreadable metadata must fail CLOSED (report newer, so the caller keeps the file)"
        );

        // Equality must count as "newer", or a filesystem whose mtime resolution
        // lands a just-written file exactly on the cutoff would let the sweep
        // delete it. This is the whole reason the comparison is `>=`, and a `>`
        // mutant passes every other test in the suite.
        let own_mtime = std::fs::metadata(&file).unwrap().modified().unwrap();
        assert!(
            modified_since(&file, own_mtime),
            "a file whose mtime equals the cutoff must be treated as newer"
        );
    }

    /// An untracked file that is newer than the sweep is a capture in flight, not
    /// an orphan — the pipeline writes the file before committing its row, so
    /// deleting it destroys a real capture. This guard is what keeps the sweep's
    /// residual race (HEU-591) from costing data.
    ///
    /// The fixture stamps the file's mtime forward rather than racing the sweep.
    /// That is deliberate: the file has to be present when the walk enumerates it
    /// (or the ordering alone spares it and this proves nothing) while still
    /// being newer than `sweep_start`. A file created by the trace hook is
    /// created *after* the walk, so it exercises the ordering, not this guard.
    #[test]
    fn sweep_spares_untracked_files_newer_than_the_sweep() {
        let conn = setup_db();
        let dir = tempfile::tempdir().unwrap();
        let screenshots_dir = dir.path().join("screenshots");
        std::fs::create_dir_all(&screenshots_dir).unwrap();

        // A settled orphan: untracked and older than the sweep. Must still be
        // deleted, so the test cannot pass by the guard disabling the sweep.
        let stale_orphan = screenshots_dir.join("stale_orphan.heif");
        std::fs::write(&stale_orphan, b"stale").unwrap();

        // A capture caught mid-write: on disk before the walk, mtime after the
        // sweep starts, no row yet.
        let in_flight = screenshots_dir.join("in_flight.heif");
        std::fs::write(&in_flight, b"in flight").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&in_flight)
            .unwrap()
            .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(60))
            .unwrap();

        let media_mgr = crate::media::MediaManager::new(dir.path().to_path_buf()).unwrap();
        let bytes_freed = sweep_orphans(&conn, &media_mgr).unwrap();

        assert!(
            in_flight.exists(),
            "an untracked file newer than the sweep must survive — it is a \
             capture in flight, not an orphan"
        );
        assert!(
            !stale_orphan.exists(),
            "an orphan older than the sweep must still be deleted, or the age \
             guard has disabled the sweep entirely"
        );
        assert_eq!(
            bytes_freed,
            b"stale".len() as u64,
            "only the settled orphan's bytes should be freed"
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

    // --- retention_days bounds (HEU-628) ---

    /// Insert one screenshot `age_days` old.
    fn insert_aged_shot(conn: &Connection, age_days: i64) {
        let meta = ScreenshotMetadata {
            timestamp: now_millis() - age_days * 86_400 * 1000,
            display_id: "display1".into(),
            app_name: None,
            app_bundle_id: None,
            window_title: None,
            image_path: format!("/tmp/aged_{age_days}.heif"),
            ocr_text: None,
            phash: None,
            resolution: None,
        };
        screenshots::insert(conn, &meta).unwrap();
    }

    fn surviving_shots(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM screenshots", [], |r| r.get(0))
            .unwrap()
    }

    /// Insert one audio segment `age_days` old. Mirrors `insert_aged_shot` for
    /// the second media table, whose timestamp column is `start_timestamp`.
    fn insert_aged_audio(conn: &Connection, age_days: i64) {
        let start = now_millis() - age_days * 86_400 * 1000;
        let meta = AudioSegmentMetadata {
            start_timestamp: start,
            end_timestamp: start + 30_000,
            source: "mic".into(),
            audio_path: format!("/tmp/aged_{age_days}.opus"),
            transcript: None,
            whisper_model: None,
            language: None,
        };
        audio::insert(conn, &meta).unwrap();
    }

    // --- cleanup outcome (HEU-629) ---

    #[test]
    fn a_disabled_retention_reports_disabled() {
        // `0` and negative are two separate conditions of the `<= 0` guard, and
        // both mean "keep forever". Neither examined anything, so neither may
        // report Completed — the scheduled task will checkpoint only on
        // Completed, and a checkpoint here would suppress the first real
        // cleanup for a whole period after retention is switched on.
        let conn = setup_db();
        let media_mgr = dummy_media_mgr();
        insert_aged_shot(&conn, 100);

        for days in [0, -1] {
            let stats = run_cleanup(&conn, &media_mgr, days).unwrap();
            assert_eq!(
                stats.outcome,
                CleanupOutcome::Disabled,
                "retention_days {days} is disabled, not completed"
            );
        }
        assert_eq!(surviving_shots(&conn), 1, "a disabled run deletes nothing");
    }

    #[test]
    fn an_ordinary_run_reports_completed() {
        let conn = setup_db();
        let media_mgr = dummy_media_mgr();
        insert_aged_shot(&conn, 100);

        let stats = run_cleanup(&conn, &media_mgr, 30).unwrap();

        assert_eq!(stats.outcome, CleanupOutcome::Completed);
        assert_eq!(stats.screenshots_deleted, 1);
    }

    // --- batch ordering (HEU-629) ---

    #[test]
    fn cleanup_keeps_only_rows_inside_the_window() {
        // Deliberately does NOT pin the ordering, despite sitting in the
        // batch-ordering section: 4 rows against a batch size of 500 all arrive in
        // one batch, so this passes with `ORDER BY` deleted, reversed, or
        // pointed at another column. `the_batch_select_orders_oldest_first` is
        // the only thing holding the ordering.
        let conn = setup_db();
        let media_mgr = dummy_media_mgr();
        for days in [100, 90, 80, 10] {
            insert_aged_shot(&conn, days);
        }

        let stats = run_cleanup(&conn, &media_mgr, 30).unwrap();

        assert_eq!(stats.screenshots_deleted, 3);
        assert_eq!(surviving_shots(&conn), 1);

        let oldest: i64 = conn
            .query_row("SELECT MIN(timestamp) FROM screenshots", [], |r| r.get(0))
            .unwrap();
        let cutoff = now_millis() - 30 * 86_400 * 1000;
        assert!(
            oldest >= cutoff,
            "the only survivor must be inside the window"
        );
    }

    #[test]
    fn the_batch_select_orders_oldest_first() {
        // The behavioural test above cannot distinguish an ordered SELECT from
        // an unordered one on a small table — SQLite happens to return rowid
        // order anyway. This pins the clause against the statement that really
        // executed, so it cannot pass by agreeing with a copy of itself.
        let _guard = lock_hooks();
        CAPTURED_SQL
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();

        let conn = setup_db();
        let media_mgr = dummy_media_mgr();
        insert_aged_shot(&conn, 100);
        insert_aged_audio(&conn, 100);

        conn.trace_v2(TraceEventCodes::SQLITE_TRACE_STMT, Some(capture_stmt));
        let cleaned = run_cleanup(&conn, &media_mgr, 30);
        conn.trace_v2(TraceEventCodes::SQLITE_TRACE_STMT, None);
        // Unwrap after deregistering, as the other hook tests here do — a panic
        // inside the hook window leaves the hook installed on a live handle.
        cleaned.unwrap();

        let captured = CAPTURED_SQL
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();

        // The exact clause, not just the substring "ORDER BY" — that would
        // accept `ORDER BY id DESC`, the wrong column, or a dropped tiebreak.
        let shot_selects: Vec<&String> = captured
            .iter()
            .filter(|sql| sql.contains("SELECT id,") && sql.contains("FROM screenshots WHERE"))
            .collect();
        assert!(
            !shot_selects.is_empty(),
            "the screenshot batch SELECT must have run, got: {captured:?}"
        );
        assert!(
            shot_selects
                .iter()
                .all(|sql| sql.contains("ORDER BY timestamp, id")),
            "screenshot batches must order by timestamp then id, got: {shot_selects:?}"
        );

        // The audio table's timestamp column differs, so assert it separately —
        // only checking one table would let a hand-edit to AUDIO_TABLE through.
        let audio_selects: Vec<&String> = captured
            .iter()
            .filter(|sql| sql.contains("SELECT id,") && sql.contains("FROM audio_segments WHERE"))
            .collect();
        assert!(
            !audio_selects.is_empty(),
            "the audio batch SELECT must have run, got: {captured:?}"
        );
        assert!(
            audio_selects
                .iter()
                .all(|sql| sql.contains("ORDER BY start_timestamp, id")),
            "audio batches must order by start_timestamp then id, got: {audio_selects:?}"
        );

        CAPTURED_SQL
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    #[test]
    fn cleanup_deletes_across_multiple_batches() {
        // No existing cleanup test crosses CLEANUP_BATCH_SIZE, so the loop that
        // repeats until a batch comes back short has never been exercised.
        // Three passes: two full batches and a short one that ends the loop.
        let conn = setup_db();
        let media_mgr = dummy_media_mgr();
        let expired = CLEANUP_BATCH_SIZE * 2 + 37;
        for _ in 0..expired {
            insert_aged_shot(&conn, 100);
        }
        insert_aged_shot(&conn, 1); // inside the window — must survive

        let stats = run_cleanup(&conn, &media_mgr, 30).unwrap();

        assert_eq!(
            stats.screenshots_deleted, expired,
            "every expired row must go, across all three batches"
        );
        assert_eq!(surviving_shots(&conn), 1, "and the fresh row must remain");

        // Row counts only. Every *deleted* row shares one `/tmp/aged_100.heif`,
        // outside `dummy_media_mgr`'s base, so every `delete_file` fails
        // `validate_path` and is swallowed by the warn in `cleanup_media`.
        // `bytes_freed` accumulating across batches is therefore still
        // uncovered — stats are HEU-631's, don't read this as covering them.
        assert_eq!(stats.bytes_freed, 0, "no file here is actually reclaimable");
    }

    #[test]
    fn an_absurd_retention_never_deletes_anything() {
        // Unchecked, `i64::MAX * 86_400 * 1000` wraps to -86_400_000, putting
        // the cutoff a day in the FUTURE — every row then qualifies for
        // deletion. Release builds wrap silently (no overflow-checks set).
        //
        // This is the end-to-end guard on that outcome, not on the arithmetic:
        // `MAX_RETENTION_DAYS` now rejects the value before the multiply is
        // reached, so either layer alone satisfies this test. The arithmetic
        // itself is pinned by `compute_cutoff_rejects_a_window_that_overflows`.
        let conn = setup_db();
        insert_aged_shot(&conn, 1); // yesterday — must survive
        let media_mgr = dummy_media_mgr();

        // An error or a no-op are both acceptable. Deleting is not — and the
        // row count is the assertion that matters, since a broken counter
        // could report zero either way.
        let _ = run_cleanup(&conn, &media_mgr, i64::MAX);

        assert_eq!(
            surviving_shots(&conn),
            1,
            "an absurd retention must never delete"
        );
    }

    #[test]
    fn a_value_just_over_the_bound_is_rejected() {
        // Pins the bound, not the arithmetic: this value is far too small to
        // overflow, so only the `> MAX_RETENTION_DAYS` clause can reject it.
        let conn = setup_db();
        insert_aged_shot(&conn, 1);
        let media_mgr = dummy_media_mgr();

        assert!(run_cleanup(&conn, &media_mgr, MAX_RETENTION_DAYS + 1).is_err());
        assert_eq!(
            surviving_shots(&conn),
            1,
            "a rejected value must not delete first"
        );
    }

    #[test]
    fn the_bound_itself_is_accepted() {
        // The `>` vs `>=` half. A 100-year retention is valid and deletes
        // nothing, which is the whole point of allowing it.
        let conn = setup_db();
        insert_aged_shot(&conn, 1);
        let media_mgr = dummy_media_mgr();

        let stats = run_cleanup(&conn, &media_mgr, MAX_RETENTION_DAYS).unwrap();
        assert_eq!(stats.screenshots_deleted, 0);
        assert_eq!(surviving_shots(&conn), 1);
    }

    #[test]
    fn zero_retention_days_deletes_nothing() {
        let conn = setup_db();
        insert_aged_shot(&conn, 100);
        let media_mgr = dummy_media_mgr();

        let stats = run_cleanup(&conn, &media_mgr, 0).unwrap();

        assert_eq!(stats.screenshots_deleted, 0);
        assert_eq!(
            surviving_shots(&conn),
            1,
            "assert rows, not just the counter"
        );
    }

    #[test]
    fn a_negative_retention_also_deletes_nothing() {
        // Other half of `<= 0`. Relax it to `== 0` and only this fails.
        let conn = setup_db();
        insert_aged_shot(&conn, 100);
        let media_mgr = dummy_media_mgr();

        let stats = run_cleanup(&conn, &media_mgr, -1).unwrap();
        assert_eq!(stats.screenshots_deleted, 0);
        assert_eq!(surviving_shots(&conn), 1);
    }

    #[test]
    fn compute_cutoff_rejects_a_window_that_overflows() {
        // The only tests that reach the checked arithmetic's error branches:
        // `run_cleanup` rejects anything above MAX_RETENTION_DAYS first, so
        // through the public path both guards are unreachable and swapping
        // either for wrapping arithmetic changes no observable behaviour.
        //
        // `checked_mul` and `checked_sub` are SEPARATE clauses and need
        // separate cases — a value large enough to overflow the multiply
        // short-circuits there and never exercises the subtract.
        assert!(
            compute_cutoff(now_millis(), i64::MAX).is_err(),
            "overflows checked_mul"
        );
        assert!(
            compute_cutoff(i64::MAX, -1).is_err(),
            "negative window makes the subtraction run off the top of i64 — \
             the only case here that overflows checked_sub"
        );
    }

    #[test]
    fn compute_cutoff_subtracts_the_window() {
        let now = 10_000_000_000i64;
        let one_day = 86_400 * 1000;
        assert_eq!(compute_cutoff(now, 1).unwrap(), now - one_day);
        assert_eq!(compute_cutoff(now, 30).unwrap(), now - 30 * one_day);
    }

    #[test]
    fn an_ordinary_retention_still_deletes() {
        // Guards against "fixing" the bug by disabling cleanup outright.
        let conn = setup_db();
        insert_aged_shot(&conn, 100);
        let media_mgr = dummy_media_mgr();

        let stats = run_cleanup(&conn, &media_mgr, 30).unwrap();
        assert_eq!(stats.screenshots_deleted, 1);
        assert_eq!(surviving_shots(&conn), 0);
    }
}
