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
use std::sync::Arc;
#[cfg(feature = "characterize")]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::mpsc::SyncSender;
use std::time::{SystemTime, UNIX_EPOCH};

use block2::RcBlock;
use objc2::AnyThread;
use objc2::rc::{Retained, autoreleasepool};
use objc2_avf_audio::{
    AVAudioBuffer, AVAudioCommonFormat, AVAudioConverter, AVAudioConverterInputStatus,
    AVAudioConverterOutputStatus, AVAudioEngine, AVAudioFormat, AVAudioPCMBuffer, AVAudioTime,
};

#[cfg(feature = "characterize")]
use crate::characterize::CharacterizationFrame;
use crate::handler::{AudioBuffer, AudioMessage};
use crate::{AudioDropCounters, AudioError, AudioSource, CHANNEL_COUNT, Result, SAMPLE_RATE};

/// Extra frames added to the output buffer capacity when resampling.
///
/// A resampler can produce a few more frames than the scaled input count due to
/// its internal state (filter delay, polyphase rounding). This headroom ensures
/// the output `AVAudioPCMBuffer` is large enough to hold those extra frames.
/// 4096 is comfortably above any real resampler's leading/trailing frame count,
/// and over-allocating the output buffer only wastes a little memory.
const RESAMPLER_HEADROOM_FRAMES: u32 = 4096;

/// What one converter call produced for one tap buffer.
///
/// The converter has six exits and only one carries samples. Collapsing the
/// other five into `None` hid the difference between the resampler holding its
/// filter tail, which happens on most calls, and a genuine failure, which
/// should never happen. `mic_convert_failed` counts the second and not the
/// first, so the two have to be told apart here.
#[derive(Debug, Clone, PartialEq)]
pub enum ConversionOutcome {
    /// The converter emitted at least one frame.
    Produced(Vec<f32>),
    /// Nothing to emit this call: the input was empty, or the converter kept
    /// the whole input as its held-back tail under `NoDataNow`. Benign.
    HeldTail,
    /// Output-buffer allocation failed, the converter returned its `Error`
    /// status, or the output exposed no float channel data. Counted once in
    /// `mic_convert_failed` by `convert_to_mono_samples`.
    Failed,
}

impl ConversionOutcome {
    /// The samples, if any were produced. What the production tap wants.
    pub fn into_produced(self) -> Option<Vec<f32>> {
        match self {
            Self::Produced(samples) => Some(samples),
            Self::HeldTail | Self::Failed => None,
        }
    }
}

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

/// Name an `AVAudioCommonFormat` for the tap-install log.
///
/// `AVAudioCommonFormat`'s derived `Debug` prints the raw discriminant —
/// `AVAudioCommonFormat(1)` — which makes the one line this ticket exists to
/// provide require a lookup table to read. It matters more than it looks:
/// [`MixEligibility::Ineligible`] collapses "not f32" and "more than two
/// channels" into a single label, so the format name is the only thing in the
/// line that tells an Int16 stereo microphone apart from a four-channel f32
/// array.
///
/// Values from `objc2-avf-audio`'s `AVAudioFormat.rs`. An unrecognized value
/// prints as `unknown(N)` rather than being silently dropped — a format
/// AVFoundation adds later is exactly the case worth seeing.
fn common_format_name(format: AVAudioCommonFormat) -> String {
    match format {
        AVAudioCommonFormat::OtherFormat => "other".into(),
        AVAudioCommonFormat::PCMFormatFloat32 => "f32".into(),
        AVAudioCommonFormat::PCMFormatFloat64 => "f64".into(),
        AVAudioCommonFormat::PCMFormatInt16 => "i16".into(),
        AVAudioCommonFormat::PCMFormatInt32 => "i32".into(),
        other => format!("unknown({})", other.0),
    }
}

/// The input device's native format, read once at tap install.
///
/// Public so code outside this crate can read the channel count and sample
/// rate, which is what a WAV header or a manifest needs. A snapshot: nothing
/// updates it if the default input device changes later (the design's
/// "Device scope" names that as pre-existing behaviour).
#[derive(Debug, Clone, PartialEq)]
pub struct NativeFormat {
    pub channels: u32,
    /// Hz.
    pub sample_rate: f64,
    pub interleaved: bool,
    /// `f32`, `i16`, ... as [`common_format_name`] spells it.
    pub common_format: String,
    /// True for 32-bit float buffers, the only layout `extract_channels` reads.
    pub float32: bool,
}

/// Whether the device active at tap install is **eligible** for the explicit
/// downmix HEU-652 will add.
///
/// This is deliberately *not* called a "path". As of HEU-649 nothing branches
/// on it: `AVAudioConverter` still performs every downmix, for every device.
/// Naming it a path and logging `downmix_path=explicit-mix` would make the
/// diagnostic assert a route that is not taken — worse than no diagnostic,
/// because the next person debugging a silent microphone would believe it.
///
/// It is also not a claim about resampling. The converter handles sample rate
/// for every device before and after HEU-652, so even an eligible device still
/// goes through it.
///
/// **Contract for callers:** classify from the native format once, at tap
/// install, and never re-evaluate. `MicrophoneCapture` and its converter are
/// built once when the pipeline is created, so a default-input change leaves
/// any classification stale until the daemon restarts. That is pre-existing
/// behaviour, not something this branch changes.
///
/// Three routes, not two: `Mono` takes a fast passthrough, `Stereo` takes the
/// measured explicit mix, and `Ineligible` stays on today's converter.
///
/// The rule in one sentence: **the explicit mix will apply only to input the
/// measurement covers; everything else keeps the path it already uses.**
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MixEligibility {
    /// One f32 channel. **A distinct route, not the stereo one.** HEU-652
    /// gives mono a fast path that skips extraction, the intermediate `Vec`,
    /// and the second PCM buffer entirely — it does not enter the explicit-mix
    /// machinery, it bypasses it. Grouped under "eligibility" only because the
    /// classification is what selects the route.
    Mono,
    /// Two f32 channels. The case HEU-549 is about; HEU-652 applies the
    /// measured policy here.
    Stereo,
    /// Everything else — non-f32 at any channel count, zero channels, or
    /// more than two. Not eligible for the explicit mix; these keep today's
    /// converter behaviour **unchanged and unmeasured**. That is a
    /// no-regression guarantee, not a claim that the current behaviour is
    /// correct for them — nothing has measured a non-f32 or multi-channel
    /// device.
    Ineligible,
}

impl MixEligibility {
    fn classify(format: AVAudioCommonFormat, channels: u32) -> Self {
        match (format, channels) {
            (AVAudioCommonFormat::PCMFormatFloat32, 1) => Self::Mono,
            (AVAudioCommonFormat::PCMFormatFloat32, 2) => Self::Stereo,
            _ => Self::Ineligible,
        }
    }

