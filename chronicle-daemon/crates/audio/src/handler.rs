//! SCStreamOutput handler for audio capture.
//!
//! Receives CMSampleBuffer callbacks from ScreenCaptureKit, extracts
//! raw PCM audio data, and forwards it as `AudioBuffer` values over
//! a bounded channel.

use std::ffi::c_char;
use std::sync::mpsc::SyncSender;
use std::time::{SystemTime, UNIX_EPOCH};

use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, AnyThread, DefinedClass};
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

/// Convert a raw byte slice of little-endian f32 PCM data into a Vec of f32 samples.
///
/// Any trailing bytes that don't form a complete f32 (4 bytes) are silently dropped.
fn bytes_to_f32_samples(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}

/// Map an `SCStreamOutputType` to an `AudioSource`.
///
/// Returns `None` for screen output (type 0), which we ignore.
fn source_from_output_type(output_type: SCStreamOutputType) -> Option<AudioSource> {
    if output_type == SCStreamOutputType::Audio {
        Some(AudioSource::System)
    } else if output_type == SCStreamOutputType::Microphone {
        Some(AudioSource::Microphone)
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

/// Ivars for the `AudioOutputHandler` ObjC class.
struct AudioOutputHandlerIvars {
    sender: SyncSender<AudioBuffer>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements. We don't implement Drop.
    #[unsafe(super(NSObject))]
    #[ivars = AudioOutputHandlerIvars]
    struct AudioOutputHandler;

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
            if let Err(e) = self.ivars().sender.try_send(buffer) {
                match e {
                    std::sync::mpsc::TrySendError::Full(_) => {
                        log::warn!(
                            "audio buffer dropped (channel full), source={}",
                            source.as_str()
                        );
                    }
                    std::sync::mpsc::TrySendError::Disconnected(_) => {
                        log::warn!(
                            "audio buffer dropped (channel closed), source={}",
                            source.as_str()
                        );
                    }
                }
            }
        }
    }
);

impl AudioOutputHandler {
    /// Create a new handler that sends audio buffers over the given channel.
    pub fn new(sender: SyncSender<AudioBuffer>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(AudioOutputHandlerIvars { sender });
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
    fn source_from_output_type_maps_microphone_to_mic() {
        let result = source_from_output_type(SCStreamOutputType::Microphone);
        assert_eq!(result, Some(AudioSource::Microphone));
    }

    #[test]
    fn source_from_output_type_ignores_screen() {
        let result = source_from_output_type(SCStreamOutputType::Screen);
        assert!(result.is_none());
    }

    #[test]
    fn audio_buffer_sent_through_sync_channel() {
        let (tx, rx) = std::sync::mpsc::sync_channel::<AudioBuffer>(4);

        let buf = AudioBuffer {
            samples: vec![0.0; 960],
            timestamp_ms: 1_700_000_000_000,
            source: AudioSource::System,
        };

        tx.try_send(buf).expect("channel should accept the buffer");

        let received = rx.try_recv().expect("should receive the buffer");
        assert_eq!(received.samples.len(), 960);
        assert_eq!(received.source, AudioSource::System);
    }

    #[test]
    fn sync_channel_drops_on_full() {
        let (tx, _rx) = std::sync::mpsc::sync_channel::<AudioBuffer>(1);

        let make_buf = || AudioBuffer {
            samples: vec![0.0],
            timestamp_ms: 100,
            source: AudioSource::Microphone,
        };

        tx.try_send(make_buf()).expect("first send should work");

        let result = tx.try_send(make_buf());
        assert!(result.is_err(), "should fail when channel is full");
    }

    #[test]
    fn handler_class_registers_with_runtime() {
        // Verifies that the ObjC class created by define_class! is valid
        // and can be instantiated.
        let (tx, _rx) = std::sync::mpsc::sync_channel::<AudioBuffer>(4);
        let handler = AudioOutputHandler::new(tx);

        // The handler should be usable as an SCStreamOutput protocol object.
        let _protocol_obj = handler.as_protocol_object();
    }
}
