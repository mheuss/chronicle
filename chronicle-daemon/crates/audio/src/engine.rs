//! AudioPipeline — encoding pipeline for audio capture.
//!
//! Creates an ObjC handler and dispatch queue for ScreenCaptureKit audio
//! callbacks, spawns an encoding thread, and delivers completed Opus
//! segments over an mpsc channel. Does not manage SCStream lifecycle --
//! the caller registers the handler on an externally managed stream.

use std::sync::mpsc;
use std::thread::{self, JoinHandle};

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_screen_capture_kit::SCStreamOutput;

use crate::accumulator::SegmentAccumulator;
use crate::handler::{AudioMessage, AudioOutputHandler};
use crate::{AudioConfig, AudioError, AudioSource, CompletedSegment, Result};

/// Audio encoding pipeline.
///
/// Owns the ObjC handler, dispatch queue, and encoding thread. External
/// callers register the handler on an `SCStream` via a borrowed
/// [`AudioHandlerToken`] obtained from [`token()`](Self::token). The
/// encoding thread converts raw PCM into Opus segments and sends them
/// over a channel.
///
/// The handler is the sole owner of the buffer sender channel. Calling
/// `stop()` drops the handler, which closes the channel and lets the
/// encoding thread flush and exit. The borrow checker prevents `stop()`
/// from being called while an `AudioHandlerToken` is outstanding.
pub struct AudioPipeline {
    handler: Option<Retained<AudioOutputHandler>>,
    queue: Retained<DispatchQueue>,
    encoding_thread: Option<JoinHandle<()>>,
}

/// Borrow-based handle for registering audio output on an `SCStream`.
///
/// Carries the handler, dispatch queue, and audio settings needed by
/// `chronicle-capture::CaptureEngine::start`. The token borrows from
/// `AudioPipeline`, so the pipeline cannot be mutably accessed (including
/// `stop()`) while a token is outstanding. The borrow checker enforces
/// handler-outlives-streams as a compile-time invariant.
pub struct AudioHandlerToken<'p> {
    handler: &'p Retained<AudioOutputHandler>,
    queue: &'p Retained<DispatchQueue>,
    pub sample_rate: u32,
    pub channel_count: u32,
    pub capture_microphone: bool,
}

impl<'p> AudioHandlerToken<'p> {
    /// Protocol-erased clone of the handler, suitable for
    /// `addStreamOutput_type_sampleHandlerQueue_error`.
    pub fn handler_clone(&self) -> Retained<ProtocolObject<dyn SCStreamOutput>> {
        ProtocolObject::from_retained(self.handler.clone())
    }

    /// Borrow of the dispatch queue shared with `SCStream`.
    pub fn queue(&self) -> &Retained<DispatchQueue> {
        self.queue
    }
}

impl AudioPipeline {
    /// Create a new audio pipeline.
    ///
    /// Spawns the encoding thread immediately. Returns the pipeline and
    /// a receiver for completed Opus segments. The caller obtains a
    /// borrow-based `AudioHandlerToken` via `token()` and passes it to
    /// `chronicle-capture::CaptureEngine::start` for SCStream registration.
    pub fn create(config: AudioConfig) -> Result<(Self, mpsc::Receiver<CompletedSegment>)> {
        let (segment_tx, segment_rx) = mpsc::channel::<CompletedSegment>();
        let (buffer_tx, buffer_rx) = mpsc::sync_channel::<AudioMessage>(64);

        let encoding_thread = spawn_encoding_thread(buffer_rx, segment_tx, config);

        let handler = AudioOutputHandler::new(buffer_tx);
        // DispatchQueue::new returns dispatch2::Queue, .into() converts to
        // Retained<DispatchQueue> for objc2 interop.
        let queue: Retained<DispatchQueue> =
            DispatchQueue::new("com.chronicle.audio.samples", None).into();

        let pipeline = Self {
            handler: Some(handler),
            queue,
            encoding_thread: Some(encoding_thread),
        };

        Ok((pipeline, segment_rx))
    }