    /// The label written to the tap-install log. Kept short and greppable —
    /// it is what someone diagnosing a silent microphone searches for, so it
    /// should stay stable.
    fn as_str(self) -> &'static str {
        match self {
            Self::Mono => "mono",
            Self::Stereo => "stereo",
            Self::Ineligible => "ineligible",
        }
    }
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
    /// The shared format converter — the same one the tap block uses.
    /// Held here so [`stop`] can `reset()` it after the engine stops, which
    /// drops the converter's held-back filter tail and keeps the previous
    /// session's audio from leaking into the next one.
    ///
    /// [`stop`]: Self::stop
    converter: Retained<AVAudioConverter>,
    /// The installed tap block. `installTapOnBus` copies the block, so the
    /// engine owns its own retained copy; this field also keeps the Rust-side
    /// `RcBlock` alive for the struct's lifetime.
    _tap_block: TapBlock,
    /// The counters the tap writes into. The tap block owns its own clone;
    /// this one exists so the pipeline's tests can assert both reach the same
    /// allocation — handing the tap a fresh set compiles cleanly and silently
    /// counts where the reporter never looks.
    ///
    /// Only the `#[cfg(test)]` accessor reads it, so a non-test build sees a
    /// dead field.
    #[cfg_attr(not(test), allow(dead_code))]
    counters: Arc<AudioDropCounters>,
}

impl MicrophoneCapture {
    /// Production capture: the tap forwards mono/48 kHz buffers over the
    /// encoding channel. `buffer_tx` is a clone of that channel's sender. See
    /// [`Self::build`] for everything else.
    pub fn new(
        buffer_tx: SyncSender<AudioMessage>,
        counters: Arc<AudioDropCounters>,
    ) -> Result<Self> {
        let (capture, _format) = Self::build(
            counters,
            |_| Ok(()),
            |converter, target_format, counters| {
                make_tap_block(converter, target_format, buffer_tx, counters)
            },
        )?;
        Ok(capture)
    }

    /// Characterization mode (HEU-650): the tap sends at most one
    /// [`CharacterizationFrame`] per callback on `characterize_tx` and sends
    /// nothing on the encoding channel. Drops count into the same
    /// `mic_full` / `mic_closed` fields the production tap uses. A callback
    /// sends at most once in this mode, and never on the encoding channel, so
    /// those fields are this channel's drops.
    ///
    /// Rejects a device that does not deliver 32-bit float before the tap is
    /// installed: `extract_channels` cannot read any other layout, so every
    /// callback would record nothing.
    ///
    /// Pass a counter set no production tap writes into, or the drop fields
    /// stop meaning what the manifest reads them as.
    ///
    /// The caller owns the sender's lifetime. `stop()` does not close the
    /// channel; the tap block holds its clone until this value is dropped.
    /// Drop the capture first, then the caller's sender, and the writer's
    /// receiver ends. One `start()` and one `stop()` per capture: the
    /// sequence counter does not reset on `stop()`, so a second session on the
    /// same capture would look seq-contiguous across a real gap in the audio.
    #[cfg(feature = "characterize")]
    pub fn new_characterizing(
        characterize_tx: SyncSender<CharacterizationFrame>,
        counters: Arc<AudioDropCounters>,
    ) -> Result<(Self, NativeFormat)> {
        Self::build(
            counters,
            characterization_input_check,
            |converter, target_format, counters| {
                make_characterizing_tap_block(converter, target_format, characterize_tx, counters)
            },
        )
    }

