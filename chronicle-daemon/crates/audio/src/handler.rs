//! SCStreamOutput handler for audio capture.
//!
//! Receives CMSampleBuffer callbacks from ScreenCaptureKit, extracts
//! raw PCM audio data, and forwards it as `AudioMessage::Buffer` values
//! over a bounded channel.

use std::ffi::c_char;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::SyncSender;
use std::time::{SystemTime, UNIX_EPOCH};

use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AnyThread, DefinedClass, define_class};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{CMBlockBuffer, CMSampleBuffer};
use objc2_screen_capture_kit::{SCStream, SCStreamOutput, SCStreamOutputType};

use crate::AudioSource;

/// A buffer of PCM audio samples received from ScreenCaptureKit.
#[derive(Debug, Clone)]
pub struct AudioBuffer {
    /// Interleaved f32 PCM samples.
    pub samples: Vec<f32>,
    /// Wall-clock timestamp in milliseconds since epoch.
    pub timestamp_ms: i64,
    /// Whether this came from system audio or the microphone.
    pub source: AudioSource,
}

/// A message on the audio encoding channel: captured PCM, or a request to
/// finalize an in-progress segment. Both travel the same channel so a flush
/// is always ordered after every buffer already queued.
#[derive(Debug)]
pub enum AudioMessage {
    Buffer(AudioBuffer),
    /// Finalize the current mic segment now — sent when the mic is disabled.
    FlushMic,
    /// Finalize the current system-audio segment now — sent when capture is
    /// stopped (system sleep or IPC pause; display sleep is HEU-496).
    FlushSystem,
}

/// Convert a raw byte slice of little-endian f32 PCM data into a Vec of f32 samples.
///
/// Any trailing bytes that don't form a complete f32 (4 bytes) are silently dropped.
fn bytes_to_f32_samples(bytes: &[u8]) -> Vec<f32> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| f32::from_le_bytes(*chunk))
        .collect()
}

/// Map an `SCStreamOutputType` to an `AudioSource`.
///
/// Returns `None` for screen output (type 0) and microphone (type 2), which
/// the SCStream no longer delivers — the microphone has its own
/// `AVAudioEngine` capture path.
fn source_from_output_type(output_type: SCStreamOutputType) -> Option<AudioSource> {
    if output_type == SCStreamOutputType::Audio {
        Some(AudioSource::System)
    } else {
        None
    }
}

/// Extract raw PCM bytes from a `CMSampleBuffer` via its backing `CMBlockBuffer`.
///
/// Returns `None` if the sample buffer has no data buffer or if the data pointer
/// cannot be obtained.
///
/// # Safety
///
/// The caller must ensure `sample_buffer` is a valid audio sample buffer with
/// f32 PCM data in its block buffer.
unsafe fn extract_pcm_bytes(sample_buffer: &CMSampleBuffer) -> Option<Vec<u8>> {
    // Get the CMBlockBuffer backing this sample buffer.
    let block_buffer: CFRetained<CMBlockBuffer> = unsafe { sample_buffer.data_buffer() }?;

    let total_length = unsafe { block_buffer.data_length() };
    if total_length == 0 {
        return None;
    }

    // Get a pointer to the contiguous data.
    let mut data_ptr: *mut c_char = std::ptr::null_mut();
    let mut length_at_offset: usize = 0;
    let status = unsafe {
        block_buffer.data_pointer(
            0,
            &mut length_at_offset,
            std::ptr::null_mut(),
            &mut data_ptr,
        )
    };

    if status != 0 || data_ptr.is_null() {
        return None;
    }

    // Copy the data out before the block buffer is released.
    let slice = unsafe { std::slice::from_raw_parts(data_ptr as *const u8, length_at_offset) };
    Some(slice.to_vec())
}

/// Which side of `TrySendError` a dropped buffer came from.
pub(crate) enum DropCause {
    /// Channel full — downstream is slow.
    Full,
    /// Channel closed — downstream is gone.
    Closed,
}

/// Record one discarded system-audio buffer.
///
/// Pure and callable from a test, so the full/closed mapping is verified
/// without a real `CMSampleBuffer`. The callback does nothing but call this.
pub(crate) fn count_system_drop(counters: &crate::AudioDropCounters, cause: DropCause) {
    match cause {
        DropCause::Full => counters.system_full.fetch_add(1, Ordering::Relaxed),
        DropCause::Closed => counters.system_closed.fetch_add(1, Ordering::Relaxed),
    };
}

