//! Frame handler bridging ScreenCaptureKit callbacks to an mpsc channel.
//!
//! `CaptureOutputHandler` is an Objective-C class (defined via `define_class!`)
//! that conforms to the `SCStreamOutput` protocol. Each callback extracts frame
//! metadata, wraps the raw sample buffer into a `CapturedFrame`, and sends it
//! over a bounded channel via `try_send` to avoid blocking the SCK callback
//! thread.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{Message, define_class, AnyThread, DefinedClass};
use objc2_core_media::CMSampleBuffer;
use objc2_screen_capture_kit::{SCStream, SCStreamOutput, SCStreamOutputType};
use tokio::sync::mpsc;

use crate::pixel_buffer;
use crate::{CapturedFrame, SendableSampleBuffer};

/// Ivars for the `CaptureOutputHandler` ObjC class.
pub(crate) struct CaptureOutputHandlerIvars {
    sender: mpsc::Sender<CapturedFrame>,
    display_id: u32,
    scale_factor: f64,
    width: u32,
    height: u32,
    frames_captured: Arc<AtomicU64>,
    frames_dropped: Arc<AtomicU64>,
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

            // Try to extract actual dimensions from the pixel buffer.
            // Fall back to the values stored at construction time.
            let (width, height) = pixel_buffer::get_image_buffer(sample_buffer)
                .map(|px_buf| {
                    let guard = pixel_buffer::PixelBufferGuard::new(px_buf);
                    match guard {
                        Ok(g) => (g.width() as u32, g.height() as u32),
                        Err(_) => (ivars.width, ivars.height),
                    }
                })
                .unwrap_or((ivars.width, ivars.height));

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
    /// * `width`           -- configured capture width in pixels
    /// * `height`          -- configured capture height in pixels
    /// * `frames_captured` -- shared counter incremented on each successful send
    /// * `frames_dropped`  -- shared counter incremented when the channel is full
    pub(crate) fn new(
        sender: mpsc::Sender<CapturedFrame>,
        display_id: u32,
        scale_factor: f64,
        width: u32,
        height: u32,
        frames_captured: Arc<AtomicU64>,
        frames_dropped: Arc<AtomicU64>,
    ) -> Retained<Self> {
        let this = Self::alloc().set_ivars(CaptureOutputHandlerIvars {
            sender,
            display_id,
            scale_factor,
            width,
            height,
            frames_captured,
            frames_dropped,
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
            1,    // display_id
            2.0,  // scale_factor
            1920, // width
            1080, // height
            frames_captured,
            frames_dropped,
        );

        // The handler should be usable as an SCStreamOutput protocol object.
        let _protocol_obj = handler.as_protocol_object();
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
            2560,
            1440,
            Arc::clone(&frames_captured),
            Arc::clone(&frames_dropped),
        );

        let ivars = handler.ivars();
        assert_eq!(ivars.display_id, 42);
        assert!((ivars.scale_factor - 2.0).abs() < f64::EPSILON);
        assert_eq!(ivars.width, 2560);
        assert_eq!(ivars.height, 1440);
    }
}
