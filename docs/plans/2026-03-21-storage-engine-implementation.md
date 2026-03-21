# Storage Engine Implementation Plan

**Date:** 2026-03-21
**Status:** Approved
**Original Design Doc:** docs/plans/2026-03-21-storage-engine-design.md
**Issue:** HEU-236
**Branch:** mrheuss/heu-236-storage-engine-sqlite-schema-fts5-indexes-and-file
**Base Branch:** main

---

> **For Claude:** REQUIRED SUB-SKILL: Use sop:subagent-driven-development or sop:executing-plans to implement this plan task-by-task.

**Goal:** Implement the `chronicle-storage` crate — SQLite database with FTS5 full-text search, media file path management, and retention cleanup.

**Architecture:** Single `Storage` struct backed by an r2d2 connection pool over SQLite in WAL mode. Async public API wraps sync rusqlite internals via `spawn_blocking`. Internal modules split by concern (screenshots, audio, search, retention, files).

**Tech Stack:** Rust, rusqlite (bundled-full for FTS5), r2d2 + r2d2_sqlite, tokio, thiserror, chrono

---

### Task 1: Update Cargo.toml and verify dependencies

**Files:**
- Modify: `chronicle-daemon/crates/storage/Cargo.toml`

**Step 1: Update Cargo.toml**

Replace the current contents with:

```toml
[package]
name = "chronicle-storage"
version = "0.1.0"
edition = "2024"
description = "Storage engine — SQLite schema, FTS5 indexes, and file management"
license = "MIT"

[dependencies]
rusqlite = { version = "0.39", features = ["bundled-full"] }
r2d2 = "0.8"
r2d2_sqlite = "0.33"
tokio = { version = "1", features = ["rt"] }
thiserror = "2"
chrono = { version = "0.4", default-features = false, features = ["clock"] }

[dev-dependencies]
tokio = { version = "1", features = ["rt", "macros"] }
tempfile = "3"
```

Notes:
- `rusqlite` updated from 0.35 to 0.39 (latest). The design doc pinned 0.35 but
  the scaffolding has no implementation code depending on a specific API.
- `bundled-full` compiles SQLite from source with FTS5 and other extensions.
- `chrono` added for date extraction in path generation (YYYY/MM/DD directories).
  The design doc used `DateTime::from_millis` — chrono provides this.
- `tempfile` in dev-dependencies for test isolation (temp directories).

**Step 2: Verify dependencies resolve**

```bash
cd chronicle-daemon && cargo check -p chronicle-storage
```

Expected: compiles successfully with no errors. If `r2d2_sqlite` 0.33 conflicts
with `rusqlite` 0.39, adjust r2d2_sqlite version or check crates.io for the
compatible pairing.

**Step 3: Commit**

```bash
git add chronicle-daemon/crates/storage/Cargo.toml chronicle-daemon/Cargo.lock
git commit -m "build(storage): update dependencies for storage engine" \
  -m "Add rusqlite (bundled-full), r2d2, r2d2_sqlite, tokio, thiserror, chrono. Update rusqlite from 0.35 to 0.39." \
  -m "Part of HEU-236"
```

---

### Task 2: Error type

**Files:**
- Create: `chronicle-daemon/crates/storage/src/error.rs`
- Modify: `chronicle-daemon/crates/storage/src/lib.rs`

**Step 1: Write the failing test**

In `error.rs`, add:

```rust
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("connection pool error: {0}")]
    Pool(#[from] r2d2::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("task join error: {0}")]
    Join(#[from] tokio::task::JoinError),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, StorageError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_error_converts_to_storage_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
        let storage_err: StorageError = io_err.into();
        assert!(matches!(storage_err, StorageError::Io(_)));
        assert!(storage_err.to_string().contains("file missing"));
    }

    #[test]
    fn other_error_displays_message() {
        let err = StorageError::Other("something went wrong".into());
        assert_eq!(err.to_string(), "something went wrong");
    }
}
```

**Step 2: Update lib.rs to declare the module**

Replace the contents of `lib.rs` with:

```rust
//! Storage engine for Chronicle.
//!
//! SQLite database with FTS5 full-text search indexes for OCR text and
//! audio transcripts. Manages on-disk media files (screenshots, audio).

pub mod error;

pub use error::{StorageError, Result};
```

**Step 3: Run tests to verify they pass**

```bash
cd chronicle-daemon && cargo test -p chronicle-storage
```

Expected: 2 tests pass.

**Step 4: Commit**

```bash
git add chronicle-daemon/crates/storage/src/error.rs chronicle-daemon/crates/storage/src/lib.rs
git commit -m "feat(storage): add StorageError type" \
  -m "Part of HEU-236"
```

---

### Task 3: Data models

**Files:**
- Create: `chronicle-daemon/crates/storage/src/models.rs`
- Modify: `chronicle-daemon/crates/storage/src/lib.rs`

**Step 1: Write models.rs**

```rust
use std::path::PathBuf;

// --- Configuration ---

pub struct StorageConfig {
    pub base_dir: PathBuf,
    pub pool_size: usize,
}

impl Default for StorageConfig {
    fn default() -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        let base_dir = PathBuf::from(home).join("Library/Application Support/Chronicle");
        Self {
            base_dir,
            pool_size: 4,
        }
    }
}

// --- Screenshot types ---

pub struct ScreenshotMetadata {
    pub timestamp: i64,
    pub display_id: String,
    pub app_name: Option<String>,
    pub app_bundle_id: Option<String>,
    pub window_title: Option<String>,
    pub image_path: String,
    pub ocr_text: Option<String>,
    pub phash: Option<Vec<u8>>,
    pub resolution: Option<String>,
}

pub struct Screenshot {
    pub id: i64,
    pub timestamp: i64,
    pub display_id: String,
    pub app_name: Option<String>,
    pub app_bundle_id: Option<String>,
    pub window_title: Option<String>,
    pub image_path: String,
    pub ocr_text: Option<String>,
    pub phash: Option<Vec<u8>>,
    pub resolution: Option<String>,
    pub created_at: i64,
}

// --- Audio types ---

pub struct AudioSegmentMetadata {
    pub start_timestamp: i64,
    pub end_timestamp: i64,
    pub source: String,
    pub audio_path: String,
    pub transcript: Option<String>,
    pub whisper_model: Option<String>,
    pub language: Option<String>,
}

pub struct AudioSegment {
    pub id: i64,
    pub start_timestamp: i64,
    pub end_timestamp: i64,
    pub source: String,
    pub audio_path: String,
    pub transcript: Option<String>,
    pub whisper_model: Option<String>,
    pub language: Option<String>,
    pub created_at: i64,
}

// --- Search types ---

#[derive(Debug, Clone, PartialEq)]
pub enum SearchFilter {
    All,
    ScreenOnly,
    AudioOnly,
}

#[derive(Debug)]
pub enum SearchSource {
    Screen(Screenshot),
    Audio(AudioSegment),
}

#[derive(Debug)]
pub struct SearchResult {
    pub source: SearchSource,
    pub snippet: String,
    pub rank: f64,
}

// --- Operational types ---

#[derive(Debug, Default)]
pub struct CleanupStats {
    pub screenshots_deleted: usize,
    pub audio_segments_deleted: usize,
    pub bytes_freed: u64,
}

#[derive(Debug)]
pub struct StorageStatus {
    pub db_size_bytes: u64,
    pub screenshot_count: u64,
    pub audio_segment_count: u64,
    pub total_disk_usage_bytes: u64,
    pub oldest_entry: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_config_default_points_to_app_support() {
        let config = StorageConfig::default();
        let path_str = config.base_dir.to_string_lossy();
        assert!(path_str.contains("Library/Application Support/Chronicle"));
        assert_eq!(config.pool_size, 4);
    }

    #[test]
    fn cleanup_stats_default_is_zero() {
        let stats = CleanupStats::default();
        assert_eq!(stats.screenshots_deleted, 0);
        assert_eq!(stats.audio_segments_deleted, 0);
        assert_eq!(stats.bytes_freed, 0);
    }
}
```

**Step 2: Add module to lib.rs**

Add `pub mod models;` and re-exports to `lib.rs`:

```rust
pub mod models;

pub use models::{
    StorageConfig, ScreenshotMetadata, Screenshot,
    AudioSegmentMetadata, AudioSegment,
    SearchFilter, SearchResult, SearchSource,
    CleanupStats, StorageStatus,
};
```

**Step 3: Run tests**

```bash
cd chronicle-daemon && cargo test -p chronicle-storage
```

