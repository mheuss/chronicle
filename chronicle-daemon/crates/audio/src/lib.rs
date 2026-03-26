//! Audio capture for Chronicle.
//!
//! Captures microphone input and system audio via ScreenCaptureKit.
//! Audio is encoded to Opus in 30-second Ogg segments and delivered
//! over an mpsc channel for downstream storage and transcription.

mod encoder;

pub use encoder::OggOpusEncoder;

use std::path::{Path, PathBuf};
use std::sync::mpsc;

/// Errors from audio capture.
#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    /// ScreenCaptureKit setup or runtime failure.
    #[error("screen capture kit error: {0}")]
    ScreenCaptureKit(String),

    /// Opus encoding failure.
    #[error("encoding error: {0}")]
    Encoding(String),

    /// File I/O failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// Microphone permission denied. System audio may still work.
    #[error("microphone permission denied")]
    MicrophonePermissionDenied,
}

/// Result alias for audio operations.
pub type Result<T> = std::result::Result<T, AudioError>;

/// Audio source type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioSource {
    Microphone,
    System,
}

impl AudioSource {
    /// String representation matching the storage schema.
    pub fn as_str(&self) -> &'static str {
        match self {
            AudioSource::Microphone => "mic",
            AudioSource::System => "system",
        }
    }
}

/// A completed audio segment ready for storage.
#[derive(Debug)]
pub struct CompletedSegment {
    /// Mic or system.
    pub source: AudioSource,
    /// Absolute path to the Opus file on disk.
    pub path: PathBuf,
    /// Segment start timestamp (ms since epoch).
    pub start_timestamp: i64,
    /// Segment end timestamp (ms since epoch).
    pub end_timestamp: i64,
}

/// Configuration for the audio engine.
#[derive(Debug, Clone)]
pub struct AudioConfig {
    /// Segment duration in seconds. Default: 30.
    pub segment_duration_secs: u32,
    /// Opus encoding bitrate in bits/sec. Default: 64000.
    pub bitrate: u32,
    /// Sample rate in Hz. Default: 48000.
    pub sample_rate: u32,
    /// Output directory for segment files.
    pub output_dir: PathBuf,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            segment_duration_secs: 30,
            bitrate: 64_000,
            sample_rate: 48_000,
            output_dir: PathBuf::from("audio"),
        }
    }
}

/// Generate a segment file path: {output_dir}/YYYY/MM/DD/{timestamp}_{source}.opus
pub fn segment_path(output_dir: &Path, timestamp_ms: i64, source: AudioSource) -> PathBuf {
    let secs = timestamp_ms / 1000;
    let dt = time_components(secs);
    output_dir
        .join(dt.year)
        .join(dt.month)
        .join(dt.day)
        .join(format!("{}_{}.opus", timestamp_ms, source.as_str()))
}

struct DateComponents {
    year: String,
    month: String,
    day: String,
}

fn time_components(unix_secs: i64) -> DateComponents {
    let days = unix_secs / 86400;
    let mut y = 1970;
    let mut remaining = days;

    loop {
        let days_in_year = if is_leap(y) { 366 } else { 365 };
        if remaining < days_in_year {
            break;
        }
        remaining -= days_in_year;
        y += 1;
    }

    let leap = is_leap(y);
    let month_days = [
        31,
        if leap { 29 } else { 28 },
        31, 30, 31, 30, 31, 31, 30, 31, 30, 31,
    ];
    let mut m = 0;
    for (i, &d) in month_days.iter().enumerate() {
        if remaining < d {
            m = i;
            break;
        }
        remaining -= d;
    }

    DateComponents {
        year: format!("{y}"),
        month: format!("{:02}", m + 1),
        day: format!("{:02}", remaining + 1),
    }
}

fn is_leap(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_source_as_str() {
        assert_eq!(AudioSource::Microphone.as_str(), "mic");
        assert_eq!(AudioSource::System.as_str(), "system");
    }

    #[test]
    fn audio_config_defaults() {
        let config = AudioConfig::default();
        assert_eq!(config.segment_duration_secs, 30);
        assert_eq!(config.bitrate, 64_000);
        assert_eq!(config.sample_rate, 48_000);
    }

    #[test]
    fn segment_path_has_correct_structure() {
        let dir = PathBuf::from("/data/audio");
        let ts = 1774526400_000_i64;
        let path = segment_path(&dir, ts, AudioSource::System);
        let path_str = path.to_str().unwrap();
        assert!(path_str.contains("2026"));
        assert!(path_str.contains("03"));
        assert!(path_str.contains("26"));
        assert!(path_str.ends_with("_system.opus"));
    }

    #[test]
    fn segment_path_mic_suffix() {
        let dir = PathBuf::from("/tmp");
        let path = segment_path(&dir, 1774526400_000, AudioSource::Microphone);
        assert!(path.to_str().unwrap().ends_with("_mic.opus"));
    }
}
