use std::path::PathBuf;

// --- Configuration ---

/// Configuration for opening a [`Storage`](crate::Storage) instance.
#[derive(Debug)]
pub struct StorageConfig {
    /// Root directory for the database file and media subdirectories.
    pub base_dir: PathBuf,
    /// Number of SQLite connections in the r2d2 pool.
    pub pool_size: usize,
}

impl Default for StorageConfig {
    fn default() -> Self {
        // Using HOME is intentional. Chronicle is macOS-only and HOME is always
        // set by the system. This avoids pulling in a `dirs` crate dependency
        // just for one path lookup.
        let home = std::env::var("HOME").expect("HOME environment variable must be set");
        let base_dir = PathBuf::from(home).join("Library/Application Support/Chronicle");
        Self {
            base_dir,
            pool_size: 4,
        }
    }
}

// --- Screenshot types ---

/// Screenshot metadata provided at insert time.
#[derive(Debug)]
pub struct ScreenshotMetadata {
    /// Unix timestamp in milliseconds.
    pub timestamp: i64,
    /// Display identifier from macOS.
    pub display_id: String,
    /// Foreground application name, if known.
    pub app_name: Option<String>,
    /// Bundle identifier of the foreground app.
    pub app_bundle_id: Option<String>,
    /// Title of the frontmost window.
    pub window_title: Option<String>,
    /// Path to the HEIF image file on disk.
    pub image_path: String,
    /// OCR-extracted text from the screenshot.
    pub ocr_text: Option<String>,
    /// Perceptual hash for near-duplicate detection.
    pub phash: Option<Vec<u8>>,
    /// Display resolution as a string, e.g. "2560x1440".
    pub resolution: Option<String>,
}

/// A screenshot record as stored in the database.
#[derive(Debug)]
pub struct Screenshot {
    /// Database row ID.
    pub id: i64,
    /// Unix timestamp in milliseconds.
    pub timestamp: i64,
    /// Display identifier from macOS.
    pub display_id: String,
    /// Foreground application name, if known.
    pub app_name: Option<String>,
    /// Bundle identifier of the foreground app.
    pub app_bundle_id: Option<String>,
    /// Title of the frontmost window.
    pub window_title: Option<String>,
    /// Path to the HEIF image file on disk.
    pub image_path: String,
    /// OCR-extracted text from the screenshot.
    pub ocr_text: Option<String>,
    /// Perceptual hash for near-duplicate detection.
    pub phash: Option<Vec<u8>>,
    /// Display resolution as a string, e.g. "2560x1440".
    pub resolution: Option<String>,
    /// Unix timestamp when the row was created.
    pub created_at: i64,
}

// --- Audio types ---

/// Audio segment metadata provided at insert time.
#[derive(Debug)]
pub struct AudioSegmentMetadata {
    /// Unix timestamp in milliseconds for the segment start.
    pub start_timestamp: i64,
    /// Unix timestamp in milliseconds for the segment end.
    pub end_timestamp: i64,
    /// Audio source identifier (e.g. "mic", "system").
    pub source: String,
    /// Path to the Opus audio file on disk.
    pub audio_path: String,
    /// Whisper-generated transcript text.
    pub transcript: Option<String>,
    /// Whisper model used for transcription.
    pub whisper_model: Option<String>,
    /// Detected language of the audio.
    pub language: Option<String>,
}

/// An audio segment record as stored in the database.
#[derive(Debug)]
pub struct AudioSegment {
    /// Database row ID.
    pub id: i64,
    /// Unix timestamp in milliseconds for the segment start.
    pub start_timestamp: i64,
    /// Unix timestamp in milliseconds for the segment end.
    pub end_timestamp: i64,
    /// Audio source identifier (e.g. "mic", "system").
    pub source: String,
    /// Path to the Opus audio file on disk.
    pub audio_path: String,
    /// Whisper-generated transcript text.
    pub transcript: Option<String>,
    /// Whisper model used for transcription.
    pub whisper_model: Option<String>,
    /// Detected language of the audio.
    pub language: Option<String>,
    /// Unix timestamp when the row was created.
    pub created_at: i64,
}

