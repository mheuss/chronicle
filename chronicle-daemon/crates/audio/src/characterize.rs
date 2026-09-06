//! Feature-gated microphone characterization (HEU-650).
//!
//! Records what the input-node tap receives and what `AVAudioConverter`
//! produces from it, as two 32-bit float WAV files, for offline analysis. The
//! audio thread only fills a [`CharacterizationFrame`] and `try_send`s it; a
//! plain thread writes the files (ADR-013). Compiled only with
//! `--features characterize`.

use std::fmt;
use std::io;
use std::path::Path;
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::thread::JoinHandle;
use std::time::Duration;

use hound::{SampleFormat, WavSpec, WavWriter};

use crate::SAMPLE_RATE;
use crate::microphone::{ConversionOutcome, NativeFormat};

/// One tap callback's worth of data: both sides of the conversion for the same
/// input buffer, so the writer never has to pair two streams.
#[derive(Debug, Clone, PartialEq)]
pub struct CharacterizationFrame {
    /// Monotonic per-session counter, assigned in the callback. A gap means a
    /// frame was dropped between the tap and the writer.
    pub seq: u64,
    /// Native input, one `Vec` per channel, already stride-corrected. Every
    /// channel has the same length.
    ///
    /// Empty means the callback got a buffer with frames it could not read as
    /// f32 planes. The `seq` was still consumed. Treat such a frame as
    /// malformed, never as silence: it is a hole in the recording.
    pub native: Vec<Vec<f32>>,
    /// The converter's result for this same buffer.
    pub outcome: ConversionOutcome,
}

/// What the writer saw, reported once when the channel closes.
///
/// `seq_gaps` counts frames missing *between* `first_seq` and `last_seq`. A
/// burst dropped before the first frame arrived shows up as `first_seq > 0`,
/// not as a gap, so the manifest checks both.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WriterReport {
    pub frames_received: u64,
    pub first_seq: Option<u64>,
    pub last_seq: Option<u64>,
    pub seq_gaps: u64,
    pub produced: u64,
    pub held_tails: u64,
    pub conversion_failures: u64,
    /// Frames whose channel count or channel lengths disagreed with the
    /// format the WAV was opened with. Not written; never expected.
    pub malformed_frames: u64,
    pub native_frames_written: u64,
    pub converted_frames_written: u64,
}

/// Why [`CharacterizationWriter::finish`] did not return a report.
#[derive(Debug)]
pub enum FinishError {
    /// The writer thread reported a WAV or I/O error.
    Writer(hound::Error),
    /// The channel did not close within the timeout. Some sender is still
    /// alive, most likely a `MicrophoneCapture` that was not dropped before
    /// the caller's own sender.
    Timeout(Duration),
}

impl fmt::Display for FinishError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Writer(e) => write!(f, "characterization writer failed: {e}"),
            Self::Timeout(d) => write!(
                f,
                "characterization writer did not finish within {d:?}: a sender is still alive"
            ),
        }
    }
}

impl std::error::Error for FinishError {}

/// Owns the thread that turns frames into two WAV files.
///
/// The thread ends when every `SyncSender<CharacterizationFrame>` is dropped.
/// `Receiver::recv` returns `Err` only after every queued frame has been
/// delivered, so there is no separate drain step.
pub struct CharacterizationWriter {
    handle: JoinHandle<()>,
    completion_rx: Receiver<Result<WriterReport, hound::Error>>,
}

