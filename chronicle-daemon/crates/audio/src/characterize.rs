//! Feature-gated microphone characterization (HEU-650): records what the tap
//! receives and what `AVAudioConverter` makes of it as two 32-bit float WAVs.
//! The audio thread only fills a [`CharacterizationFrame`] and `try_send`s it;
//! a plain thread writes the files (ADR-013).

use std::fmt;
use std::io;
use std::path::Path;
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::thread::JoinHandle;
use std::time::Duration;

use hound::{SampleFormat, WavSpec, WavWriter};

use crate::AudioDropSnapshot;
use crate::SAMPLE_RATE;
use crate::microphone::{ConversionOutcome, NativeFormat};

/// One tap callback's worth of data: both sides of the conversion for one buffer.
#[derive(Debug, Clone, PartialEq)]
pub struct CharacterizationFrame {
    /// Per-session counter assigned in the callback; a gap is a dropped frame.
    pub seq: u64,
    /// Native input, one `Vec` per channel, stride-corrected. Empty marks a
    /// buffer the tap could not read as f32 planes; its `seq` was still
    /// consumed, so treat it as a hole, never as silence.
    pub native: Vec<Vec<f32>>,
    /// The converter's result for this same buffer.
    pub outcome: ConversionOutcome,
}

/// What the writer saw, reported once when the channel closes. `seq_gaps`
/// counts frames missing between `first_seq` and `last_seq`; a burst lost
/// before the first frame shows as `first_seq > 0`, not as a gap.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WriterReport {
    pub frames_received: u64,
    pub first_seq: Option<u64>,
    /// The last `seq` received, which is also the highest.
    pub last_seq: Option<u64>,
    pub seq_gaps: u64,
    pub produced: u64,
    pub held_tails: u64,
    pub conversion_failures: u64,
    /// Frames whose channel count or lengths disagreed with the WAV's format.
    /// Skipped on the native side only, so the two files are offset after the
    /// first one.
    pub malformed_frames: u64,
    pub native_frames_written: u64,
    pub converted_frames_written: u64,
}

/// Why [`CharacterizationWriter::finish`] did not return a clean report.
#[derive(Debug)]
pub enum FinishError {
    /// The writer thread hit a WAV or I/O error (`partial` holds the counts so
    /// far) or panicked (`partial` is `None`). The files may be truncated.
    Writer {
        error: hound::Error,
        partial: Option<WriterReport>,
    },
    /// The channel did not close in time: a sender is still alive, usually a
    /// `MicrophoneCapture` not dropped before the caller's sender. The thread
    /// keeps running; if the process exits first, both WAVs are cut off.
    Timeout(Duration),
}

impl fmt::Display for FinishError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Writer {
                error,
                partial: Some(p),
            } => {
                let WriterReport {
                    frames_received,
                    first_seq,
                    last_seq,
                    seq_gaps,
                    produced,
                    held_tails,
                    conversion_failures,
                    malformed_frames,
                    native_frames_written,
                    converted_frames_written,
                } = p;
                let opt = |v: &Option<u64>| v.map_or_else(|| "none".to_string(), |v| v.to_string());
                write!(
                    f,
                    "characterization writer failed: {error} (partial report: frames_received {frames_received}, \
                     first_seq {}, last_seq {}, seq_gaps {seq_gaps}, produced {produced}, held_tails {held_tails}, \
                     conversion_failures {conversion_failures}, malformed_frames {malformed_frames}, \
                     native_frames_written {native_frames_written}, converted_frames_written {converted_frames_written})",
                    opt(first_seq),
                    opt(last_seq),
                )
            }
            Self::Writer {
                error,
                partial: None,
            } => write!(f, "characterization writer failed: {error} (counts lost)"),
            Self::Timeout(d) => write!(
                f,
                "characterization writer did not finish within {d:?}: a sender is still alive; \
                 the files complete only if this process outlives the writer thread"
            ),
        }
    }
}

impl std::error::Error for FinishError {}

/// A clean report, or the error plus whatever was counted before it.
type WriterResult = Result<WriterReport, (hound::Error, WriterReport)>;