    /// Build the engine, install the input-node tap, and `prepare()`.
    ///
    /// Shared by both constructors. `validate` sees the native format before
    /// anything is built on it and can refuse the device; `make_block` gets
    /// the converter, the 48 kHz mono target format, and the counters, and
    /// returns the block to install. Everything else is identical for both.
    ///
    /// Does **not** start capture — no microphone access, no TCC prompt. Any
    /// AVFoundation setup failure returns [`AudioError`] rather than panicking
    /// so callers can treat a microphone-setup failure as soft.
    ///
    /// `counters` is shared with the daemon's drop reporter, which does all
    /// the logging for the drops the tap records. ADR-013 forbids a logger on
    /// this path, including a throttled one.
    fn build(
        counters: Arc<AudioDropCounters>,
        validate: impl FnOnce(&NativeFormat) -> Result<()>,
        make_block: impl FnOnce(
            Retained<AVAudioConverter>,
            Retained<AVAudioFormat>,
            Arc<AudioDropCounters>,
        ) -> TapBlock,
    ) -> Result<(Self, NativeFormat)> {
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

            // Read what the device actually delivers. Logged once, after the
            // tap is installed (below), rather than per callback: the tap
            // block runs on a real-time audio thread, and ADR-013 wants no
            // logger there at all. (That path already carries pre-existing
            // per-buffer warnings this branch does not touch — logging the
            // format per callback would make it worse, not better.)
            // SAFETY: all four are plain property reads on a valid format.
            let native_channels = unsafe { native_format.channelCount() };
            let native_rate = unsafe { native_format.sampleRate() };
            let native_common = unsafe { native_format.commonFormat() };
            let native_interleaved = unsafe { native_format.isInterleaved() };
            let mix_eligibility = MixEligibility::classify(native_common, native_channels);
            let format = NativeFormat {
                channels: native_channels,
                sample_rate: native_rate,
                interleaved: native_interleaved,
                common_format: common_format_name(native_common),
                float32: native_common == AVAudioCommonFormat::PCMFormatFloat32,
            };
            // Refuse here, before the converter, the tap install and
            // `prepare()`, so a rejected device leaves no tap on the input
            // node and no "tap installed" line behind.
            validate(&format)?;

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

            // The tap block runs the converter on every input buffer; we
            // also keep an owned reference on `Self` so `stop()` can reset
            // it. `Retained::clone` just bumps the ObjC retain count.
            let tap_block = make_block(converter.clone(), target_format, Arc::clone(&counters));

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

            // The tap is installed and the engine prepared, so this really is
            // a tap-install record. Logged here rather than where the format
            // was read because the converter (above) and the tap install can
            // both fail — a line emitted earlier would claim an install that
            // never happened.
            //
            // `mix_eligibility` describes what HEU-652 will do with this
            // device. Today `AVAudioConverter` performs every downmix
            // regardless, which is why it is not called a path.
            //
            // This is `info!`, which the daemon's default filter
            // (`warn,chronicle=info`) admits — so it appears in a normal run
            // with no `RUST_LOG` set. It fires once per tap install, not per
            // callback, which is why a logger is allowed here at all: ADR-013
            // forbids one on the tap block itself.
            log::info!(
                "microphone tap installed (capture starts on mic-on): \
                 {native_channels} ch, {native_rate} Hz, \
                 interleaved={native_interleaved}, format={}, mix_eligibility={}",
                format.common_format,
                mix_eligibility.as_str()
            );

            Ok((
                Self {
                    engine,
                    converter,
                    _tap_block: tap_block,
                    counters,
                },
                format,
            ))
        })
    }

    /// The drop counters this tap writes into. See the field's doc comment.
    #[cfg(test)]
    pub(crate) fn counters(&self) -> &Arc<AudioDropCounters> {
        &self.counters
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
    ///
    /// Also resets the shared `AVAudioConverter` after stopping the engine.
    /// The converter holds back a small filter tail under `NoDataNow` (see
    /// [`convert_to_mono_samples`]); resetting it on stop drops that tail
    /// so it does not leak into the next capture session as stale audio
    /// timestamped to the new session.
    pub fn stop(&self) -> Result<()> {
        autoreleasepool(|_| {
            // SAFETY: stop is safe to call whether or not the engine is
            // running; it releases the resources allocated by prepare.
            unsafe { self.engine.stop() };
            // After the engine stops, no further tap callbacks fire, so it
            // is safe to reset the converter without racing the tap block.
            // SAFETY: reset has no preconditions beyond &self.
            unsafe { self.converter.reset() };
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

/// The input-node tap block. Both constructors install one of these.
type TapBlock = RcBlock<dyn Fn(NonNull<AVAudioPCMBuffer>, NonNull<AVAudioTime>)>;

/// Build the input-node tap block.
///
/// The block captures the converter, the target format, a clone of the
/// encoding-channel sender, and a clone of the drop counters. It runs on an audio thread, so it wraps its ObjC
/// work in an `autoreleasepool` and uses `try_send` — never blocking the
/// audio thread (the Structured Backpressure pattern).
fn make_tap_block(
    converter: Retained<AVAudioConverter>,
    target_format: Retained<AVAudioFormat>,
    buffer_tx: SyncSender<AudioMessage>,
    counters: Arc<AudioDropCounters>,
) -> TapBlock {
    RcBlock::new(
        move |input_buffer: NonNull<AVAudioPCMBuffer>, _when: NonNull<AVAudioTime>| {
            autoreleasepool(|_| {
                // SAFETY: the engine hands the tap a valid PCM buffer that
                // lives for the duration of this callback.
                let input_buffer = unsafe { input_buffer.as_ref() };

                let Some(samples) =
                    convert_to_mono_samples(&converter, &target_format, input_buffer, &counters)
                        .into_produced()
                else {
                    return;
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
                // buffer and count it if the channel is full (downstream slow)
                // or closed (downstream gone). ADR-013 forbids a logger here —
                // including a throttled one — so the daemon's drop reporter
                // does the logging off-thread.
                crate::drops::send_audio(
                    &buffer_tx,
                    message,
                    &counters,
                    crate::drops::AudioSourceKind::Microphone,
                );
            });
        },
    )
}

/// The install-time check behind [`MicrophoneCapture::new_characterizing`]:
/// only 32-bit float input can be recorded.
#[cfg(feature = "characterize")]
fn characterization_input_check(format: &NativeFormat) -> Result<()> {
    if format.float32 {
        Ok(())
    } else {
        Err(AudioError::Microphone(format!(
            "characterization needs 32-bit float input; the device delivers {}",
            format.common_format
        )))
    }
}

/// One callback's characterization work: copy the native channels out, run
/// the converter, stamp a sequence number.
///
/// Returns `None` without consuming a sequence number for a zero-frame
/// buffer, so that leaves no gap to misread as a drop. A buffer that has
/// frames but cannot be read as f32 planes (the device changed under the
/// engine, say) still gets a frame and a sequence number, with `native`
/// empty. That is the frame's malformed marker: the seq is consumed, so the
/// hole stays visible to whatever consumes the frames.
#[cfg(feature = "characterize")]
fn characterize_buffer(
    converter: &AVAudioConverter,
    target_format: &AVAudioFormat,
    input_buffer: &AVAudioPCMBuffer,
    counters: &AudioDropCounters,
    next_seq: &AtomicU64,
) -> Option<CharacterizationFrame> {
    // SAFETY: frameLength is a plain property read.
    if unsafe { input_buffer.frameLength() } == 0 {
        return None;
    }
    let native = extract_channels(input_buffer).unwrap_or_default();
    let outcome = convert_to_mono_samples(converter, target_format, input_buffer, counters);
    let seq = next_seq.fetch_add(1, Ordering::Relaxed);
    Some(CharacterizationFrame {
        seq,
        native,
        outcome,
    })
}

/// The characterization tap block. Same shape as [`make_tap_block`]: an
/// `autoreleasepool`, one bounded `try_send`, counters, no logger.
#[cfg(feature = "characterize")]
fn make_characterizing_tap_block(
    converter: Retained<AVAudioConverter>,
    target_format: Retained<AVAudioFormat>,
    characterize_tx: SyncSender<CharacterizationFrame>,
    counters: Arc<AudioDropCounters>,
) -> TapBlock {
    let next_seq = AtomicU64::new(0);
    RcBlock::new(
        move |input_buffer: NonNull<AVAudioPCMBuffer>, _when: NonNull<AVAudioTime>| {
            autoreleasepool(|_| {
                // SAFETY: the engine hands the tap a valid PCM buffer that
                // lives for the duration of this callback.
                let input_buffer = unsafe { input_buffer.as_ref() };

                let Some(frame) = characterize_buffer(
                    &converter,
                    &target_format,
                    input_buffer,
                    &counters,
                    &next_seq,
                ) else {
                    return;
                };

                crate::drops::send_audio(
                    &characterize_tx,
                    frame,
                    &counters,
                    crate::drops::AudioSourceKind::Microphone,
                );
            });
        },
    )
}

/// Convert one native-format tap buffer to mono/48 kHz/f32 samples.
///
/// Runs the converter into a freshly allocated target-format buffer, then
/// copies channel zero out as an owned `Vec<f32>`. Every exit is named by
/// [`ConversionOutcome`]; a `Failed` is counted in `mic_convert_failed` here,
/// in one place, so callers never count.
///
/// The converter is shared across tap buffers and keeps its resampler state
/// between calls: the input block signals `NoDataNow`, not `EndOfStream`, so
/// successive tap buffers resample as one continuous stream.
///
/// Under `NoDataNow`, each call returns slightly fewer frames than the
/// converter could ideally produce: it keeps the trailing partial block of
/// its internal packetization (~700 frames, ~15 ms at 48 kHz) buffered for
/// the next call. The next tap buffer flushes that tail, so within a
/// capture session the stream is continuous. To prevent the buffered tail
/// from carrying into the next capture session, [`MicrophoneCapture::stop`]
/// calls `reset()` on the shared converter after stopping the engine, so
/// each new session starts with a clean resampler.
///
/// The output is always a single (mono) channel, so the `AudioBuffer.samples`
/// contract — "interleaved f32" — is trivially satisfied: for one channel,
/// interleaved and deinterleaved layouts are identical.
fn convert_to_mono_samples(
    converter: &AVAudioConverter,
    target_format: &AVAudioFormat,
    input_buffer: &AVAudioPCMBuffer,
    counters: &AudioDropCounters,
) -> ConversionOutcome {
    let outcome = convert_uncounted(converter, target_format, input_buffer);
    if outcome == ConversionOutcome::Failed {
        counters.mic_convert_failed.fetch_add(1, Ordering::Relaxed);
    }
    outcome
}

/// The conversion itself. Split from [`convert_to_mono_samples`] so the three
/// `Failed` exits are counted by one line rather than three.
fn convert_uncounted(
    converter: &AVAudioConverter,
    target_format: &AVAudioFormat,
    input_buffer: &AVAudioPCMBuffer,
) -> ConversionOutcome {
    // SAFETY: frameLength is a plain property read.
    let input_frames = unsafe { input_buffer.frameLength() };
    if input_frames == 0 {
        return ConversionOutcome::HeldTail;
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
    let Some(output_buffer) = (unsafe {
        AVAudioPCMBuffer::initWithPCMFormat_frameCapacity(
            AVAudioPCMBuffer::alloc(),
            target_format,
            output_capacity,
        )
    }) else {
        return ConversionOutcome::Failed;
    };

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
        return ConversionOutcome::Failed;
    }

    // SAFETY: frameLength reflects how many frames the converter produced.
    let output_frames = unsafe { output_buffer.frameLength() } as usize;
    if output_frames == 0 {
        return ConversionOutcome::HeldTail;
    }

    // SAFETY: the target format is 32-bit float, so floatChannelData is
    // non-nil and points to `channelCount` pointers, each to `frameLength`
    // samples. The target format is mono, so channel zero is the whole signal.
    let channel_data = unsafe { output_buffer.floatChannelData() };
    let Some(channel_ptr) = NonNull::new(channel_data) else {
        return ConversionOutcome::Failed;
    };
    // SAFETY: channel_ptr points to at least one channel pointer.
    let channel_zero = unsafe { channel_ptr.as_ptr().read() };
    // SAFETY: channel_zero points to `output_frames` valid f32 samples; the
    // standard mono format is non-interleaved with stride 1.
    let channel_slice = unsafe { std::slice::from_raw_parts(channel_zero.as_ptr(), output_frames) };

    ConversionOutcome::Produced(mono_samples(channel_slice, output_frames))
}

/// Copy `frames` samples out of each channel plane, honouring `stride`.
///
/// This is the pure, testable core of channel extraction. `planes[c]` is the
/// slice `AVAudioPCMBuffer::floatChannelData` exposes for channel `c`, and
/// `stride` is `AVAudioPCMBuffer::stride` — the sample spacing between
/// consecutive frames of one channel.
///
/// - Deinterleaved buffers report `stride == 1`: each plane is that channel's
///   own contiguous run.
/// - Interleaved buffers report `stride == channelCount` and share one
///   allocation; frame `f` of channel `c` lives at `plane[f * stride]`, where
///   the plane pointer already starts at channel `c`'s first sample.
///
/// **Every returned channel has the same length.** A short plane truncates
/// *all* channels to a common safe frame count rather than truncating itself
/// alone. That matters downstream: HEU-652's `mix_to_mono` treats equal channel
/// lengths as an extraction invariant and asserts on it, so returning ragged
/// channels here would break an invariant two tickets away.
///
/// The result is therefore one of two shapes: **empty**, or **exactly
/// `planes.len()` channels of equal length**. It is never a partial set. A
/// caller that indexes `result[1]` must check for the empty case first — a
/// zero-frame buffer yields no channels at all, not N empty ones.
fn extract_channels_from_planes(planes: &[&[f32]], frames: usize, stride: usize) -> Vec<Vec<f32>> {
    if planes.is_empty() || frames == 0 || stride == 0 {
        return Vec::new();
    }

    // Frame `f` of a channel lives at index `f * stride`, so a plane of length
    // `len` can supply `(len - 1) / stride + 1` frames. The smallest across all
    // planes bounds every channel.
    let supplied = planes
        .iter()
        .map(|plane| {
            if plane.is_empty() {
                0
            } else {
                (plane.len() - 1) / stride + 1
            }
        })
        .min()
        // The `planes.is_empty()` guard above is what makes this infallible.
        .expect("planes is non-empty");

    let usable = frames.min(supplied);
    if usable == 0 {
        return Vec::new();
    }

    planes
        .iter()
        .map(|plane| (0..usable).map(|f| plane[f * stride]).collect())
        .collect()
}

/// Extract every channel of `buffer` as owned `f32` samples.
///
/// Returns `None` when the buffer cannot be read as f32 channel data:
///
/// - the format is not 32-bit float (`floatChannelData` is nil for every
///   other format, and dereferencing it would be undefined behaviour),
/// - the buffer has zero valid frames, zero channels, or a zero stride, or
/// - the plane length required to cover `frames` at `stride` overflows
///   `usize`.
///
/// Returning `None` **silently** is deliberate: this runs on the audio
/// callback, which must never touch the logger (ADR-013). The diagnostic for
/// a non-f32 device is the one-time classification log at tap install, not a
/// per-buffer warning.
///
/// When it returns `Some`, the result always carries **exactly `channelCount`
/// channels of exactly `frameLength` samples each** — never a partial or empty
/// set. The empty-result path documented on [`extract_channels_from_planes`] is
/// unreachable from here, because the guards below reject every input that
/// could produce it. Callers do not need a defensive empty check.
///
/// **Callers must supply an `autoreleasepool`.** This makes ObjC property
/// calls and does not open a pool of its own; the tap block already runs inside
/// one, so both HEU-650's feature-gated characterization path and HEU-652's
/// production path are covered.
// The only caller is the characterization tap, behind the `characterize`
// feature, so a default build still sees this as dead. HEU-652 wires the
// production tap block through it and removes the attribute.
#[cfg_attr(not(feature = "characterize"), allow(dead_code))]
fn extract_channels(buffer: &AVAudioPCMBuffer) -> Option<Vec<Vec<f32>>> {
    // SAFETY: each of these four is a plain property read with no
    // preconditions, sound in any buffer state. `format` is bound to a local
    // so its `Retained` outlives the `channelCount` read.
    let frames = unsafe { buffer.frameLength() } as usize;
    let format = unsafe { buffer.format() };
    let channels = unsafe { format.channelCount() } as usize;
    let stride = unsafe { buffer.stride() };

    // SAFETY: also a plain property read.
    let data = unsafe { buffer.floatChannelData() };

    // A nil return is how AVFoundation reports "not 32-bit float" — that is
    // the value's meaning, not the reason the call above is sound.
    if data.is_null() || frames == 0 || channels == 0 || stride == 0 {
        return None;
    }

    // `floatChannelData` returns `channelCount` pointers, each addressing
    // `frameLength` valid samples spaced by `stride` samples. The last sample
    // of a channel therefore sits at `(frames - 1) * stride`, and the readable
    // run behind each pointer is that index plus one.
    //
    // The `- 1` is load-bearing, not defensive rounding: `frames * stride`
    // would over-read by `stride - 1` elements past the last channel.
    let plane_len = (frames - 1).checked_mul(stride)?.checked_add(1)?;

    let mut planes: Vec<&[f32]> = Vec::with_capacity(channels);
    for ch in 0..channels {
        // SAFETY: three preconditions, and the interleaved case is the
        // non-obvious one.
        //
        // 1. `data` is non-null (checked above) and points to exactly
        //    `channelCount` pointers — `format.channelCount()` is the same
        //    value AVFoundation sized that array with.
        //
        // 2. Each pointer has at least `plane_len` readable f32s behind it.
        //    Deinterleaved: every channel is its own allocation, `stride == 1`,
        //    and `plane_len == frames`. Interleaved: all `channelCount` slices
        //    alias ONE allocation of `frames * channelCount` samples, and
        //    channel `c`'s pointer starts at offset `c`. The highest channel
        //    therefore ends at `(channelCount - 1) + (frames - 1) * stride`,
        //    which for `stride == channelCount` is exactly the final element —
        //    an exact fit with zero slack, verified against the real allocator
        //    for both layouts. Widening `plane_len` breaks this.
        //
        // 3. The slices deliberately alias each other in the interleaved case.
        //    That is sound only because every one of them is a shared `&[f32]`
        //    and nothing holds a `&mut` to the buffer for their lifetime — the
        //    buffer is borrowed immutably by this function's signature and
        //    outlives every slice built here.
        //
        // `frames <= frameCapacity` is guaranteed by AVFoundation, which
        // rejects an out-of-range `setFrameLength:`.
        let ptr = unsafe { *data.add(ch) };
        planes.push(unsafe { std::slice::from_raw_parts(ptr.as_ptr(), plane_len) });
    }

    Some(extract_channels_from_planes(&planes, frames, stride))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_channels_deinterleaved_splits_both_channels() {
        // stride == 1 is the deinterleaved layout: each channel pointer walks
        // its own contiguous run of samples.
        let left = [1.0_f32, 2.0, 3.0];
        let right = [-1.0_f32, -2.0, -3.0];
        let planes: Vec<&[f32]> = vec![&left, &right];

        let result = extract_channels_from_planes(&planes, 3, 1);

        assert_eq!(result, vec![vec![1.0, 2.0, 3.0], vec![-1.0, -2.0, -3.0]]);
    }

    #[test]
    fn extract_channels_interleaved_splits_both_channels() {
        // stride == 2 with one shared allocation: L R L R L R. Each channel's
        // plane pointer already starts at that channel's first sample, so
        // channel 1 begins one sample in.
        let interleaved = [1.0_f32, -1.0, 2.0, -2.0, 3.0, -3.0];
        let planes: Vec<&[f32]> = vec![&interleaved[0..], &interleaved[1..]];

        let result = extract_channels_from_planes(&planes, 3, 2);

        assert_eq!(result, vec![vec![1.0, 2.0, 3.0], vec![-1.0, -2.0, -3.0]]);
    }

    #[test]
    fn extract_channels_handles_more_than_two_channels() {
        let a = [1.0_f32, 2.0];
        let b = [3.0_f32, 4.0];
        let c = [5.0_f32, 6.0];
        let d = [7.0_f32, 8.0];
        let planes: Vec<&[f32]> = vec![&a, &b, &c, &d];

        let result = extract_channels_from_planes(&planes, 2, 1);

        assert_eq!(
            result,
            vec![
                vec![1.0, 2.0],
                vec![3.0, 4.0],
                vec![5.0, 6.0],
                vec![7.0, 8.0]
            ]
        );
    }

    #[test]
    fn extract_channels_mono_is_an_identity_copy() {
        let only = [0.5_f32, -0.5];
        let planes: Vec<&[f32]> = vec![&only];

        let result = extract_channels_from_planes(&planes, 2, 1);

        assert_eq!(result, vec![vec![0.5, -0.5]]);
    }

    #[test]
    fn extract_channels_clamps_overlong_frame_count() {
        // A frame count past the end of the plane truncates; it must not panic
        // and must not read past the slice.
        let short = [1.0_f32, 2.0];
        let planes: Vec<&[f32]> = vec![&short];

        let result = extract_channels_from_planes(&planes, 99, 1);

        assert_eq!(result, vec![vec![1.0, 2.0]]);
    }

    #[test]
    fn extract_channels_clamps_overlong_frame_count_when_interleaved() {
        // The stride == 1 case above cannot protect the clamp arithmetic:
        // `(len - 1) / stride + 1` collapses to `len` there, so the divide is a
        // no-op and a naive `plane.len()` bound survives it. At stride > 1 the
        // permissive version reads past the plane and panics, which is the
        // failure this test exists to prevent.
        //
        // Channel 0's plane is deliberately one sample short of the full run.
        let interleaved = [1.0_f32, -1.0, 2.0, -2.0, 3.0, -3.0];
        let planes: Vec<&[f32]> = vec![&interleaved[0..5], &interleaved[1..]];

        let result = extract_channels_from_planes(&planes, 99, 2);

        assert_eq!(result, vec![vec![1.0, 2.0, 3.0], vec![-1.0, -2.0, -3.0]]);
    }

    #[test]
    fn extract_channels_keeps_every_channel_the_same_length() {
        // One short plane truncates ALL channels, not just itself. HEU-652's
        // mix_to_mono treats equal channel lengths as an extraction invariant;
        // ragged output here would break it two tickets away.
        let long = [1.0_f32, 2.0, 3.0, 4.0];
        let short = [9.0_f32, 8.0];
        let planes: Vec<&[f32]> = vec![&long, &short];

        let result = extract_channels_from_planes(&planes, 4, 1);

        assert_eq!(result, vec![vec![1.0, 2.0], vec![9.0, 8.0]]);
        assert_eq!(
            result[0].len(),
            result[1].len(),
            "channels must be equal length"
        );
    }

    #[test]
    fn extract_channels_degenerate_inputs_give_empty() {
        // Named for all four cases, not just two: zero stride is what kills a
        // missing divide-by-zero guard, and an empty plane among non-empty ones
        // is what kills a missing `is_empty` check inside the `supplied` fold
        // (its `plane.len() - 1` would underflow).
        let plane = [1.0_f32];
        let planes: Vec<&[f32]> = vec![&plane];

        assert!(
            extract_channels_from_planes(&planes, 0, 1).is_empty(),
            "zero frames"
        );
        assert!(
            extract_channels_from_planes(&[], 4, 1).is_empty(),
            "no planes"
        );
        assert!(
            extract_channels_from_planes(&planes, 4, 0).is_empty(),
            "zero stride"
        );

        let empty: [f32; 0] = [];
        let mixed: Vec<&[f32]> = vec![&plane, &empty];
        assert!(
            extract_channels_from_planes(&mixed, 4, 1).is_empty(),
            "one empty plane among non-empty"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn extract_channels_reads_a_real_deinterleaved_buffer() {
        autoreleasepool(|_| {
            let format = unsafe {
                AVAudioFormat::initStandardFormatWithSampleRate_channels(
                    AVAudioFormat::alloc(),
                    48_000.0,
                    2,
                )
            }
            .expect("stereo format should build");

            let buffer = unsafe {
                AVAudioPCMBuffer::initWithPCMFormat_frameCapacity(
                    AVAudioPCMBuffer::alloc(),
                    &format,
                    4,
                )
            }
            .expect("buffer should allocate");

            // frameLength FIRST — floatChannelData points at `frameLength`
            // valid samples, so on a fresh buffer (frameLength == 0) there is
            // nothing valid to write into yet.
            unsafe { buffer.setFrameLength(4) };

            // The name claims deinterleaved; assert it rather than assume it,
            // so a layout surprise fails with "stride was 2" instead of a
            // confusing value mismatch below.
            assert_eq!(
                unsafe { buffer.stride() },
                1,
                "standard format should be deinterleaved"
            );

            let channels = unsafe { buffer.floatChannelData() };
            for ch in 0..2usize {
                let plane = unsafe { *channels.add(ch) };
                for f in 0..4usize {
                    unsafe { plane.as_ptr().add(f).write(ch as f32 * 10.0 + f as f32) };
                }
            }

            let result = extract_channels(&buffer).expect("f32 buffer should extract");

            assert_eq!(
                result,
                vec![vec![0.0, 1.0, 2.0, 3.0], vec![10.0, 11.0, 12.0, 13.0]]
            );
        });
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn extract_channels_honours_frame_length_not_capacity() {
        // Every real tap buffer has frameLength < frameCapacity — the engine
        // hands back a buffer sized to a hint and fills part of it. Nothing
        // else in this suite pins that extraction reads frameLength rather
        // than capacity, so a capacity-based implementation would return
        // stale samples past the valid region and go unnoticed.
        autoreleasepool(|_| {
            let format = unsafe {
                AVAudioFormat::initStandardFormatWithSampleRate_channels(
                    AVAudioFormat::alloc(),
                    48_000.0,
                    2,
                )
            }
            .expect("stereo format should build");

            let buffer = unsafe {
                AVAudioPCMBuffer::initWithPCMFormat_frameCapacity(
                    AVAudioPCMBuffer::alloc(),
                    &format,
                    8,
                )
            }
            .expect("buffer should allocate");

            // Fill all 8 frames, then declare only the first 4 valid.
            unsafe { buffer.setFrameLength(8) };
            let channels = unsafe { buffer.floatChannelData() };
            for ch in 0..2usize {
                let plane = unsafe { *channels.add(ch) };
                for f in 0..8usize {
                    unsafe { plane.as_ptr().add(f).write(ch as f32 * 100.0 + f as f32) };
                }
            }
            unsafe { buffer.setFrameLength(4) };

            let result = extract_channels(&buffer).expect("f32 buffer should extract");

            // Only the first 4 of each channel — never the tail 4.
            assert_eq!(
                result,
                vec![vec![0.0, 1.0, 2.0, 3.0], vec![100.0, 101.0, 102.0, 103.0]]
            );
        });
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn extract_channels_is_identical_across_layouts() {
        // Same signal, both layouts, identical output. Writing through the
        // same (pointer, stride) access pattern extraction uses would let a
        // wrong-but-self-consistent interpretation pass, so the physical
        // layout is asserted explicitly before anything is written.
        fn build_and_extract(interleaved: bool) -> Vec<Vec<f32>> {
            autoreleasepool(|_| {
                let format = unsafe {
                    AVAudioFormat::initWithCommonFormat_sampleRate_channels_interleaved(
                        AVAudioFormat::alloc(),
                        AVAudioCommonFormat::PCMFormatFloat32,
                        48_000.0,
                        2,
                        interleaved,
                    )
                }
                .expect("stereo f32 format should build");

                let buffer = unsafe {
                    AVAudioPCMBuffer::initWithPCMFormat_frameCapacity(
                        AVAudioPCMBuffer::alloc(),
                        &format,
                        4,
                    )
                }
                .expect("buffer should allocate");

                // frameLength FIRST — see the deinterleaved test above.
                unsafe { buffer.setFrameLength(4) };

                let stride = unsafe { buffer.stride() };
                let data = unsafe { buffer.floatChannelData() };
                assert!(!data.is_null(), "f32 buffer must expose float channel data");

                // Verify the physical interpretation BEFORE writing through it.
                assert_eq!(
                    stride,
                    if interleaved { 2 } else { 1 },
                    "stride for interleaved={interleaved}"
                );
                let p0 = unsafe { *data.add(0) }.as_ptr();
                let p1 = unsafe { *data.add(1) }.as_ptr();
                if interleaved {
                    // One shared allocation, so `offset_from` is defined here:
                    // channel 1 starts one f32 after channel 0.
                    //
                    // SAFETY: both pointers are into the same interleaved
                    // buffer, which is what `offset_from` requires.
                    let gap = unsafe { p1.offset_from(p0) };
                    assert_eq!(gap, 1, "interleaved channel pointers must be adjacent");
                } else {
                    // Deinterleaved planes may be *separate* allocations —
                    // AVFoundation guarantees only distinct chunks, not one
                    // block. `offset_from` across two allocations is undefined
                    // behaviour, so compare addresses as integers instead.
                    assert_ne!(p0, p1, "deinterleaved planes must be distinct");
                    let d = (p0 as usize).abs_diff(p1 as usize) / size_of::<f32>();
                    assert!(d >= 4, "deinterleaved planes must not overlap, gap={d}");
                }

                for ch in 0..2usize {
                    let plane = unsafe { *data.add(ch) };
                    for f in 0..4usize {
                        let value = ch as f32 * 10.0 + f as f32;
                        unsafe { plane.as_ptr().add(f * stride).write(value) };
                    }
                }

                extract_channels(&buffer).expect("f32 buffer should extract")
            })
        }

        let deinterleaved = build_and_extract(false);
        let interleaved = build_and_extract(true);

        let expected = vec![
            vec![0.0_f32, 1.0, 2.0, 3.0],
            vec![10.0_f32, 11.0, 12.0, 13.0],
        ];

        assert_eq!(deinterleaved, expected, "deinterleaved layout");
        assert_eq!(interleaved, expected, "interleaved layout");
        assert_eq!(deinterleaved, interleaved, "layouts must agree");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn extract_channels_returns_none_for_a_zero_frame_buffer() {
        // Not hypothetical: a freshly allocated AVAudioPCMBuffer has
        // frameLength == 0, and HEU-650 calls this on every tap callback.
        //
        // This guard is load-bearing. Without it, `(frames - 1)` panics with
        // "attempt to subtract with overflow" in a debug build. The suite's
        // other zero-frames coverage exercises `extract_channels_from_planes`,
        // which is a different function with its own separate guard — deleting
        // this one leaves all of those green.
        autoreleasepool(|_| {
            let format = unsafe {
                AVAudioFormat::initStandardFormatWithSampleRate_channels(
                    AVAudioFormat::alloc(),
                    48_000.0,
                    2,
                )
            }
            .expect("stereo format should build");

            let buffer = unsafe {
                AVAudioPCMBuffer::initWithPCMFormat_frameCapacity(
                    AVAudioPCMBuffer::alloc(),
                    &format,
                    4,
                )
            }
            .expect("buffer should allocate");

            // Deliberately no setFrameLength — frameLength is 0.
            assert_eq!(unsafe { buffer.frameLength() }, 0);
            assert!(extract_channels(&buffer).is_none());
        });
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn extract_channels_returns_none_for_non_f32() {
        autoreleasepool(|_| {
            // Int16 has no float channel data; extraction must decline rather
            // than dereference nil. It must also stay silent — this runs per
            // callback.
            let format = unsafe {
                AVAudioFormat::initWithCommonFormat_sampleRate_channels_interleaved(
                    AVAudioFormat::alloc(),
                    AVAudioCommonFormat::PCMFormatInt16,
                    48_000.0,
                    2,
                    true,
                )
            }
            .expect("int16 format should build");

            let buffer = unsafe {
                AVAudioPCMBuffer::initWithPCMFormat_frameCapacity(
                    AVAudioPCMBuffer::alloc(),
                    &format,
                    4,
                )
            }
            .expect("buffer should allocate");
            unsafe { buffer.setFrameLength(4) };

            assert!(extract_channels(&buffer).is_none());
        });
    }

    #[test]
    fn mix_eligibility_is_stereo_for_f32_two_channels() {
        assert_eq!(
            MixEligibility::classify(AVAudioCommonFormat::PCMFormatFloat32, 2),
            MixEligibility::Stereo
        );
    }

    #[test]
    fn mix_eligibility_is_mono_for_f32_one_channel() {
        assert_eq!(
            MixEligibility::classify(AVAudioCommonFormat::PCMFormatFloat32, 1),
            MixEligibility::Mono
        );
    }

    #[test]
    fn mix_eligibility_is_ineligible_above_two_channels() {
        // No measurement covers arrays. Whatever such a device does today, it
        // keeps doing — the explicit mix will simply not be applied.
        //
        // 3 is the load-bearing case: it is the first value past the boundary,
        // and an arm that wrongly admitted it would be invisible to a test
        // that only checks 4. Both are asserted for that reason.
        for channels in [3, 4, 8] {
            assert_eq!(
                MixEligibility::classify(AVAudioCommonFormat::PCMFormatFloat32, channels),
                MixEligibility::Ineligible,
                "{channels} channels must not be eligible"
            );
        }
    }

    #[test]
    fn mix_eligibility_is_ineligible_for_non_f32() {
        // AVAudioConverter normalizes non-f32 today. Rejecting here would take
        // a working device to a hard failure, which HEU-649 explicitly forbids:
        // input the measurement does not cover keeps the path it already uses.
        //
        // Every non-f32 format is checked at BOTH channel counts that would
        // otherwise be eligible. Checking only stereo leaves an arm like
        // `(PCMFormatInt16, 1) => Mono` undetectable.
        for format in [
            AVAudioCommonFormat::PCMFormatInt16,
            AVAudioCommonFormat::PCMFormatInt32,
            AVAudioCommonFormat::PCMFormatFloat64,
            AVAudioCommonFormat::OtherFormat,
        ] {
            for channels in [1, 2] {
                assert_eq!(
                    MixEligibility::classify(format, channels),
                    MixEligibility::Ineligible,
                    "format {format:?} at {channels} ch must not be eligible"
                );
            }
        }
    }

    #[test]
    fn mix_eligibility_is_ineligible_for_zero_channels() {
        assert_eq!(
            MixEligibility::classify(AVAudioCommonFormat::PCMFormatFloat32, 0),
            MixEligibility::Ineligible
        );
    }

    #[test]
    fn common_format_names_are_distinct_and_stable() {
        // Same reasoning as the eligibility labels below, and it matters more
        // here: `Ineligible` collapses "not f32" and "more than two channels",
        // so this name is the only field in the log line that separates an
        // Int16 stereo microphone from a four-channel f32 array. A swapped arm
        // would ship silently and corrupt exactly the evidence HEU-651 reads.
        assert_eq!(
            common_format_name(AVAudioCommonFormat::PCMFormatFloat32),
            "f32"
        );
        assert_eq!(
            common_format_name(AVAudioCommonFormat::PCMFormatFloat64),
            "f64"
        );
        assert_eq!(
            common_format_name(AVAudioCommonFormat::PCMFormatInt16),
            "i16"
        );
        assert_eq!(
            common_format_name(AVAudioCommonFormat::PCMFormatInt32),
            "i32"
        );
        assert_eq!(
            common_format_name(AVAudioCommonFormat::OtherFormat),
            "other"
        );
    }

    #[test]
    fn common_format_name_surfaces_unknown_values() {
        // A format AVFoundation adds later is the case worth seeing, so it
        // prints the discriminant rather than being folded into "other".
        assert_eq!(common_format_name(AVAudioCommonFormat(99)), "unknown(99)");
    }

    #[test]
    fn mix_eligibility_labels_are_distinct_and_stable() {
        // The label is what lands in the log and what a future reader greps
        // for. Classification tests do not catch a swapped or duplicated label.
        assert_eq!(MixEligibility::Mono.as_str(), "mono");
        assert_eq!(MixEligibility::Stereo.as_str(), "stereo");
        assert_eq!(MixEligibility::Ineligible.as_str(), "ineligible");
    }

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

            let out = convert_to_mono_samples(
                &converter,
                &target_format,
                &input_buffer,
                &AudioDropCounters::default(),
            )
            .into_produced()
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

    /// A buffer with `frameLength == 0` has nothing to convert. That is the
    /// benign no-output case, not a failure, and it must not count.
    #[cfg(target_os = "macos")]
    #[test]
    fn convert_reports_held_tail_for_a_zero_frame_buffer() {
        autoreleasepool(|_| {
            // SAFETY: alloc yields a fresh AVAudioFormat; unwrapped below.
            let format = unsafe {
                AVAudioFormat::initStandardFormatWithSampleRate_channels(
                    AVAudioFormat::alloc(),
                    SAMPLE_RATE as f64,
                    CHANNEL_COUNT,
                )
            }
            .expect("48 kHz mono format should build");
            // SAFETY: format is valid; 16 is a non-zero capacity.
            let input = unsafe {
                AVAudioPCMBuffer::initWithPCMFormat_frameCapacity(
                    AVAudioPCMBuffer::alloc(),
                    &format,
                    16,
                )
            }
            .expect("buffer should allocate");
            // Deliberately no setFrameLength — frameLength stays 0.
            // SAFETY: both formats are valid PCM formats.
            let converter = unsafe {
                AVAudioConverter::initFromFormat_toFormat(
                    AVAudioConverter::alloc(),
                    &format,
                    &format,
                )
            }
            .expect("identity converter should build");
            let counters = AudioDropCounters::default();

            let outcome = convert_to_mono_samples(&converter, &format, &input, &counters);

            assert_eq!(outcome, ConversionOutcome::HeldTail);
            assert_eq!(
                counters.snapshot().mic_convert_failed,
                0,
                "a zero-frame buffer is not a conversion failure"
            );
        });
    }

    #[test]
    fn into_produced_keeps_only_samples() {
        assert_eq!(
            ConversionOutcome::Produced(vec![0.5]).into_produced(),
            Some(vec![0.5])
        );
        assert_eq!(ConversionOutcome::HeldTail.into_produced(), None);
        assert_eq!(ConversionOutcome::Failed.into_produced(), None);
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
            let single = convert_to_mono_samples(
                &single_converter,
                &target_format,
                &full,
                &AudioDropCounters::default(),
            )
            .into_produced()
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
            let mut chunked = convert_to_mono_samples(
                &chunk_converter,
                &target_format,
                &chunk0,
                &AudioDropCounters::default(),
            )
            .into_produced()
            .expect("chunk 0 conversion should yield samples");
            chunked.extend(
                convert_to_mono_samples(
                    &chunk_converter,
                    &target_format,
                    &chunk1,
                    &AudioDropCounters::default(),
                )
                .into_produced()
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

            let out =
                convert_to_mono_samples(&converter, &format, &input, &AudioDropCounters::default())
                    .into_produced()
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
        let mic = MicrophoneCapture::new(tx, Arc::new(AudioDropCounters::default()))
            .expect("microphone setup should succeed");

        assert!(!mic.is_running(), "engine should not run before start()");

        mic.start().expect("start should succeed");
        assert!(mic.is_running(), "engine should run after start()");

        mic.stop().expect("stop should succeed");
        assert!(!mic.is_running(), "engine should not run after stop()");
    }

    /// Build a deinterleaved 48 kHz stereo f32 buffer with `frames` frames.
    /// Channel 0 holds `left + f * RAMP` at frame `f`, channel 1 the same on
    /// `right`, so a stride or offset bug shows at the tail, not just at 0.
    #[cfg(all(target_os = "macos", feature = "characterize"))]
    fn make_stereo_buffer(
        frames: u32,
        left: f32,
        right: f32,
    ) -> (Retained<AVAudioFormat>, Retained<AVAudioPCMBuffer>) {
        // SAFETY: alloc yields a fresh AVAudioFormat; unwrapped below.
        let format = unsafe {
            AVAudioFormat::initStandardFormatWithSampleRate_channels(
                AVAudioFormat::alloc(),
                SAMPLE_RATE as f64,
                2,
            )
        }
        .expect("48 kHz stereo format should build");
        // SAFETY: format is valid; frames is non-zero.
        let buffer = unsafe {
            AVAudioPCMBuffer::initWithPCMFormat_frameCapacity(
                AVAudioPCMBuffer::alloc(),
                &format,
                frames,
            )
        }
        .expect("buffer should allocate");
        // SAFETY: frames <= frameCapacity.
        unsafe { buffer.setFrameLength(frames) };
        // SAFETY: an f32 format, so floatChannelData is non-nil and points to
        // two channel pointers, each to `frames` writable samples (stride 1).
        let data = unsafe { buffer.floatChannelData() };
        assert!(!data.is_null(), "f32 buffer must expose float channel data");
        for (ch, value) in [(0usize, left), (1, right)] {
            // SAFETY: ch < channelCount; the plane holds `frames` f32s.
            let plane = unsafe { *data.add(ch) };
            let samples =
                unsafe { std::slice::from_raw_parts_mut(plane.as_ptr(), frames as usize) };
            for (f, sample) in samples.iter_mut().enumerate() {
                *sample = value + f as f32 * RAMP;
            }
        }
        (format, buffer)
    }

    /// Per-frame increment `make_stereo_buffer` adds, small enough that 4800
    /// frames stay well inside full scale.
    #[cfg(all(target_os = "macos", feature = "characterize"))]
    const RAMP: f32 = 0.000_01;

    #[cfg(all(target_os = "macos", feature = "characterize"))]
    fn stereo_to_mono_converter(input: &AVAudioFormat) -> Retained<AVAudioConverter> {
        // SAFETY: alloc yields a fresh AVAudioFormat; unwrapped below.
        let target = unsafe {
            AVAudioFormat::initStandardFormatWithSampleRate_channels(
                AVAudioFormat::alloc(),
                SAMPLE_RATE as f64,
                CHANNEL_COUNT,
            )
        }
        .expect("48 kHz mono format should build");
        // SAFETY: both formats are valid PCM formats.
        unsafe {
            AVAudioConverter::initFromFormat_toFormat(AVAudioConverter::alloc(), input, &target)
        }
        .expect("stereo -> mono converter should build")
    }

    /// One frame carries both channels, in order, plus the converter's
    /// outcome, and consumes exactly one sequence number.
    #[cfg(all(target_os = "macos", feature = "characterize"))]
    #[test]
    fn characterize_buffer_carries_both_channels_and_the_outcome() {
        autoreleasepool(|_| {
            let (format, buffer) = make_stereo_buffer(4_800, 0.25, -0.25);
            let converter = stereo_to_mono_converter(&format);
            // SAFETY: outputFormat is a plain property read on a valid converter.
            let target = unsafe { converter.outputFormat() };
            let counters = AudioDropCounters::default();
            let seq = AtomicU64::new(0);

            let frame = characterize_buffer(&converter, &target, &buffer, &counters, &seq)
                .expect("a 4800-frame f32 buffer must produce a frame");

            assert_eq!(frame.seq, 0);
            assert_eq!(frame.native.len(), 2, "one Vec per channel");
            assert_eq!(frame.native[0].len(), 4_800);
            assert_eq!(frame.native[1].len(), 4_800);
            assert_eq!(frame.native[0][0], 0.25);
            assert_eq!(frame.native[1][0], -0.25);
            assert_eq!(frame.native[0][4_799], 0.25 + 4_799.0 * RAMP);
            assert_eq!(frame.native[1][4_799], -0.25 + 4_799.0 * RAMP);
            assert!(
                matches!(frame.outcome, ConversionOutcome::Produced(ref s) if !s.is_empty()),
                "100 ms of stereo must convert to some mono output, got {:?}",
                frame.outcome
            );
            assert_eq!(seq.load(Ordering::Relaxed), 1, "one frame, one seq");

            let second = characterize_buffer(&converter, &target, &buffer, &counters, &seq)
                .expect("second frame");
            assert_eq!(second.seq, 1, "seq must be monotonic");
            assert_eq!(counters.snapshot().mic_convert_failed, 0);
        });
    }

    /// A zero-frame buffer records nothing and must not burn a sequence
    /// number, or the writer would report a gap that was never a drop.
    #[cfg(all(target_os = "macos", feature = "characterize"))]
    #[test]
    fn characterize_buffer_skips_a_zero_frame_buffer_without_a_seq() {
        autoreleasepool(|_| {
            let (format, buffer) = make_stereo_buffer(16, 0.0, 0.0);
            // SAFETY: 0 <= frameCapacity.
            unsafe { buffer.setFrameLength(0) };
            let converter = stereo_to_mono_converter(&format);
            // SAFETY: outputFormat is a plain property read on a valid converter.
            let target = unsafe { converter.outputFormat() };
            let counters = AudioDropCounters::default();
            let seq = AtomicU64::new(0);

            assert!(characterize_buffer(&converter, &target, &buffer, &counters, &seq).is_none());
            assert_eq!(seq.load(Ordering::Relaxed), 0);
        });
    }

    /// A buffer with frames that cannot be read as f32 planes must not vanish
    /// silently: it takes a seq and arrives with no channels, the marker a
    /// consumer reads as malformed.
    #[cfg(all(target_os = "macos", feature = "characterize"))]
    #[test]
    fn characterize_buffer_marks_an_unreadable_buffer_as_empty() {
        autoreleasepool(|_| {
            // SAFETY: alloc yields a fresh AVAudioFormat; unwrapped below.
            let format = unsafe {
                AVAudioFormat::initWithCommonFormat_sampleRate_channels_interleaved(
                    AVAudioFormat::alloc(),
                    AVAudioCommonFormat::PCMFormatInt16,
                    SAMPLE_RATE as f64,
                    2,
                    true,
                )
            }
            .expect("int16 format should build");
            // SAFETY: format is valid; 64 is a non-zero capacity.
            let buffer = unsafe {
                AVAudioPCMBuffer::initWithPCMFormat_frameCapacity(
                    AVAudioPCMBuffer::alloc(),
                    &format,
                    64,
                )
            }
            .expect("buffer should allocate");
            // SAFETY: 64 <= frameCapacity.
            unsafe { buffer.setFrameLength(64) };
            let converter = stereo_to_mono_converter(&format);
            // SAFETY: outputFormat is a plain property read on a valid converter.
            let target = unsafe { converter.outputFormat() };
            let counters = AudioDropCounters::default();
            let seq = AtomicU64::new(0);

            let frame = characterize_buffer(&converter, &target, &buffer, &counters, &seq)
                .expect("an unreadable buffer with frames still produces a frame");

            assert_eq!(frame.seq, 0);
            assert!(frame.native.is_empty(), "no f32 planes to read");
            assert_eq!(seq.load(Ordering::Relaxed), 1, "the hole consumed a seq");
        });
    }

    #[cfg(feature = "characterize")]
    #[test]
    fn characterization_input_check_admits_only_float32() {
        let mut format = NativeFormat {
            channels: 2,
            sample_rate: 48_000.0,
            interleaved: false,
            common_format: "f32".into(),
            float32: true,
        };
        assert!(characterization_input_check(&format).is_ok());

        format.common_format = "i16".into();
        format.float32 = false;
        let err = characterization_input_check(&format).unwrap_err();
        assert!(err.to_string().contains("i16"), "{err}");
    }
}