Expected: 4 tests pass (2 from error, 2 from models).

**Step 4: Commit**

```bash
git add chronicle-daemon/crates/storage/src/models.rs chronicle-daemon/crates/storage/src/lib.rs
git commit -m "feat(storage): add data models" \
  -m "ScreenshotMetadata, AudioSegmentMetadata, SearchResult, CleanupStats, StorageConfig, and related types." \
  -m "Part of HEU-236"
```

---

### Task 4: Migration SQL

**Files:**
- Create: `chronicle-daemon/crates/storage/src/migrations/001_initial_schema.sql`

**Step 1: Create the migration file**

```sql
-- Chronicle storage schema v1
-- Tables, FTS5 indexes, sync triggers, indexes, and default config.

-- Main tables

CREATE TABLE IF NOT EXISTS screenshots (
    id            INTEGER PRIMARY KEY,
    timestamp     INTEGER NOT NULL,
    display_id    TEXT    NOT NULL,
    app_name      TEXT,
    app_bundle_id TEXT,
    window_title  TEXT,
    image_path    TEXT    NOT NULL,
    ocr_text      TEXT,
    phash         BLOB,
    resolution    TEXT,
    created_at    INTEGER NOT NULL DEFAULT (unixepoch('subsec') * 1000)
);

CREATE TABLE IF NOT EXISTS audio_segments (
    id              INTEGER PRIMARY KEY,
    start_timestamp INTEGER NOT NULL,
    end_timestamp   INTEGER NOT NULL,
    source          TEXT    NOT NULL CHECK (source IN ('mic', 'system')),
    audio_path      TEXT    NOT NULL,
    transcript      TEXT,
    whisper_model   TEXT,
    language        TEXT,
    created_at      INTEGER NOT NULL DEFAULT (unixepoch('subsec') * 1000)
);

CREATE TABLE IF NOT EXISTS config (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- FTS5 virtual tables (external content)

CREATE VIRTUAL TABLE IF NOT EXISTS screenshots_fts USING fts5(
    ocr_text, app_name, window_title,
    content=screenshots, content_rowid=id
);

CREATE VIRTUAL TABLE IF NOT EXISTS audio_fts USING fts5(
    transcript,
    content=audio_segments, content_rowid=id
);

-- FTS sync triggers: screenshots

CREATE TRIGGER IF NOT EXISTS screenshots_ai AFTER INSERT ON screenshots BEGIN
    INSERT INTO screenshots_fts(rowid, ocr_text, app_name, window_title)
    VALUES (new.id, new.ocr_text, new.app_name, new.window_title);
END;

CREATE TRIGGER IF NOT EXISTS screenshots_ad AFTER DELETE ON screenshots BEGIN
    INSERT INTO screenshots_fts(screenshots_fts, rowid, ocr_text, app_name, window_title)
    VALUES ('delete', old.id, old.ocr_text, old.app_name, old.window_title);
END;

CREATE TRIGGER IF NOT EXISTS screenshots_au AFTER UPDATE ON screenshots BEGIN
    INSERT INTO screenshots_fts(screenshots_fts, rowid, ocr_text, app_name, window_title)
    VALUES ('delete', old.id, old.ocr_text, old.app_name, old.window_title);
    INSERT INTO screenshots_fts(rowid, ocr_text, app_name, window_title)
    VALUES (new.id, new.ocr_text, new.app_name, new.window_title);
END;

-- FTS sync triggers: audio

CREATE TRIGGER IF NOT EXISTS audio_ai AFTER INSERT ON audio_segments BEGIN
    INSERT INTO audio_fts(rowid, transcript) VALUES (new.id, new.transcript);
END;

CREATE TRIGGER IF NOT EXISTS audio_ad AFTER DELETE ON audio_segments BEGIN
    INSERT INTO audio_fts(audio_fts, rowid, transcript) VALUES ('delete', old.id, old.transcript);
END;

CREATE TRIGGER IF NOT EXISTS audio_au AFTER UPDATE ON audio_segments BEGIN
    INSERT INTO audio_fts(audio_fts, rowid, transcript) VALUES ('delete', old.id, old.transcript);
    INSERT INTO audio_fts(rowid, transcript) VALUES (new.id, new.transcript);
END;

-- Indexes

CREATE INDEX IF NOT EXISTS idx_screenshots_timestamp ON screenshots(timestamp);
CREATE INDEX IF NOT EXISTS idx_screenshots_app ON screenshots(app_bundle_id, timestamp);
CREATE INDEX IF NOT EXISTS idx_audio_timestamp ON audio_segments(start_timestamp);
CREATE INDEX IF NOT EXISTS idx_audio_source ON audio_segments(source, start_timestamp);

-- Default configuration

INSERT OR IGNORE INTO config (key, value) VALUES ('retention_days', '30');
INSERT OR IGNORE INTO config (key, value) VALUES ('capture_interval_ms', '2000');
```

**Step 2: Verify the file is valid SQL**

No automated check at this point — schema.rs (Task 5) will test that this
migration runs successfully against an in-memory SQLite database.

**Step 3: Commit**

```bash
git add chronicle-daemon/crates/storage/src/migrations/001_initial_schema.sql
git commit -m "feat(storage): add initial schema migration" \
  -m "Tables, FTS5 indexes, sync triggers, indexes, and default config for screenshots and audio segments." \
  -m "Part of HEU-236"
```

---

### Task 5: Schema module

**Files:**
- Create: `chronicle-daemon/crates/storage/src/schema.rs`
- Modify: `chronicle-daemon/crates/storage/src/lib.rs`

**Step 1: Write the failing test**

In `schema.rs`, write a test that expects the migration to run and tables to
exist:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn migration_creates_all_tables() {
        let conn = Connection::open_in_memory().unwrap();
        setup_connection(&conn).unwrap();
        migrate(&conn).unwrap();

        // Verify main tables exist
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();

        assert!(tables.contains(&"screenshots".to_string()));
        assert!(tables.contains(&"audio_segments".to_string()));
        assert!(tables.contains(&"config".to_string()));
        assert!(tables.contains(&"screenshots_fts".to_string()));
        assert!(tables.contains(&"audio_fts".to_string()));
    }

    #[test]
    fn migration_seeds_default_config() {
        let conn = Connection::open_in_memory().unwrap();
        setup_connection(&conn).unwrap();
        migrate(&conn).unwrap();

        let retention: String = conn
            .query_row("SELECT value FROM config WHERE key = 'retention_days'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(retention, "30");
    }

    #[test]
    fn migration_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        setup_connection(&conn).unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap(); // second run should not error
    }

    #[test]
    fn setup_connection_enables_wal() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let conn = Connection::open(&db_path).unwrap();
        setup_connection(&conn).unwrap();

        let journal_mode: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(journal_mode.to_lowercase(), "wal");
    }
}
```

**Step 2: Run tests to verify they fail**

```bash
cd chronicle-daemon && cargo test -p chronicle-storage
```

Expected: FAIL — `setup_connection` and `migrate` don't exist yet.

**Step 3: Write the implementation**

```rust
use rusqlite::Connection;
use crate::error::Result;

const MIGRATIONS: &[&str] = &[
    include_str!("migrations/001_initial_schema.sql"),
];

/// Configure connection-level PRAGMAs. Call on every new connection.
pub(crate) fn setup_connection(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    Ok(())
}

/// Run pending migrations. Uses PRAGMA user_version to track progress.
pub(crate) fn migrate(conn: &Connection) -> Result<()> {
    let current_version: u32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;

    for (i, migration) in MIGRATIONS.iter().enumerate() {
        let version = (i + 1) as u32;
        if version > current_version {
            conn.execute_batch(migration)?;
            conn.pragma_update(None, "user_version", version)?;
        }
    }

    Ok(())
}
```

**Step 4: Add module to lib.rs**

Add `pub(crate) mod schema;` to lib.rs.

**Step 5: Run tests to verify they pass**

```bash
cd chronicle-daemon && cargo test -p chronicle-storage
```

Expected: all tests pass (4 from schema + 2 from error + 2 from models = 8).

**Step 6: Commit**

```bash
git add chronicle-daemon/crates/storage/src/schema.rs chronicle-daemon/crates/storage/src/lib.rs
git commit -m "feat(storage): add schema migration runner" \
  -m "Runs embedded SQL migrations using PRAGMA user_version. Sets WAL mode and connection pragmas." \
  -m "Part of HEU-236"