// --- Search types ---

/// Controls which content types a search query should include.
#[derive(Debug, Clone, PartialEq)]
pub enum SearchFilter {
    /// Search both screenshots and audio segments.
    All,
    /// Search screenshots only.
    ScreenOnly,
    /// Search audio segments only.
    AudioOnly,
}

/// The backing record for a search hit.
#[derive(Debug)]
pub enum SearchSource {
    /// Hit came from a screenshot's OCR text.
    Screen(Screenshot),
    /// Hit came from an audio segment's transcript.
    Audio(AudioSegment),
}

/// A single full-text search result with its ranking score.
#[derive(Debug)]
pub struct SearchResult {
    /// The screenshot or audio segment that matched.
    pub source: SearchSource,
    /// Matched text excerpt from the FTS index.
    pub snippet: String,
    /// FTS5 relevance score (lower is better).
    pub rank: f64,
}

// --- Operational types ---

/// How a cleanup run ended.
///
/// `Completed` is the `Default`, and that attribute is load-bearing. Two paths
/// build their stats with `..CleanupStats::default()` and never assign
/// `outcome`: `Storage::sweep_orphans`, and — the one that matters —
/// `retention::run_cleanup_interruptible`'s success path. Move `#[default]` to
/// `Disabled` and every ordinary cleanup run reports `Disabled`, so the
/// scheduled task stops writing checkpoints and each restart re-runs cleanup
/// immediately instead of honouring the period.
/// `an_ordinary_run_reports_completed` in `retention.rs` pins this end to end.
///
/// The scheduled cleanup task (`chronicle-daemon/src/retention_task.rs`) reads
/// this and persists a checkpoint only on `Completed`. A `Disabled` run
/// examined nothing, and a `StopObserved` run may have stopped before
/// exhausting its work; a checkpoint from either would delay the next real
/// cleanup by up to a period. That is the reason this enum exists rather than a bare
/// success/failure.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum CleanupOutcome {
    /// Ran to exhaustion — no expired records remain.
    #[default]
    Completed,
    /// `retention_days` was zero or less, so nothing was examined. Through the
    /// public boundary only zero reaches here; a negative is refused earlier as
    /// [`CleanupOutcome::ConfigInvalid`].
    Disabled,
    /// The stored `retention_days` was refused by `Retention::classify` — not a
    /// number, negative, or above `MAX_RETENTION_DAYS`.
    ///
    /// Nothing was examined and nothing was deleted, and like the other
    /// non-`Completed` variants it records no checkpoint, so the next run
    /// retries.
    ConfigInvalid,
    /// A stop predicate ended the run at a batch boundary. The checkpoint is a
    /// schedule timestamp, not a resume position — every run re-selects from
    /// the oldest expired row.
    ///
    /// Does not guarantee expired rows remain — a stop can land before a
    /// table's first `SELECT`, or after a final batch that emptied it. The
    /// over-report costs one redundant cleanup on the next run.
    StopObserved,
}

/// Summary of what a cleanup or orphan-sweep operation removed.
#[derive(Debug, Default)]
pub struct CleanupStats {
    /// Number of screenshot records deleted.
    pub screenshots_deleted: usize,
    /// Number of audio segment records deleted.
    pub audio_segments_deleted: usize,
    /// Total bytes freed from disk.
    pub bytes_freed: u64,
    /// How the run ended.
    pub outcome: CleanupOutcome,
}

/// Aggregate statistics about the storage database and media files.
#[derive(Debug)]
pub struct StorageStatus {
    /// Size of the SQLite database file (including WAL and SHM).
    pub db_size_bytes: u64,
    /// Total number of screenshot records.
    pub screenshot_count: u64,
    /// Total number of audio segment records.
    pub audio_segment_count: u64,
    /// Combined size of the database and all media files.
    pub total_disk_usage_bytes: u64,
    /// Timestamp of the oldest record, or `None` if the database is empty.
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
