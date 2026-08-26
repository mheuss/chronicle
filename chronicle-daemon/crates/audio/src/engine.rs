//! AudioPipeline — encoding pipeline for audio capture.
//!
//! Spawns an encoding thread and delivers completed Opus segments over an
//! mpsc channel. System audio arrives through an ObjC handler the caller
//! registers on an externally managed ScreenCaptureKit stream; the
//! microphone arrives through a dedicated `AVAudioEngine` capture path that
//! the pipeline owns and toggles via [`set_microphone_enabled`]. The
//! pipeline does not manage SCStream lifecycle.
//!
//! [`set_microphone_enabled`]: AudioPipeline::set_microphone_enabled

use std::sync::Arc;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_screen_capture_kit::SCStreamOutput;

use crate::accumulator::SegmentAccumulator;
use crate::handler::{AudioMessage, AudioOutputHandler};
use crate::microphone::MicrophoneCapture;
use crate::{AudioConfig, AudioDropCounters, AudioError, AudioSource, CompletedSegment, Result};

/// Outcome of a microphone toggle. The microphone is a soft feature — a
/// failure never breaks the encoding pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MicToggleOutcome {
    Enabled,
    Disabled,
    Failed { reason: String },
}

/// Audio encoding pipeline.
///
/// Owns the ObjC handler, dispatch queue, microphone capture path, and
/// encoding thread. External callers register the handler on an `SCStream`
/// via a borrowed [`AudioHandlerToken`] obtained from [`token()`](Self::token).
/// The encoding thread converts raw PCM into Opus segments and sends them
/// over a channel.
///
/// The encoding channel has three senders — the ObjC handler (system audio),
/// the [`MicrophoneCapture`] tap, and `flush_tx`. Calling `stop()` drops all
/// three, which closes the channel and lets the encoding thread flush and
/// exit. The borrow checker prevents `stop()` from being called while an
/// `AudioHandlerToken` is outstanding.
pub struct AudioPipeline {
    handler: Option<Retained<AudioOutputHandler>>,
    queue: Retained<DispatchQueue>,
    encoding_thread: Option<JoinHandle<()>>,
    /// Dedicated microphone capture path. `None` if AVAudioEngine setup failed
    /// at `create` — the microphone is a soft feature.
    microphone: Option<MicrophoneCapture>,
    /// A clone of the encoding-channel sender, kept so a disable can send
    /// `AudioMessage::FlushMic`. `None` after `stop()`.
    flush_tx: Option<mpsc::SyncSender<AudioMessage>>,
    /// Buffers the real-time callbacks discarded, shared with the handler and
    /// the microphone tap. Created here and never rebuilt, so the counts are
    /// monotonic for the life of the process.
    drop_counters: Arc<AudioDropCounters>,
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
    /// Shared handle to the real-time drop counters.
    ///
    /// Cloneable so the daemon's reporter can keep reading after the pipeline
    /// is stopped and dropped — the final shutdown report depends on that.
    pub fn drop_counters(&self) -> Arc<AudioDropCounters> {
        Arc::clone(&self.drop_counters)
    }

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

        // The encoding channel now has three senders: the ObjC handler, the
        // microphone tap, and `flush_tx`. Clone for the first two; keep the
        // original as `flush_tx`.
        let drop_counters = Arc::new(AudioDropCounters::default());

        let handler = AudioOutputHandler::new(buffer_tx.clone(), Arc::clone(&drop_counters));

        // Build the microphone path eagerly. AVAudioEngine setup is plain-
        // thread-safe and does not touch the microphone, so a `&self` toggle
        // needs no interior mutability. A setup failure is soft: store `None`
        // and the mic stays unavailable.
        let microphone = match MicrophoneCapture::new(buffer_tx.clone()) {
            Ok(mic) => Some(mic),
            Err(e) => {
                log::warn!("microphone capture unavailable: {e}");
                None
            }
        };

        // DispatchQueue::new returns dispatch2::Queue, .into() converts to
        // Retained<DispatchQueue> for objc2 interop.
        let queue: Retained<DispatchQueue> =
            DispatchQueue::new("com.chronicle.audio.samples", None).into();

        let pipeline = Self {
            handler: Some(handler),
            queue,
            encoding_thread: Some(encoding_thread),
            microphone,
            flush_tx: Some(buffer_tx),
            drop_counters,
        };