/// Ivars for the `AudioOutputHandler` ObjC class.
pub struct AudioOutputHandlerIvars {
    sender: SyncSender<AudioMessage>,
    counters: Arc<crate::AudioDropCounters>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements. We don't implement Drop.
    #[unsafe(super(NSObject))]
    #[ivars = AudioOutputHandlerIvars]
    pub struct AudioOutputHandler;

    // SAFETY: NSObjectProtocol has no extra requirements.
    unsafe impl NSObjectProtocol for AudioOutputHandler {}

    // SAFETY: We implement the optional SCStreamOutput callback method. The
    // method signature matches the protocol definition. We only read from the
    // sample buffer and never store references to ObjC objects past the callback.
    unsafe impl SCStreamOutput for AudioOutputHandler {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn stream_did_output_sample_buffer_of_type(
            &self,
            _stream: &SCStream,
            sample_buffer: &CMSampleBuffer,
            r#type: SCStreamOutputType,
        ) {
            // Ignore screen output.
            let source = match source_from_output_type(r#type) {
                Some(s) => s,
                None => return,
            };

            // Extract raw PCM bytes from the sample buffer.
            let raw_bytes = match unsafe { extract_pcm_bytes(sample_buffer) } {
                Some(b) if !b.is_empty() => b,
                _ => return,
            };

            let samples = bytes_to_f32_samples(&raw_bytes);
            if samples.is_empty() {
                return;
            }

            // Wall-clock timestamp. PTS-to-epoch conversion is fragile
            // (depends on undocumented clock behavior across macOS versions)
            // and unnecessary for 30-second segment boundaries.
            let timestamp_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64;

            let buffer = AudioBuffer {
                samples,
                timestamp_ms,
                source,
            };

            // Best-effort send. Drop on full to avoid blocking the SCK callback thread.
            //
            // ADR-013: no logger on this thread. The daemon's drop reporter
            // turns these counts into log lines off-thread. `source` is always
            // System here — the microphone has its own AVAudioEngine path
            // (ADR-010) with its own counters.
            if let Err(e) = self.ivars().sender.try_send(AudioMessage::Buffer(buffer)) {
                let cause = match e {
                    std::sync::mpsc::TrySendError::Full(_) => DropCause::Full,
                    std::sync::mpsc::TrySendError::Disconnected(_) => DropCause::Closed,
                };
                count_system_drop(&self.ivars().counters, cause);
            }
        }
    }
);

impl AudioOutputHandler {
    /// Create a new handler that sends audio buffers over the given channel.
    ///
    /// `counters` is shared with the daemon's drop reporter, which does all
    /// the logging for the drops this handler records.
    pub fn new(
        sender: SyncSender<AudioMessage>,
        counters: Arc<crate::AudioDropCounters>,
    ) -> Retained<Self> {
        let this = Self::alloc().set_ivars(AudioOutputHandlerIvars { sender, counters });
        unsafe { objc2::msg_send![super(this), init] }
    }

