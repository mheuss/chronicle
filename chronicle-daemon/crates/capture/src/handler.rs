//! Frame handler bridging ScreenCaptureKit callbacks to an mpsc channel.
//!
//! `FrameHandler` implements `SCStreamOutputTrait` so it can be registered
//! with an `SCStream`. Each callback extracts frame metadata, wraps the raw
//! sample buffer into a `CapturedFrame`, and sends it over a bounded channel
//! via `try_send` to avoid blocking the SCK callback thread.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use screencapturekit::prelude::*;
use tokio::sync::mpsc;

use crate::CapturedFrame;

/// Bridges ScreenCaptureKit sample-buffer callbacks into a bounded mpsc channel.
///
/// One `FrameHandler` is created per display. It counts delivered and dropped
/// frames via shared atomic counters so the `CaptureEngine` can report status
/// without locking.
pub(crate) struct FrameHandler {
    sender: mpsc::Sender<CapturedFrame>,
    display_id: u32,
    scale_factor: f64,
    width: u32,
    height: u32,
    frames_captured: Arc<AtomicU64>,
    frames_dropped: Arc<AtomicU64>,
}

impl FrameHandler {
    /// Create a new frame handler for a specific display.
    ///
    /// * `sender`          - bounded channel sender for delivering frames
    /// * `display_id`      - macOS CGDirectDisplayID
    /// * `scale_factor`    - retina scale (1.0 or 2.0)
    /// * `width`           - configured capture width in pixels
    /// * `height`          - configured capture height in pixels
    /// * `frames_captured` - shared counter incremented on each successful send
    /// * `frames_dropped`  - shared counter incremented when the channel is full
    pub(crate) fn new(
        sender: mpsc::Sender<CapturedFrame>,
        display_id: u32,
        scale_factor: f64,
        width: u32,
        height: u32,
        frames_captured: Arc<AtomicU64>,
        frames_dropped: Arc<AtomicU64>,
    ) -> Self {
        Self {
            sender,
            display_id,
            scale_factor,
            width,
            height,
            frames_captured,
            frames_dropped,
        }
    }
}

impl SCStreamOutputTrait for FrameHandler {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, of_type: SCStreamOutputType) {
        if of_type != SCStreamOutputType::Screen {
            return;
        }

        // Try to extract actual dimensions from the pixel buffer. Fall back to
        // the values stored at construction time if the image buffer is absent.
        let (width, height) = sample
            .image_buffer()
            .map(|buf| (buf.width() as u32, buf.height() as u32))
            .unwrap_or((self.width, self.height));

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;

        let frame = CapturedFrame {
            image_buffer: sample,
            display_id: self.display_id,
            timestamp,
            width,
            height,
            scale_factor: self.scale_factor,
        };

        match self.sender.try_send(frame) {
            Ok(()) => {
                self.frames_captured.fetch_add(1, Ordering::Relaxed);
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.frames_dropped.fetch_add(1, Ordering::Relaxed);
                log::warn!(
                    "Frame dropped for display {} (channel full)",
                    self.display_id
                );
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.frames_dropped.fetch_add(1, Ordering::Relaxed);
                log::warn!(
                    "Frame dropped for display {} (channel closed)",
                    self.display_id
                );
            }
        }
    }
}
