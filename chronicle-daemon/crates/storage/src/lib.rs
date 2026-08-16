//! Storage engine for Chronicle.
//!
//! SQLite database with FTS5 full-text search indexes for OCR text and
//! audio transcripts. Manages on-disk media files (screenshots, audio).

use std::path::{Path, PathBuf};

use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;

use crate::media::MediaManager;

pub(crate) mod audio;
/// Error types for storage operations.
pub mod error;
pub(crate) mod files;
pub mod media;
/// Data models, configuration, and query types.
pub mod models;
pub(crate) mod retention;
pub(crate) mod schema;
pub(crate) mod screenshots;
pub(crate) mod search;

pub use error::{Result, StorageError};
pub use models::{
    AudioSegment, AudioSegmentMetadata, CleanupStats, Screenshot, ScreenshotMetadata, SearchFilter,
    SearchResult, SearchSource, StorageConfig, StorageStatus,
};

/// SQLite-backed storage engine for screenshots, audio, and full-text search.
pub struct Storage {
    pub(crate) pool: Pool<SqliteConnectionManager>,
    pub(crate) base_dir: PathBuf,
    media_mgr: MediaManager,
}

impl Storage {
    /// Open or create a storage database at the configured path.
    ///
    /// Creates the base directory, builds the connection pool, and runs
    /// pending schema migrations.
    pub async fn open(config: StorageConfig) -> Result<Self> {
        let base_dir = config.base_dir.clone();
        let pool_size = u32::try_from(config.pool_size)
            .map_err(|_| StorageError::Other("pool_size too large".into()))?;

        let db_path = base_dir.join("chronicle.db");
        let manager = SqliteConnectionManager::file(&db_path);

        let pool = tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&base_dir)?;

