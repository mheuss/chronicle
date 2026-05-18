//! Microphone capture via AVAudioEngine.
//!
//! A dedicated capture path, independent of the screen-capture SCStream.
//! Toggling the microphone start/stops this engine; screen and system-audio
//! capture are never touched. See the HEU-330 design, section 2.1.
//!
//! The engine's input-node tap delivers the device's *native* format, which
//! is hardware-dependent (e.g. 44.1 kHz on some mics). The encoder and
//! [`SegmentAccumulator`](crate::accumulator) require exactly 48 kHz mono
//! f32, so an `AVAudioConverter` normalizes every tap buffer to that target
//! format. The conversion is an identity passthrough when the device already
//! delivers 48 kHz mono f32, and a real resample/downmix otherwise.

use std::cell::Cell;
use std::ptr::NonNull;
use std::sync::mpsc::{SyncSender, TrySendError};
use std::time::{SystemTime, UNIX_EPOCH};

use block2::RcBlock;
use objc2::AnyThread;
use objc2::rc::{Retained, autoreleasepool};
use objc2_avf_audio::{
    AVAudioBuffer, AVAudioConverter, AVAudioConverterInputStatus, AVAudioConverterOutputStatus,
    AVAudioEngine, AVAudioFormat, AVAudioPCMBuffer, AVAudioTime,
};

use crate::handler::{AudioBuffer, AudioMessage};
use crate::{AudioError, AudioSource, CHANNEL_COUNT, Result, SAMPLE_RATE};

/// Extra frames added to the output buffer capacity when resampling.
///
/// A resampler can produce a few more frames than the scaled input count due to
/// its internal state (filter delay, polyphase rounding). This headroom ensures
/// the output `AVAudioPCMBuffer` is large enough to hold those extra frames.
const RESAMPLER_HEADROOM_FRAMES: u32 = 4096;

/// Copy one deinterleaved f32 channel into an owned `Vec<f32>`.
///
/// `channel` is a slice of f32 PCM samples for a single channel — the layout
/// `AVAudioPCMBuffer::floatChannelData` exposes for channel zero of a
/// non-interleaved buffer. `frame_count` is the number of valid sample frames
/// (`AVAudioPCMBuffer::frameLength`); a `frame_count` larger than the slice is
/// clamped so the copy never reads past the buffer.
///
/// The returned vector holds at most `frame_count` samples. The microphone
/// path normalizes to mono before this point, so channel zero already is the
/// whole signal — no downmix happens here.
fn mono_samples(channel: &[f32], frame_count: usize) -> Vec<f32> {
    channel[..frame_count.min(channel.len())].to_vec()
}

/// Microphone capture driven by an `AVAudioEngine` input-node tap.
///
/// Construction installs the tap and prepares the engine but does not start
/// it — no microphone access and no TCC prompt happen until [`start`]. The
/// tap normalizes each buffer to mono/48 kHz/f32 and forwards it over the
/// encoding channel as [`AudioMessage::Buffer`].
///
/// [`start`]: Self::start
pub struct MicrophoneCapture {
    /// The capture engine. `start`/`stop`/`is_running` drive it directly.
    engine: Retained<AVAudioEngine>,
    /// The installed tap block. The engine holds a raw pointer to it, so it
    /// must stay alive for as long as the tap is installed — i.e. for the
    /// lifetime of this struct.
    _tap_block: RcBlock<dyn Fn(NonNull<AVAudioPCMBuffer>, NonNull<AVAudioTime>)>,
}