    /// Borrow a token bundle for SCStream registration.
    ///
    /// Returns `None` after `stop()`. While the returned token is alive,
    /// `stop()` cannot be called — the borrow checker prevents it.
    pub fn token(
        &self,
        sample_rate: u32,
        channel_count: u32,
        capture_microphone: bool,
    ) -> Option<AudioHandlerToken<'_>> {
        let handler = self.handler.as_ref()?;
        Some(AudioHandlerToken {
            handler,
            queue: &self.queue,
            sample_rate,
            channel_count,
            capture_microphone,
        })
    }

    /// Stop the encoding pipeline.
    ///
    /// Drops the handler (and its buffer sender) so the encoding thread's
    /// `recv()` returns `Err`, which triggers a flush of any remaining
    /// samples. Blocks until the encoding thread exits.
    ///
    /// While an [`AudioHandlerToken`] is outstanding, this method cannot be
    /// called; the borrow checker enforces it.
    pub fn stop(&mut self) -> Result<()> {
        // Drop the handler to close the buffer sender channel.
        self.handler = None;

        if let Some(thread) = self.encoding_thread.take() {
            thread
                .join()
                .map_err(|_| AudioError::ScreenCaptureKit("encoding thread panicked".into()))?;
        }

        Ok(())
    }
}

impl Drop for AudioPipeline {
    fn drop(&mut self) {
        if self.handler.is_some() || self.encoding_thread.is_some() {
            log::warn!("AudioPipeline dropped without calling stop() — stopping now");
            if let Err(e) = self.stop() {
                log::error!("AudioPipeline::stop() failed in drop: {e}");
            }
        }
    }
}

/// Spawn the encoding thread that reads AudioMessages and dispatches
/// them to the appropriate SegmentAccumulator.
fn spawn_encoding_thread(
    buffer_rx: mpsc::Receiver<AudioMessage>,
    segment_tx: mpsc::Sender<CompletedSegment>,
    config: AudioConfig,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("audio-encoder".into())
        .spawn(move || {
            run_encoding_loop(buffer_rx, segment_tx, &config);
        })
        .expect("failed to spawn audio encoding thread")
}