            // Harden base directory permissions.
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&base_dir, std::fs::Permissions::from_mode(0o700))?;

            // Establish WAL on a single connection so the pool's parallel init doesn't race on it.
            {
                let bootstrap = rusqlite::Connection::open(&db_path)?;
                schema::setup_connection(&bootstrap)?;
            }

            let pool = Pool::builder()
                .max_size(pool_size)
                .connection_customizer(Box::new(ConnectionCustomizer))
                .build(manager)?;

            let conn = pool.get()?;
            schema::migrate(&conn)?;

            // Harden DB file permissions.
            let db_file = base_dir.join("chronicle.db");
            if db_file.exists() {
                std::fs::set_permissions(&db_file, std::fs::Permissions::from_mode(0o600))?;
            }

            Ok::<_, StorageError>(pool)
        })
        .await??;

        let base_dir = config.base_dir;
        let media_mgr = MediaManager::new(base_dir.clone())?;
        Ok(Self {
            pool,
            base_dir,
            media_mgr,
        })
    }

    /// The root directory for the database file and media subdirectories.
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    /// The media file manager for this storage instance.
    pub fn media_manager(&self) -> &MediaManager {
        &self.media_mgr
    }

    // --- Screenshot operations ---

    /// Reserve a unique file path for a new screenshot image.
    pub async fn allocate_screenshot_path(
        &self,
        timestamp: i64,
        display_id: &str,
    ) -> Result<PathBuf> {
        let media_mgr = self.media_mgr.clone();
        let display_id = display_id.to_string();
        tokio::task::spawn_blocking(move || {
            media_mgr.allocate_path("screenshots", timestamp, &display_id, "heif")
        })
        .await?
    }

    /// Insert a screenshot record and return the assigned row ID.
    pub async fn insert_screenshot(&self, meta: ScreenshotMetadata) -> Result<i64> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.get()?;
            screenshots::insert(&conn, &meta)
        })
        .await?
    }

    /// Fetch a single screenshot by row ID.
    pub async fn get_screenshot(&self, id: i64) -> Result<Screenshot> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.get()?;
            screenshots::get(&conn, id)
        })
        .await?
    }

    /// Fetch a single screenshot by row id, returning `None` if the row
    /// doesn't exist (instead of an error). Used by the IPC layer to map
    /// not-found cleanly without depending on `rusqlite` internals.
    pub async fn get_screenshot_opt(&self, id: i64) -> Result<Option<Screenshot>> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.get()?;
            match screenshots::get(&conn, id) {
                Ok(s) => Ok(Some(s)),
                Err(StorageError::Database(rusqlite::Error::QueryReturnedNoRows)) => Ok(None),
                Err(e) => Err(e),
            }
        })
        .await?
    }

    /// Return screenshots within a time range, optionally filtered by display.
    pub async fn get_timeline(
        &self,
        start: i64,
        end: i64,
        display_id: Option<String>,
    ) -> Result<Vec<Screenshot>> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.get()?;
            screenshots::get_timeline(&conn, start, end, display_id.as_deref())
        })
        .await?
    }

    /// Attach or replace the OCR text for a screenshot.
    pub async fn update_ocr_text(&self, id: i64, ocr_text: String) -> Result<()> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.get()?;
            screenshots::update_ocr_text(&conn, id, &ocr_text)
        })
        .await?
    }

    // --- Audio operations ---

    /// Reserve a unique file path for a new audio segment.
    pub async fn allocate_audio_path(&self, timestamp: i64, source: &str) -> Result<PathBuf> {
        let media_mgr = self.media_mgr.clone();
        let source = source.to_string();
        tokio::task::spawn_blocking(move || {
            media_mgr.allocate_path("audio", timestamp, &source, "opus")
        })
        .await?
    }

    /// Insert an audio segment record and return the assigned row ID.
    pub async fn insert_audio_segment(&self, meta: AudioSegmentMetadata) -> Result<i64> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.get()?;
            audio::insert(&conn, &meta)
        })
        .await?
    }

    /// Fetch a single audio segment by row ID.
    pub async fn get_audio_segment(&self, id: i64) -> Result<AudioSegment> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.get()?;
            audio::get(&conn, id)
        })
        .await?
    }

    /// Attach or replace **only** the transcript column for an audio segment.
    ///
    /// Prefer [`Storage::update_transcript_full`] — that is what the transcription
    /// pipeline uses, and it also records the model variant and detected language.
    /// This narrower setter is retained for the transcript-only case (e.g. a
    /// future backfill that has no model metadata to record) and currently has no
    /// production caller.
    ///
    /// **It cannot express "transcribed, no speech found."** Taking `String`, the
    /// closest it can do is `""` — a fourth state that is neither NULL nor
    /// speech, and that nothing queries for. To record a silent segment, call
    /// [`Storage::update_transcript_full`] with `transcript: None` (HEU-620),
    /// whose docs also carry the backfill-predicate guidance.
    pub async fn update_transcript(&self, id: i64, transcript: String) -> Result<()> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.get()?;
            audio::update_transcript(&conn, id, &transcript)
        })
        .await?
    }

    /// Attach transcript + model variant + detected language in one update.
    /// Reuses the `audio_fts` reindex trigger that fires on any audio_segments
    /// update, so writing the transcript indexes it for search.
    ///
    /// `transcript: None` records that transcription ran and found no speech.
    /// That is a different state from an untranscribed row, which has no
    /// `whisper_model` — see HEU-620. Passing `None` also clears any previous
    /// transcript from `audio_fts` via the same trigger.
    ///
    /// Transcript text is trimmed, and an all-whitespace `Some` normalizes to
    /// `None`, so the three-state encoding cannot be broken from here:
    /// `Some("")` would otherwise persist a fourth state that is neither NULL
    /// nor speech and that nothing queries for. Callers need not trim first —
    /// padding is stripped rather than stored, so a sloppy `Some("  hi  ")`
    /// cannot land a padded row in `audio_fts` either (BR-4).
    ///
    /// **`language` is cleared with it.** A `transcript NULL` row that still
    /// carries a language is precisely the shape migration 003 exists to remove:
    /// whisper reports a detection even for silence, derived from noise, and
    /// `search.rs` hands that column back to callers. `pipeline::transcribe_loop`
    /// already pairs them; this makes the pairing hold for every caller of *this
    /// function*.
    ///
    /// It is **not** a storage-wide invariant, and do not read it as one.
    /// [`Storage::insert_audio_segment`] takes `transcript` and `language` as
    /// independent fields of [`AudioSegmentMetadata`] and inserts them
    /// unguarded, so that path can still create the shape this one prevents. No
    /// caller does today — every production insert passes both as `None` — but
    /// an insert path that starts recording transcripts would need the pairing
    /// applied there too.
    ///
    /// # Backfill guidance
    ///
    /// `whisper_model IS NULL` is the right predicate for routine work and
    /// `transcript IS NULL` is not — the latter re-queues every silent segment
    /// forever. But note what that costs: before HEU-620 a segment that produced
    /// no text stayed NULL and was retried on the next pass, so an engine-side
    /// regression healed itself. Now such a row is stamped with `whisper_model`
    /// and routine backfill will never revisit it. That is not hypothetical —
    /// `set_detect_language(true)` once returned empty text for every segment of
    /// real speech (see the whisper-rs notes).
    ///
    /// Recovering from that class of bug needs something wider than the routine
    /// predicate — but not necessarily a destructive one. `transcript IS NULL AND
    /// whisper_model IS NOT NULL` selects exactly the affected rows read-only, at
    /// the cost of re-transcribing genuinely silent segments; re-NULLing
    /// `whisper_model` first is the alternative, and it mutates. Reach for the
    /// widened `SELECT` before the `UPDATE`.
    ///
    /// Separately, because `whisper_model` records the *variant*, the column
    /// makes a variant-targeted re-run expressible without mutating anything
    /// first (`whisper_model != 'large-v3'`). That is a property of the column,
    /// not of the `whisper_model IS NULL` predicate, which by construction skips
    /// every already-stamped row.
    pub async fn update_transcript_full(
        &self,
        id: i64,
        transcript: Option<String>,
        whisper_model: String,
        language: Option<String>,
    ) -> Result<()> {
        // Collapse the fourth state at the boundary — see the note above. Trim
        // rather than merely test, and clear `language` alongside, so no caller
        // can write a row shape the migration would have to clean up later.
        let (transcript, language) = match transcript
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
        {
            text @ Some(_) => (text, language),
            None => (None, None),
        };
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.get()?;
            audio::update_transcript_full(
                &conn,
                id,
                transcript.as_deref(),
                &whisper_model,
                language.as_deref(),
            )
        })
        .await?
    }

    // --- Config operations ---

    /// Read a configuration value by key. Returns `None` if the key doesn't exist.
    pub async fn get_config(&self, key: &str) -> Result<Option<String>> {
        let pool = self.pool.clone();
        let key = key.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = pool.get()?;
            match conn.query_row(
                "SELECT value FROM config WHERE key = ?1",
                rusqlite::params![key],
                |row| row.get::<_, String>(0),
            ) {
                Ok(value) => Ok(Some(value)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(e.into()),
            }
        })
        .await?
    }

    // --- Search operations ---

    /// Run a full-text search across screenshots and audio transcripts.
    pub async fn search(
        &self,
        query: &str,
        filter: SearchFilter,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<SearchResult>> {
        let pool = self.pool.clone();
        let query = query.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = pool.get()?;
            search::search(&conn, &query, &filter, limit, offset)
        })
        .await?
    }

    // --- Retention operations ---

    /// Delete records and media files older than the configured retention period.
    pub async fn run_cleanup(&self) -> Result<CleanupStats> {
        let pool = self.pool.clone();
        let media_mgr = self.media_mgr.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.get()?;
            let retention_days: i64 = match conn.query_row(
                "SELECT value FROM config WHERE key = 'retention_days'",
                [],
                |row| {
                    let val: String = row.get(0)?;
                    Ok(val)
                },
            ) {
                Ok(val) => {
                    let days = val.parse::<i64>().map_err(|e| {
                        StorageError::Other(format!("invalid retention_days value: {e}"))
                    })?;
                    if days < 0 {
                        return Err(StorageError::Other(
                            "retention_days must be non-negative".into(),
                        ));
                    }
                    days
                }
                Err(rusqlite::Error::QueryReturnedNoRows) => 30,
                Err(e) => return Err(e.into()),
            };
            retention::run_cleanup(&conn, &media_mgr, retention_days)
        })
        .await?
    }

    /// Remove media files on disk that have no matching database record.
    pub async fn sweep_orphans(&self) -> Result<CleanupStats> {
        let pool = self.pool.clone();
        let media_mgr = self.media_mgr.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.get()?;
            let bytes_freed = retention::sweep_orphans(&conn, &media_mgr)?;
            Ok(CleanupStats {
                bytes_freed,
                ..CleanupStats::default()
            })
        })
        .await?
    }

    // --- Status operations ---

    /// Gather aggregate statistics about the database and on-disk storage.
    pub async fn status(&self) -> Result<StorageStatus> {
        let pool = self.pool.clone();
        let media_mgr = self.media_mgr.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.get()?;

            let screenshot_count: u64 =
                conn.query_row("SELECT COUNT(*) FROM screenshots", [], |row| {
                    row.get::<_, i64>(0).map(|v| v as u64)
                })?;

            let audio_segment_count: u64 =
                conn.query_row("SELECT COUNT(*) FROM audio_segments", [], |row| {
                    row.get::<_, i64>(0).map(|v| v as u64)
                })?;

            // Find oldest entry across both tables
            let oldest_screenshot: Option<i64> =
                conn.query_row("SELECT MIN(timestamp) FROM screenshots", [], |row| {
                    row.get(0)
                })?;

            let oldest_audio: Option<i64> = conn.query_row(
                "SELECT MIN(start_timestamp) FROM audio_segments",
                [],
                |row| row.get(0),
            )?;

            let oldest_entry = match (oldest_screenshot, oldest_audio) {
                (Some(s), Some(a)) => Some(s.min(a)),
                (Some(s), None) => Some(s),
                (None, Some(a)) => Some(a),
                (None, None) => None,
            };

            // DB file size (include WAL and SHM sidecars)
            let db_path = media_mgr.base_dir().join("chronicle.db");
            let db_size_bytes = match std::fs::metadata(&db_path) {
                Ok(m) => m.len(),
                Err(e) => {
                    log::warn!("failed to read db metadata at {}: {}", db_path.display(), e);
                    0
                }
            };
            let wal_size = match std::fs::metadata(db_path.with_extension("db-wal")) {
                Ok(m) => m.len(),
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    log::warn!("failed to read WAL metadata: {}", e);
                    0
                }
                Err(_) => 0, // WAL file not existing is normal
            };
            let shm_size = match std::fs::metadata(db_path.with_extension("db-shm")) {
                Ok(m) => m.len(),
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    log::warn!("failed to read SHM metadata: {}", e);
                    0
                }
                Err(_) => 0,
            };
            let db_size_bytes = db_size_bytes + wal_size + shm_size;

            // Total disk usage from screenshots/ and audio/ directories
            let screenshots_size = media_mgr.dir_size("screenshots");
            let audio_size = media_mgr.dir_size("audio");
            let total_disk_usage_bytes = db_size_bytes + screenshots_size + audio_size;

            Ok(StorageStatus {
                db_size_bytes,
                screenshot_count,
                audio_segment_count,
                total_disk_usage_bytes,
                oldest_entry,
            })
        })
        .await?
    }

    /// Write a configuration value, creating or replacing the key.
    pub async fn set_config(&self, key: &str, value: &str) -> Result<()> {
        let pool = self.pool.clone();
        let key = key.to_string();
        let value = value.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = pool.get()?;
            conn.execute(
                "INSERT INTO config (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                rusqlite::params![key, value],
            )?;
            Ok(())
        })
        .await?
    }
}

