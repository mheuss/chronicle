//! Screen capture engine for Chronicle.
//!
//! Uses ScreenCaptureKit to capture all connected displays with adaptive
//! frame rates based on screen activity and input events.

use screencapturekit::cm::CMSampleBuffer;

/// Error types for capture operations.
pub mod error;

/// Capture engine managing per-display SCStreams.
pub mod engine;

/// Frame handler bridging SCK callbacks to an mpsc channel.
pub(crate) mod handler;

/// App metadata extraction — foreground app and window title.
pub mod metadata;

pub use engine::CaptureEngine;
pub use error::{CaptureError, Result};
pub use metadata::{AppMetadata, get_frontmost_app};

/// Configuration for the capture engine.
#[derive(Debug, Clone)]
pub struct CaptureConfig {
    /// Minimum time between frames in seconds. Default: 2.0
    pub frame_interval_secs: f64,
    /// Backpressure buffer size for the frame channel. Default: 32
    pub channel_buffer_size: usize,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            frame_interval_secs: 2.0,
            channel_buffer_size: 32,
        }
    }
}

/// A captured frame delivered over the channel.
///
/// Does not derive `Debug` because `CMSampleBuffer` from screencapturekit
/// does not implement `Debug`. Use the metadata fields for logging.
pub struct CapturedFrame {
    /// Raw sample buffer from ScreenCaptureKit.
    pub image_buffer: CMSampleBuffer,
    /// macOS display identifier (CGDirectDisplayID).
    pub display_id: u32,
    /// Unix timestamp in milliseconds.
    pub timestamp: i64,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Retina scale factor (1.0 or 2.0).
    pub scale_factor: f64,
}

/// Health snapshot of the capture engine.
#[derive(Debug, Clone)]
pub struct CaptureStatus {
    /// Number of displays currently being captured.
    pub active_displays: usize,
    /// Total frames sent to the channel.
    pub total_frames_captured: u64,
    /// Total frames dropped due to channel backpressure.
    pub total_frames_dropped: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_config_defaults() {
        let config = CaptureConfig::default();
        assert!((config.frame_interval_secs - 2.0).abs() < f64::EPSILON);
        assert_eq!(config.channel_buffer_size, 32);
    }
}