/// The encoding loop. Reads messages, dispatches buffers to accumulators,
/// handles flush requests, and flushes on shutdown.
fn run_encoding_loop(
    buffer_rx: mpsc::Receiver<AudioMessage>,
    segment_tx: mpsc::Sender<CompletedSegment>,
    config: &AudioConfig,
) {
    let mut mic_acc = SegmentAccumulator::new(
        AudioSource::Microphone,
        crate::SAMPLE_RATE,
        config.segment_duration_secs,
        config.bitrate,
        opus::Application::Voip, // Speech-optimized for mic
        &config.output_dir,
        segment_tx.clone(),
    );

    let mut sys_acc = SegmentAccumulator::new(
        AudioSource::System,
        crate::SAMPLE_RATE,
        config.segment_duration_secs,
        config.bitrate,
        opus::Application::Audio, // General audio for system output
        &config.output_dir,
        segment_tx,
    );

    // Process messages until the channel disconnects.
    while let Ok(msg) = buffer_rx.recv() {
        match msg {
            AudioMessage::Buffer(buf) => {
                let acc = match buf.source {
                    AudioSource::Microphone => &mut mic_acc,
                    AudioSource::System => &mut sys_acc,
                };
                if let Err(e) = acc.push(&buf.samples, buf.timestamp_ms) {
                    log::error!("encoding error for {}: {e}", buf.source.as_str());
                }
            }
            AudioMessage::FlushMic => {
                if let Err(e) = mic_acc.flush() {
                    log::error!("failed to flush mic accumulator on request: {e}");
                }
            }
        }
    }

    // Flush remaining samples from both accumulators.
    if let Err(e) = mic_acc.flush() {
        log::error!("failed to flush mic accumulator: {e}");
    }
    if let Err(e) = sys_acc.flush() {
        log::error!("failed to flush system accumulator: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::AudioBuffer;

    #[test]
    fn create_returns_pipeline_and_segment_receiver() {
        let dir = tempfile::tempdir().unwrap();
        let config = AudioConfig {
            segment_duration_secs: 30,
            bitrate: 64_000,
            output_dir: dir.path().to_path_buf(),
        };
        let (pipeline, _segment_rx) = AudioPipeline::create(config).unwrap();
        let _token = pipeline
            .token(48_000, 1, false)
            .expect("token available before stop");
    }

    #[test]
    fn encoding_loop_dispatches_buffers_to_correct_accumulator() {
        let dir = tempfile::tempdir().unwrap();
        let config = AudioConfig {
            segment_duration_secs: 1,
            bitrate: 64_000,
            output_dir: dir.path().to_path_buf(),
        };

        let (segment_tx, segment_rx) = mpsc::channel::<CompletedSegment>();
        let (buffer_tx, buffer_rx) = mpsc::sync_channel::<AudioMessage>(64);

        // Spawn the encoding loop in a thread.
        let config_clone = config.clone();
        let handle = thread::spawn(move || {
            run_encoding_loop(buffer_rx, segment_tx, &config_clone);
        });

        let timestamp = 1_700_000_000_000_i64;

        // Send enough mic samples for one full segment (1 second at 48kHz).
        let mic_samples = vec![0.0_f32; 48_000];
        buffer_tx
            .send(AudioMessage::Buffer(AudioBuffer {
                samples: mic_samples,
                timestamp_ms: timestamp,
                source: AudioSource::Microphone,
            }))
            .unwrap();

        // Send enough system samples for one full segment.
        let sys_samples = vec![0.0_f32; 48_000];
        buffer_tx
            .send(AudioMessage::Buffer(AudioBuffer {
                samples: sys_samples,
                timestamp_ms: timestamp,
                source: AudioSource::System,
            }))
            .unwrap();

        // Drop sender to signal shutdown.
        drop(buffer_tx);

        // Wait for the encoding thread to finish.
        handle.join().unwrap();

        // Collect all completed segments.
        let mut mic_segments = Vec::new();
        let mut sys_segments = Vec::new();
        while let Ok(seg) = segment_rx.try_recv() {
            match seg.source {
                AudioSource::Microphone => mic_segments.push(seg),
                AudioSource::System => sys_segments.push(seg),
            }
        }

        assert_eq!(mic_segments.len(), 1, "expected 1 mic segment");
        assert_eq!(sys_segments.len(), 1, "expected 1 system segment");

        // Verify file paths exist.
        assert!(
            mic_segments[0].path.exists(),
            "mic segment file should exist"
        );
        assert!(
            sys_segments[0].path.exists(),
            "system segment file should exist"
        );
    }

    #[test]
    fn encoding_loop_flushes_partial_segments_on_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let config = AudioConfig {
            segment_duration_secs: 1,
            bitrate: 64_000,
            output_dir: dir.path().to_path_buf(),
        };

        let (segment_tx, segment_rx) = mpsc::channel::<CompletedSegment>();
        let (buffer_tx, buffer_rx) = mpsc::sync_channel::<AudioMessage>(64);

        let config_clone = config.clone();
        let handle = thread::spawn(move || {
            run_encoding_loop(buffer_rx, segment_tx, &config_clone);
        });

        let timestamp = 1_700_000_000_000_i64;

        // Send half a segment of mic samples.
        let half_samples = vec![0.0_f32; 24_000];
        buffer_tx
            .send(AudioMessage::Buffer(AudioBuffer {
                samples: half_samples,
                timestamp_ms: timestamp,
                source: AudioSource::Microphone,
            }))
            .unwrap();

        // Drop sender to trigger shutdown + flush.
        drop(buffer_tx);
        handle.join().unwrap();

        let mut segments: Vec<CompletedSegment> = Vec::new();
        while let Ok(seg) = segment_rx.try_recv() {
            segments.push(seg);
        }

        assert_eq!(segments.len(), 1, "expected 1 partial segment from flush");
        assert_eq!(segments[0].source, AudioSource::Microphone);
        assert!(segments[0].path.exists(), "flushed file should exist");
    }

    #[test]
    fn flush_request_finalizes_partial_mic_segment() {
        let dir = tempfile::tempdir().unwrap();
        let config = AudioConfig {
            segment_duration_secs: 30,
            bitrate: 64_000,
            output_dir: dir.path().to_path_buf(),
        };

        let (segment_tx, segment_rx) = mpsc::channel::<CompletedSegment>();
        let (buffer_tx, buffer_rx) = mpsc::sync_channel::<AudioMessage>(64);

        let config_clone = config.clone();
        let handle = thread::spawn(move || {
            run_encoding_loop(buffer_rx, segment_tx, &config_clone);
        });

        let timestamp = 1_700_000_000_000_i64;

        // Send half a second of mic samples — well below the 30s boundary.
        let partial_samples = vec![0.0_f32; 24_000];
        buffer_tx
            .send(AudioMessage::Buffer(AudioBuffer {
                samples: partial_samples,
                timestamp_ms: timestamp,
                source: AudioSource::Microphone,
            }))
            .unwrap();

        // Request a mic flush. This must finalize the partial segment now,
        // ordered after the buffer above.
        buffer_tx.send(AudioMessage::FlushMic).unwrap();

        // Assert the segment arrives while the sender is still alive, so a
        // channel-close shutdown flush cannot be the cause.
        let segment = segment_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("FlushMic should finalize the partial mic segment");
        assert_eq!(segment.source, AudioSource::Microphone);
        assert_eq!(segment.start_timestamp, timestamp);
        assert_eq!(segment.end_timestamp, timestamp + 500);
        assert!(segment.path.exists(), "flushed mic file should exist");

        // Now drop the sender and join the encoding thread.
        drop(buffer_tx);
        handle.join().unwrap();

        // No further segments — the buffer was fully drained by FlushMic.
        assert!(
            segment_rx.try_recv().is_err(),
            "no extra segment expected from shutdown flush"
        );
    }

    #[test]
    fn encoding_loop_exits_cleanly_with_no_buffers() {
        let dir = tempfile::tempdir().unwrap();
        let config = AudioConfig {
            segment_duration_secs: 1,
            bitrate: 64_000,
            output_dir: dir.path().to_path_buf(),
        };

        let (segment_tx, segment_rx) = mpsc::channel::<CompletedSegment>();
        let (buffer_tx, buffer_rx) = mpsc::sync_channel::<AudioMessage>(64);

        let config_clone = config.clone();
        let handle = thread::spawn(move || {
            run_encoding_loop(buffer_rx, segment_tx, &config_clone);
        });

        // Drop sender immediately — no buffers sent.
        drop(buffer_tx);
        handle.join().unwrap();

        // No segments should have been produced.
        assert!(
            segment_rx.try_recv().is_err(),
            "no segments expected when no buffers sent"
        );
    }

    #[test]
    fn stop_shuts_down_encoding_thread() {
        let dir = tempfile::tempdir().unwrap();
        let config = AudioConfig {
            segment_duration_secs: 30,
            bitrate: 64_000,
            output_dir: dir.path().to_path_buf(),
        };
        let (mut pipeline, _segment_rx) = AudioPipeline::create(config).unwrap();

        // Stopping should cleanly shut down the encoding thread.
        let result = pipeline.stop();
        assert!(result.is_ok());
    }

    #[test]
    fn token_exposes_audio_settings() {
        let dir = tempfile::tempdir().unwrap();
        let config = AudioConfig {
            segment_duration_secs: 30,
            bitrate: 64_000,
            output_dir: dir.path().to_path_buf(),
        };
        let (pipeline, _rx) = AudioPipeline::create(config).unwrap();
        let token = pipeline.token(48_000, 1, false).expect("token available");
        assert_eq!(token.sample_rate, 48_000);
        assert_eq!(token.channel_count, 1);
        assert!(!token.capture_microphone);
    }

    #[test]
    fn token_is_none_after_stop() {
        let dir = tempfile::tempdir().unwrap();
        let config = AudioConfig {
            segment_duration_secs: 30,
            bitrate: 64_000,
            output_dir: dir.path().to_path_buf(),
        };
        let (mut pipeline, _rx) = AudioPipeline::create(config).unwrap();
        pipeline.stop().unwrap();
        assert!(pipeline.token(48_000, 1, false).is_none());
    }
}
