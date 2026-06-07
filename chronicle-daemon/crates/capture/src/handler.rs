//! Frame handler bridging ScreenCaptureKit callbacks to an mpsc channel.
//!
//! `CaptureOutputHandler` is an Objective-C class (defined via `define_class!`)
//! that conforms to the `SCStreamOutput` protocol. Each callback extracts frame
//! metadata, wraps the raw sample buffer into a `CapturedFrame`, and sends it
//! over a bounded channel via `try_send` to avoid blocking the SCK callback
//! thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AnyThread, DefinedClass, Message, define_class};
use objc2_core_media::CMSampleBuffer;
use objc2_screen_capture_kit::{SCStream, SCStreamOutput, SCStreamOutputType};
use tokio::sync::mpsc;

use crate::pixel_buffer;
use crate::{CapturedFrame, SendableSampleBuffer};

/// Warn on the first null-image-buffer frame per display, then every Nth, so a
/// genuinely stuck display keeps surfacing without spamming the log at boot.
const NULL_BUFFER_WARN_EVERY: u64 = 150; // ~5 min/display at ~0.5 fps

fn should_warn(null_frame_count: u64) -> bool {
    null_frame_count == 1 || null_frame_count.is_multiple_of(NULL_BUFFER_WARN_EVERY)
}

/// Ivars for the `CaptureOutputHandler` ObjC class.
pub(crate) struct CaptureOutputHandlerIvars {
    sender: mpsc::Sender<CapturedFrame>,
    display_id: u32,
    scale_factor: f64,
    frames_captured: Arc<AtomicU64>,
    frames_dropped: Arc<AtomicU64>,
    /// Per-display count of frames skipped for having no image buffer (HEU-493).
    null_buffer_frames: AtomicU64,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements. We don't implement Drop.
    #[unsafe(super(NSObject))]
    #[ivars = CaptureOutputHandlerIvars]
    pub(crate) struct CaptureOutputHandler;

    // SAFETY: NSObjectProtocol has no extra requirements.
    unsafe impl NSObjectProtocol for CaptureOutputHandler {}

    // SAFETY: We implement the optional SCStreamOutput callback method. The
    // method signature matches the protocol definition. We only read from the
    // sample buffer and never store references to ObjC objects past the callback.
    unsafe impl SCStreamOutput for CaptureOutputHandler {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn stream_did_output_sample_buffer_of_type(
            &self,
            _stream: &SCStream,
            sample_buffer: &CMSampleBuffer,
            r#type: SCStreamOutputType,
        ) {
            if r#type != SCStreamOutputType::Screen {
                return;
            }

            let ivars = self.ivars();

            // ScreenCaptureKit delivers frames with no image buffer during cold
            // display init (boot/wake) — there are no pixels to encode. Skip them
            // here so they never reach the pipeline, which would otherwise log an
            // error and bump frames_failed (HEU-493).
            let Some(px_buf) = pixel_buffer::get_image_buffer(sample_buffer) else {
                let n = ivars.null_buffer_frames.fetch_add(1, Ordering::Relaxed) + 1;
                if should_warn(n) {
                    log::warn!(
                        "display {}: SCK frame had no image buffer (count={n}); \
                         skipping. Expected briefly at boot/wake; a persistent \
                         count means the display isn't producing frames.",
                        ivars.display_id
                    );
                }
                return;
            };
            let width = pixel_buffer::width(px_buf) as u32;
            let height = pixel_buffer::height(px_buf) as u32;

            // Retain the sample buffer so it lives beyond this callback.
            let retained: Retained<CMSampleBuffer> = sample_buffer.retain();

            // i64 to match chronicle-storage's timestamp convention (SQLite INTEGER).
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64;

            let frame = CapturedFrame {
                sample_buffer: SendableSampleBuffer(retained),
                display_id: ivars.display_id,
                timestamp,
                width,
                height,
                scale_factor: ivars.scale_factor,
            };

            match ivars.sender.try_send(frame) {
                Ok(()) => {
                    ivars.frames_captured.fetch_add(1, Ordering::Relaxed);
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    ivars.frames_dropped.fetch_add(1, Ordering::Relaxed);
                    log::warn!(
                        "Frame dropped for display {} (channel full)",
                        ivars.display_id
                    );
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    ivars.frames_dropped.fetch_add(1, Ordering::Relaxed);
                    log::warn!(
                        "Frame dropped for display {} (channel closed)",
                        ivars.display_id
                    );
                }
            }
        }
    }
);

impl CaptureOutputHandler {
    /// Create a new handler for a specific display.
    ///
    /// * `sender`          -- bounded channel sender for delivering frames
    /// * `display_id`      -- macOS CGDirectDisplayID
    /// * `scale_factor`    -- retina scale (1.0 or 2.0)
    /// * `frames_captured` -- shared counter incremented on each successful send
    /// * `frames_dropped`  -- shared counter incremented when the channel is full
    pub(crate) fn new(
        sender: mpsc::Sender<CapturedFrame>,
        display_id: u32,
        scale_factor: f64,
        frames_captured: Arc<AtomicU64>,
        frames_dropped: Arc<AtomicU64>,
    ) -> Retained<Self> {
        let this = Self::alloc().set_ivars(CaptureOutputHandlerIvars {
            sender,
            display_id,
            scale_factor,
            frames_captured,
            frames_dropped,
            null_buffer_frames: AtomicU64::new(0),
        });
        unsafe { objc2::msg_send![super(this), init] }
    }

    /// Get a reference suitable for passing to `SCStream::addStreamOutput`.
    pub(crate) fn as_protocol_object(&self) -> &ProtocolObject<dyn SCStreamOutput> {
        ProtocolObject::from_ref(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handler_class_registers_with_runtime() {
        // Verifies that the ObjC class created by define_class! is valid
        // and can be instantiated.
        let (tx, _rx) = mpsc::channel(4);
        let frames_captured = Arc::new(AtomicU64::new(0));
        let frames_dropped = Arc::new(AtomicU64::new(0));

        let handler = CaptureOutputHandler::new(
            tx,
            1,   // display_id
            2.0, // scale_factor
            frames_captured,
            frames_dropped,
        );

        // The handler should be usable as an SCStreamOutput protocol object.
        let _protocol_obj = handler.as_protocol_object();
    }

    #[test]
    fn should_warn_fires_first_then_every_nth() {
        assert!(should_warn(1));
        assert!(!should_warn(2));
        assert!(!should_warn(149));
        assert!(should_warn(150));
        assert!(!should_warn(151));
        assert!(should_warn(300));
    }

    #[test]
    fn handler_ivars_are_accessible() {
        let (tx, _rx) = mpsc::channel(4);
        let frames_captured = Arc::new(AtomicU64::new(0));
        let frames_dropped = Arc::new(AtomicU64::new(0));

        let handler = CaptureOutputHandler::new(
            tx,
            42,
            2.0,
            Arc::clone(&frames_captured),
            Arc::clone(&frames_dropped),
        );

        let ivars = handler.ivars();
        assert_eq!(ivars.display_id, 42);
        assert!((ivars.scale_factor - 2.0).abs() < f64::EPSILON);
    }
}