#[derive(Debug)]
struct ConnectionCustomizer;

impl r2d2::CustomizeConnection<rusqlite::Connection, rusqlite::Error> for ConnectionCustomizer {
    // `on_acquire` runs once per connection creation (not per checkout from the
    // pool). This is fine because SQLite PRAGMAs like journal_mode, synchronous,
    // foreign_keys, and busy_timeout are connection-persistent — they stick for
    // the lifetime of the connection and don't need to be re-applied on each
    // checkout.
    fn on_acquire(
        &self,
        conn: &mut rusqlite::Connection,
    ) -> std::result::Result<(), rusqlite::Error> {
        schema::setup_connection(conn).map_err(|e| match e {
            StorageError::Database(e) => e,
            other => rusqlite::Error::ModuleError(other.to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// The three-state encoding must not be breakable through
    /// `update_transcript_full`. `Some("")` and `Some("   ")` would each persist
    /// a fourth state — neither NULL nor speech, and queried by nothing — so
    /// they collapse to NULL here, taking `language` with them.
    ///
    /// Scoped deliberately: [`Storage::update_transcript`] is also public, takes
    /// `String`, and CAN still write `""`. Its own docs say so. It has no
    /// production caller, which is why the hole is documented rather than closed.
    ///
    /// Today's only `update_transcript_full` caller trims first, which is exactly
    /// why this needs its own test: the guard is unreachable from production and
    /// would rot unnoticed.
    #[tokio::test]
    async fn update_transcript_full_normalizes_blank_transcript_to_null() {
        let dir = tempdir().unwrap();
        let storage = Storage::open(StorageConfig {
            base_dir: dir.path().to_path_buf(),
            pool_size: 2,
        })
        .await
        .unwrap();

        for blank in ["", "   ", "\n\t"] {
            let id = storage
                .insert_audio_segment(crate::models::AudioSegmentMetadata {
                    start_timestamp: 1_700_000_000_000,
                    end_timestamp: 1_700_000_030_000,
                    source: "mic".into(),
                    audio_path: "/data/audio/seg.opus".into(),
                    transcript: Some("prior text".into()),
                    whisper_model: None,
                    language: None,
                })
                .await
                .unwrap();

            storage
                .update_transcript_full(id, Some(blank.into()), "base".into(), Some("en".into()))
                .await
                .unwrap();

            let seg = storage.get_audio_segment(id).await.unwrap();
            assert!(
                seg.transcript.is_none(),
                "a blank transcript ({blank:?}) must collapse to NULL, not persist a fourth state"
            );
            assert!(
                seg.language.is_none(),
                "language is cleared with the transcript ({blank:?}) — a NULL-transcript row \
                 carrying a language is the shape migration 003 exists to remove"
            );
            assert_eq!(
                seg.whisper_model.as_deref(),
                Some("base"),
                "the attempt is still recorded"
            );
        }
    }

    /// The trim is real, not just a test: padding is stripped rather than
    /// stored, so a caller that skips its own trim cannot land a padded row in
    /// `audio_fts` (BR-4). `language` survives here — the pairing only fires
    /// when the transcript collapses to nothing.
    #[tokio::test]
    async fn update_transcript_full_trims_stored_transcript() {
        let dir = tempdir().unwrap();
        let storage = Storage::open(StorageConfig {
            base_dir: dir.path().to_path_buf(),
            pool_size: 2,
        })
        .await
        .unwrap();

        let id = storage
            .insert_audio_segment(crate::models::AudioSegmentMetadata {
                start_timestamp: 1_700_000_000_000,
                end_timestamp: 1_700_000_030_000,
                source: "mic".into(),
                audio_path: "/data/audio/seg.opus".into(),
                transcript: None,
                whisper_model: None,
                language: None,
            })
            .await
            .unwrap();

        storage
            .update_transcript_full(id, Some("  hi  ".into()), "base".into(), Some("en".into()))
            .await
            .unwrap();

        let seg = storage.get_audio_segment(id).await.unwrap();
        assert_eq!(seg.transcript.as_deref(), Some("hi"), "padding is stripped");
        assert_eq!(
            seg.language.as_deref(),
            Some("en"),
            "language survives when there IS speech"
        );
    }

    #[tokio::test]
    async fn open_creates_database_file() {
        let dir = tempdir().unwrap();
        let config = StorageConfig {
            base_dir: dir.path().to_path_buf(),
            pool_size: 2,
        };
        let _storage = Storage::open(config).await.unwrap();
        assert!(dir.path().join("chronicle.db").exists());
    }

    #[tokio::test]
    async fn open_runs_migrations() {
        let dir = tempdir().unwrap();
        let config = StorageConfig {
            base_dir: dir.path().to_path_buf(),
            pool_size: 2,
        };
        let storage = Storage::open(config).await.unwrap();

        let pool = &storage.pool;
        let conn = pool.get().unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM config", [], |row| row.get(0))
            .unwrap();
        assert!(count > 0, "config table should have default rows");
    }

    #[tokio::test]
    async fn open_enables_wal_mode() {
        let dir = tempdir().unwrap();
        let config = StorageConfig {
            base_dir: dir.path().to_path_buf(),
            pool_size: 2,
        };
        let storage = Storage::open(config).await.unwrap();

        let conn = storage.pool.get().unwrap();
        let mode: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
    }

    #[tokio::test]
    async fn get_config_returns_default_value() {
        let dir = tempdir().unwrap();
        let config = StorageConfig {
            base_dir: dir.path().to_path_buf(),
            pool_size: 2,
        };
        let storage = Storage::open(config).await.unwrap();

        let value = storage.get_config("retention_days").await.unwrap();
        assert_eq!(value, Some("30".to_string()));
    }

    #[tokio::test]
    async fn set_config_updates_value() {
        let dir = tempdir().unwrap();
        let config = StorageConfig {
            base_dir: dir.path().to_path_buf(),
            pool_size: 2,
        };
        let storage = Storage::open(config).await.unwrap();

        storage.set_config("retention_days", "60").await.unwrap();
        let value = storage.get_config("retention_days").await.unwrap();
        assert_eq!(value, Some("60".to_string()));
    }

    #[tokio::test]
    async fn get_config_returns_none_for_missing_key() {
        let dir = tempdir().unwrap();
        let config = StorageConfig {
            base_dir: dir.path().to_path_buf(),
            pool_size: 2,
        };
        let storage = Storage::open(config).await.unwrap();

        let value = storage.get_config("nonexistent_key").await.unwrap();
        assert_eq!(value, None);
    }

    #[tokio::test]
    async fn status_returns_correct_counts() {
        let dir = tempdir().unwrap();
        let config = StorageConfig {
            base_dir: dir.path().to_path_buf(),
            pool_size: 2,
        };
        let storage = Storage::open(config).await.unwrap();

        let screenshot_meta = ScreenshotMetadata {
            timestamp: 1_700_000_000_000,
            display_id: "display1".into(),
            app_name: None,
            app_bundle_id: None,
            window_title: None,
            image_path: "/data/shot.heif".into(),
            ocr_text: None,
            phash: None,
            resolution: None,
        };
        storage.insert_screenshot(screenshot_meta).await.unwrap();

        let audio_meta = AudioSegmentMetadata {
            start_timestamp: 1_700_000_010_000,
            end_timestamp: 1_700_000_040_000,
            source: "mic".into(),
            audio_path: "/data/audio.opus".into(),
            transcript: None,
            whisper_model: None,
            language: None,
        };
        storage.insert_audio_segment(audio_meta).await.unwrap();

        let status = storage.status().await.unwrap();
        assert_eq!(status.screenshot_count, 1);
        assert_eq!(status.audio_segment_count, 1);
        assert!(status.db_size_bytes > 0);
        // oldest_entry should be the screenshot's timestamp (earlier)
        assert_eq!(status.oldest_entry, Some(1_700_000_000_000));
    }

    #[tokio::test]
    async fn base_dir_returns_configured_path() {
        let dir = tempdir().unwrap();
        let config = StorageConfig {
            base_dir: dir.path().to_path_buf(),
            pool_size: 2,
        };
        let storage = Storage::open(config).await.unwrap();
        assert_eq!(storage.base_dir(), dir.path());
    }

    #[tokio::test]
    async fn media_manager_returns_base_dir() {
        let dir = tempdir().unwrap();
        let config = StorageConfig {
            base_dir: dir.path().to_path_buf(),
            pool_size: 2,
        };
        let storage = Storage::open(config).await.unwrap();
        assert_eq!(storage.media_manager().base_dir(), dir.path());
    }

    #[tokio::test]
    async fn status_on_empty_db() {
        let dir = tempdir().unwrap();
        let config = StorageConfig {
            base_dir: dir.path().to_path_buf(),
            pool_size: 2,
        };
        let storage = Storage::open(config).await.unwrap();

        let status = storage.status().await.unwrap();
        assert_eq!(status.screenshot_count, 0);
        assert_eq!(status.audio_segment_count, 0);
        assert_eq!(status.oldest_entry, None);
    }

    #[tokio::test]
    async fn sweep_orphans_removes_untracked_files_on_startup() {
        let dir = tempdir().unwrap();
        let config = StorageConfig {
            base_dir: dir.path().to_path_buf(),
            pool_size: 2,
        };
        let storage = Storage::open(config).await.unwrap();

        // Tracked screenshots, allocated and inserted the way production does.
        // Using allocate_screenshot_path (not a hand-built path) is the point:
        // it stores whatever canonical form the real code stores, so this test
        // fails if the sweep's canonicalization ever stops matching it.
        //
        // Several rows, not one: a sweep that dropped rows while building its
        // tracked set — an off-by-one, or a stray LIMIT — passes a single-row
        // fixture while deleting live captures at scale.
        let mut tracked_paths = Vec::new();
        for i in 0..3 {
            let timestamp = 1_700_000_000_000 + i;
            let tracked_path = storage
                .allocate_screenshot_path(timestamp, "display1")
                .await
                .unwrap();
            std::fs::write(&tracked_path, b"tracked data").unwrap();
            storage
                .insert_screenshot(ScreenshotMetadata {
                    timestamp,
                    display_id: "display1".into(),
                    app_name: None,
                    app_bundle_id: None,
                    window_title: None,
                    image_path: tracked_path.to_string_lossy().into_owned(),
                    ocr_text: None,
                    phash: None,
                    resolution: None,
                })
                .await
                .unwrap();
            tracked_paths.push(tracked_path);
        }

        // Simulate a crash: create an orphan file in the screenshots directory
        let orphan_dir = dir.path().join("screenshots/2026/03/21");
        std::fs::create_dir_all(&orphan_dir).unwrap();
        let orphan_file = orphan_dir.join("999_orphan.heif");
        std::fs::write(&orphan_file, b"orphan data").unwrap();
        assert!(orphan_file.exists());

        // Run sweep (what startup would do)
        let stats = storage.sweep_orphans().await.unwrap();
        assert!(stats.bytes_freed > 0, "should have freed orphan bytes");
        assert!(!orphan_file.exists(), "orphan file should be deleted");
        for tracked_path in &tracked_paths {
            assert!(
                tracked_path.exists(),
                "a screenshot with a DB row must survive the sweep: {}",
                tracked_path.display()
            );
        }
        assert_eq!(
            stats.bytes_freed,
            b"orphan data".len() as u64,
            "only the orphan's bytes should be freed, not the tracked file's"
        );
    }

    #[tokio::test]
    async fn get_screenshot_opt_returns_none_for_unknown_id() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::open(StorageConfig {
            base_dir: dir.path().to_path_buf(),
            pool_size: 1,
        })
        .await
        .unwrap();
        let result = storage.get_screenshot_opt(999_999).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn status_handles_metadata_errors_gracefully() {
        let dir = tempdir().unwrap();
        let config = StorageConfig {
            base_dir: dir.path().to_path_buf(),
            pool_size: 2,
        };
        let storage = Storage::open(config).await.unwrap();

        // Delete the database file and its sidecars to simulate metadata read failure
        let db_path = dir.path().join("chronicle.db");
        std::fs::remove_file(&db_path).unwrap();
        let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
        let _ = std::fs::remove_file(db_path.with_extension("db-shm"));

        // status() should still succeed (returning 0 for unreadable fields)
        let status = storage.status().await.unwrap();
        assert_eq!(
            status.db_size_bytes, 0,
            "should report 0 for missing db file"
        );
    }
}