```

---

### Task 6: File management

**Files:**
- Create: `chronicle-daemon/crates/storage/src/files.rs`
- Modify: `chronicle-daemon/crates/storage/src/lib.rs`

**Step 1: Write the failing test**

In `files.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn screenshot_path_has_correct_structure() {
        let base = PathBuf::from("/data");
        // 2026-03-21 12:00:00.000 UTC = 1774036800000 ms
        let ts: i64 = 1774036800000;
        let path = screenshot_path(&base, ts, "display1");

        assert_eq!(
            path,
            PathBuf::from("/data/screenshots/2026/03/21/1774036800000_display1.heif")
        );
    }

    #[test]
    fn audio_path_has_correct_structure() {
        let base = PathBuf::from("/data");
        let ts: i64 = 1774036800000;
        let path = audio_path(&base, ts, "mic");

        assert_eq!(
            path,
            PathBuf::from("/data/audio/2026/03/21/1774036800000_mic.opus")
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
```

**Step 2: Run tests to verify they fail**

```bash
cd chronicle-daemon && cargo test -p chronicle-storage
```

Expected: FAIL — functions don't exist.

**Step 3: Write the implementation**

```rust
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
```

**Step 4: Add module to lib.rs**

Add `pub(crate) mod files;` to lib.rs.

**Step 5: Run tests to verify they pass**

```bash
cd chronicle-daemon && cargo test -p chronicle-storage
```

Expected: all tests pass (3 from files + previous = 11 total).

**Step 6: Commit**

```bash
git add chronicle-daemon/crates/storage/src/files.rs chronicle-daemon/crates/storage/src/lib.rs
git commit -m "feat(storage): add file path generation and directory management" \
  -m "Date-partitioned paths for screenshots (HEIF) and audio (Opus). Creates parent directories on demand." \
  -m "Part of HEU-236"
```

---

### Task 7: Storage struct and open

**Files:**
- Modify: `chronicle-daemon/crates/storage/src/lib.rs`

This is the core struct that ties everything together. After this task, callers
can open a database and it will be ready for use.

**Step 1: Write the failing test**

Add to `lib.rs`:

```rust
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
        let storage = Storage::open(config).await.unwrap();
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

        // Verify we can query a table created by migrations
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
```

**Step 2: Run tests to verify they fail**

```bash
cd chronicle-daemon && cargo test -p chronicle-storage
```

Expected: FAIL — `Storage` struct and `open` don't exist.

**Step 3: Write the implementation**

Update `lib.rs` to add the `Storage` struct and `open` method:

```rust
use std::path::PathBuf;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;

pub struct Storage {
    pub(crate) pool: Pool<SqliteConnectionManager>,
    pub(crate) base_dir: PathBuf,
}

impl Storage {
    pub async fn open(config: StorageConfig) -> Result<Self> {
        let base_dir = config.base_dir.clone();

        // Ensure the base directory exists
        std::fs::create_dir_all(&base_dir)?;

        let db_path = base_dir.join("chronicle.db");
        let manager = SqliteConnectionManager::file(&db_path);

        let pool = tokio::task::spawn_blocking(move || {
            let pool = Pool::builder()
                .max_size(config.pool_size as u32)
                .connection_customizer(Box::new(ConnectionCustomizer))
                .build(manager)?;

            // Run migrations on one connection
            let conn = pool.get()?;
            schema::migrate(&conn)?;

            Ok::<_, StorageError>(pool)
        })
        .await??;

        Ok(Self { pool, base_dir })
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
```

Note: `ConnectionCustomizer` applies PRAGMAs on every connection checkout from
the pool, as specified in the design doc.

**Step 4: Run tests to verify they pass**

```bash
cd chronicle-daemon && cargo test -p chronicle-storage
```

Expected: all tests pass.

**Step 5: Commit**

```bash
git add chronicle-daemon/crates/storage/src/lib.rs
git commit -m "feat(storage): add Storage struct with connection pool and migration" \
  -m "r2d2 pool over SQLite with WAL mode. Runs migrations on open, applies pragmas on each connection checkout." \
  -m "Part of HEU-236"
```

---

### Task 8: Screenshot operations

**Files:**
- Create: `chronicle-daemon/crates/storage/src/screenshots.rs`
- Modify: `chronicle-daemon/crates/storage/src/lib.rs`

**Step 1: Write the failing tests**

In `screenshots.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ScreenshotMetadata;
    use crate::schema;

    fn setup_db() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        schema::setup_connection(&conn).unwrap();
        schema::migrate(&conn).unwrap();
        conn
    }

    fn sample_metadata() -> ScreenshotMetadata {
        ScreenshotMetadata {
            timestamp: 1774036800000,
            display_id: "display1".into(),
            app_name: Some("Safari".into()),
            app_bundle_id: Some("com.apple.Safari".into()),
            window_title: Some("GitHub - Chronicle".into()),
            image_path: "/data/screenshots/2026/03/21/1774036800000_display1.heif".into(),
            ocr_text: Some("Hello world login button".into()),
            phash: Some(vec![0xAB, 0xCD]),
            resolution: Some("2560x1440@2".into()),
        }
    }

    #[test]
    fn insert_and_get_screenshot() {
        let conn = setup_db();
        let meta = sample_metadata();
        let id = insert(&conn, &meta).unwrap();
        assert!(id > 0);

        let screenshot = get(&conn, id).unwrap();
        assert_eq!(screenshot.timestamp, meta.timestamp);
        assert_eq!(screenshot.display_id, "display1");
        assert_eq!(screenshot.app_name.as_deref(), Some("Safari"));
        assert_eq!(screenshot.image_path, meta.image_path);
        assert_eq!(screenshot.ocr_text.as_deref(), Some("Hello world login button"));
    }

    #[test]
    fn get_timeline_filters_by_range_and_display() {
        let conn = setup_db();

        // Insert 3 screenshots: two in range, one out
        let mut meta = sample_metadata();
        meta.timestamp = 1000;
        insert(&conn, &meta).unwrap();

        meta.timestamp = 2000;
        insert(&conn, &meta).unwrap();

        meta.timestamp = 5000;
        insert(&conn, &meta).unwrap();

        let results = get_timeline(&conn, 500, 3000, Some("display1")).unwrap();
        assert_eq!(results.len(), 2);

        let results_all_displays = get_timeline(&conn, 500, 3000, None).unwrap();
        assert_eq!(results_all_displays.len(), 2);
    }

    #[test]
    fn update_ocr_text_updates_existing_row() {
        let conn = setup_db();
        let meta = sample_metadata();
        let id = insert(&conn, &meta).unwrap();

        update_ocr_text(&conn, id, "updated OCR content").unwrap();

        let screenshot = get(&conn, id).unwrap();
        assert_eq!(screenshot.ocr_text.as_deref(), Some("updated OCR content"));
    }

    #[test]
    fn insert_triggers_fts_index() {
        let conn = setup_db();
        let meta = sample_metadata();
        insert(&conn, &meta).unwrap();

        // FTS should find the screenshot by OCR text
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM screenshots_fts WHERE screenshots_fts MATCH 'login'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn update_ocr_triggers_fts_reindex() {
        let conn = setup_db();
        let meta = sample_metadata();
        let id = insert(&conn, &meta).unwrap();

        update_ocr_text(&conn, id, "new search terms dashboard").unwrap();

        // Old text should not match
        let old_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM screenshots_fts WHERE screenshots_fts MATCH 'login'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(old_count, 0);

        // New text should match
        let new_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM screenshots_fts WHERE screenshots_fts MATCH 'dashboard'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(new_count, 1);
    }
}
```

**Step 2: Run tests to verify they fail**

```bash
cd chronicle-daemon && cargo test -p chronicle-storage
```

Expected: FAIL — functions don't exist.

**Step 3: Write the implementation**

```rust
use rusqlite::{params, Connection};
use crate::error::Result;
use crate::models::{Screenshot, ScreenshotMetadata};

pub(crate) fn insert(conn: &Connection, meta: &ScreenshotMetadata) -> Result<i64> {
    conn.execute(
        "INSERT INTO screenshots (timestamp, display_id, app_name, app_bundle_id, window_title, image_path, ocr_text, phash, resolution)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            meta.timestamp,
            meta.display_id,
            meta.app_name,
            meta.app_bundle_id,
            meta.window_title,
            meta.image_path,
            meta.ocr_text,
            meta.phash,
            meta.resolution,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub(crate) fn get(conn: &Connection, id: i64) -> Result<Screenshot> {
    let screenshot = conn.query_row(
        "SELECT id, timestamp, display_id, app_name, app_bundle_id, window_title, image_path, ocr_text, phash, resolution, created_at
         FROM screenshots WHERE id = ?1",
        params![id],
        |row| {
            Ok(Screenshot {
                id: row.get(0)?,
                timestamp: row.get(1)?,
                display_id: row.get(2)?,
                app_name: row.get(3)?,
                app_bundle_id: row.get(4)?,
                window_title: row.get(5)?,
                image_path: row.get(6)?,
                ocr_text: row.get(7)?,
                phash: row.get(8)?,
                resolution: row.get(9)?,
                created_at: row.get(10)?,
            })
        },
    )?;
    Ok(screenshot)
}

pub(crate) fn get_timeline(
    conn: &Connection,
    start: i64,
    end: i64,
    display_id: Option<&str>,
) -> Result<Vec<Screenshot>> {
    let mut sql = String::from(
        "SELECT id, timestamp, display_id, app_name, app_bundle_id, window_title, image_path, ocr_text, phash, resolution, created_at
         FROM screenshots WHERE timestamp >= ?1 AND timestamp <= ?2"
    );

    if display_id.is_some() {
        sql.push_str(" AND display_id = ?3");
    }
    sql.push_str(" ORDER BY timestamp ASC");

    let mut stmt = conn.prepare(&sql)?;

    let rows = if let Some(did) = display_id {
        stmt.query_map(params![start, end, did], row_to_screenshot)?
    } else {
        stmt.query_map(params![start, end], row_to_screenshot)?
    };

    let mut results = Vec::new();
    for row in rows {
        results.push(row??);
    }
    Ok(results)
}

pub(crate) fn update_ocr_text(conn: &Connection, id: i64, ocr_text: &str) -> Result<()> {
    conn.execute(
        "UPDATE screenshots SET ocr_text = ?1 WHERE id = ?2",
        params![ocr_text, id],
    )?;
    Ok(())
}

fn row_to_screenshot(row: &rusqlite::Row) -> rusqlite::Result<Result<Screenshot>> {
    Ok(Ok(Screenshot {
        id: row.get(0)?,
        timestamp: row.get(1)?,
        display_id: row.get(2)?,
        app_name: row.get(3)?,
        app_bundle_id: row.get(4)?,
        window_title: row.get(5)?,
        image_path: row.get(6)?,
        ocr_text: row.get(7)?,
        phash: row.get(8)?,
        resolution: row.get(9)?,
        created_at: row.get(10)?,
    }))
}
```

Note: The `row_to_screenshot` helper returns `rusqlite::Result<Result<Screenshot>>`
to work with `query_map`. The implementer may choose to simplify this with a
different mapping approach — the test behavior is what matters.

**Step 4: Add module to lib.rs and wire up async methods**

Add `pub(crate) mod screenshots;` to lib.rs, and add async methods to `Storage`.

Note: The design doc uses `&str` references for `display_id`, `ocr_text`, and
similar parameters. The async wrappers use owned `String` instead because values
must be moved into the `spawn_blocking` closure (borrowed references can't cross
the move boundary). This is a necessary deviation from the design doc signatures.

```rust
impl Storage {
    pub async fn allocate_screenshot_path(&self, timestamp: i64, display_id: &str) -> Result<PathBuf> {
        let path = files::screenshot_path(&self.base_dir, timestamp, display_id);
        files::ensure_parent_dir(&path)?;
        Ok(path)
    }

    pub async fn insert_screenshot(&self, metadata: ScreenshotMetadata) -> Result<i64> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.get()?;
            screenshots::insert(&conn, &metadata)
        }).await?
    }

    pub async fn get_screenshot(&self, id: i64) -> Result<Screenshot> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.get()?;
            screenshots::get(&conn, id)
        }).await?
    }

    pub async fn get_timeline(&self, start: i64, end: i64, display_id: Option<String>) -> Result<Vec<Screenshot>> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.get()?;
            screenshots::get_timeline(&conn, start, end, display_id.as_deref())
        }).await?
    }

    pub async fn update_ocr_text(&self, id: i64, ocr_text: String) -> Result<()> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.get()?;
            screenshots::update_ocr_text(&conn, id, &ocr_text)
        }).await?
    }
}
```

**Step 5: Run tests to verify they pass**

```bash
cd chronicle-daemon && cargo test -p chronicle-storage
```

Expected: all tests pass.

**Step 6: Commit**

```bash
git add chronicle-daemon/crates/storage/src/screenshots.rs chronicle-daemon/crates/storage/src/lib.rs
git commit -m "feat(storage): add screenshot insert, get, timeline, and OCR update" \
  -m "Sync internal functions with async wrappers. FTS5 triggers verified via tests." \
  -m "Part of HEU-236"