/// Highest native rate a device may claim. hound's bytes per second must also
/// fit a `u32`; `spawn` checks that, since it depends on the channel count.
const MAX_SAMPLE_RATE_HZ: f64 = 768_000.0;

const BITS_PER_SAMPLE: u16 = 32;

/// Owns the thread that turns frames into two WAV files. It ends when every
/// sender is dropped; `recv` errors only after the queue drains, so there is
/// no drain step.
#[must_use = "call finish(), or the report is lost"]
pub struct CharacterizationWriter {
    handle: JoinHandle<()>,
    completion_rx: Receiver<WriterResult>,
}

impl CharacterizationWriter {
    /// Open both WAVs and start the thread. `native_path` gets `format`'s
    /// channel count and rate; `converted_path` gets one channel at 48 kHz.
    /// Opening here means a bad path fails before any audio is recorded; if
    /// the second file fails, the first is removed. Each file stops just short
    /// of 4 GiB, the limit of a WAV data chunk, with a `Writer` error.
    pub fn spawn(
        frames: Receiver<CharacterizationFrame>,
        format: &NativeFormat,
        native_path: &Path,
        converted_path: &Path,
    ) -> Result<Self, hound::Error> {
        let channels = match u16::try_from(format.channels) {
            Ok(n) if n > 0 => n,
            _ => {
                return Err(hound::Error::IoError(io::Error::other(format!(
                    "{} is not a channel count a WAV header can hold (1 to 65535)",
                    format.channels
                ))));
            }
        };
        // `as u32` would saturate a rate too large for the header, and hound
        // computes bytes per second in u32 and panics on overflow, so both
        // are refused here.
        let rate = format.sample_rate;
        let bytes_per_sample = f64::from(BITS_PER_SAMPLE.div_ceil(8));
        let bytes_per_sec = rate.round() * bytes_per_sample * f64::from(channels);
        if !rate.is_finite()
            || rate < 1.0
            || rate > MAX_SAMPLE_RATE_HZ
            || bytes_per_sec > f64::from(u32::MAX)
        {
            return Err(hound::Error::IoError(io::Error::other(format!(
                "{rate} Hz x {channels} channels is not a format a WAV header can hold \
                 (1 to {MAX_SAMPLE_RATE_HZ} Hz, and bytes per second within u32)"
            ))));
        }
        let native_spec = WavSpec {
            channels,
            sample_rate: format.sample_rate.round() as u32,
            bits_per_sample: BITS_PER_SAMPLE,
            sample_format: SampleFormat::Float,
        };
        let converted_spec = WavSpec {
            channels: 1,
            sample_rate: SAMPLE_RATE,
            bits_per_sample: BITS_PER_SAMPLE,
            sample_format: SampleFormat::Float,
        };
        let mut native = WavWriter::create(native_path, native_spec)?;
        let mut converted = match WavWriter::create(converted_path, converted_spec) {
            Ok(writer) => writer,
            Err(e) => {
                drop(native);
                let _ = std::fs::remove_file(native_path);
                return Err(e);
            }
        };

        let (completion_tx, completion_rx) = channel();
        let handle = std::thread::Builder::new()
            .name("mic-characterization-writer".into())
            .spawn(move || {
                let result = match write_all(frames, &mut native, &mut converted) {
                    Ok(report) => match native.finalize().and_then(|()| converted.finalize()) {
                        Ok(()) => Ok(report),
                        Err(e) => Err((e, report)),
                    },
                    Err(failed) => Err(failed),
                };
                // After a `finish` timeout the receiver is gone; nobody to tell.
                let _ = completion_tx.send(result);
            })?;

        Ok(Self {
            handle,
            completion_rx,
        })
    }

