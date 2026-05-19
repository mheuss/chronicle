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
//! format. When the device already delivers 48 kHz mono f32 this is an
//! identity passthrough. Otherwise it is a real resample/downmix. The
//! converter's input block signals `NoDataNow` (not `EndOfStream`) between tap
//! buffers, so it keeps its resampler state across them — a non-48 kHz mic is
//! resampled as one continuous stream rather than one isolated resample per
//! buffer.

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
/// 4096 is comfortably above any real resampler's leading/trailing frame count,
/// and over-allocating the output buffer only wastes a little memory.
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
/// Dropping `MicrophoneCapture` releases the `AVAudioEngine`, which tears the
/// input-node tap down with it — there is no explicit `removeTapOnBus`.
///
/// [`start`]: Self::start
pub struct MicrophoneCapture {
    /// The capture engine. `start`/`stop`/`is_running` drive it directly.
    engine: Retained<AVAudioEngine>,
    /// The installed tap block. `installTapOnBus` copies the block, so the
    /// engine owns its own retained copy; this field also keeps the Rust-side
    /// `RcBlock` alive for the struct's lifetime.
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
/// The converter is shared across tap buffers and keeps its resampler state
/// between calls: the input block signals `NoDataNow`, not `EndOfStream`, so
/// successive tap buffers resample as one continuous stream.
///
/// Under `NoDataNow`, each call returns slightly fewer frames than the
/// converter could ideally produce: it keeps the trailing partial block of
/// its internal packetization (~700 frames, ~15 ms at 48 kHz) buffered for
/// the next call. The next tap buffer flushes that tail, so within a
/// capture session the stream is continuous. Across capture sessions the
/// shared converter retains its buffered tail — re-enabling the microphone
/// emits the previous session's tail at the start of the new session. That
/// is acceptable for this use case: the cross-session seam is ~15 ms of
/// stale audio, well below perceptual relevance for transcription, and
/// resampler continuity within a session matters more than seam-free
/// session boundaries.
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
    // first call, then signals `NoDataNow` — not `EndOfStream` — so the
    // converter keeps its resampler state for the next tap buffer instead of
    // draining and resetting. `provided` guards the single hand-off so a
    // second call does not re-feed the same buffer. A `Cell` gives the
    // interior mutability an ObjC block (a `Fn`, not `FnMut`) requires; the
    // block runs synchronously on this thread, so the lack of thread-safety
    // is fine.
    let provided = Cell::new(false);
    let input_ptr: *const AVAudioPCMBuffer = input_buffer;
    let input_block = RcBlock::new(
        move |_packets: u32, status: NonNull<AVAudioConverterInputStatus>| -> *mut AVAudioBuffer {
            // The converter gives a valid out-status pointer; writing through
            // it below is the unsafe part, guarded at each write.
            let status = status.as_ptr();
            if provided.get() {
                // This tap buffer was already handed over. Signal NoDataNow,
                // not EndOfStream, so the converter returns `InputRanDry` with
                // its resampler state intact for the next tap buffer.
                // SAFETY: status points to a writable AVAudioConverterInputStatus.
                unsafe { *status = AVAudioConverterInputStatus::NoDataNow };
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

    // With the input block signaling NoDataNow, the converter returns
    // `InputRanDry` once it has consumed the tap buffer — the expected status
    // here. Only `Error` indicates a real failure.
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

    /// Run a synthetic 44.1 kHz mono buffer through `convert_to_mono_samples`
    /// with a real `AVAudioConverter`. Building an `AVAudioPCMBuffer` and an
    /// `AVAudioConverter` touches no hardware and triggers no TCC prompt —
    /// only starting an `AVAudioEngine` does — so this is a regular test.
    #[cfg(target_os = "macos")]
    #[test]
    fn convert_resamples_44100_to_48000() {
        autoreleasepool(|_| {
            let input_rate = 44_100u32;
            let input_frames = 4_410u32; // 0.1 s at 44.1 kHz

            // Synthetic 44.1 kHz mono f32 input format.
            // SAFETY: alloc yields a fresh AVAudioFormat; the standard
            // initializer returns nil only on failure, unwrapped below.
            let input_format = unsafe {
                AVAudioFormat::initStandardFormatWithSampleRate_channels(
                    AVAudioFormat::alloc(),
                    input_rate as f64,
                    CHANNEL_COUNT,
                )
            }
            .expect("44.1 kHz mono format should build");

            // Input buffer sized to hold exactly input_frames.
            // SAFETY: input_format is a valid PCM format; input_frames is a
            // non-zero frame count.
            let input_buffer = unsafe {
                AVAudioPCMBuffer::initWithPCMFormat_frameCapacity(
                    AVAudioPCMBuffer::alloc(),
                    &input_format,
                    input_frames,
                )
            }
            .expect("input buffer should allocate");

            // frameLength defaults to 0; the converter only reads frameLength
            // frames, so it must be set to the count of real samples.
            // SAFETY: input_frames <= frameCapacity.
            unsafe { input_buffer.setFrameLength(input_frames) };

            // Fill channel zero with a known constant.
            // SAFETY: the format is 32-bit float, so floatChannelData is
            // non-nil and points to one channel pointer (mono) to
            // frameLength f32 samples with stride 1.
            let channel = unsafe { input_buffer.floatChannelData() };
            let channel_zero = NonNull::new(channel)
                .expect("float channel data should be non-nil for an f32 format");
            // SAFETY: channel_zero points to at least one channel pointer.
            let samples_ptr = unsafe { channel_zero.as_ptr().read() };
            // SAFETY: samples_ptr points to input_frames valid, writable f32s.
            let samples = unsafe {
                std::slice::from_raw_parts_mut(samples_ptr.as_ptr(), input_frames as usize)
            };
            samples.fill(0.25_f32);

            // 48 kHz mono f32 target — the same target the production path uses.
            // SAFETY: alloc yields a fresh AVAudioFormat; unwrapped below.
            let target_format = unsafe {
                AVAudioFormat::initStandardFormatWithSampleRate_channels(
                    AVAudioFormat::alloc(),
                    SAMPLE_RATE as f64,
                    CHANNEL_COUNT,
                )
            }
            .expect("48 kHz mono format should build");

            // SAFETY: both formats are valid PCM formats.
            let converter = unsafe {
                AVAudioConverter::initFromFormat_toFormat(
                    AVAudioConverter::alloc(),
                    &input_format,
                    &target_format,
                )
            }
            .expect("44.1 -> 48 kHz converter should build");

            let out = convert_to_mono_samples(&converter, &target_format, &input_buffer)
                .expect("conversion should yield samples");

            assert!(!out.is_empty(), "converted output must not be empty");

            // A resampler is not exact frame-for-frame, so allow a tolerance
            // around the ideal scaled length.
            let expected = (input_frames as u64 * SAMPLE_RATE as u64 / input_rate as u64) as usize;
            let tolerance = expected / 10 + 64;
            assert!(
                out.len().abs_diff(expected) <= tolerance,
                "output length {} should be near {expected} (tolerance {tolerance})",
                out.len(),
            );
        });
    }

    /// Build a mono f32 buffer of `frames` samples of a 997 Hz sine wave, at
    /// `format`'s sample rate, where sample `n` carries the wave value for
    /// absolute frame index `first_frame + n`. Building chunks from one
    /// contiguous waveform this way lets a test feed the same signal whole or in
    /// pieces.
    #[cfg(target_os = "macos")]
    fn make_sine_buffer(
        format: &AVAudioFormat,
        frames: u32,
        first_frame: u32,
    ) -> Retained<AVAudioPCMBuffer> {
        assert!(
            frames > 0,
            "make_sine_buffer requires a non-zero frame count"
        );

        // SAFETY: format is a valid PCM format; frames is a non-zero count.
        let buffer = unsafe {
            AVAudioPCMBuffer::initWithPCMFormat_frameCapacity(
                AVAudioPCMBuffer::alloc(),
                format,
                frames,
            )
        }
        .expect("buffer should allocate");

        // SAFETY: frames <= frameCapacity.
        unsafe { buffer.setFrameLength(frames) };

        // SAFETY: a 32-bit float format, so floatChannelData is non-nil and points
        // to one channel pointer (mono) to `frames` writable f32 samples.
        let channel = unsafe { buffer.floatChannelData() };
        let channel_zero =
            NonNull::new(channel).expect("float channel data should be non-nil for an f32 format");
        // SAFETY: channel_zero points to at least one channel pointer.
        let samples_ptr = unsafe { channel_zero.as_ptr().read() };
        // SAFETY: samples_ptr points to `frames` valid, writable f32 samples.
        let samples =
            unsafe { std::slice::from_raw_parts_mut(samples_ptr.as_ptr(), frames as usize) };

        // SAFETY: sampleRate is a plain property read.
        let rate = unsafe { format.sampleRate() } as f32;
        for (n, sample) in samples.iter_mut().enumerate() {
            let frame = (first_frame as usize + n) as f32;
            *sample = (2.0 * std::f32::consts::PI * 997.0 * frame / rate).sin() * 0.5;
        }

        buffer
    }

    /// Feeds one sine wave through `convert_to_mono_samples` two ways — as a
    /// single 44.1 kHz buffer, and as two chunks through one shared converter —
    /// and asserts the results match. Under the old per-buffer `EndOfStream` the
    /// chunked path restarts resampling at the chunk boundary and the outputs
    /// diverge at the seam; with `NoDataNow` the shared converter keeps its
    /// resampler state and the two paths agree.
    ///
    /// The probe is 997 Hz: not a divisor of the 4410-frame chunk length, so the
    /// chunk seam never lands on a cycle boundary and the whole waveform stays
    /// diagnostic.
    ///
    /// Builds only `AVAudioPCMBuffer`/`AVAudioConverter` objects — no
    /// `AVAudioEngine` — so it touches no hardware and triggers no TCC prompt.
    #[cfg(target_os = "macos")]
    #[test]
    fn convert_resamples_continuously_across_chunks() {
        autoreleasepool(|_| {
            const CHUNK: u32 = 4_410; // 0.1 s at 44.1 kHz
            const TOTAL: u32 = CHUNK * 2; // 0.2 s

            // 44.1 kHz mono f32 input format and the 48 kHz mono f32 target.
            // SAFETY: alloc yields a fresh AVAudioFormat; the standard
            // initializer returns nil only on failure, unwrapped below.
            let input_format = unsafe {
                AVAudioFormat::initStandardFormatWithSampleRate_channels(
                    AVAudioFormat::alloc(),
                    44_100.0,
                    CHANNEL_COUNT,
                )
            }
            .expect("44.1 kHz mono format should build");
            // SAFETY: as above.
            let target_format = unsafe {
                AVAudioFormat::initStandardFormatWithSampleRate_channels(
                    AVAudioFormat::alloc(),
                    SAMPLE_RATE as f64,
                    CHANNEL_COUNT,
                )
            }
            .expect("48 kHz mono format should build");

            // Single-shot: the whole signal through one fresh converter.
            let full = make_sine_buffer(&input_format, TOTAL, 0);
            // SAFETY: both formats are valid PCM formats.
            let single_converter = unsafe {
                AVAudioConverter::initFromFormat_toFormat(
                    AVAudioConverter::alloc(),
                    &input_format,
                    &target_format,
                )
            }
            .expect("converter should build");
            let single = convert_to_mono_samples(&single_converter, &target_format, &full)
                .expect("single-shot conversion should yield samples");

            // Chunked: the same signal in two halves through ONE shared converter.
            let chunk0 = make_sine_buffer(&input_format, CHUNK, 0);
            let chunk1 = make_sine_buffer(&input_format, CHUNK, CHUNK);
            // SAFETY: both formats are valid PCM formats.
            let chunk_converter = unsafe {
                AVAudioConverter::initFromFormat_toFormat(
                    AVAudioConverter::alloc(),
                    &input_format,
                    &target_format,
                )
            }
            .expect("converter should build");
            let mut chunked = convert_to_mono_samples(&chunk_converter, &target_format, &chunk0)
                .expect("chunk 0 conversion should yield samples");
            chunked.extend(
                convert_to_mono_samples(&chunk_converter, &target_format, &chunk1)
                    .expect("chunk 1 conversion should yield samples"),
            );

            // Continuity at the API level means: every frame the chunked path
            // emits matches the single-shot path's same frame index. We compare
            // on `chunked.len()` because the chunked path emits fewer total
            // frames — each `convertToBuffer:` call under `NoDataNow` keeps a
            // larger filter tail inside the converter than the single-shot path
            // does. That tail is not lost: in production the next tap buffer
            // flushes it. The API-level continuity claim is an identical
            // overlap, not matching totals.
            assert!(
                !chunked.is_empty(),
                "chunked path produced no samples — the second tap buffer was rejected",
            );
            assert!(
                chunked.len() <= single.len(),
                "chunked ({}) emitted more frames than single ({}) — unexpected",
                chunked.len(),
                single.len(),
            );

            for i in 0..chunked.len() {
                let diff = (single[i] - chunked[i]).abs();
                assert!(
                    diff < 1e-5,
                    "single-shot and chunked output diverge at frame {i} \
                     (single={}, chunked={}, diff={diff}) — the resampler \
                     restarted at the chunk seam",
                    single[i],
                    chunked[i],
                );
            }

            // Sanity bound on the converter's held-back tail. Single-shot holds
            // back ~18 frames; the chunked path holds back roughly two calls'
            // worth (the last call's tail plus residue carried across the first
            // call's NoDataNow), so it is the larger of the two. Bound it
            // loosely so a regression that drops a lot more is still caught.
            let ideal = (TOTAL as u64 * SAMPLE_RATE as u64 / 44_100) as usize;
            let single_shortfall = ideal.saturating_sub(single.len());
            let chunked_shortfall = ideal.saturating_sub(chunked.len());
            println!(
                "continuity test: single shortfall = {single_shortfall}, \
                 chunked shortfall = {chunked_shortfall} frames",
            );
            assert!(
                chunked_shortfall < 1024,
                "chunked path held back {chunked_shortfall} frames — far more than \
                 the expected cross-chunk filter tail",
            );
        });
    }

    /// Runs a 48 kHz mono f32 signal through `convert_to_mono_samples` with a
    /// 48 kHz→48 kHz converter — the identity passthrough a 48 kHz microphone
    /// hits. The converter does no resampling, so the output must equal the input.
    /// Guards that the `NoDataNow` input-block signal does not regress the
    /// passthrough path (requirement R2).
    ///
    /// Builds only `AVAudioPCMBuffer`/`AVAudioConverter` objects — no
    /// `AVAudioEngine` — so it touches no hardware and triggers no TCC prompt.
    #[cfg(target_os = "macos")]
    #[test]
    fn convert_passes_48000_through_unchanged() {
        autoreleasepool(|_| {
            const FRAMES: u32 = 4_800; // 0.1 s at 48 kHz

            // 48 kHz mono f32 — both the input and the target. An AVAudioConverter
            // between identical formats is an identity passthrough.
            // SAFETY: alloc yields a fresh AVAudioFormat; the standard initializer
            // returns nil only on failure, unwrapped below.
            let format = unsafe {
                AVAudioFormat::initStandardFormatWithSampleRate_channels(
                    AVAudioFormat::alloc(),
                    SAMPLE_RATE as f64,
                    CHANNEL_COUNT,
                )
            }
            .expect("48 kHz mono format should build");

            let input = make_sine_buffer(&format, FRAMES, 0);

            // Read the input samples back for comparison before converting.
            // SAFETY: a 32-bit float format — floatChannelData is non-nil, one
            // channel pointer to FRAMES f32 samples.
            let channel = unsafe { input.floatChannelData() };
            let channel_zero = NonNull::new(channel).expect("float channel data should be non-nil");
            // SAFETY: channel_zero points to at least one channel pointer.
            let samples_ptr = unsafe { channel_zero.as_ptr().read() };
            // SAFETY: samples_ptr points to FRAMES valid f32 samples.
            let expected: Vec<f32> =
                unsafe { std::slice::from_raw_parts(samples_ptr.as_ptr(), FRAMES as usize) }
                    .to_vec();

            // SAFETY: both formats are valid PCM formats.
            let converter = unsafe {
                AVAudioConverter::initFromFormat_toFormat(
                    AVAudioConverter::alloc(),
                    &format,
                    &format,
                )
            }
            .expect("identity converter should build");

            let out = convert_to_mono_samples(&converter, &format, &input)
                .expect("identity conversion should yield samples");

            // The converter holds back its internal-block tail under NoDataNow
            // (same behavior as the resampling case — see the continuity test).
            // A single call never emits more than the input length; the held-back
            // remainder flushes on the next tap buffer in production. The
            // API-level identity claim is: every emitted sample matches the
            // corresponding input sample exactly.
            assert!(!out.is_empty(), "identity passthrough produced no samples",);
            assert!(
                out.len() <= FRAMES as usize,
                "identity passthrough emitted {} frames, more than the {FRAMES} input",
                out.len(),
            );
            let shortfall = (FRAMES as usize).saturating_sub(out.len());
            println!("identity passthrough: held-back tail = {shortfall} frames");
            assert!(
                shortfall < 1024,
                "identity passthrough held back {shortfall} frames — far more than \
                 the expected single-call filter tail",
            );
            for (i, (got, want)) in out.iter().zip(expected.iter()).enumerate() {
                assert!(
                    (got - want).abs() < 1e-6,
                    "identity passthrough changed frame {i}: got {got}, want {want}",
                );
            }
        });
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