impl MicrophoneCapture {
    /// Build the engine, install the input-node tap, and `prepare()`.
    ///
    /// Does **not** start capture — no microphone access, no TCC prompt. Any
    /// AVFoundation setup failure returns [`AudioError`] rather than panicking
    /// so callers can treat a microphone-setup failure as soft.
    ///
    /// `buffer_tx` is a clone of the encoding channel's sender. The tap holds
    /// its own clone and best-effort sends each normalized buffer.
    pub fn new(buffer_tx: SyncSender<AudioMessage>) -> Result<Self> {
        autoreleasepool(|_| {
            // SAFETY: AVAudioEngine::new builds a fresh engine connected to
            // the default audio device. No preconditions.
            let engine = unsafe { AVAudioEngine::new() };

            // SAFETY: inputNode is the engine's singleton input node. Reading
            // it before start does not yet engage the microphone hardware.
            let input = unsafe { engine.inputNode() };

            // The device's native input format. Hardware-dependent — may be
            // 44.1 kHz, may be multi-channel.
            // SAFETY: bus 0 is the input node's sole output bus.
            let native_format = unsafe { input.outputFormatForBus(0) };

            // The format the encoder requires: 48 kHz, mono, f32. The
            // "standard" initializer yields deinterleaved f32, so channel
            // zero of a converted buffer is the whole mono signal.
            // SAFETY: alloc yields a fresh, uninitialized AVAudioFormat object.
            // initStandardFormatWithSampleRate_channels: consumes it and
            // returns nil on failure, which is handled by the .ok_or_else
            // below. No other preconditions apply.
            let target_format = unsafe {
                AVAudioFormat::initStandardFormatWithSampleRate_channels(
                    AVAudioFormat::alloc(),
                    SAMPLE_RATE as f64,
                    CHANNEL_COUNT,
                )
            }
            .ok_or_else(|| {
                AudioError::Microphone("failed to build 48kHz mono target format".into())
            })?;

            // One converter, native -> target. Used on every tap buffer; an
            // identity passthrough when native already matches the target.
            // SAFETY: both formats are valid PCM formats.
            let converter = unsafe {
                AVAudioConverter::initFromFormat_toFormat(
                    AVAudioConverter::alloc(),
                    &native_format,
                    &target_format,
                )
            }
            .ok_or_else(|| {
                AudioError::Microphone("failed to build microphone format converter".into())
            })?;

            let tap_block = make_tap_block(converter, target_format, buffer_tx);

            // Install a nil-format tap. nil means "deliver the native
            // hardware format" — installing an explicit non-native format on
            // an input-node tap risks an uncatchable ObjC exception, so the
            // converter above does the normalization instead. The buffer size
            // is only a hint.
            // SAFETY: bus 0 is valid; the block pointer comes from a live
            // RcBlock kept in `_tap_block`; nil format is permitted.
            unsafe {
                input.installTapOnBus_bufferSize_format_block(
                    0,
                    4096,
                    None,
                    &*tap_block as *const _ as *mut _,
                );
            }

            // Allocate hardware-render resources up front so `start` is fast.
            // SAFETY: no preconditions.
            unsafe { engine.prepare() };

            Ok(Self {
                engine,
                _tap_block: tap_block,
            })
        })
    }

    /// Start capture. Fast — the engine is already prepared. Microphone on.
    ///
    /// This is the call that engages the microphone hardware and triggers the
    /// TCC permission prompt on first use.
    pub fn start(&self) -> Result<()> {
        autoreleasepool(|_| {
            // SAFETY: the engine was prepared in `new`. startAndReturnError
            // returns the NSError as a Rust Result.
            unsafe { self.engine.startAndReturnError() }.map_err(|err| {
                AudioError::Microphone(format!("failed to start microphone engine: {err}"))
            })
        })
    }

    /// Stop capture. Releases the OS microphone. Microphone off.
    pub fn stop(&self) -> Result<()> {
        autoreleasepool(|_| {
            // SAFETY: stop is safe to call whether or not the engine is
            // running; it releases the resources allocated by prepare.
            unsafe { self.engine.stop() };
        });
        Ok(())
    }

    /// Whether the engine is currently capturing.
    ///
    /// Read live from `AVAudioEngine.isRunning`, not a stored flag.
    pub fn is_running(&self) -> bool {
        // SAFETY: isRunning is a plain property read, safe in any state.
        unsafe { self.engine.isRunning() }
    }
}

