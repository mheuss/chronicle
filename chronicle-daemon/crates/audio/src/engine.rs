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
use crate::handler::{AudioBuffer, AudioOutputHandler};
use crate::{AudioConfig, AudioError, AudioSource, CompletedSegment, Result};

/// Audio encoding pipeline.
///
/// Owns the ObjC handler, dispatch queue, and encoding thread. The handler
/// and queue are exposed for external registration on an SCStream. The
/// encoding thread converts raw PCM into Opus segments and sends them
/// over a channel.
///
/// The handler is the sole owner of the buffer sender channel. Calling
/// `stop()` drops the handler, which closes the channel and lets the
/// encoding thread flush and exit. If the caller holds a `Retained`
/// clone from `handler()`, they must drop it before `stop()` for the
/// encoding thread to exit.
pub struct AudioPipeline {
    handler: Option<Retained<AudioOutputHandler>>,
    queue: Retained<DispatchQueue>,
    encoding_thread: Option<JoinHandle<()>>,
}

impl AudioPipeline {
    /// Create a new audio pipeline.
    ///
    /// Spawns the encoding thread immediately. Returns the pipeline and
    /// a receiver for completed Opus segments. The caller should retrieve
    /// the handler and queue via `handler()` and `queue()`, then register
    /// them on an SCStream.
    pub fn create(config: AudioConfig) -> Result<(Self, mpsc::Receiver<CompletedSegment>)> {
        let (segment_tx, segment_rx) = mpsc::channel::<CompletedSegment>();
        let (buffer_tx, buffer_rx) = mpsc::sync_channel::<AudioBuffer>(64);

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

    /// Protocol-erased handler for registering on an SCStream.
    ///
    /// Returns `None` after `stop()` has been called.
    pub fn handler(&self) -> Option<Retained<ProtocolObject<dyn SCStreamOutput>>> {
        self.handler
            .as_ref()
            .map(|h| ProtocolObject::from_retained(h.clone()))
    }

    /// Dispatch queue for audio sample delivery.
    pub fn queue(&self) -> Retained<DispatchQueue> {
        self.queue.clone()
    }

    /// Stop the encoding pipeline.
    ///
    /// Drops the handler (and its buffer sender) so the encoding thread's
    /// `recv()` returns `Err`, which triggers a flush of any remaining
    /// samples. Blocks until the encoding thread exits.
    ///
    /// The caller must drop any `Retained` references from `handler()`
    /// before calling this, otherwise the handler stays alive and the
    /// encoding thread won't exit.
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

/// Spawn the encoding thread that reads AudioBuffers and dispatches
/// them to the appropriate SegmentAccumulator.
fn spawn_encoding_thread(
    buffer_rx: mpsc::Receiver<AudioBuffer>,
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

/// The encoding loop. Reads buffers, dispatches to accumulators,
/// and flushes on shutdown.
fn run_encoding_loop(
    buffer_rx: mpsc::Receiver<AudioBuffer>,
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

    // Process buffers until the channel disconnects.
    while let Ok(buf) = buffer_rx.recv() {
        let acc = match buf.source {
            AudioSource::Microphone => &mut mic_acc,
            AudioSource::System => &mut sys_acc,
        };
        if let Err(e) = acc.push(&buf.samples, buf.timestamp_ms) {
            log::error!("encoding error for {}: {e}", buf.source.as_str());
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

    #[test]
    fn create_returns_pipeline_and_segment_receiver() {
        let dir = tempfile::tempdir().unwrap();
        let config = AudioConfig {
            segment_duration_secs: 30,
            bitrate: 64_000,
            output_dir: dir.path().to_path_buf(),
        };
        let (pipeline, _segment_rx) = AudioPipeline::create(config).unwrap();
        // Handler and queue should be available
        let _handler = pipeline.handler().expect("handler should be Some before stop");
        let _queue = pipeline.queue();
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
        let (buffer_tx, buffer_rx) = mpsc::sync_channel::<AudioBuffer>(64);

        // Spawn the encoding loop in a thread.
        let config_clone = config.clone();
        let handle = thread::spawn(move || {
            run_encoding_loop(buffer_rx, segment_tx, &config_clone);
        });

        let timestamp = 1_700_000_000_000_i64;

        // Send enough mic samples for one full segment (1 second at 48kHz).
        let mic_samples = vec![0.0_f32; 48_000];
        buffer_tx
            .send(AudioBuffer {
                samples: mic_samples,
                timestamp_ms: timestamp,
                source: AudioSource::Microphone,
            })
            .unwrap();

        // Send enough system samples for one full segment.
        let sys_samples = vec![0.0_f32; 48_000];
        buffer_tx
            .send(AudioBuffer {
                samples: sys_samples,
                timestamp_ms: timestamp,
                source: AudioSource::System,
            })
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
        let (buffer_tx, buffer_rx) = mpsc::sync_channel::<AudioBuffer>(64);

        let config_clone = config.clone();
        let handle = thread::spawn(move || {
            run_encoding_loop(buffer_rx, segment_tx, &config_clone);
        });

        let timestamp = 1_700_000_000_000_i64;

        // Send half a segment of mic samples.
        let half_samples = vec![0.0_f32; 24_000];
        buffer_tx
            .send(AudioBuffer {
                samples: half_samples,
                timestamp_ms: timestamp,
                source: AudioSource::Microphone,
            })
            .unwrap();

        // Drop sender to trigger shutdown + flush.
        drop(buffer_tx);
        handle.join().unwrap();

        let mut segments: Vec<CompletedSegment> = Vec::new();
        while let Ok(seg) = segment_rx.try_recv() {
            segments.push(seg);
        }

        assert_eq!(
            segments.len(),
            1,
            "expected 1 partial segment from flush"
        );
        assert_eq!(segments[0].source, AudioSource::Microphone);
        assert!(segments[0].path.exists(), "flushed file should exist");
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
        let (buffer_tx, buffer_rx) = mpsc::sync_channel::<AudioBuffer>(64);

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
    fn handler_returns_none_after_stop() {
        let dir = tempfile::tempdir().unwrap();
        let config = AudioConfig {
            segment_duration_secs: 30,
            bitrate: 64_000,
            output_dir: dir.path().to_path_buf(),
        };
        let (mut pipeline, _segment_rx) = AudioPipeline::create(config).unwrap();

        assert!(pipeline.handler().is_some(), "handler should exist before stop");
        pipeline.stop().unwrap();
        assert!(pipeline.handler().is_none(), "handler should be None after stop");
    }
}