```

---

### Task 9: Audio operations

**Files:**
- Create: `chronicle-daemon/crates/storage/src/audio.rs`
- Modify: `chronicle-daemon/crates/storage/src/lib.rs`

**Step 1: Write the failing tests**

In `audio.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::AudioSegmentMetadata;
    use crate::schema;

    fn setup_db() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        schema::setup_connection(&conn).unwrap();
        schema::migrate(&conn).unwrap();
        conn
    }

    fn sample_metadata() -> AudioSegmentMetadata {
        AudioSegmentMetadata {
            start_timestamp: 1774036800000,
            end_timestamp: 1774036830000,
            source: "mic".into(),
            audio_path: "/data/audio/2026/03/21/1774036800000_mic.opus".into(),
            transcript: Some("meeting notes about project timeline".into()),
            whisper_model: Some("base.en".into()),
            language: Some("en".into()),
        }
    }

    #[test]
    fn insert_and_get_audio_segment() {
        let conn = setup_db();
        let meta = sample_metadata();
        let id = insert(&conn, &meta).unwrap();
        assert!(id > 0);

        let segment = get(&conn, id).unwrap();
        assert_eq!(segment.start_timestamp, meta.start_timestamp);
        assert_eq!(segment.source, "mic");
        assert_eq!(segment.transcript.as_deref(), Some("meeting notes about project timeline"));
    }

    #[test]
    fn insert_rejects_invalid_source() {
        let conn = setup_db();
        let mut meta = sample_metadata();
        meta.source = "invalid".into();
        let result = insert(&conn, &meta);
        assert!(result.is_err());
    }

    #[test]
    fn update_transcript_updates_existing_row() {
        let conn = setup_db();
        let meta = sample_metadata();
        let id = insert(&conn, &meta).unwrap();

        update_transcript(&conn, id, "updated transcript text").unwrap();

        let segment = get(&conn, id).unwrap();
        assert_eq!(segment.transcript.as_deref(), Some("updated transcript text"));
    }

    #[test]
    fn insert_triggers_fts_index() {
        let conn = setup_db();
        let meta = sample_metadata();
        insert(&conn, &meta).unwrap();

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM audio_fts WHERE audio_fts MATCH 'timeline'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn update_transcript_triggers_fts_reindex() {
        let conn = setup_db();
        let meta = sample_metadata();
        let id = insert(&conn, &meta).unwrap();

        update_transcript(&conn, id, "completely different content").unwrap();

        let old_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM audio_fts WHERE audio_fts MATCH 'timeline'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(old_count, 0);

        let new_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM audio_fts WHERE audio_fts MATCH 'different'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(new_count, 1);
    }
}
```

**Step 2: Run tests to verify they fail**

```bash
cd chronicle-daemon && cargo test -p chronicle-storage
```

Expected: FAIL — functions don't exist.

**Step 3: Write the implementation**

```rust
use rusqlite::{params, Connection};
use crate::error::Result;
use crate::models::{AudioSegment, AudioSegmentMetadata};