/// Build the input-node tap block.
///
/// The block captures the converter, the target format, and a clone of the
/// encoding-channel sender. It runs on an audio thread, so it wraps its ObjC
/// work in an `autoreleasepool` and uses `try_send` — never blocking the
/// audio thread (the Structured Backpressure pattern).
fn make_tap_block(
    converter: Retained<AVAudioConverter>,
    target_format: Retained<AVAudioFormat>,
    buffer_tx: SyncSender<AudioMessage>,
) -> RcBlock<dyn Fn(NonNull<AVAudioPCMBuffer>, NonNull<AVAudioTime>)> {
    RcBlock::new(
        move |input_buffer: NonNull<AVAudioPCMBuffer>, _when: NonNull<AVAudioTime>| {
            autoreleasepool(|_| {
                // SAFETY: the engine hands the tap a valid PCM buffer that
                // lives for the duration of this callback.
                let input_buffer = unsafe { input_buffer.as_ref() };

                let samples =
                    match convert_to_mono_samples(&converter, &target_format, input_buffer) {
                        Some(s) if !s.is_empty() => s,
                        _ => return,
                    };

                // Wall-clock timestamp, matching AudioOutputHandler. PTS-to-
                // epoch conversion is fragile and unnecessary for 30-second
                // segment boundaries.
                let timestamp_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64;

                let message = AudioMessage::Buffer(AudioBuffer {
                    samples,
                    timestamp_ms,
                    source: AudioSource::Microphone,
                });

                // Best-effort send. Never block the audio thread: drop the
                // buffer and warn if the channel is full (downstream slow) or
                // closed (downstream gone).
                match buffer_tx.try_send(message) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => {
                        log::warn!("microphone buffer dropped (channel full)");
                    }
                    Err(TrySendError::Disconnected(_)) => {
                        log::warn!("microphone buffer dropped (channel closed)");
                    }
                }
            });
        },
    )
}