    /// Wait up to `timeout` for the channel to close and the files to
    /// finalize. `join` has no timeout, so the completion channel supplies it.
    pub fn finish(self, timeout: Duration) -> Result<WriterReport, FinishError> {
        match self.completion_rx.recv_timeout(timeout) {
            Ok(Ok(report)) => {
                let _ = self.handle.join();
                Ok(report)
            }
            Ok(Err((error, partial))) => {
                let _ = self.handle.join();
                Err(FinishError::Writer {
                    error,
                    partial: Some(partial),
                })
            }
            Err(RecvTimeoutError::Timeout) => Err(FinishError::Timeout(timeout)),
            Err(RecvTimeoutError::Disconnected) => {
                // Dropped without sending: the thread panicked. Keep the payload.
                let detail = match self.handle.join() {
                    Ok(()) => "the writer thread exited without reporting".to_string(),
                    Err(payload) => {
                        format!("the writer thread panicked: {}", panic_message(&payload))
                    }
                };
                Err(FinishError::Writer {
                    error: hound::Error::IoError(io::Error::other(detail)),
                    partial: None,
                })
            }
        }
    }
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

/// hound adds 60 bytes to the data length for the RIFF size field; 4096
/// covers that with room.
const WAV_DATA_LIMIT_BYTES: u64 = u32::MAX as u64 - 4096;

/// The writer loop. An error comes back with every count collected so far.
fn write_all<N: io::Write + io::Seek, C: io::Write + io::Seek>(
    frames: Receiver<CharacterizationFrame>,
    native: &mut WavWriter<N>,
    converted: &mut WavWriter<C>,
) -> WriterResult {
    let mut report = WriterReport::default();
    match write_frames(frames, native, converted, &mut report) {
        Ok(()) => Ok(report),
        Err(e) => Err((e, report)),
    }
}

/// Assumes `seq` never repeats or goes backwards: one tap block owns the
/// counter and one FIFO carries it.
fn write_frames<N: io::Write + io::Seek, C: io::Write + io::Seek>(
    frames: Receiver<CharacterizationFrame>,
    native: &mut WavWriter<N>,
    converted: &mut WavWriter<C>,
    report: &mut WriterReport,
) -> Result<(), hound::Error> {
    let channels = usize::from(native.spec().channels);

    for frame in frames {
        report.frames_received += 1;
        match report.last_seq {
            Some(last) if frame.seq > last + 1 => report.seq_gaps += frame.seq - last - 1,
            Some(_) => {}
            None => report.first_seq = Some(frame.seq),
        }
        report.last_seq = Some(frame.seq);

        let well_formed = frame.native.len() == channels
            && !frame.native.is_empty()
            && frame
                .native
                .iter()
                .all(|ch| ch.len() == frame.native[0].len());
        if well_formed {
            let frames_in = frame.native[0].len();
            check_room(native, frames_in * channels, "mic-native.wav")?;
            // Frame-major: a WAV interleaves frames and `native` is planar. The
            // channel-major loop opens fine and analyses wrong.
            for f in 0..frames_in {
                for channel in &frame.native {
                    native.write_sample(channel[f])?;
                }
            }
            report.native_frames_written += frames_in as u64;
        } else {
            report.malformed_frames += 1;
        }

        match frame.outcome {
            ConversionOutcome::Produced(samples) => {
                check_room(converted, samples.len(), "mic-converted.wav")?;
                for sample in &samples {
                    converted.write_sample(*sample)?;
                }
                report.converted_frames_written += samples.len() as u64;
                report.produced += 1;
            }
            ConversionOutcome::HeldTail => report.held_tails += 1,
            ConversionOutcome::Failed => report.conversion_failures += 1,
        }
    }

    Ok(())
}

/// Refuse a write that would carry the data chunk past its 32-bit length.
fn check_room<W: io::Write + io::Seek>(
    writer: &WavWriter<W>,
    samples_to_add: usize,
    name: &str,
) -> Result<(), hound::Error> {
    let bytes_per_sample = u64::from(writer.spec().bits_per_sample).div_ceil(8);
    let bytes_after = (u64::from(writer.len()) + samples_to_add as u64) * bytes_per_sample;
    if bytes_after > WAV_DATA_LIMIT_BYTES {
        return Err(hound::Error::IoError(io::Error::other(format!(
            "{name} reached the 4 GiB WAV data limit; the session is over"
        ))));
    }
    Ok(())
}

/// Transcription attempts per candidate. Fixed by the design: one.
pub const ATTEMPTS: u32 = 1;

/// What a recording needs beside it to mean anything later, rendered as
/// `key: value` lines: the device and its settings, the pinned phrase, model,
/// duration and attempt count, the format, and the drop and validity counts.
/// Nothing derived from the audio. `recorded_secs` comes from the frames
/// written; `requested_secs` from the caller.
#[derive(Debug)]
pub struct Manifest<'a> {
    pub device: &'a str,
    pub mode: &'a str,
    pub gain: &'a str,
    pub phrase: &'a str,
    pub model_variant: &'a str,
    pub macos_version: &'a str,
    pub recorded_at_unix_secs: u64,
    pub requested_secs: u64,
    pub format: &'a NativeFormat,
    pub native_wav: &'a Path,
    pub converted_wav: &'a Path,
    pub report: &'a WriterReport,
    pub drops: AudioDropSnapshot,
}

