# Storage Engine — Design Document

**Date:** 2026-03-21
**Status:** Approved
**Issue:** HEU-236
**Branch:** mrheuss/heu-236-storage-engine-sqlite-schema-fts5-indexes-and-file
**Base Branch:** main

---

## Overview

Implement the storage layer that all other Chronicle components depend on. A
single `Storage` struct backed by SQLite (WAL mode, FTS5) manages all metadata,
full-text search indexes, media file paths, and retention cleanup. The crate
exposes an async API (wrapping sync rusqlite via `spawn_blocking`) and stays free
of encoding/codec concerns — capture and audio crates write the actual media
files.

---

## 1. Database Schema

SQLite database at `~/Library/Application Support/Chronicle/chronicle.db`.

### 1.1 Main Tables

```sql
CREATE TABLE screenshots (
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

CREATE TABLE audio_segments (
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

CREATE TABLE config (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
```

- **INTEGER PRIMARY KEY** — SQLite rowid alias. Fast, auto-incrementing.
- **Timestamps as integer millis** — faster comparisons, smaller than text.
- **`phash` as BLOB** — perceptual hashes are binary data.
- **`resolution` as TEXT** — stores `"WxH@scale"` (e.g., `"2560x1440@2"`).
- **`config` as key-value** — default values seeded at schema creation
  (`retention_days=30`, `capture_interval_ms=2000`, etc.).

### 1.2 FTS5 Tables (External Content)

FTS5 tables reference the main tables — no data duplication. Sync maintained via
triggers.

```sql
CREATE VIRTUAL TABLE screenshots_fts USING fts5(
    ocr_text, app_name, window_title,
    content=screenshots, content_rowid=id
);

CREATE VIRTUAL TABLE audio_fts USING fts5(
    transcript,
    content=audio_segments, content_rowid=id
);
```

### 1.3 Sync Triggers

```sql
-- Screenshots FTS sync
CREATE TRIGGER screenshots_ai AFTER INSERT ON screenshots BEGIN
    INSERT INTO screenshots_fts(rowid, ocr_text, app_name, window_title)
    VALUES (new.id, new.ocr_text, new.app_name, new.window_title);
END;

CREATE TRIGGER screenshots_ad AFTER DELETE ON screenshots BEGIN
    INSERT INTO screenshots_fts(screenshots_fts, rowid, ocr_text, app_name, window_title)
    VALUES ('delete', old.id, old.ocr_text, old.app_name, old.window_title);
END;

CREATE TRIGGER screenshots_au AFTER UPDATE ON screenshots BEGIN
    INSERT INTO screenshots_fts(screenshots_fts, rowid, ocr_text, app_name, window_title)
    VALUES ('delete', old.id, old.ocr_text, old.app_name, old.window_title);
    INSERT INTO screenshots_fts(rowid, ocr_text, app_name, window_title)
    VALUES (new.id, new.ocr_text, new.app_name, new.window_title);
END;

-- Audio FTS sync
CREATE TRIGGER audio_ai AFTER INSERT ON audio_segments BEGIN
    INSERT INTO audio_fts(rowid, transcript) VALUES (new.id, new.transcript);
END;

CREATE TRIGGER audio_ad AFTER DELETE ON audio_segments BEGIN
    INSERT INTO audio_fts(audio_fts, rowid, transcript) VALUES ('delete', old.id, old.transcript);
END;

CREATE TRIGGER audio_au AFTER UPDATE ON audio_segments BEGIN
    INSERT INTO audio_fts(audio_fts, rowid, transcript) VALUES ('delete', old.id, old.transcript);
    INSERT INTO audio_fts(rowid, transcript) VALUES (new.id, new.transcript);
END;
```

### 1.4 Indexes

```sql
CREATE INDEX idx_screenshots_timestamp ON screenshots(timestamp);
CREATE INDEX idx_screenshots_app ON screenshots(app_bundle_id, timestamp);
CREATE INDEX idx_audio_timestamp ON audio_segments(start_timestamp);
CREATE INDEX idx_audio_source ON audio_segments(source, start_timestamp);
```

---

## 2. File Management

### 2.1 Directory Structure

```
~/Library/Application Support/Chronicle/
├── chronicle.db
├── chronicle.db-wal
├── chronicle.db-shm
├── screenshots/
│   └── YYYY/MM/DD/
│       └── {timestamp}_{display_id}.heif
└── audio/
    └── YYYY/MM/DD/
        └── {timestamp}_{source}.opus
```

### 2.2 Path Generation

The storage crate owns a `base_dir: PathBuf`. All path logic is relative to it.

```rust
pub fn screenshot_path(&self, timestamp: i64, display_id: &str) -> PathBuf {
    let dt = DateTime::from_millis(timestamp);
    self.base_dir
        .join("screenshots")
        .join(format!("{}/{:02}/{:02}", dt.year(), dt.month(), dt.day()))
        .join(format!("{}_{}.heif", timestamp, display_id))
}

pub fn audio_path(&self, timestamp: i64, source: &str) -> PathBuf {
    let dt = DateTime::from_millis(timestamp);
    self.base_dir
        .join("audio")
        .join(format!("{}/{:02}/{:02}", dt.year(), dt.month(), dt.day()))
        .join(format!("{}_{}.opus", timestamp, source))
}
```

