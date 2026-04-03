//! Screen capture engine for Chronicle.
//!
//! Uses ScreenCaptureKit to capture all connected displays with adaptive
//! frame rates based on screen activity and input events.

use std::fmt;

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_core_media::CMSampleBuffer;
use objc2_screen_capture_kit::SCStreamOutput;

/// Error types for capture operations.
pub mod error;

/// Capture engine managing per-display SCStreams.
pub mod engine;

/// Frame handler bridging SCK callbacks to an mpsc channel.
pub(crate) mod handler;

/// HEIF encoding for captured frames.
pub mod encoder;

/// App metadata extraction — foreground app and window title.
pub mod metadata;

/// CoreVideo pixel buffer FFI.
pub(crate) mod pixel_buffer;

pub use engine::CaptureEngine;
pub use encoder::encode_heif;
pub use error::{CaptureError, Result};
pub use metadata::{AppMetadata, get_frontmost_app};

/// Thread-safe wrapper around `Retained<CMSampleBuffer>`.
///
/// Apple documents CMSampleBuffer as immutable and thread-safe once created.
/// The objc2 crate doesn't mark it `Send` because it can't verify this
/// generically, but for our use case (passing captured frames from the SCK
/// callback thread to a tokio task) this is safe.
pub struct SendableSampleBuffer(pub Retained<CMSampleBuffer>);

// SAFETY: CMSampleBuffer is immutable after creation and documented as
// thread-safe by Apple. We only read from it (HEIF encoding, pixel access).
unsafe impl Send for SendableSampleBuffer {}

// SAFETY: Same rationale — CMSampleBuffer is immutable. Shared references
// (&SendableSampleBuffer) across await points in tokio::spawn are safe.
unsafe impl Sync for SendableSampleBuffer {}

impl SendableSampleBuffer {
    /// Borrow the inner CMSampleBuffer.
    pub fn inner(&self) -> &CMSampleBuffer {
        &self.0
    }
}

/// Audio output configuration for piggybacking audio on a capture stream.
///
/// The capture engine attaches this to the primary display's SCStream so
/// audio and screen capture share a single ScreenCaptureKit session.
pub struct AudioOutputConfig {
    /// Protocol-erased SCStreamOutput handler from the audio crate.
    pub handler: Retained<ProtocolObject<dyn SCStreamOutput>>,
    /// Dispatch queue for audio sample delivery.
    pub queue: Retained<DispatchQueue>,
    /// Audio sample rate in Hz (48000 for Opus native).
    pub sample_rate: u32,
    /// Number of audio channels (1 = mono).
    pub channel_count: u32,
    /// Whether to also capture microphone input. Default: false.
    pub capture_microphone: bool,
}

/// Configuration for the capture engine.
pub struct CaptureConfig {
    /// Minimum time between frames in seconds. Default: 2.0
    pub frame_interval_secs: f64,
    /// Backpressure buffer size for the frame channel. Default: 32
    pub channel_buffer_size: usize,
    /// Optional audio handler to register on the primary display's stream.
    pub audio: Option<AudioOutputConfig>,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            frame_interval_secs: 2.0,
            channel_buffer_size: 32,
            audio: None,
        }
    }
}

impl fmt::Debug for CaptureConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CaptureConfig")
            .field("frame_interval_secs", &self.frame_interval_secs)
            .field("channel_buffer_size", &self.channel_buffer_size)
            .field("audio", &self.audio.as_ref().map(|_| "Some(...)"))
            .finish()
    }
}

/// A captured frame delivered over the channel.
///
/// Does not derive `Debug` because `CMSampleBuffer` does not implement `Debug`.
/// Use the metadata fields for logging.
pub struct CapturedFrame {
    /// Raw sample buffer from ScreenCaptureKit.
    pub sample_buffer: SendableSampleBuffer,
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

    #[test]
    fn capture_config_defaults_no_audio() {
        let config = CaptureConfig::default();
        assert!(config.audio.is_none());
    }
}
