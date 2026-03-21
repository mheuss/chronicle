use std::path::PathBuf;

// --- Configuration ---

#[derive(Debug)]
pub struct StorageConfig {
    pub base_dir: PathBuf,
    pub pool_size: usize,
}

impl Default for StorageConfig {
    fn default() -> Self {
        // Using HOME is intentional. Chronicle is macOS-only and HOME is always
        // set by the system. This avoids pulling in a `dirs` crate dependency
        // just for one path lookup.
        let home = std::env::var("HOME")
            .expect("HOME environment variable must be set");
        let base_dir = PathBuf::from(home).join("Library/Application Support/Chronicle");
        Self {
            base_dir,
            pool_size: 4,
        }
    }
}

// --- Screenshot types ---

#[derive(Debug)]
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

#[derive(Debug)]
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

#[derive(Debug)]
pub struct AudioSegmentMetadata {
    pub start_timestamp: i64,
    pub end_timestamp: i64,
    pub source: String,
    pub audio_path: String,
    pub transcript: Option<String>,
    pub whisper_model: Option<String>,
    pub language: Option<String>,
}

#[derive(Debug)]
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
