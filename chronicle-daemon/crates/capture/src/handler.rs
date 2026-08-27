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

use crate::drops::send_frame;
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
    /// Process-lifetime drop counters, shared across every engine the daemon
    /// builds. Distinct from `frames_dropped`, which stays per-engine because
    /// `CaptureStats` reports it over IPC.
    drop_counters: Arc<crate::CaptureDropCounters>,
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

            // Two counters by design: `frames_dropped` stays per-engine because
            // CaptureStats reports it over IPC, while `drop_counters` is shared
            // across engine rebuilds so the reporter's totals stay monotonic.
            // No logger here — pipeline.md flags this delivery thread as
            // latency-sensitive, and the drop reporter logs off-thread instead.
            //
            // The accounting lives in `send_frame` so a test can drive it with
            // a real channel; this body is a call precisely so there is nothing
            // here for a test to be unable to reach.
            send_frame(
                &ivars.sender,
                frame,
                &ivars.frames_captured,
                &ivars.frames_dropped,
                &ivars.drop_counters,
            );
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
    /// * `drop_counters`   -- process-lifetime counters read by the daemon's
    ///   drop reporter, which does all the logging for these drops
    pub(crate) fn new(
        sender: mpsc::Sender<CapturedFrame>,
        display_id: u32,
        scale_factor: f64,
        frames_captured: Arc<AtomicU64>,
        frames_dropped: Arc<AtomicU64>,
        drop_counters: Arc<crate::CaptureDropCounters>,
    ) -> Retained<Self> {
        let this = Self::alloc().set_ivars(CaptureOutputHandlerIvars {
            sender,
            display_id,
            scale_factor,
            frames_captured,
            frames_dropped,
            null_buffer_frames: AtomicU64::new(0),
            drop_counters,
        });
        unsafe { objc2::msg_send![super(this), init] }
    }

    /// Build the handler for one display, taking the drop counters from the
    /// config.
    ///
    /// The `Arc::clone` of `config.drop_counters` lives here rather than at
    /// the `engine.rs` call site so a plain unit test can reach it: that call
    /// site sits past `SCShareableContent`, which needs Screen Recording TCC,
    /// so everything downstream of it is `#[ignore]`d. A one-token slip there
    /// — `Arc::clone` to `Arc::new` — used to pass the entire suite.
    pub(crate) fn for_display(
        config: &crate::CaptureConfig<'_>,
        sender: mpsc::Sender<CapturedFrame>,
        display_id: u32,
        scale_factor: f64,
        frames_captured: Arc<AtomicU64>,
        frames_dropped: Arc<AtomicU64>,
    ) -> Retained<Self> {
        Self::new(
            sender,
            display_id,
            scale_factor,
            frames_captured,
            frames_dropped,
            Arc::clone(&config.drop_counters),
        )
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
            Arc::new(crate::CaptureDropCounters::default()),
        );

        // The handler should be usable as an SCStreamOutput protocol object.
        let _protocol_obj = handler.as_protocol_object();
    }

    #[test]
    fn handler_writes_into_the_counters_it_was_given() {
        // A handler wired to its own fresh set compiles cleanly and counts
        // where the reporter never looks — assert it shares the caller's.
        let (tx, _rx) = mpsc::channel(4);
        let drops = Arc::new(crate::CaptureDropCounters::default());
        let handler = CaptureOutputHandler::new(
            tx,
            42,
            2.0,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::clone(&drops),
        );

        assert!(Arc::ptr_eq(&drops, &handler.ivars().drop_counters));
    }

    #[test]
    fn for_display_takes_the_counters_from_the_config() {
        // Covers the hop that engine.rs used to own, where a fresh set
        // instead of a clone passed the whole workspace suite.
        let (tx, _rx) = mpsc::channel(4);
        let config = crate::CaptureConfig::default();
        let handler = CaptureOutputHandler::for_display(
            &config,
            tx,
            7,
            1.0,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
        );

        assert!(
            Arc::ptr_eq(&config.drop_counters, &handler.ivars().drop_counters),
            "handler must share the config's counters, not a fresh set"
        );
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
            Arc::new(crate::CaptureDropCounters::default()),
        );

        let ivars = handler.ivars();
        assert_eq!(ivars.display_id, 42);
        assert!((ivars.scale_factor - 2.0).abs() < f64::EPSILON);
    }
}