impl CharacterizationWriter {
    /// Open both WAV files and start the writer thread.
    ///
    /// `mic-native.wav` gets `format.channels` channels at the device's
    /// native rate; `mic-converted.wav` gets one channel at 48 kHz. Both are
    /// 32-bit float. Opening happens here so a bad path fails before any
    /// audio is recorded.
    pub fn spawn(
        frames: Receiver<CharacterizationFrame>,
        format: &NativeFormat,
        native_path: &Path,
        converted_path: &Path,
    ) -> Result<Self, hound::Error> {
        let channels = u16::try_from(format.channels).map_err(|_| {
            hound::Error::IoError(io::Error::other(format!(
                "{} channels does not fit a WAV header",
                format.channels
            )))
        })?;
        let native_spec = WavSpec {
            channels,
            sample_rate: format.sample_rate.round() as u32,
            bits_per_sample: 32,
            sample_format: SampleFormat::Float,
        };
        let converted_spec = WavSpec {
            channels: 1,
            sample_rate: SAMPLE_RATE,
            bits_per_sample: 32,
            sample_format: SampleFormat::Float,
        };
        let mut native = WavWriter::create(native_path, native_spec)?;
        let mut converted = WavWriter::create(converted_path, converted_spec)?;

        let (completion_tx, completion_rx) = channel();
        let handle = std::thread::Builder::new()
            .name("mic-characterization-writer".into())
            .spawn(move || {
                let result = write_all(frames, &mut native, &mut converted).and_then(|report| {
                    native.finalize()?;
                    converted.finalize()?;
                    Ok(report)
                });
                // If `finish` already timed out the receiver is gone, and
                // there is nobody left to tell.
                let _ = completion_tx.send(result);
            })?;

        Ok(Self {
            handle,
            completion_rx,
        })
    }

    /// Wait up to `timeout` for the channel to close and the files to
    /// finalize, then return the report.
    ///
    /// `JoinHandle::join` has no timeout, so the bound comes from the
    /// completion channel. On timeout the thread is left running; the caller
    /// still owns a sender and can only exit.
    pub fn finish(self, timeout: Duration) -> Result<WriterReport, FinishError> {
        match self.completion_rx.recv_timeout(timeout) {
            Ok(Ok(report)) => {
                let _ = self.handle.join();
                Ok(report)
            }
            Ok(Err(e)) => {
                let _ = self.handle.join();
                Err(FinishError::Writer(e))
            }
            Err(RecvTimeoutError::Timeout) => Err(FinishError::Timeout(timeout)),
            Err(RecvTimeoutError::Disconnected) => {
                let _ = self.handle.join();
                Err(FinishError::Writer(hound::Error::IoError(
                    io::Error::other("the writer thread exited without reporting"),
                )))
            }
        }
    }
}

/// The writer loop. Generic over the sink so a test can hand it a failing
/// one; the round-trip tests use real files to exercise `finalize`.
fn write_all<W: io::Write + io::Seek>(
    frames: Receiver<CharacterizationFrame>,
    native: &mut WavWriter<W>,
    converted: &mut WavWriter<W>,
) -> Result<WriterReport, hound::Error> {
    let channels = usize::from(native.spec().channels);
    let mut report = WriterReport::default();

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
            // Frame-major, channel-minor: a multichannel WAV interleaves
            // frames, and `native` is planar. The channel-major loop is the
            // one the data structure invites; it produces a file that opens
            // fine and analyses wrong.
            let frames_in = frame.native[0].len();
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

    Ok(report)
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
        // Frame-major: L0 R0 L1 R1 L2 R2. A channel-major loop would produce
        // 1 2 3 10 20 30 and open without error.
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

    /// The shutdown trap: `finish` must not return until every sender is
    /// gone, and must return promptly once they are.
    #[test]
    fn writer_finishes_only_after_the_last_sender_drops() {
        let s = session(&stereo_48k());
        let late = s.tx.clone();
        drop(s.tx);
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(late);
        });

        let started = Instant::now();
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

    /// A sink that accepts the WAV header and then refuses everything, so
    /// the write error surfaces inside `write_all` and comes back as `Err`.
    /// `finish` maps that `Err` to `FinishError::Writer` in one arm; this is
    /// the half that has logic in it.
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
        // Unbounded: every frame is queued before `write_all` runs, and a
        // bounded channel would block the sends with nobody draining.
        let (tx, rx) = std::sync::mpsc::channel();
        for _ in 0..64 {
            tx.send(frame(0, vec![vec![0.0; 16]], ConversionOutcome::HeldTail))
                .unwrap();
        }
        drop(tx);

        let err = write_all(rx, &mut native, &mut converted).unwrap_err();

        assert!(
            matches!(err, hound::Error::IoError(ref e) if e.to_string().contains("disk full")),
            "{err}"
        );
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
}