pub(crate) fn insert(conn: &Connection, meta: &AudioSegmentMetadata) -> Result<i64> {
    conn.execute(
        "INSERT INTO audio_segments (start_timestamp, end_timestamp, source, audio_path, transcript, whisper_model, language)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            meta.start_timestamp,
            meta.end_timestamp,
            meta.source,
            meta.audio_path,
            meta.transcript,
            meta.whisper_model,
            meta.language,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub(crate) fn get(conn: &Connection, id: i64) -> Result<AudioSegment> {
    let segment = conn.query_row(
        "SELECT id, start_timestamp, end_timestamp, source, audio_path, transcript, whisper_model, language, created_at
         FROM audio_segments WHERE id = ?1",
        params![id],
        |row| {
            Ok(AudioSegment {
                id: row.get(0)?,
                start_timestamp: row.get(1)?,
                end_timestamp: row.get(2)?,
                source: row.get(3)?,
                audio_path: row.get(4)?,
                transcript: row.get(5)?,
                whisper_model: row.get(6)?,
                language: row.get(7)?,
                created_at: row.get(8)?,
            })
        },
    )?;
    Ok(segment)
}

pub(crate) fn update_transcript(conn: &Connection, id: i64, transcript: &str) -> Result<()> {
    conn.execute(
        "UPDATE audio_segments SET transcript = ?1 WHERE id = ?2",
        params![transcript, id],
    )?;
    Ok(())
}
```

**Step 4: Add module to lib.rs and wire up async methods**

Add `pub(crate) mod audio;` and the async wrappers. Same `&str` → `String`
deviation as Task 8 for `update_transcript` (move into `spawn_blocking`):

```rust
impl Storage {
    pub async fn allocate_audio_path(&self, timestamp: i64, source: &str) -> Result<PathBuf> {
        let path = files::audio_path(&self.base_dir, timestamp, source);
        files::ensure_parent_dir(&path)?;
        Ok(path)
    }

    pub async fn insert_audio_segment(&self, metadata: AudioSegmentMetadata) -> Result<i64> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.get()?;
            audio::insert(&conn, &metadata)
        }).await?
    }

    pub async fn get_audio_segment(&self, id: i64) -> Result<AudioSegment> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.get()?;
            audio::get(&conn, id)
        }).await?
    }

    pub async fn update_transcript(&self, id: i64, transcript: String) -> Result<()> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.get()?;
            audio::update_transcript(&conn, id, &transcript)
        }).await?
    }
}
```

**Step 5: Run tests to verify they pass**

```bash
cd chronicle-daemon && cargo test -p chronicle-storage
```

Expected: all tests pass.

**Step 6: Commit**

```bash
git add chronicle-daemon/crates/storage/src/audio.rs chronicle-daemon/crates/storage/src/lib.rs
git commit -m "feat(storage): add audio segment insert, get, and transcript update" \
  -m "Sync internal functions with async wrappers. CHECK constraint enforces mic|system source. FTS5 triggers verified." \
  -m "Part of HEU-236"
```

---

### Task 10: Config operations

**Files:**
- Modify: `chronicle-daemon/crates/storage/src/lib.rs`

Config methods are simple enough to live directly in `lib.rs` without a separate
module.

**Step 1: Write the failing tests**

Add to the `tests` module in `lib.rs`:

```rust
#[tokio::test]
async fn get_config_returns_default_value() {
    let dir = tempdir().unwrap();
    let config = StorageConfig { base_dir: dir.path().to_path_buf(), pool_size: 2 };
    let storage = Storage::open(config).await.unwrap();

    let value = storage.get_config("retention_days").await.unwrap();
    assert_eq!(value.as_deref(), Some("30"));
}

#[tokio::test]
async fn set_config_updates_value() {
    let dir = tempdir().unwrap();
    let config = StorageConfig { base_dir: dir.path().to_path_buf(), pool_size: 2 };
    let storage = Storage::open(config).await.unwrap();

    storage.set_config("retention_days", "60").await.unwrap();
    let value = storage.get_config("retention_days").await.unwrap();
    assert_eq!(value.as_deref(), Some("60"));
}

#[tokio::test]
async fn get_config_returns_none_for_missing_key() {
    let dir = tempdir().unwrap();
    let config = StorageConfig { base_dir: dir.path().to_path_buf(), pool_size: 2 };
    let storage = Storage::open(config).await.unwrap();

    let value = storage.get_config("nonexistent").await.unwrap();
    assert!(value.is_none());
}
```

**Step 2: Run tests to verify they fail**

```bash
cd chronicle-daemon && cargo test -p chronicle-storage
```

Expected: FAIL — methods don't exist.

**Step 3: Write the implementation**

Add to `Storage` impl in `lib.rs`:

```rust
impl Storage {
    pub async fn get_config(&self, key: &str) -> Result<Option<String>> {
        let pool = self.pool.clone();
        let key = key.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = pool.get()?;
            let result = conn.query_row(
                "SELECT value FROM config WHERE key = ?1",
                params![key],
                |row| row.get(0),
            );
            match result {
                Ok(value) => Ok(Some(value)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(e.into()),
            }
        }).await?
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
                params![key, value],
            )?;
            Ok(())
        }).await?
    }
}
```

**Step 4: Run tests to verify they pass**

```bash
cd chronicle-daemon && cargo test -p chronicle-storage
```

Expected: all tests pass.

**Step 5: Commit**

```bash
git add chronicle-daemon/crates/storage/src/lib.rs
git commit -m "feat(storage): add config get and set operations" \
  -m "Key-value config with upsert semantics. Default retention_days and capture_interval_ms seeded by migration." \
  -m "Part of HEU-236"