    /// Get a reference suitable for passing to `SCStream::addStreamOutput`.
    pub fn as_protocol_object(&self) -> &ProtocolObject<dyn SCStreamOutput> {
        ProtocolObject::from_ref(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_buffer_stores_samples_and_metadata() {
        let samples = vec![0.1_f32, 0.2, -0.3];
        let buf = AudioBuffer {
            samples: samples.clone(),
            timestamp_ms: 1_700_000_000_000,
            source: AudioSource::System,
        };

        assert_eq!(buf.samples, samples);
        assert_eq!(buf.timestamp_ms, 1_700_000_000_000);
        assert_eq!(buf.source, AudioSource::System);
    }

    #[test]
    fn audio_buffer_clone_is_independent() {
        let original = AudioBuffer {
            samples: vec![1.0, 2.0],
            timestamp_ms: 100,
            source: AudioSource::Microphone,
        };
        let mut cloned = original.clone();
        cloned.samples.push(3.0);

        assert_eq!(original.samples.len(), 2);
        assert_eq!(cloned.samples.len(), 3);
    }

    #[test]
    fn bytes_to_f32_samples_converts_le_bytes() {
        let values = [0.5_f32, -0.25, 1.0];
        let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();

        let result = bytes_to_f32_samples(&bytes);

        assert_eq!(result.len(), 3);
        assert!((result[0] - 0.5).abs() < f32::EPSILON);
        assert!((result[1] - (-0.25)).abs() < f32::EPSILON);
        assert!((result[2] - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn bytes_to_f32_samples_truncates_partial_sample() {
        let value = 0.75_f32;
        let mut bytes = value.to_le_bytes().to_vec();
        bytes.push(0xFF); // extra byte

        let result = bytes_to_f32_samples(&bytes);

        assert_eq!(result.len(), 1);
        assert!((result[0] - 0.75).abs() < f32::EPSILON);
    }

    #[test]
    fn bytes_to_f32_samples_empty_input_gives_empty_output() {
        let result = bytes_to_f32_samples(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn bytes_to_f32_samples_too_short_gives_empty_output() {
        let result = bytes_to_f32_samples(&[0, 1, 2]);
        assert!(result.is_empty());
    }

    #[test]
    fn source_from_output_type_maps_audio_to_system() {
        let result = source_from_output_type(SCStreamOutputType::Audio);
        assert_eq!(result, Some(AudioSource::System));
    }

    #[test]
    fn source_from_output_type_ignores_screen() {
        let result = source_from_output_type(SCStreamOutputType::Screen);
        assert!(result.is_none());
    }

    #[test]
    fn audio_buffer_sent_through_sync_channel() {
        let (tx, rx) = std::sync::mpsc::sync_channel::<AudioMessage>(4);

        let buf = AudioBuffer {
            samples: vec![0.0; 960],
            timestamp_ms: 1_700_000_000_000,
            source: AudioSource::System,
        };

        tx.try_send(AudioMessage::Buffer(buf))
            .expect("channel should accept the buffer");

        let received = rx.try_recv().expect("should receive the message");
        match received {
            AudioMessage::Buffer(b) => {
                assert_eq!(b.samples.len(), 960);
                assert_eq!(b.source, AudioSource::System);
            }
            AudioMessage::FlushMic | AudioMessage::FlushSystem => {
                panic!("expected a buffer message")
            }
        }
    }

    #[test]
    fn sync_channel_drops_on_full() {
        let (tx, _rx) = std::sync::mpsc::sync_channel::<AudioMessage>(1);

        let make_buf = || {
            AudioMessage::Buffer(AudioBuffer {
                samples: vec![0.0],
                timestamp_ms: 100,
                source: AudioSource::Microphone,
            })
        };

        tx.try_send(make_buf()).expect("first send should work");

        let result = tx.try_send(make_buf());
        assert!(result.is_err(), "should fail when channel is full");
    }

    #[test]
    fn try_send_error_maps_to_the_right_counter() {
        let c = crate::AudioDropCounters::default();

        count_system_drop(&c, DropCause::Full);
        assert_eq!(c.snapshot().system_full, 1);
        assert_eq!(c.snapshot().system_closed, 0, "full must not bump closed");

        count_system_drop(&c, DropCause::Closed);
        assert_eq!(c.snapshot().system_closed, 1);
        assert_eq!(c.snapshot().system_full, 1, "closed must not bump full");
    }

    #[test]
    fn handler_carries_the_shared_counters() {
        let (tx, _rx) = std::sync::mpsc::sync_channel::<AudioMessage>(1);
        let counters = Arc::new(crate::AudioDropCounters::default());
        let handler = AudioOutputHandler::new(tx, Arc::clone(&counters));
        counters
            .system_full
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(handler.ivars().counters.snapshot().system_full, 1);
    }

    #[test]
    fn handler_class_registers_with_runtime() {
        // Verifies that the ObjC class created by define_class! is valid
        // and can be instantiated.
        let (tx, _rx) = std::sync::mpsc::sync_channel::<AudioMessage>(4);
        let handler = AudioOutputHandler::new(tx, Arc::new(crate::AudioDropCounters::default()));

        // The handler should be usable as an SCStreamOutput protocol object.
        let _protocol_obj = handler.as_protocol_object();
    }
}