        Ok((pipeline, segment_rx))
    }

    /// Borrow a token bundle for SCStream registration.
    ///
    /// Returns `None` after `stop()`. While the returned token is alive,
    /// `stop()` cannot be called — the borrow checker prevents it.
    pub fn token(&self, sample_rate: u32, channel_count: u32) -> Option<AudioHandlerToken<'_>> {
        let handler = self.handler.as_ref()?;
        Some(AudioHandlerToken {
            handler,
            queue: &self.queue,
            sample_rate,
            channel_count,
        })
    }

    /// Toggle microphone capture. Takes `&self` (not `&mut self`): a running
    /// `CaptureEngine` holds an `AudioHandlerToken` that immutably borrows this
    /// pipeline for its whole life, so a `&mut self` method would not compile.
    /// Every operation here is `&self` — `MicrophoneCapture`'s methods and
    /// `SyncSender::try_send` both take `&self`. See design §2.3.
    pub fn set_microphone_enabled(&self, enabled: bool) -> MicToggleOutcome {
        let Some(mic) = self.microphone.as_ref() else {
            // No mic path (AVAudioEngine setup failed). The microphone is not
            // capturing, so a disable is a satisfied no-op; only an enable fails.
            return if enabled {
                MicToggleOutcome::Failed {
                    reason: "microphone path unavailable".into(),
                }
            } else {
                MicToggleOutcome::Disabled
            };
        };
        // Idempotent — single fact: the engine is running or it is not.
        if enabled && mic.is_running() {
            return MicToggleOutcome::Enabled;
        }
        if !enabled && !mic.is_running() {
            return MicToggleOutcome::Disabled;
        }
        if enabled {
            match mic.start() {
                Ok(()) => MicToggleOutcome::Enabled,
                Err(e) => MicToggleOutcome::Failed {
                    reason: e.to_string(),
                },
            }
        } else {
            match mic.stop() {
                Ok(()) => {
                    // Finalize the partial mic segment so a later re-enable does
                    // not produce one segment spanning the off-period.
                    if let Some(tx) = &self.flush_tx
                        && let Err(e) = tx.send(AudioMessage::FlushMic)
                    {
                        // FlushMic is a control message, not an audio buffer —
                        // it must not be dropped under backpressure, so this
                        // blocks for a channel slot rather than `try_send`. A
                        // send error means the encoding channel is closed,
                        // which is a genuine failure.
                        return MicToggleOutcome::Failed {
                            reason: format!("failed to finalize microphone segment: {e}"),
                        };
                    }
                    MicToggleOutcome::Disabled
                }
                Err(e) => MicToggleOutcome::Failed {
                    reason: e.to_string(),
                },
            }
        }
    }

    /// Finalize the system accumulator's partial segment now. Sent when capture
    /// stops (system sleep or IPC pause; display sleep is HEU-496) so a
    /// later restart does not
    /// produce one segment spanning the off-period. Symmetric with the `FlushMic`
    /// send inside `set_microphone_enabled`.
    ///
    /// Blocking send — `FlushSystem` is a control message that must not be
    /// dropped under backpressure. A send error means the encoding channel is
    /// closed, which is a genuine failure. `Ok` no-op after `stop()`.
    pub fn flush_system_segment(&self) -> Result<()> {
        if let Some(tx) = &self.flush_tx {
            tx.send(AudioMessage::FlushSystem).map_err(|e| {
                AudioError::ScreenCaptureKit(format!("failed to flush system segment: {e}"))
            })?;
        }
        Ok(())
    }

    /// Stop the encoding pipeline.
    ///
    /// Stops the microphone first so the OS releases it synchronously, then
    /// drops every encoding-channel sender — the microphone tap, `flush_tx`,
    /// and the handler — so the encoding thread's `recv()` returns `Err`,
    /// which triggers a flush of any remaining samples. Blocks until the
    /// encoding thread exits.
    ///
    /// While an [`AudioHandlerToken`] is outstanding, this method cannot be
    /// called; the borrow checker enforces it.
    pub fn stop(&mut self) -> Result<()> {
        // Stop the microphone engine so the OS microphone is released
        // synchronously. Best-effort — a stop error must not block shutdown.
        if let Some(mic) = &self.microphone {
            let _ = mic.stop();
        }

        // Drop every encoding-channel sender so the encoding thread can exit.
        // If any sender survived, `recv()` would never return `Err`.
        self.microphone = None;
        self.flush_tx = None;
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
            AudioMessage::FlushSystem => {
                if let Err(e) = sys_acc.flush() {
                    log::error!("failed to flush system accumulator on request: {e}");
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
    fn drop_counters_handle_shares_one_allocation() {
        let dir = tempfile::tempdir().unwrap();
        let config = AudioConfig {
            segment_duration_secs: 30,
            bitrate: 64_000,
            output_dir: dir.path().to_path_buf(),
        };
        let (pipeline, _rx) = AudioPipeline::create(config).unwrap();

        // Every handle must reach the same counters, or the reporter would
        // read a set nothing writes to.
        let counters = pipeline.drop_counters();
        counters
            .system_full
            .fetch_add(2, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(pipeline.drop_counters().snapshot().system_full, 2);
        assert!(Arc::ptr_eq(&counters, &pipeline.drop_counters()));
    }

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
            .token(48_000, 1)
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
    fn flush_request_finalizes_partial_system_segment() {
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

        // Send half a second of system samples — well below the 30s boundary.
        let partial_samples = vec![0.0_f32; 24_000];
        buffer_tx
            .send(AudioMessage::Buffer(AudioBuffer {
                samples: partial_samples,
                timestamp_ms: timestamp,
                source: AudioSource::System,
            }))
            .unwrap();

        // Request a system flush. This must finalize the partial segment now,
        // ordered after the buffer above.
        buffer_tx.send(AudioMessage::FlushSystem).unwrap();

        // Assert the segment arrives while the sender is still alive, so a
        // channel-close shutdown flush cannot be the cause.
        let segment = segment_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("FlushSystem should finalize the partial system segment");
        assert_eq!(segment.source, AudioSource::System);
        assert_eq!(segment.start_timestamp, timestamp);
        assert_eq!(segment.end_timestamp, timestamp + 500);
        assert!(segment.path.exists(), "flushed system file should exist");

        // Now drop the sender and join the encoding thread.
        drop(buffer_tx);
        handle.join().unwrap();

        // No further segments — the buffer was fully drained by FlushSystem.
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
        let token = pipeline.token(48_000, 1).expect("token available");
        assert_eq!(token.sample_rate, 48_000);
        assert_eq!(token.channel_count, 1);
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
        assert!(pipeline.token(48_000, 1).is_none());
    }

    /// Build a pipeline whose `microphone` field is `None` — the soft-failure
    /// state a pipeline lands in when `AVAudioEngine` setup fails at `create`.
    ///
    /// Goes through the real `create` so `queue`, `encoding_thread`, and
    /// `flush_tx` are all correctly initialized, then overrides only the one
    /// field these tests need to control. A struct literal would have to
    /// hand-build the queue and encoding thread, duplicating `create`.
    fn pipeline_without_mic() -> AudioPipeline {
        let dir = tempfile::tempdir().unwrap();
        let config = AudioConfig {
            segment_duration_secs: 30,
            bitrate: 64_000,
            output_dir: dir.path().to_path_buf(),
        };
        let (mut pipeline, _segment_rx) = AudioPipeline::create(config).unwrap();
        pipeline.microphone = None;
        pipeline
    }

    #[test]
    fn set_mic_enabled_returns_failed_when_no_mic_path() {
        let mut pipeline = pipeline_without_mic();
        match pipeline.set_microphone_enabled(true) {
            MicToggleOutcome::Failed { .. } => {}
            other => panic!("expected Failed with no mic path, got {other:?}"),
        }
        pipeline.stop().expect("pipeline should stop cleanly");
    }

    #[test]
    fn set_mic_enabled_disabled_when_no_mic_path() {
        // With no mic engine the microphone is not capturing, so a disable is
        // a satisfied no-op.
        let mut pipeline = pipeline_without_mic();
        assert_eq!(
            pipeline.set_microphone_enabled(false),
            MicToggleOutcome::Disabled
        );
        pipeline.stop().expect("pipeline should stop cleanly");
    }

    #[test]
    fn flush_system_segment_succeeds_before_and_after_stop() {
        let dir = tempfile::tempdir().unwrap();
        let config = AudioConfig {
            segment_duration_secs: 30,
            bitrate: 64_000,
            output_dir: dir.path().to_path_buf(),
        };
        let (mut pipeline, _segment_rx) = AudioPipeline::create(config).unwrap();

        // Before stop: flush_tx is Some — should succeed.
        assert!(
            pipeline.flush_system_segment().is_ok(),
            "flush_system_segment should return Ok before stop"
        );

        pipeline.stop().unwrap();

        // After stop: flush_tx is None — must be a graceful Ok no-op.
        assert!(
            pipeline.flush_system_segment().is_ok(),
            "flush_system_segment should return Ok after stop"
        );
    }

    /// Exercises the real start/stop/idempotency paths on a live
    /// `AVAudioEngine`. Needs a real Mac microphone, so it is `#[ignore]`d —
    /// run manually with `cargo test -p chronicle-audio set_mic -- --ignored`.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires a real microphone; run manually"]
    fn set_mic_enable_disable_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let config = AudioConfig {
            segment_duration_secs: 30,
            bitrate: 64_000,
            output_dir: dir.path().to_path_buf(),
        };
        let (mut pipeline, _segment_rx) = AudioPipeline::create(config).unwrap();
        let mic = pipeline
            .microphone
            .as_ref()
            .expect("microphone path should be available on a real Mac");

        assert!(!mic.is_running(), "mic should not run before enable");

        assert_eq!(
            pipeline.set_microphone_enabled(true),
            MicToggleOutcome::Enabled
        );
        assert!(
            pipeline.microphone.as_ref().unwrap().is_running(),
            "mic should run after enable"
        );

        // Enabling again is idempotent — already running, no second start.
        assert_eq!(
            pipeline.set_microphone_enabled(true),
            MicToggleOutcome::Enabled
        );
        assert!(pipeline.microphone.as_ref().unwrap().is_running());

        assert_eq!(
            pipeline.set_microphone_enabled(false),
            MicToggleOutcome::Disabled
        );
        assert!(
            !pipeline.microphone.as_ref().unwrap().is_running(),
            "mic should not run after disable"
        );

        pipeline.stop().unwrap();
    }
}