```

---

### Task 11: Unified search

**Files:**
- Create: `chronicle-daemon/crates/storage/src/search.rs`
- Modify: `chronicle-daemon/crates/storage/src/lib.rs`

**Step 1: Write the failing tests**

In `search.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AudioSegmentMetadata, ScreenshotMetadata, SearchFilter};
    use crate::{audio, schema, screenshots};

    fn setup_db() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        schema::setup_connection(&conn).unwrap();
        schema::migrate(&conn).unwrap();
        conn
    }

    fn insert_test_data(conn: &rusqlite::Connection) {
        screenshots::insert(conn, &ScreenshotMetadata {
            timestamp: 1000,
            display_id: "d1".into(),
            app_name: Some("Safari".into()),
            app_bundle_id: Some("com.apple.Safari".into()),
            window_title: Some("GitHub".into()),
            image_path: "/img/1.heif".into(),
            ocr_text: Some("deployment pipeline kubernetes cluster".into()),
            phash: None,
            resolution: None,
        }).unwrap();

        audio::insert(conn, &AudioSegmentMetadata {
            start_timestamp: 2000,
            end_timestamp: 2030,
            source: "mic".into(),
            audio_path: "/audio/1.opus".into(),
            transcript: Some("discussing the kubernetes deployment strategy".into()),
            whisper_model: Some("base.en".into()),
            language: Some("en".into()),
        }).unwrap();
    }

    #[test]
    fn search_finds_screenshot_by_ocr_text() {
        let conn = setup_db();
        insert_test_data(&conn);

        let results = search(&conn, "pipeline", &SearchFilter::All, 10, 0).unwrap();
        assert_eq!(results.len(), 1);
        assert!(matches!(&results[0].source, SearchSource::Screen(_)));
    }

    #[test]
    fn search_finds_audio_by_transcript() {
        let conn = setup_db();
        insert_test_data(&conn);

        let results = search(&conn, "strategy", &SearchFilter::All, 10, 0).unwrap();
        assert_eq!(results.len(), 1);
        assert!(matches!(&results[0].source, SearchSource::Audio(_)));
    }

    #[test]
    fn search_finds_both_with_shared_term() {
        let conn = setup_db();
        insert_test_data(&conn);

        let results = search(&conn, "kubernetes", &SearchFilter::All, 10, 0).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn search_screen_only_filter() {
        let conn = setup_db();
        insert_test_data(&conn);

        let results = search(&conn, "kubernetes", &SearchFilter::ScreenOnly, 10, 0).unwrap();
        assert_eq!(results.len(), 1);
        assert!(matches!(&results[0].source, SearchSource::Screen(_)));
    }

    #[test]
    fn search_audio_only_filter() {
        let conn = setup_db();
        insert_test_data(&conn);

        let results = search(&conn, "kubernetes", &SearchFilter::AudioOnly, 10, 0).unwrap();
        assert_eq!(results.len(), 1);
        assert!(matches!(&results[0].source, SearchSource::Audio(_)));
    }

    #[test]
    fn search_respects_limit_and_offset() {
        let conn = setup_db();
        insert_test_data(&conn);

        let results = search(&conn, "kubernetes", &SearchFilter::All, 1, 0).unwrap();
        assert_eq!(results.len(), 1);

        let results = search(&conn, "kubernetes", &SearchFilter::All, 10, 2).unwrap();
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn search_returns_empty_for_no_match() {
        let conn = setup_db();
        insert_test_data(&conn);

        let results = search(&conn, "nonexistentterm", &SearchFilter::All, 10, 0).unwrap();
        assert!(results.is_empty());
    }
}
```

**Step 2: Run tests to verify they fail**

```bash
cd chronicle-daemon && cargo test -p chronicle-storage
```

Expected: FAIL — `search` function doesn't exist.

**Step 3: Write the implementation**

The search function queries both FTS tables, joins back to the main tables for
full data, unions the results, and orders by FTS5 rank.

```rust
use rusqlite::{params, Connection};
use crate::error::Result;
use crate::models::*;

pub(crate) fn search(
    conn: &Connection,
    query: &str,
    filter: &SearchFilter,
    limit: usize,
    offset: usize,
) -> Result<Vec<SearchResult>> {
    let mut results = Vec::new();

    if matches!(filter, SearchFilter::All | SearchFilter::ScreenOnly) {
        let mut stmt = conn.prepare(
            "SELECT s.id, s.timestamp, s.display_id, s.app_name, s.app_bundle_id,
                    s.window_title, s.image_path, s.ocr_text, s.phash, s.resolution, s.created_at,
                    snippet(screenshots_fts, 0, '<b>', '</b>', '...', 32) as snip,
                    rank
             FROM screenshots_fts
             JOIN screenshots s ON s.id = screenshots_fts.rowid
             WHERE screenshots_fts MATCH ?1
             ORDER BY rank"
        )?;

        let rows = stmt.query_map(params![query], |row| {
            Ok(SearchResult {
                source: SearchSource::Screen(Screenshot {
                    id: row.get(0)?,
                    timestamp: row.get(1)?,
                    display_id: row.get(2)?,
                    app_name: row.get(3)?,
                    app_bundle_id: row.get(4)?,
                    window_title: row.get(5)?,
                    image_path: row.get(6)?,
                    ocr_text: row.get(7)?,
                    phash: row.get(8)?,
                    resolution: row.get(9)?,
                    created_at: row.get(10)?,
                }),
                snippet: row.get(11)?,
                rank: row.get(12)?,
            })
        })?;

        for row in rows {
            results.push(row?);
        }
    }

    if matches!(filter, SearchFilter::All | SearchFilter::AudioOnly) {
        let mut stmt = conn.prepare(
            "SELECT a.id, a.start_timestamp, a.end_timestamp, a.source, a.audio_path,
                    a.transcript, a.whisper_model, a.language, a.created_at,
                    snippet(audio_fts, 0, '<b>', '</b>', '...', 32) as snip,
                    rank
             FROM audio_fts
             JOIN audio_segments a ON a.id = audio_fts.rowid
             WHERE audio_fts MATCH ?1
             ORDER BY rank"
        )?;

        let rows = stmt.query_map(params![query], |row| {
            Ok(SearchResult {
                source: SearchSource::Audio(AudioSegment {
                    id: row.get(0)?,
                    start_timestamp: row.get(1)?,
                    end_timestamp: row.get(2)?,
                    source: row.get(3)?,
                    audio_path: row.get(4)?,
                    transcript: row.get(5)?,
                    whisper_model: row.get(6)?,
                    language: row.get(7)?,
                    created_at: row.get(8)?,
                }),
                snippet: row.get(9)?,
                rank: row.get(10)?,
            })
        })?;

        for row in rows {
            results.push(row?);
        }
    }

    // Sort combined results by rank (lower is better in FTS5)
    results.sort_by(|a, b| a.rank.partial_cmp(&b.rank).unwrap_or(std::cmp::Ordering::Equal));

    // Apply limit and offset
    let results: Vec<_> = results.into_iter().skip(offset).take(limit).collect();

    Ok(results)
}
```

**Step 4: Add module to lib.rs and wire up async method**

Add `pub(crate) mod search;` and:

```rust
impl Storage {
    pub async fn search(&self, query: &str, filter: SearchFilter, limit: usize, offset: usize) -> Result<Vec<SearchResult>> {
        let pool = self.pool.clone();
        let query = query.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = pool.get()?;
            search::search(&conn, &query, &filter, limit, offset)
        }).await?
    }
}
```

**Step 5: Run tests to verify they pass**

```bash
cd chronicle-daemon && cargo test -p chronicle-storage
```

Expected: all tests pass.

**Step 6: Commit**

```bash
git add chronicle-daemon/crates/storage/src/search.rs chronicle-daemon/crates/storage/src/lib.rs
git commit -m "feat(storage): add unified FTS5 search across screenshots and audio" \
  -m "Queries both FTS tables, joins to main tables, merges by rank. Supports All/ScreenOnly/AudioOnly filters with limit+offset." \
  -m "Part of HEU-236"
```

---

### Task 12: Retention cleanup

**Files:**
- Create: `chronicle-daemon/crates/storage/src/retention.rs`
- Modify: `chronicle-daemon/crates/storage/src/lib.rs`

**Step 1: Write the failing tests**

In `retention.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AudioSegmentMetadata, ScreenshotMetadata};
    use crate::{audio, schema, screenshots};
    use std::fs;

    fn setup_db() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        schema::setup_connection(&conn).unwrap();
        schema::migrate(&conn).unwrap();
        conn
    }

    #[test]
    fn cleanup_deletes_expired_screenshots() {
        let conn = setup_db();
        let now = chrono::Utc::now().timestamp_millis();
        let old = now - (31 * 86_400 * 1000); // 31 days ago

        screenshots::insert(&conn, &ScreenshotMetadata {
            timestamp: old,
            display_id: "d1".into(),
            app_name: None, app_bundle_id: None, window_title: None,
            image_path: "/nonexistent/old.heif".into(),
            ocr_text: None, phash: None, resolution: None,
        }).unwrap();

        screenshots::insert(&conn, &ScreenshotMetadata {
            timestamp: now,
            display_id: "d1".into(),
            app_name: None, app_bundle_id: None, window_title: None,
            image_path: "/nonexistent/new.heif".into(),
            ocr_text: None, phash: None, resolution: None,
        }).unwrap();

        let stats = run_cleanup(&conn, 30).unwrap();
        assert_eq!(stats.screenshots_deleted, 1);

        // New screenshot should still exist
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM screenshots", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn cleanup_deletes_expired_audio() {
        let conn = setup_db();
        let now = chrono::Utc::now().timestamp_millis();
        let old = now - (31 * 86_400 * 1000);

        audio::insert(&conn, &AudioSegmentMetadata {
            start_timestamp: old,
            end_timestamp: old + 30000,
            source: "mic".into(),
            audio_path: "/nonexistent/old.opus".into(),
            transcript: None, whisper_model: None, language: None,
        }).unwrap();

        let stats = run_cleanup(&conn, 30).unwrap();
        assert_eq!(stats.audio_segments_deleted, 1);
    }

    #[test]
    fn cleanup_deletes_associated_files() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("old.heif");
        fs::write(&file_path, b"fake image data").unwrap();

        let conn = setup_db();
        let old = chrono::Utc::now().timestamp_millis() - (31 * 86_400 * 1000);

        screenshots::insert(&conn, &ScreenshotMetadata {
            timestamp: old,
            display_id: "d1".into(),
            app_name: None, app_bundle_id: None, window_title: None,
            image_path: file_path.to_string_lossy().into(),
            ocr_text: None, phash: None, resolution: None,
        }).unwrap();

        let stats = run_cleanup(&conn, 30).unwrap();
        assert_eq!(stats.screenshots_deleted, 1);
        assert!(!file_path.exists(), "file should be deleted");
        assert!(stats.bytes_freed > 0);
    }

    #[test]
    fn cleanup_handles_missing_files_gracefully() {
        let conn = setup_db();
        let old = chrono::Utc::now().timestamp_millis() - (31 * 86_400 * 1000);

        screenshots::insert(&conn, &ScreenshotMetadata {
            timestamp: old,
            display_id: "d1".into(),
            app_name: None, app_bundle_id: None, window_title: None,
            image_path: "/nonexistent/file.heif".into(),
            ocr_text: None, phash: None, resolution: None,
        }).unwrap();

        // Should not error even though file doesn't exist
        let stats = run_cleanup(&conn, 30).unwrap();
        assert_eq!(stats.screenshots_deleted, 1);
    }

    #[test]
    fn sweep_orphans_deletes_untracked_files() {
        let dir = tempfile::tempdir().unwrap();
        let screenshots_dir = dir.path().join("screenshots/2026/03/21");
        fs::create_dir_all(&screenshots_dir).unwrap();

        let orphan = screenshots_dir.join("999_d1.heif");
        fs::write(&orphan, b"orphan data").unwrap();

        let conn = setup_db();
        let stats = sweep_orphans(&conn, dir.path()).unwrap();
        assert_eq!(stats.screenshots_deleted + stats.audio_segments_deleted, 1);
        assert!(!orphan.exists());
    }
}
```

**Step 2: Run tests to verify they fail**

```bash
cd chronicle-daemon && cargo test -p chronicle-storage
```

Expected: FAIL — functions don't exist.

**Step 3: Write the implementation**

```rust
use std::path::Path;
use rusqlite::{params, Connection};
use crate::error::Result;
use crate::models::CleanupStats;

