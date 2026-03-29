//! Audio capture for Chronicle.
//!
//! Captures microphone input and system audio via ScreenCaptureKit.
//! Audio is encoded to Opus in 30-second Ogg segments and delivered
//! over an mpsc channel for downstream storage and transcription.

mod accumulator;
mod encoder;
mod engine;
pub(crate) mod handler;

pub use encoder::OggOpusEncoder;
pub use engine::AudioEngine;

use std::path::{Path, PathBuf};

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

/// Fixed sample rate: 48kHz is Opus's native rate and what SCK delivers.
pub const SAMPLE_RATE: u32 = 48_000;

/// Configuration for the audio engine.
#[derive(Debug, Clone)]
pub struct AudioConfig {
    /// Segment duration in seconds. Default: 30.
    pub segment_duration_secs: u32,
    /// Opus encoding bitrate in bits/sec. Default: 64000.
    pub bitrate: u32,
    /// Output directory for segment files.
    pub output_dir: PathBuf,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            segment_duration_secs: 30,
            bitrate: 64_000,
            output_dir: PathBuf::from("audio"),
        }
    }
}

/// Generate a flat segment file path: {output_dir}/{timestamp}_{source}.opus
///
/// Staging paths are flat — no date subdirectories. The permanent path
/// (allocated by chronicle-storage) handles the YYYY/MM/DD hierarchy.
pub fn segment_path(output_dir: &Path, timestamp_ms: i64, source: AudioSource) -> PathBuf {
    output_dir.join(format!("{}_{}.opus", timestamp_ms, source.as_str()))
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
        assert_eq!(SAMPLE_RATE, 48_000);
    }

    #[test]
    fn segment_path_is_flat() {
        let dir = PathBuf::from("/data/staging");
        let ts = 1774526400_000_i64;
        let path = segment_path(&dir, ts, AudioSource::System);
        assert_eq!(
            path,
            PathBuf::from("/data/staging/1774526400000_system.opus")
        );
    }

    #[test]
    fn segment_path_mic_suffix() {
        let dir = PathBuf::from("/tmp");
        let path = segment_path(&dir, 1774526400_000, AudioSource::Microphone);
        assert_eq!(
            path,
            PathBuf::from("/tmp/1774526400000_mic.opus")
        );
    }
}
