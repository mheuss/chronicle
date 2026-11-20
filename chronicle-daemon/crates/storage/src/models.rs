use std::ffi::OsStr;
use std::path::{Path, PathBuf};

// --- Configuration ---

/// The App Group container ID shared by the daemon and the UI.
pub const APP_GROUP_ID: &str = "7TJEE2YF4G.chronicle";

/// Why [`data_root`] could not resolve a directory.
#[derive(Debug, thiserror::Error)]
pub enum DataRootError {
    #[error("CHRONICLE_DATA_DIR must be an absolute path, got \"{}\"", .0.display())]
    RelativeOverride(PathBuf),
    #[error("HOME must be an absolute path to find the app group container, got \"{}\"", .0.display())]
    RelativeHome(PathBuf),
}

/// Chronicle's data root: `override_dir` when it is set and not empty, else the
/// App Group container under `home`. The override comes from `CHRONICLE_DATA_DIR`.
/// A relative override or home is an error. Each process would otherwise resolve
/// it against its own working directory.
pub fn data_root(home: &Path, override_dir: Option<&OsStr>) -> Result<PathBuf, DataRootError> {
    match override_dir {
        Some(dir) if !dir.is_empty() => {
            let dir = PathBuf::from(dir);
            if dir.is_absolute() {
                Ok(dir)
            } else {
                Err(DataRootError::RelativeOverride(dir))
            }
        }
        _ if !home.is_absolute() => Err(DataRootError::RelativeHome(home.to_path_buf())),
        _ => Ok(home.join("Library/Group Containers").join(APP_GROUP_ID)),
    }
}

/// Configuration for opening a [`Storage`](crate::Storage) instance.
#[derive(Debug)]
pub struct StorageConfig {
    /// Root directory for the database file and media subdirectories.
    pub base_dir: PathBuf,
    /// Number of SQLite connections in the r2d2 pool.
    pub pool_size: usize,
}

impl StorageConfig {
    /// Resolves `base_dir` with [`data_root`] from `HOME` and `CHRONICLE_DATA_DIR`.
    pub fn from_env() -> Result<Self, DataRootError> {
        // HOME instead of a `dirs` dependency: Chronicle is macOS-only. An unset
        // HOME reads as empty, which `data_root` rejects unless the override is set.
        let home = std::env::var_os("HOME").unwrap_or_default();
        let base_dir = data_root(
            Path::new(&home),
            std::env::var_os("CHRONICLE_DATA_DIR").as_deref(),
        )?;
        Ok(Self {
            base_dir,
            pool_size: 4,
        })
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

/// An audio row the pipeline still owes a transcription attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingTranscription {
    /// Database row ID.
    pub id: i64,
    /// Path to the Opus audio file on disk.
    pub audio_path: PathBuf,
}

// --- Search types ---

pub use chronicle_ipc::SearchFilter;

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

    const HOME: &str = "/Users/someone";
    const GROUP_ROOT: &str = "/Users/someone/Library/Group Containers/7TJEE2YF4G.chronicle";

    #[test]
    fn data_root_is_the_group_container_without_an_override() {
        assert_eq!(
            data_root(Path::new(HOME), None).unwrap(),
            PathBuf::from(GROUP_ROOT)
        );
    }

    #[test]
    fn data_root_ignores_an_empty_override() {
        assert_eq!(
            data_root(Path::new(HOME), Some(OsStr::new(""))).unwrap(),
            PathBuf::from(GROUP_ROOT)
        );
    }

    #[test]
    fn data_root_uses_an_absolute_override() {
        assert_eq!(
            data_root(Path::new(HOME), Some(OsStr::new("/tmp/chronicle-dev"))).unwrap(),
            PathBuf::from("/tmp/chronicle-dev")
        );
    }

    #[test]
    fn data_root_rejects_a_relative_override() {
        let err = data_root(Path::new(HOME), Some(OsStr::new("data"))).unwrap_err();
        assert!(matches!(&err, DataRootError::RelativeOverride(dir) if dir == Path::new("data")));
        assert_eq!(
            err.to_string(),
            "CHRONICLE_DATA_DIR must be an absolute path, got \"data\""
        );
    }

    #[test]
    fn data_root_rejects_an_empty_or_relative_home() {
        for home in ["", "home"] {
            let err = data_root(Path::new(home), None).unwrap_err();
            assert!(matches!(&err, DataRootError::RelativeHome(h) if h == Path::new(home)));
        }
        let dir = data_root(Path::new(""), Some(OsStr::new("/tmp/chronicle-dev"))).unwrap();
        assert_eq!(dir, Path::new("/tmp/chronicle-dev"));
    }

    #[test]
    fn cleanup_stats_default_is_zero() {
        let stats = CleanupStats::default();
        assert_eq!(stats.screenshots_deleted, 0);
        assert_eq!(stats.audio_segments_deleted, 0);
        assert_eq!(stats.bytes_freed, 0);
    }
}