impl Manifest<'_> {
    /// Invalid on any dropped frame (a `seq` gap, a first `seq` above zero,
    /// `mic_full`, `mic_closed`), a conversion failure, or a malformed frame.
    /// `mic_convert_failed` needs no check: it always lands in one of those.
    pub fn measurement_valid(&self) -> bool {
        let r = self.report;
        r.first_seq == Some(0)
            && r.seq_gaps == 0
            && r.conversion_failures == 0
            && r.malformed_frames == 0
            && self.drops.mic_full == 0
            && self.drops.mic_closed == 0
    }

    pub fn recorded_secs(&self) -> f64 {
        self.report.native_frames_written as f64 / self.format.sample_rate
    }

    pub fn converted_secs(&self) -> f64 {
        self.report.converted_frames_written as f64 / f64::from(SAMPLE_RATE)
    }

    pub fn render(&self) -> String {
        let r = self.report;
        let f = self.format;
        let opt = |v: Option<u64>| v.map_or_else(|| "none".to_string(), |n| n.to_string());
        let lines = [
            format!("device: {}", self.device),
            format!("mode: {}", self.mode),
            format!("gain: {}", self.gain),
            format!("phrase: {}", self.phrase),
            format!("model_variant: {}", self.model_variant),
            format!("attempts: {ATTEMPTS}"),
            format!("macos_version: {}", self.macos_version),
            format!("recorded_at_unix_secs: {}", self.recorded_at_unix_secs),
            format!("requested_secs: {}", self.requested_secs),
            format!("recorded_secs: {:.2}", self.recorded_secs()),
            format!("converted_secs: {:.2}", self.converted_secs()),
            format!("native_channels: {}", f.channels),
            format!("native_sample_rate_hz: {}", f.sample_rate),
            format!("native_interleaved: {}", f.interleaved),
            format!("native_common_format: {}", f.common_format),
            format!("native_wav: {}", self.native_wav.display()),
            format!("converted_wav: {}", self.converted_wav.display()),
            format!("frames_received: {}", r.frames_received),
            format!("first_seq: {}", opt(r.first_seq)),
            format!("last_seq: {}", opt(r.last_seq)),
            format!("seq_gaps: {}", r.seq_gaps),
            format!("produced: {}", r.produced),
            format!("held_tails: {}", r.held_tails),
            format!("conversion_failures: {}", r.conversion_failures),
            format!("malformed_frames: {}", r.malformed_frames),
            format!("native_frames_written: {}", r.native_frames_written),
            format!("converted_frames_written: {}", r.converted_frames_written),
            format!("drops_mic_full: {}", self.drops.mic_full),
            format!("drops_mic_closed: {}", self.drops.mic_closed),
            format!(
                "drops_mic_convert_failed: {}",
                self.drops.mic_convert_failed
            ),
            format!("measurement_valid: {}", self.measurement_valid()),
        ];
        let mut out = lines.join("\n");
        out.push('\n');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::mpsc::{SyncSender, sync_channel};
    use std::time::Instant;

    use hound::WavReader;

    fn stereo_48k() -> NativeFormat {
        NativeFormat {
            channels: 2,
            sample_rate: 48_000.0,
            interleaved: false,
            common_format: "f32".into(),
            float32: true,
        }
    }

    struct Session {
        tx: SyncSender<CharacterizationFrame>,
        writer: CharacterizationWriter,
        native: PathBuf,
        converted: PathBuf,
        _dir: tempfile::TempDir,
    }

    fn session(format: &NativeFormat) -> Session {
        let dir = tempfile::tempdir().unwrap();
        let native = dir.path().join("mic-native.wav");
        let converted = dir.path().join("mic-converted.wav");
        let (tx, rx) = sync_channel(8);
        let writer = CharacterizationWriter::spawn(rx, format, &native, &converted)
            .expect("writer should spawn");
        Session {
            tx,
            writer,
            native,
            converted,
            _dir: dir,
        }
    }

    fn frame(seq: u64, native: Vec<Vec<f32>>, outcome: ConversionOutcome) -> CharacterizationFrame {
        CharacterizationFrame {
            seq,
            native,
            outcome,
        }
    }

    fn read_all(path: &Path) -> (hound::WavSpec, Vec<f32>) {
        let mut reader = WavReader::open(path).expect("wav should open");
        let spec = reader.spec();
        let samples = reader.samples::<f32>().map(|s| s.unwrap()).collect();
        (spec, samples)
    }

    #[test]
    fn writer_round_trips_planar_channels_frame_major() {
        let s = session(&stereo_48k());
        s.tx.send(frame(
            0,
            vec![vec![1.0, 2.0, 3.0], vec![10.0, 20.0, 30.0]],
            ConversionOutcome::Produced(vec![0.5, 0.25]),
        ))
        .unwrap();
        drop(s.tx);

        let report = s
            .writer
            .finish(Duration::from_secs(5))
            .expect("writer should finish");

        assert_eq!(report.frames_received, 1);
        assert_eq!(report.produced, 1);
        assert_eq!(report.native_frames_written, 3);
        assert_eq!(report.converted_frames_written, 2);
        assert_eq!(report.first_seq, Some(0));
        assert_eq!(report.last_seq, Some(0));

        let (spec, samples) = read_all(&s.native);
        assert_eq!(spec.channels, 2);
        assert_eq!(spec.sample_rate, 48_000);
        assert_eq!(spec.bits_per_sample, 32);
        assert_eq!(spec.sample_format, hound::SampleFormat::Float);
        // Frame-major: L0 R0 L1 R1 L2 R2. Channel-major gives 1 2 3 10 20 30.
        assert_eq!(samples, vec![1.0, 10.0, 2.0, 20.0, 3.0, 30.0]);

        let (spec, samples) = read_all(&s.converted);
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.sample_rate, 48_000);
        assert_eq!(samples, vec![0.5, 0.25]);
    }

    #[test]
    fn writer_counts_seq_gaps_and_records_the_range() {
        let s = session(&stereo_48k());
        for seq in [3, 4, 7, 8] {
            s.tx.send(frame(
                seq,
                vec![vec![0.0], vec![0.0]],
                ConversionOutcome::HeldTail,
            ))
            .unwrap();
        }
        drop(s.tx);

        let report = s.writer.finish(Duration::from_secs(5)).unwrap();

        assert_eq!(report.frames_received, 4);
        assert_eq!(report.first_seq, Some(3));
        assert_eq!(report.last_seq, Some(8));
        assert_eq!(report.seq_gaps, 2, "5 and 6 are missing");
    }

    #[test]
    fn writer_counts_failed_and_held_tail_separately() {
        let s = session(&stereo_48k());
        for outcome in [
            ConversionOutcome::HeldTail,
            ConversionOutcome::Failed,
            ConversionOutcome::HeldTail,
        ] {
            s.tx.send(frame(0, vec![vec![0.0], vec![0.0]], outcome))
                .unwrap();
        }
        drop(s.tx);

        let report = s.writer.finish(Duration::from_secs(5)).unwrap();

        assert_eq!(report.held_tails, 2);
        assert_eq!(report.conversion_failures, 1);
        assert_eq!(report.produced, 0);
        assert_eq!(report.converted_frames_written, 0);
    }

    #[test]
    fn writer_skips_a_frame_whose_channels_disagree_with_the_format() {
        let s = session(&stereo_48k());
        s.tx.send(frame(0, vec![vec![1.0, 2.0]], ConversionOutcome::HeldTail))
            .unwrap();
        s.tx.send(frame(
            1,
            vec![vec![1.0, 2.0], vec![3.0]],
            ConversionOutcome::HeldTail,
        ))
        .unwrap();
        drop(s.tx);

        let report = s.writer.finish(Duration::from_secs(5)).unwrap();

        assert_eq!(report.malformed_frames, 2);
        assert_eq!(report.native_frames_written, 0);
        let (_, samples) = read_all(&s.native);
        assert!(samples.is_empty());
    }

    /// `finish` must not return until every sender is gone, and promptly then.
    #[test]
    fn writer_finishes_only_after_the_last_sender_drops() {
        let s = session(&stereo_48k());
        let late = s.tx.clone();
        drop(s.tx);
        // Started before the releaser thread, so its sleep only lengthens the wait.
        let started = Instant::now();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(late);
        });

        let report = s.writer.finish(Duration::from_secs(5)).unwrap();
        releaser.join().unwrap();

        assert!(
            started.elapsed() >= Duration::from_millis(250),
            "finish returned before the last sender was dropped"
        );
        assert_eq!(report.frames_received, 0);
    }

    #[test]
    fn finish_times_out_while_a_sender_is_still_alive() {
        let s = session(&stereo_48k());
        let err = s.writer.finish(Duration::from_millis(200)).unwrap_err();
        assert!(matches!(err, FinishError::Timeout(_)), "got {err}");
        drop(s.tx);
    }

    /// A sink that accepts the WAV header and refuses everything after it.
    struct FailingSink {
        written: usize,
    }

    impl std::io::Write for FailingSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.written >= 64 {
                return Err(std::io::Error::other("disk full"));
            }
            self.written += buf.len();
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl std::io::Seek for FailingSink {
        fn seek(&mut self, _pos: std::io::SeekFrom) -> std::io::Result<u64> {
            Ok(0)
        }
    }

    #[test]
    fn write_all_returns_the_sink_error() {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut native = hound::WavWriter::new(FailingSink { written: 0 }, spec).unwrap();
        let mut converted = hound::WavWriter::new(FailingSink { written: 0 }, spec).unwrap();
        // Unbounded: every frame is queued before anyone drains.
        let (tx, rx) = std::sync::mpsc::channel();
        for _ in 0..64 {
            tx.send(frame(0, vec![vec![0.0; 16]], ConversionOutcome::HeldTail))
                .unwrap();
        }
        drop(tx);

        let (err, partial) = write_all(rx, &mut native, &mut converted).unwrap_err();

        assert!(
            matches!(err, hound::Error::IoError(ref e) if e.to_string().contains("disk full")),
            "{err}"
        );
        assert!(
            partial.frames_received >= 1,
            "the counts up to the failure come back with the error"
        );
    }

    #[test]
    fn writer_error_display_carries_every_partial_count() {
        let partial = WriterReport {
            frames_received: 9,
            first_seq: Some(0),
            last_seq: Some(8),
            seq_gaps: 1,
            produced: 7,
            held_tails: 1,
            conversion_failures: 1,
            malformed_frames: 0,
            native_frames_written: 43_200,
            converted_frames_written: 33_600,
        };
        let shown = FinishError::Writer {
            error: hound::Error::IoError(std::io::Error::other("disk full")),
            partial: Some(partial),
        }
        .to_string();
        for needle in [
            "disk full",
            "frames_received 9",
            "first_seq 0",
            "last_seq 8",
            "seq_gaps 1",
            "produced 7",
            "held_tails 1",
            "conversion_failures 1",
            "malformed_frames 0",
            "native_frames_written 43200",
            "converted_frames_written 33600",
        ] {
            assert!(shown.contains(needle), "{needle:?} missing from {shown:?}");
        }
        let lost = FinishError::Writer {
            error: hound::Error::IoError(std::io::Error::other("disk full")),
            partial: None,
        }
        .to_string();
        assert!(lost.contains("counts lost"), "{lost:?}");
    }

    #[test]
    fn spawn_removes_the_native_file_when_the_converted_open_fails() {
        let dir = tempfile::tempdir().unwrap();
        let native = dir.path().join("mic-native.wav");
        let converted = Path::new("/nonexistent-dir-for-heu-650/mic-converted.wav");
        let (_tx, rx) = sync_channel::<CharacterizationFrame>(1);

        assert!(CharacterizationWriter::spawn(rx, &stereo_48k(), &native, converted).is_err());

        assert!(
            !native.exists(),
            "a half-opened pair must not leave a stray native WAV"
        );
    }

    #[test]
    fn spawn_rejects_a_sample_rate_a_header_cannot_hold() {
        let dir = tempfile::tempdir().unwrap();
        let native = dir.path().join("mic-native.wav");
        let converted = dir.path().join("mic-converted.wav");
        for bad in [f64::NAN, 0.0, -48_000.0, 6.0e8, 1.0e12] {
            let (_tx, rx) = sync_channel::<CharacterizationFrame>(1);
            let format = NativeFormat {
                sample_rate: bad,
                ..stereo_48k()
            };
            assert!(
                CharacterizationWriter::spawn(rx, &format, &native, &converted).is_err(),
                "{bad} Hz must be refused, not saturated to 0"
            );
        }
    }

    /// A rate under the ceiling can still overflow bytes per second at a high
    /// channel count.
    #[test]
    fn spawn_rejects_a_bytes_per_second_that_overflows_u32() {
        let dir = tempfile::tempdir().unwrap();
        let native = dir.path().join("mic-native.wav");
        let converted = dir.path().join("mic-converted.wav");
        let (_tx, rx) = sync_channel::<CharacterizationFrame>(1);
        let format = NativeFormat {
            channels: 2_000,
            sample_rate: MAX_SAMPLE_RATE_HZ, // MAX_SAMPLE_RATE_HZ x 4 bytes x 2000 channels > u32::MAX
            ..stereo_48k()
        };
        let Err(err) = CharacterizationWriter::spawn(rx, &format, &native, &converted) else {
            panic!("a bytes-per-second overflow must be refused");
        };
        assert!(err.to_string().contains("bytes per second"), "{err}");
    }

    #[test]
    fn spawn_rejects_zero_channels() {
        let dir = tempfile::tempdir().unwrap();
        let native = dir.path().join("mic-native.wav");
        let converted = dir.path().join("mic-converted.wav");
        let (_tx, rx) = sync_channel::<CharacterizationFrame>(1);
        let format = NativeFormat {
            channels: 0,
            ..stereo_48k()
        };
        assert!(
            CharacterizationWriter::spawn(rx, &format, &native, &converted).is_err(),
            "zero channels would panic hound at finalize; refuse it at spawn"
        );
    }

    #[test]
    fn panic_message_reads_str_and_string_payloads() {
        let s: Box<dyn std::any::Any + Send> = Box::new("boom");
        assert_eq!(panic_message(&*s), "boom");
        let owned: Box<dyn std::any::Any + Send> = Box::new(String::from("owned boom"));
        assert_eq!(panic_message(&*owned), "owned boom");
        let other: Box<dyn std::any::Any + Send> = Box::new(7u8);
        assert_eq!(panic_message(&*other), "<non-string panic payload>");
    }

    #[test]
    fn check_room_refuses_a_write_past_the_data_limit() {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let writer = hound::WavWriter::new(std::io::Cursor::new(Vec::new()), spec).unwrap();
        let room = (WAV_DATA_LIMIT_BYTES / 4) as usize;
        assert!(check_room(&writer, room, "t").is_ok());
        assert!(check_room(&writer, room + 1, "t").is_err());
    }

    #[test]
    fn spawn_fails_when_the_output_directory_is_missing() {
        let (_tx, rx) = sync_channel::<CharacterizationFrame>(1);
        let missing = Path::new("/nonexistent-dir-for-heu-650/mic-native.wav");
        assert!(
            CharacterizationWriter::spawn(rx, &stereo_48k(), missing, missing).is_err(),
            "opening a WAV in a missing directory must fail at spawn, not at finish"
        );
    }

    fn sample_report() -> WriterReport {
        WriterReport {
            frames_received: 100,
            first_seq: Some(0),
            last_seq: Some(99),
            produced: 98,
            held_tails: 2,
            native_frames_written: 480_000,
            converted_frames_written: 479_300,
            ..WriterReport::default()
        }
    }

    fn sample_manifest<'a>(
        report: &'a WriterReport,
        drops: AudioDropSnapshot,
        format: &'a NativeFormat,
    ) -> Manifest<'a> {
        Manifest {
            device: "Blue Yeti",
            mode: "stereo",
            gain: "50%",
            phrase: "the quick brown fox jumps over the lazy dog",
            model_variant: "base",
            macos_version: "26.1",
            recorded_at_unix_secs: 1_757_100_000,
            requested_secs: 10,
            format,
            native_wav: Path::new("/tmp/take/mic-native.wav"),
            converted_wav: Path::new("/tmp/take/mic-converted.wav"),
            report,
            drops,
        }
    }

    #[test]
    fn manifest_renders_every_field_as_a_key_value_line() {
        let report = sample_report();
        let format = stereo_48k();
        let text = sample_manifest(&report, AudioDropSnapshot::default(), &format).render();

        for expected in [
            "device: Blue Yeti",
            "mode: stereo",
            "gain: 50%",
            "phrase: the quick brown fox jumps over the lazy dog",
            "model_variant: base",
            "attempts: 1",
            "macos_version: 26.1",
            "recorded_at_unix_secs: 1757100000",
            "requested_secs: 10",
            "recorded_secs: 10.00",
            "converted_secs: 9.99",
            "native_channels: 2",
            "native_sample_rate_hz: 48000",
            "native_interleaved: false",
            "native_common_format: f32",
            "native_wav: /tmp/take/mic-native.wav",
            "converted_wav: /tmp/take/mic-converted.wav",
            "frames_received: 100",
            "first_seq: 0",
            "last_seq: 99",
            "seq_gaps: 0",
            "produced: 98",
            "held_tails: 2",
            "conversion_failures: 0",
            "malformed_frames: 0",
            "native_frames_written: 480000",
            "converted_frames_written: 479300",
            "drops_mic_full: 0",
            "drops_mic_closed: 0",
            "drops_mic_convert_failed: 0",
            "measurement_valid: true",
        ] {
            assert!(text.contains(expected), "missing `{expected}` in:\n{text}");
        }
    }

    #[test]
    fn manifest_is_valid_only_with_no_gaps_no_failures_and_seq_from_zero() {
        let format = stereo_48k();
        let clean = sample_report();
        assert!(sample_manifest(&clean, AudioDropSnapshot::default(), &format).measurement_valid());

        let gap = WriterReport {
            seq_gaps: 1,
            ..sample_report()
        };
        assert!(!sample_manifest(&gap, AudioDropSnapshot::default(), &format).measurement_valid());

        let failed = WriterReport {
            conversion_failures: 1,
            ..sample_report()
        };
        assert!(
            !sample_manifest(&failed, AudioDropSnapshot::default(), &format).measurement_valid()
        );

        let late_start = WriterReport {
            first_seq: Some(3),
            ..sample_report()
        };
        assert!(
            !sample_manifest(&late_start, AudioDropSnapshot::default(), &format)
                .measurement_valid(),
            "a burst dropped before the first frame is a hole too"
        );

        let dropped = AudioDropSnapshot {
            mic_full: 1,
            ..AudioDropSnapshot::default()
        };
        assert!(!sample_manifest(&clean, dropped, &format).measurement_valid());

        let empty = WriterReport::default();
        assert!(
            !sample_manifest(&empty, AudioDropSnapshot::default(), &format).measurement_valid()
        );
    }
}
