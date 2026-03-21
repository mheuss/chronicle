//! Storage engine for Chronicle.
//!
//! SQLite database with FTS5 full-text search indexes for OCR text and
//! audio transcripts. Manages on-disk media files (screenshots, audio).

pub mod error;
pub mod models;
pub(crate) mod schema;
pub(crate) mod files;

pub use error::{StorageError, Result};
pub use models::{
    StorageConfig, ScreenshotMetadata, Screenshot,
    AudioSegmentMetadata, AudioSegment,
    SearchFilter, SearchResult, SearchSource,
    CleanupStats, StorageStatus,
};
