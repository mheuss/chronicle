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
        std::fs::create_dir_all(&base_dir)?;

        let db_path = base_dir.join("chronicle.db");
        let manager = SqliteConnectionManager::file(&db_path);

        let pool = tokio::task::spawn_blocking(move || {
            let pool = Pool::builder()
                .max_size(config.pool_size as u32)
                .connection_customizer(Box::new(ConnectionCustomizer))
                .build(manager)?;

            let conn = pool.get()?;
            schema::migrate(&conn)?;

            Ok::<_, StorageError>(pool)
        })
        .await??;

        Ok(Self { pool, base_dir })
    }

    // --- Screenshot operations ---

    pub async fn allocate_screenshot_path(&self, timestamp: i64, display_id: &str) -> Result<PathBuf> {
        let path = files::screenshot_path(&self.base_dir, timestamp, display_id);
        files::ensure_parent_dir(&path)?;
        Ok(path)
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
}

#[derive(Debug)]
struct ConnectionCustomizer;

impl r2d2::CustomizeConnection<rusqlite::Connection, rusqlite::Error> for ConnectionCustomizer {
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
}