const CLEANUP_BATCH_SIZE: usize = 500;

pub(crate) fn run_cleanup(conn: &Connection, retention_days: u32) -> Result<CleanupStats> {
    let cutoff = chrono::Utc::now().timestamp_millis() - (retention_days as i64 * 86_400 * 1000);
    let mut stats = CleanupStats::default();

    // Delete expired screenshots in batches
    loop {
        let batch: Vec<(i64, String)> = {
            let mut stmt = conn.prepare(
                "SELECT id, image_path FROM screenshots WHERE timestamp < ?1 LIMIT ?2"
            )?;
            stmt.query_map(params![cutoff, CLEANUP_BATCH_SIZE as i64], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
        };

        if batch.is_empty() { break; }

        let ids: Vec<i64> = batch.iter().map(|(id, _)| *id).collect();
        let placeholders: String = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!("DELETE FROM screenshots WHERE id IN ({})", placeholders);
        let params: Vec<&dyn rusqlite::types::ToSql> = ids.iter().map(|id| id as &dyn rusqlite::types::ToSql).collect();
        conn.execute(&sql, params.as_slice())?;
        stats.screenshots_deleted += batch.len();

        for (_, path) in &batch {
            stats.bytes_freed += delete_file_if_exists(path);
        }
    }

    // Delete expired audio segments in batches
    loop {
        let batch: Vec<(i64, String)> = {
            let mut stmt = conn.prepare(
                "SELECT id, audio_path FROM audio_segments WHERE start_timestamp < ?1 LIMIT ?2"
            )?;
            stmt.query_map(params![cutoff, CLEANUP_BATCH_SIZE as i64], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
        };

        if batch.is_empty() { break; }

        let ids: Vec<i64> = batch.iter().map(|(id, _)| *id).collect();
        let placeholders: String = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!("DELETE FROM audio_segments WHERE id IN ({})", placeholders);
        let params: Vec<&dyn rusqlite::types::ToSql> = ids.iter().map(|id| id as &dyn rusqlite::types::ToSql).collect();
        conn.execute(&sql, params.as_slice())?;
        stats.audio_segments_deleted += batch.len();

        for (_, path) in &batch {
            stats.bytes_freed += delete_file_if_exists(path);
        }
    }

    Ok(stats)
}

pub(crate) fn sweep_orphans(conn: &Connection, base_dir: &Path) -> Result<CleanupStats> {
    let mut stats = CleanupStats::default();

    // Sweep screenshot orphans
    sweep_directory(conn, base_dir, "screenshots", "SELECT 1 FROM screenshots WHERE image_path = ?1", &mut stats)?;

    // Sweep audio orphans
    sweep_directory(conn, base_dir, "audio", "SELECT 1 FROM audio_segments WHERE audio_path = ?1", &mut stats)?;

    Ok(stats)
}

fn sweep_directory(
    conn: &Connection,
    base_dir: &Path,
    subdir: &str,
    check_sql: &str,
    stats: &mut CleanupStats,
) -> Result<()> {
    let dir = base_dir.join(subdir);
    if !dir.exists() { return Ok(()); }

    for entry in walkdir(&dir)? {
        let path = entry;
        let path_str = path.to_string_lossy().to_string();

        let exists: bool = conn
            .query_row(check_sql, params![path_str], |_| Ok(true))
            .unwrap_or(false);

        if !exists {
            let freed = delete_file_if_exists(&path_str);
            stats.bytes_freed += freed;
            if subdir == "screenshots" {
                stats.screenshots_deleted += 1;
            } else {
                stats.audio_segments_deleted += 1;
            }
        }
    }

    Ok(())
}

fn walkdir(dir: &Path) -> Result<Vec<std::path::PathBuf>> {
    let mut files = Vec::new();
    if !dir.is_dir() { return Ok(files); }
    walk_recursive(dir, &mut files)?;
    Ok(files)
}

fn walk_recursive(dir: &Path, files: &mut Vec<std::path::PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            walk_recursive(&path, files)?;
        } else {
            files.push(path);
        }
    }
    Ok(())
}

fn delete_file_if_exists(path: &str) -> u64 {
    let path = Path::new(path);
    match std::fs::metadata(path) {
        Ok(meta) => {
            let size = meta.len();
            let _ = std::fs::remove_file(path);
            size
        }
        Err(_) => 0,
    }
}
```

**Step 4: Add module to lib.rs and wire up async methods**

Add `pub(crate) mod retention;` and:

```rust
impl Storage {
    pub async fn run_cleanup(&self) -> Result<CleanupStats> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.get()?;
            let retention_days: u32 = conn
                .query_row("SELECT value FROM config WHERE key = 'retention_days'", [], |row| {
                    let val: String = row.get(0)?;
                    Ok(val.parse::<u32>().unwrap_or(30))
                })
                .unwrap_or(30);
            retention::run_cleanup(&conn, retention_days)
        }).await?
    }

    pub async fn sweep_orphans(&self) -> Result<CleanupStats> {
        let pool = self.pool.clone();
        let base_dir = self.base_dir.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.get()?;
            retention::sweep_orphans(&conn, &base_dir)
        }).await?
    }
}
```

**Step 5: Run tests to verify they pass**

```bash
cd chronicle-daemon && cargo test -p chronicle-storage
```

Expected: all tests pass.

**Step 6: Commit**

```bash
git add chronicle-daemon/crates/storage/src/retention.rs chronicle-daemon/crates/storage/src/lib.rs
git commit -m "feat(storage): add retention cleanup and orphan sweep" \
  -m "Batched deletion (500 rows) with DB-first ordering. Orphan sweep walks media directories and removes untracked files." \
  -m "Part of HEU-236"
```

---

### Task 13: Storage status

**Files:**
- Modify: `chronicle-daemon/crates/storage/src/lib.rs`

**Step 1: Write the failing tests**

Add to the `tests` module in `lib.rs`:

```rust
#[tokio::test]
async fn status_returns_correct_counts() {
    let dir = tempdir().unwrap();
    let config = StorageConfig { base_dir: dir.path().to_path_buf(), pool_size: 2 };
    let storage = Storage::open(config).await.unwrap();

    storage.insert_screenshot(ScreenshotMetadata {
        timestamp: 1000,
        display_id: "d1".into(),
        app_name: None, app_bundle_id: None, window_title: None,
        image_path: "/fake/1.heif".into(),
        ocr_text: None, phash: None, resolution: None,
    }).await.unwrap();

    storage.insert_audio_segment(AudioSegmentMetadata {
        start_timestamp: 2000,
        end_timestamp: 2030,
        source: "mic".into(),
        audio_path: "/fake/1.opus".into(),
        transcript: None, whisper_model: None, language: None,
    }).await.unwrap();

    let status = storage.status().await.unwrap();
    assert_eq!(status.screenshot_count, 1);
    assert_eq!(status.audio_segment_count, 1);
    assert!(status.db_size_bytes > 0);
    assert_eq!(status.oldest_entry, Some(1000));
}

#[tokio::test]
async fn status_on_empty_db() {
    let dir = tempdir().unwrap();
    let config = StorageConfig { base_dir: dir.path().to_path_buf(), pool_size: 2 };
    let storage = Storage::open(config).await.unwrap();

    let status = storage.status().await.unwrap();
    assert_eq!(status.screenshot_count, 0);
    assert_eq!(status.audio_segment_count, 0);
    assert!(status.oldest_entry.is_none());
}
```

**Step 2: Run tests to verify they fail**

```bash
cd chronicle-daemon && cargo test -p chronicle-storage
```

Expected: FAIL — `status` method doesn't exist.

**Step 3: Write the implementation**

Add to `Storage` impl in `lib.rs`:

```rust
impl Storage {
    pub async fn status(&self) -> Result<StorageStatus> {
        let pool = self.pool.clone();
        let base_dir = self.base_dir.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.get()?;

            let screenshot_count: u64 = conn
                .query_row("SELECT COUNT(*) FROM screenshots", [], |row| row.get(0))?;

            let audio_segment_count: u64 = conn
                .query_row("SELECT COUNT(*) FROM audio_segments", [], |row| row.get(0))?;

            let oldest_screenshot: Option<i64> = conn
                .query_row("SELECT MIN(timestamp) FROM screenshots", [], |row| row.get(0))
                .unwrap_or(None);

            let oldest_audio: Option<i64> = conn
                .query_row("SELECT MIN(start_timestamp) FROM audio_segments", [], |row| row.get(0))
                .unwrap_or(None);

            let oldest_entry = match (oldest_screenshot, oldest_audio) {
                (Some(s), Some(a)) => Some(s.min(a)),
                (Some(s), None) => Some(s),
                (None, Some(a)) => Some(a),
                (None, None) => None,
            };

            let db_path = base_dir.join("chronicle.db");
            let db_size_bytes = std::fs::metadata(&db_path).map(|m| m.len()).unwrap_or(0);

            // Walk media directories to calculate total disk usage
            let mut total_disk_usage_bytes = db_size_bytes;
            for subdir in &["screenshots", "audio"] {
                let dir = base_dir.join(subdir);
                if dir.exists() {
                    total_disk_usage_bytes += dir_size(&dir);
                }
            }

            Ok(StorageStatus {
                db_size_bytes,
                screenshot_count,
                audio_segment_count,
                total_disk_usage_bytes,
                oldest_entry,
            })
        }).await?
    }
}