### 2.3 Directory Creation

Directories created on demand via `std::fs::create_dir_all` on the parent when a
path is allocated.

### 2.4 Atomic Write Contract

The storage crate doesn't write media files. Callers (capture/audio crates) are
expected to write to a temp file in the same directory and rename. The storage
crate only records metadata after the caller confirms the file exists.

**Flow:**

1. Caller asks `storage.allocate_screenshot_path(timestamp, display_id)` — gets
   a `PathBuf`
2. Storage creates the parent directory if needed
3. Caller writes the file (temp + rename)
4. Caller calls `storage.insert_screenshot(metadata)` with the path
5. Insert goes into SQLite, triggers update FTS

---

## 3. Public API

All methods are async, wrapping sync rusqlite via `spawn_blocking`.

### 3.1 Initialization

```rust
pub struct Storage { /* connection pool, base_dir */ }

pub struct StorageConfig {
    pub base_dir: PathBuf,  // defaults to ~/Library/Application Support/Chronicle/
    pub pool_size: usize,   // defaults to 4
}

impl Storage {
    pub async fn open(config: StorageConfig) -> Result<Self>;
    // Opens/creates DB, runs migrations, enables WAL, seeds default config
}
```

### 3.2 Screenshots

```rust
impl Storage {
    pub async fn allocate_screenshot_path(&self, timestamp: i64, display_id: &str) -> Result<PathBuf>;
    pub async fn insert_screenshot(&self, metadata: ScreenshotMetadata) -> Result<i64>;
    pub async fn get_screenshot(&self, id: i64) -> Result<Screenshot>;
    pub async fn get_timeline(&self, start: i64, end: i64, display_id: Option<&str>) -> Result<Vec<Screenshot>>;
    pub async fn update_ocr_text(&self, id: i64, ocr_text: &str) -> Result<()>;
}
```

### 3.3 Audio

```rust
impl Storage {
    pub async fn allocate_audio_path(&self, timestamp: i64, source: &str) -> Result<PathBuf>;
    pub async fn insert_audio_segment(&self, metadata: AudioSegmentMetadata) -> Result<i64>;
    pub async fn get_audio_segment(&self, id: i64) -> Result<AudioSegment>;
    pub async fn update_transcript(&self, id: i64, transcript: &str) -> Result<()>;
}
```

### 3.4 Search

```rust
pub enum SearchFilter { All, ScreenOnly, AudioOnly }

pub struct SearchResult {
    pub source: SearchSource,  // Screen { ... } or Audio { ... }
    pub snippet: String,       // FTS5 snippet with match highlighting
    pub rank: f64,             // FTS5 rank score
}

impl Storage {
    pub async fn search(&self, query: &str, filter: SearchFilter, limit: usize, offset: usize) -> Result<Vec<SearchResult>>;
}
```

### 3.5 Config

```rust
impl Storage {
    pub async fn get_config(&self, key: &str) -> Result<Option<String>>;
    pub async fn set_config(&self, key: &str, value: &str) -> Result<()>;
}
```

### 3.6 Retention & Cleanup

```rust
pub struct CleanupStats {
    pub screenshots_deleted: usize,
    pub audio_segments_deleted: usize,
    pub bytes_freed: u64,
}

impl Storage {
    pub async fn run_cleanup(&self) -> Result<CleanupStats>;
    pub async fn sweep_orphans(&self) -> Result<CleanupStats>;
}
```

### 3.7 Status

```rust
pub struct StorageStatus {
    pub db_size_bytes: u64,
    pub screenshot_count: u64,
    pub audio_segment_count: u64,
    pub total_disk_usage_bytes: u64,
    pub oldest_entry: Option<i64>,
}

impl Storage {
    pub async fn status(&self) -> Result<StorageStatus>;
}
```

### 3.8 Notes

- **Metadata vs model types.** `ScreenshotMetadata` is the insert input.
  `Screenshot` is the full row (includes `id`, `created_at`). Same for audio.
- **Pagination.** `limit` + `offset` on search. Sufficient for debounced
  search-as-you-type.
- **Late updates.** `update_ocr_text` and `update_transcript` exist because OCR
  and transcription run asynchronously after initial capture insert. The FTS
  `AFTER UPDATE` triggers keep search indexes in sync automatically.

---

## 4. Connection Management

### 4.1 Pool

`r2d2` with `r2d2_sqlite`. Default pool size of 4 connections.

```rust
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;

pub struct Storage {
    pool: Pool<SqliteConnectionManager>,
    base_dir: PathBuf,
}
```

### 4.2 Connection Pragmas

Applied on each connection checkout:

```sql
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 5000;
```

- **WAL** — concurrent readers + writer.
- **synchronous = NORMAL** — safe with WAL, faster than FULL.
- **busy_timeout = 5000** — wait up to 5s if DB is locked rather than failing
  immediately.

