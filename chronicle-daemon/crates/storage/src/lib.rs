//! Storage engine for Chronicle.
//!
//! SQLite database with FTS5 full-text search indexes for OCR text and
//! audio transcripts. Manages on-disk media files (screenshots, audio).

use std::path::PathBuf;

use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;

pub mod error;
pub mod models;
pub(crate) mod schema;
pub(crate) mod files;
pub(crate) mod screenshots;
pub(crate) mod audio;
pub(crate) mod search;
pub(crate) mod retention;

pub use error::{StorageError, Result};
pub use models::{
    StorageConfig, ScreenshotMetadata, Screenshot,
    AudioSegmentMetadata, AudioSegment,
    SearchFilter, SearchResult, SearchSource,
    CleanupStats, StorageStatus,
};

pub struct Storage {
    pub(crate) pool: Pool<SqliteConnectionManager>,
    pub(crate) base_dir: PathBuf,
}

impl Storage {
    pub async fn open(config: StorageConfig) -> Result<Self> {
        let base_dir = config.base_dir.clone();
        let pool_size = u32::try_from(config.pool_size)
            .map_err(|_| StorageError::Other("pool_size too large".into()))?;

        let db_path = base_dir.join("chronicle.db");
        let manager = SqliteConnectionManager::file(&db_path);

        let pool = tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&base_dir)?;

            let pool = Pool::builder()
                .max_size(pool_size)
                .connection_customizer(Box::new(ConnectionCustomizer))
                .build(manager)?;

            let conn = pool.get()?;
            schema::migrate(&conn)?;

            Ok::<_, StorageError>(pool)
        })
        .await??;

        let base_dir = config.base_dir;
        Ok(Self { pool, base_dir })
    }

    // --- Screenshot operations ---

    pub async fn allocate_screenshot_path(&self, timestamp: i64, display_id: &str) -> Result<PathBuf> {
        let base_dir = self.base_dir.clone();
        let display_id = display_id.to_string();
        tokio::task::spawn_blocking(move || {
            files::allocate_path(&base_dir, timestamp, &display_id, "screenshots", "heif")
        })
        .await?
    }

    pub async fn insert_screenshot(&self, meta: ScreenshotMetadata) -> Result<i64> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.get()?;
            screenshots::insert(&conn, &meta)
        })
        .await?
    }

    pub async fn get_screenshot(&self, id: i64) -> Result<Screenshot> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.get()?;
            screenshots::get(&conn, id)
        })
        .await?
    }

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

    pub async fn update_ocr_text(&self, id: i64, ocr_text: String) -> Result<()> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.get()?;
            screenshots::update_ocr_text(&conn, id, &ocr_text)
        })
        .await?
    }

    // --- Audio operations ---

    pub async fn allocate_audio_path(&self, timestamp: i64, source: &str) -> Result<PathBuf> {
        let base_dir = self.base_dir.clone();
        let source = source.to_string();
        tokio::task::spawn_blocking(move || {
            files::allocate_path(&base_dir, timestamp, &source, "audio", "opus")
        })
        .await?
    }

    pub async fn insert_audio_segment(&self, meta: AudioSegmentMetadata) -> Result<i64> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.get()?;
            audio::insert(&conn, &meta)
        })
        .await?
    }

    pub async fn get_audio_segment(&self, id: i64) -> Result<AudioSegment> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.get()?;
            audio::get(&conn, id)
        })
        .await?
    }

    pub async fn update_transcript(&self, id: i64, transcript: String) -> Result<()> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.get()?;
            audio::update_transcript(&conn, id, &transcript)
        })
        .await?
    }

    // --- Config operations ---

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

    pub async fn run_cleanup(&self) -> Result<CleanupStats> {
        let pool = self.pool.clone();
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
                Ok(val) => val.parse::<i64>().map_err(|e| {
                    StorageError::Other(format!("invalid retention_days value: {e}"))
                })?,
                Err(rusqlite::Error::QueryReturnedNoRows) => 30,
                Err(e) => return Err(e.into()),
            };
            retention::run_cleanup(&conn, retention_days)
        })
        .await?
    }

    pub async fn sweep_orphans(&self) -> Result<CleanupStats> {
        let pool = self.pool.clone();
        let base_dir = self.base_dir.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.get()?;
            let bytes_freed = retention::sweep_orphans(&conn, &base_dir)?;
            Ok(CleanupStats {
                bytes_freed,
                ..CleanupStats::default()
            })
        })
        .await?
    }

    // --- Status operations ---

    pub async fn status(&self) -> Result<StorageStatus> {
        let pool = self.pool.clone();
        let base_dir = self.base_dir.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.get()?;

            let screenshot_count: u64 = conn
                .query_row("SELECT COUNT(*) FROM screenshots", [], |row| {
                    row.get::<_, i64>(0).map(|v| v as u64)
                })?;

            let audio_segment_count: u64 = conn
                .query_row("SELECT COUNT(*) FROM audio_segments", [], |row| {
                    row.get::<_, i64>(0).map(|v| v as u64)
                })?;

            // Find oldest entry across both tables
            let oldest_screenshot: Option<i64> = conn
                .query_row(
                    "SELECT MIN(timestamp) FROM screenshots",
                    [],
                    |row| row.get(0),
                )?;

            let oldest_audio: Option<i64> = conn
                .query_row(
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
            let db_path = base_dir.join("chronicle.db");
            let db_size_bytes = std::fs::metadata(&db_path)
                .map(|m| m.len())
                .unwrap_or(0);
            let wal_size = std::fs::metadata(db_path.with_extension("db-wal"))
                .map(|m| m.len())
                .unwrap_or(0);
            let shm_size = std::fs::metadata(db_path.with_extension("db-shm"))
                .map(|m| m.len())
                .unwrap_or(0);
            let db_size_bytes = db_size_bytes + wal_size + shm_size;

            // Total disk usage from screenshots/ and audio/ directories
            let screenshots_size = files::dir_size(&base_dir.join("screenshots"));
            let audio_size = files::dir_size(&base_dir.join("audio"));
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
    fn on_acquire(&self, conn: &mut rusqlite::Connection) -> std::result::Result<(), rusqlite::Error> {
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
}