fn dir_size(path: &std::path::Path) -> u64 {
    let mut size = 0;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                size += dir_size(&p);
            } else {
                size += entry.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    size
}
```

**Step 4: Run tests to verify they pass**

```bash
cd chronicle-daemon && cargo test -p chronicle-storage
```

Expected: all tests pass.

**Step 5: Commit**

```bash
git add chronicle-daemon/crates/storage/src/lib.rs
git commit -m "feat(storage): add status reporting" \
  -m "Returns DB size, row counts, total disk usage, and oldest entry timestamp." \
  -m "Part of HEU-236"
```

---

### Task 14: Integration test — full workflow

**Files:**
- Create: `chronicle-daemon/crates/storage/tests/integration.rs`

This test exercises the complete async API end-to-end: open database, allocate
paths, insert data, search, update text, re-search, cleanup, status.

**Step 1: Write the integration test**

```rust
use chronicle_storage::*;
use tempfile::tempdir;

#[tokio::test]
async fn full_workflow() {
    let dir = tempdir().unwrap();
    let config = StorageConfig {
        base_dir: dir.path().to_path_buf(),
        pool_size: 2,
    };

    // Open
    let storage = Storage::open(config).await.unwrap();

    // Allocate paths
    let screenshot_path = storage.allocate_screenshot_path(1774036800000, "display1").await.unwrap();
    assert!(screenshot_path.parent().unwrap().exists());
    assert!(screenshot_path.to_string_lossy().contains("2026/03/21"));

    let audio_path = storage.allocate_audio_path(1774036800000, "mic").await.unwrap();
    assert!(audio_path.parent().unwrap().exists());

    // Write fake files so status can measure disk usage
    std::fs::write(&screenshot_path, b"fake heif data").unwrap();
    std::fs::write(&audio_path, b"fake opus data").unwrap();

    // Insert screenshot
    let ss_id = storage.insert_screenshot(ScreenshotMetadata {
        timestamp: 1774036800000,
        display_id: "display1".into(),
        app_name: Some("Safari".into()),
        app_bundle_id: Some("com.apple.Safari".into()),
        window_title: Some("Chronicle Repo".into()),
        image_path: screenshot_path.to_string_lossy().into(),
        ocr_text: Some("kubernetes deployment pipeline".into()),
        phash: None,
        resolution: Some("2560x1440@2".into()),
    }).await.unwrap();

    // Insert audio
    let audio_id = storage.insert_audio_segment(AudioSegmentMetadata {
        start_timestamp: 1774036800000,
        end_timestamp: 1774036830000,
        source: "mic".into(),
        audio_path: audio_path.to_string_lossy().into(),
        transcript: Some("discussing the kubernetes migration plan".into()),
        whisper_model: Some("base.en".into()),
        language: Some("en".into()),
    }).await.unwrap();

    // Search — should find both
    let results = storage.search("kubernetes", SearchFilter::All, 10, 0).await.unwrap();
    assert_eq!(results.len(), 2);

    // Search — screen only
    let results = storage.search("kubernetes", SearchFilter::ScreenOnly, 10, 0).await.unwrap();
    assert_eq!(results.len(), 1);

    // Update OCR text
    storage.update_ocr_text(ss_id, "updated content grafana dashboard".to_string()).await.unwrap();

    // Old term gone, new term found
    let results = storage.search("pipeline", SearchFilter::ScreenOnly, 10, 0).await.unwrap();
    assert_eq!(results.len(), 0);
    let results = storage.search("grafana", SearchFilter::ScreenOnly, 10, 0).await.unwrap();
    assert_eq!(results.len(), 1);

    // Update transcript
    storage.update_transcript(audio_id, "new transcript about CI pipeline".to_string()).await.unwrap();
    let results = storage.search("pipeline", SearchFilter::AudioOnly, 10, 0).await.unwrap();
    assert_eq!(results.len(), 1);

    // Timeline
    let timeline = storage.get_timeline(
        1774036700000, 1774036900000, Some("display1".into())
    ).await.unwrap();
    assert_eq!(timeline.len(), 1);

    // Config
    let retention = storage.get_config("retention_days").await.unwrap();
    assert_eq!(retention.as_deref(), Some("30"));
    storage.set_config("retention_days", "7").await.unwrap();
    let retention = storage.get_config("retention_days").await.unwrap();
    assert_eq!(retention.as_deref(), Some("7"));

    // Status
    let status = storage.status().await.unwrap();
    assert_eq!(status.screenshot_count, 1);
    assert_eq!(status.audio_segment_count, 1);
    assert!(status.db_size_bytes > 0);
    assert!(status.total_disk_usage_bytes > status.db_size_bytes);

    // Cleanup (nothing expired with 7 day retention and fresh data)
    let stats = storage.run_cleanup().await.unwrap();
    assert_eq!(stats.screenshots_deleted, 0);
}
```

**Step 2: Run the integration test**

```bash
cd chronicle-daemon && cargo test -p chronicle-storage --test integration
```

Expected: PASS. If any assertion fails, the issue is in a prior task's
implementation — fix the root cause, don't modify the test.

**Step 3: Commit**

```bash
git add chronicle-daemon/crates/storage/tests/integration.rs
git commit -m "test(storage): add full workflow integration test" \
  -m "Exercises open, allocate, insert, search, update, timeline, config, status, and cleanup end-to-end." \
  -m "Part of HEU-236"
```

---

### Task 15: Verify the crate builds clean

**Step 1: Run full test suite**

```bash
cd chronicle-daemon && cargo test -p chronicle-storage
```

Expected: all unit and integration tests pass.

**Step 2: Run clippy**

```bash
cd chronicle-daemon && cargo clippy -p chronicle-storage -- -D warnings
```

Expected: no warnings.

**Step 3: Fix any issues**

If clippy or tests fail, fix the issues. Common clippy findings to expect:
unused imports, unnecessary clones, missing `#[must_use]`.

**Step 4: Commit fixes if any**

```bash
git add chronicle-daemon/crates/storage/
git commit -m "chore(storage): fix clippy warnings" \
  -m "Part of HEU-236"
```

Only commit this step if there were actual fixes. Skip if clean.

---

### Task 16: Write developer guide

**Files:**
- Create: `docs/guides/index.md`
- Create: `docs/guides/storage-engine.md`

**Step 1: Create index.md**

A contributor-oriented table of contents. Links to per-component guides.

```markdown
# Developer Guides

How the pieces of Chronicle work and how to change them. Start here if you're
new to the codebase.

For architecture decisions, see [docs/decisions/](../decisions/INDEX.md).
For the project overview, see the [README](../../README.md).

## Components

| Guide | Crate | What it covers |
|-------|-------|----------------|
| [Storage Engine](storage-engine.md) | `chronicle-storage` | SQLite schema, FTS5 search, file management, retention |
```

**Step 2: Create storage-engine.md**

Write the guide covering:
- What the storage engine does (high level)
- How data flows through it (allocate path → write file → insert metadata → FTS
  triggers → search)
- The module structure and where to find things
- How to add a new column or table (migration pattern)
- How search works (external content FTS5, triggers, snippet generation)
- How cleanup works (batch deletion, orphan sweep)
- How to run the tests

Target audience: a developer seeing this crate for the first time. Refer to
ADRs for decision rationale rather than restating it. Use code examples from the
actual implementation.

**Step 3: Commit**

```bash
git add docs/guides/
git commit -m "docs: add developer guide for storage engine" \
  -m "Part of HEU-236"
```