### 4.3 Schema Migrations

On `Storage::open`:

1. Check `PRAGMA user_version`
2. Run pending migrations in order
3. Bump `user_version`

Migrations embedded in the binary:

```rust
const MIGRATIONS: &[&str] = &[
    include_str!("migrations/001_initial_schema.sql"),
];
```

---

## 5. Retention & Cleanup

### 5.1 Retention Cleanup

`run_cleanup()` reads `retention_days` from the config table, calculates the
cutoff, and deletes expired data.

**Ordering:** Delete DB rows first, then files. If the process crashes between
the two, orphaned files on disk are harmless — next sweep catches them. The
reverse (files first) would leave DB rows pointing at missing files.

**Batching:** Delete in chunks of ~500 rows to keep write transactions short and
avoid blocking readers.

```rust
loop {
    let batch = query("SELECT id, image_path FROM screenshots
                       WHERE timestamp < ?1 LIMIT 500", cutoff);
    if batch.is_empty() { break; }
    delete_rows(batch.ids);
    delete_files(batch.paths);
}
```

### 5.2 Orphan Sweeping

Walk `screenshots/` and `audio/` directories, check each file against the DB.
Delete files with no matching row. Catches:

- Crash between `allocate_path` and `insert` (file written, metadata never
  recorded)
- Crash between DB delete and file delete during cleanup

Runs less frequently — on daemon startup or once daily.

### 5.3 Scheduling

The storage crate exposes `run_cleanup()` and `sweep_orphans()` but does not
schedule them. The daemon owns scheduling. Storage stays free of timer/scheduler
concerns.

---

## 6. Crate Structure

### 6.1 Dependencies

```toml
[dependencies]
rusqlite = { version = "0.35", features = ["bundled-full"] }
r2d2 = "0.8"
r2d2_sqlite = "0.25"
tokio = { version = "1", features = ["rt"] }
thiserror = "2"
```

- **`bundled-full`** — compiles SQLite from source with FTS5, JSON, and other
  extensions enabled.

### 6.2 Module Layout

```
crates/storage/src/
├── lib.rs              -- pub Storage struct, re-exports
├── error.rs            -- StorageError enum (thiserror)
├── schema.rs           -- migrations, PRAGMA setup
├── models.rs           -- ScreenshotMetadata, Screenshot, AudioSegmentMetadata, etc.
├── screenshots.rs      -- insert, get, timeline, update_ocr_text
├── audio.rs            -- insert, get, update_transcript
├── search.rs           -- unified FTS5 search
├── retention.rs        -- cleanup + orphan sweep
├── files.rs            -- path generation, directory creation
└── migrations/
    └── 001_initial_schema.sql
```

### 6.3 Internal Pattern

Each module contains sync functions taking `&rusqlite::Connection`. The `Storage`
impl grabs a connection from the pool, calls `spawn_blocking`, and delegates.

```rust
// screenshots.rs (internal, sync)
pub(crate) fn insert(conn: &Connection, metadata: &ScreenshotMetadata) -> Result<i64> { ... }

// lib.rs (public, async)
impl Storage {
    pub async fn insert_screenshot(&self, metadata: ScreenshotMetadata) -> Result<i64> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.get()?;
            screenshots::insert(&conn, &metadata)
        }).await?
    }
}
```

### 6.4 Error Type

```rust
#[derive(Debug, thiserror::Error)]
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
```

---

## Architectural Decisions

| # | Decision | Rationale |
|---|---|---|
| 1 | External content FTS5 | No data duplication. Text lives in main tables, FTS indexes reference it via triggers. Standard SQLite pattern. |
| 2 | WAL mode + synchronous=NORMAL | Concurrent reads + writes essential for capture daemon writing while UI searches. NORMAL is safe with WAL. |
| 3 | Async API wrapping sync rusqlite | Callers (daemon, IPC) are async. spawn_blocking keeps the async boundary thin while rusqlite stays sync internally. |
| 4 | r2d2 connection pool (size 4) | Battle-tested pooling. 4 connections covers 1 writer + 2-3 readers for a single-user local app. |
| 5 | Storage manages paths, not encoding | Keeps codec dependencies out of the storage crate. Capture/audio crates own file encoding. |
| 6 | Batched retention deletion (500 rows) | Short write transactions avoid blocking readers during cleanup. |
| 7 | DB-first deletion ordering | Orphaned files are harmless. Dangling DB references (files deleted first) would break search results. |
| 8 | Embedded migrations with user_version | No external migration files. Clean upgrade path as schema evolves. |

## Out of Scope

- **Encryption at rest** — planned for a future version, not v1.
- **Media encoding/decoding** — owned by capture and audio crates.
- **Cleanup scheduling** — owned by the daemon. Storage exposes the operations.
- **IPC protocol** — owned by the IPC crate. Storage is a dependency, not a server.
- **Backup/export** — not in v1. SQLite's `.backup` API makes this easy to add later.