/// Convert one native-format tap buffer to mono/48 kHz/f32 samples.
///
/// Runs the converter into a freshly allocated target-format buffer, then
/// copies channel zero out as an owned `Vec<f32>`. Returns `None` if the
/// output buffer cannot be allocated, the conversion fails, or the converted
/// buffer exposes no float channel data.
///
/// The output is always a single (mono) channel, so the `AudioBuffer.samples`
/// contract — "interleaved f32" — is trivially satisfied: for one channel,
/// interleaved and deinterleaved layouts are identical.
fn convert_to_mono_samples(
    converter: &AVAudioConverter,
    target_format: &AVAudioFormat,
    input_buffer: &AVAudioPCMBuffer,
) -> Option<Vec<f32>> {
    // SAFETY: frameLength is a plain property read.
    let input_frames = unsafe { input_buffer.frameLength() };
    if input_frames == 0 {
        return None;
    }

    // Sample-rate conversion can expand the frame count. Size the output
    // buffer by the sample-rate ratio plus headroom for the resampler's
    // leading/trailing frames.
    // SAFETY: sampleRate is a plain property read.
    let input_rate = unsafe { input_buffer.format().sampleRate() };
    let ratio = if input_rate > 0.0 {
        SAMPLE_RATE as f64 / input_rate
    } else {
        1.0
    };
    let output_capacity =
        ((input_frames as f64 * ratio).ceil() as u32).saturating_add(RESAMPLER_HEADROOM_FRAMES);

    // SAFETY: target_format is a valid PCM format; output_capacity is a
    // non-zero frame count.
    let output_buffer = unsafe {
        AVAudioPCMBuffer::initWithPCMFormat_frameCapacity(
            AVAudioPCMBuffer::alloc(),
            target_format,
            output_capacity,
        )
    }?;

    // The converter's input block hands the whole input buffer over on its
    // first call, then signals end-of-stream. `provided` guards the single
    // hand-off so a second call does not re-feed the same buffer. A `Cell`
    // gives the interior mutability an ObjC block (a `Fn`, not `FnMut`)
    // requires; the block runs synchronously on this thread, so the lack of
    // thread-safety is fine.
    let provided = Cell::new(false);
    let input_ptr: *const AVAudioPCMBuffer = input_buffer;
    let input_block = RcBlock::new(
        move |_packets: u32, status: NonNull<AVAudioConverterInputStatus>| -> *mut AVAudioBuffer {
            // The converter gives a valid out-status pointer; writing through
            // it below is the unsafe part, guarded at each write.
            let status = status.as_ptr();
            if provided.get() {
                // SAFETY: status points to a writable AVAudioConverterInputStatus.
                unsafe { *status = AVAudioConverterInputStatus::EndOfStream };
                std::ptr::null_mut()
            } else {
                provided.set(true);
                // SAFETY: status points to a writable AVAudioConverterInputStatus.
                unsafe { *status = AVAudioConverterInputStatus::HaveData };
                // The converter wants an `*mut AVAudioBuffer`; AVAudioPCMBuffer
                // is a subclass. The buffer outlives this callback.
                input_ptr as *mut AVAudioBuffer
            }
        },
    );

    // SAFETY: output_buffer is a fresh target-format buffer; the input block
    // pointer comes from a live RcBlock held until this call returns.
    let status = unsafe {
        converter.convertToBuffer_error_withInputFromBlock(
            output_buffer.as_ref(),
            None,
            &*input_block as *const _ as *mut _,
        )
    };

    if status == AVAudioConverterOutputStatus::Error {
        log::warn!("microphone format conversion failed");
        return None;
    }

    // SAFETY: frameLength reflects how many frames the converter produced.
    let output_frames = unsafe { output_buffer.frameLength() } as usize;
    if output_frames == 0 {
        return None;
    }

    // SAFETY: the target format is 32-bit float, so floatChannelData is
    // non-nil and points to `channelCount` pointers, each to `frameLength`
    // samples. The target format is mono, so channel zero is the whole signal.
    let channel_data = unsafe { output_buffer.floatChannelData() };
    let channel_ptr = NonNull::new(channel_data)?;
    // SAFETY: channel_ptr points to at least one channel pointer.
    let channel_zero = unsafe { channel_ptr.as_ptr().read() };
    // SAFETY: channel_zero points to `output_frames` valid f32 samples; the
    // standard mono format is non-interleaved with stride 1.
    let channel_slice = unsafe { std::slice::from_raw_parts(channel_zero.as_ptr(), output_frames) };

    Some(mono_samples(channel_slice, output_frames))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mono_samples_copies_channel_zero() {
        let channel = [0.1_f32, -0.2, 0.3, -0.4, 0.5];
        let result = mono_samples(&channel, 5);

        assert_eq!(result, vec![0.1_f32, -0.2, 0.3, -0.4, 0.5]);
    }

    #[test]
    fn mono_samples_respects_frame_count() {
        // The engine may hand back a buffer whose capacity exceeds the valid
        // frame count; only `frame_count` samples are real audio.
        let channel = [1.0_f32, 2.0, 3.0, 0.0, 0.0, 0.0];
        let result = mono_samples(&channel, 3);

        assert_eq!(result, vec![1.0_f32, 2.0, 3.0]);
    }

    #[test]
    fn mono_samples_clamps_overlong_frame_count() {
        // A frame count larger than the slice must not panic — clamp it.
        let channel = [1.0_f32, 2.0];
        let result = mono_samples(&channel, 99);

        assert_eq!(result, vec![1.0_f32, 2.0]);
    }

    #[test]
    fn mono_samples_empty_channel_gives_empty_vec() {
        let result = mono_samples(&[], 0);
        assert!(result.is_empty());
    }

    /// Constructs `MicrophoneCapture`, starts and stops it, and checks that
    /// `is_running` tracks the engine state. Needs a real Mac microphone, so
    /// it is `#[ignore]`d — run manually with
    /// `cargo test -p chronicle-audio mic_capture -- --ignored`.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires a real microphone; run manually"]
    fn mic_capture_start_stop() {
        let (tx, _rx) = std::sync::mpsc::sync_channel::<AudioMessage>(64);
        let mic = MicrophoneCapture::new(tx).expect("microphone setup should succeed");

        assert!(!mic.is_running(), "engine should not run before start()");

        mic.start().expect("start should succeed");
        assert!(mic.is_running(), "engine should run after start()");

        mic.stop().expect("stop should succeed");
        assert!(!mic.is_running(), "engine should not run after stop()");
    }
}
