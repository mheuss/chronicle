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

use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
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

/// Builds a microphone path from an encoding-channel sender and the shared
/// drop counters. Production uses `MicrophoneCapture::new`.
type MicFactory = Box<
    dyn Fn(mpsc::SyncSender<AudioMessage>, Arc<AudioDropCounters>) -> Result<MicrophoneCapture>,
>;

/// Audio encoding pipeline.
///
/// Owns the ObjC handler, dispatch queue, microphone capture path, and
/// encoding thread. External callers register the handler on an `SCStream`
/// via a borrowed [`AudioHandlerToken`] obtained from [`token()`](Self::token).
/// The encoding thread converts raw PCM into Opus segments and sends them
/// over a channel.
///
/// The encoding channel has up to three senders — the ObjC handler (system
/// audio), the [`MicrophoneCapture`] tap while one is built, and `flush_tx`.
/// Calling `stop()` drops all three, which closes the channel and lets the
/// encoding thread flush and exit. The borrow checker prevents `stop()` from
/// being called while an `AudioHandlerToken` is outstanding.
pub struct AudioPipeline {
    handler: Option<Retained<AudioOutputHandler>>,
    queue: Retained<DispatchQueue>,
    encoding_thread: Option<JoinHandle<()>>,
    /// Empty until the first enable builds it, and again after
    /// [`drop_microphone`](Self::drop_microphone). No method may lock it while
    /// it already holds a guard.
    microphone: Mutex<Option<MicrophoneCapture>>,
    mic_factory: MicFactory,
    /// A clone of the encoding-channel sender, kept so a disable can send
    /// `AudioMessage::FlushMic`. `None` after `stop()`.
    flush_tx: Option<mpsc::SyncSender<AudioMessage>>,
    /// Buffers the SCK audio handler and the microphone tap discarded.
    /// Created here and never rebuilt, so the counts are monotonic for the
    /// life of the process.
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
        Self::create_with_mic_factory(config, Box::new(MicrophoneCapture::new))
    }

    fn create_with_mic_factory(
        config: AudioConfig,
        mic_factory: MicFactory,
    ) -> Result<(Self, mpsc::Receiver<CompletedSegment>)> {
        let (segment_tx, segment_rx) = mpsc::channel::<CompletedSegment>();
        let (buffer_tx, buffer_rx) = mpsc::sync_channel::<AudioMessage>(64);

        let encoding_thread = spawn_encoding_thread(buffer_rx, segment_tx, config);

        let drop_counters = Arc::new(AudioDropCounters::default());

        // Clone one sender for the handler and keep the original as
        // `flush_tx`. The microphone tap gets its clone when it is built.
        let handler = AudioOutputHandler::new(buffer_tx.clone(), Arc::clone(&drop_counters));

        // DispatchQueue::new returns dispatch2::Queue, .into() converts to
        // Retained<DispatchQueue> for objc2 interop.
        let queue: Retained<DispatchQueue> =
            DispatchQueue::new("com.chronicle.audio.samples", None).into();

        let pipeline = Self {
            handler: Some(handler),
            queue,
            encoding_thread: Some(encoding_thread),
            microphone: Mutex::new(None),
            mic_factory,
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
    /// An enable first builds the engine if the slot is empty, which reads the
    /// input node, the call TCC checks.
    pub fn set_microphone_enabled(&self, enabled: bool) -> MicToggleOutcome {
        let mut slot = self.mic_slot();
        if enabled && let Err(reason) = self.fill_mic_slot(&mut slot) {
            return MicToggleOutcome::Failed { reason };
        }
        let Some(mic) = slot.as_ref() else {
            // Never built, so the microphone is not capturing: a disable is a
            // satisfied no-op.
            return MicToggleOutcome::Disabled;
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
            self.stop_mic(mic)
        }
    }

    /// Stop the microphone, then drop its engine under the same lock, so the
    /// next enable builds a new one (design §1.1). Returns the stop's outcome.
    pub fn drop_microphone(&self) -> MicToggleOutcome {
        let mut slot = self.mic_slot();
        let outcome = match slot.as_ref() {
            Some(mic) => self.stop_mic(mic),
            None => MicToggleOutcome::Disabled,
        };
        *slot = None;
        outcome
    }

    /// Stop `mic` and finalize its partial segment.
    fn stop_mic(&self, mic: &MicrophoneCapture) -> MicToggleOutcome {
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

    fn mic_slot(&self) -> MutexGuard<'_, Option<MicrophoneCapture>> {
        self.microphone
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Build the microphone path into `slot` if it is empty. Fails after
    /// `stop()`, when no encoding-channel sender is left to hand it.
    fn fill_mic_slot(
        &self,
        slot: &mut Option<MicrophoneCapture>,
    ) -> std::result::Result<(), String> {
        if slot.is_some() {
            return Ok(());
        }
        let tx = self
            .flush_tx
            .as_ref()
            .ok_or_else(|| "audio pipeline is stopped".to_string())?
            .clone();
        let mic =
            (self.mic_factory)(tx, Arc::clone(&self.drop_counters)).map_err(|e| e.to_string())?;
        *slot = Some(mic);
        Ok(())
    }

    /// Finalize the system accumulator's partial segment now. Sent when capture
    /// stops (system sleep or IPC pause; display sleep is CHR-57) so a
    /// later restart does not
    /// produce one segment spanning the off-period. Symmetric with the `FlushMic`
    /// send inside `set_microphone_enabled`.
    ///
    /// Blocking send — `FlushSystem` is a control message that must not be
    /// dropped under backpressure. A send error means the encoding channel is
    /// closed, which is a genuine failure. `Ok` no-op after `stop()`.
    pub fn flush_system_segment(&self) -> Result<()> {
        if let Some(tx) = &self.flush_tx {
            tx.send(AudioMessage::FlushSystem).map_err(|_| {
                AudioError::EncodingChannelClosed("failed to flush system segment".into())
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
        let slot = self
            .microphone
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(mic) = slot.as_ref() {
            let _ = mic.stop();
        }

        // Drop every encoding-channel sender so the encoding thread can exit.
        // If any sender survived, `recv()` would never return `Err`.
        *slot = None;
        self.flush_tx = None;
        self.handler = None;

        if let Some(thread) = self.encoding_thread.take() {
            thread
                .join()
                .map_err(|_| AudioError::Encoding("encoding thread panicked".into()))?;
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
    use std::sync::atomic::{AtomicUsize, Ordering};

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

        // And the handler must be writing into that same allocation — handing
        // it a fresh set compiles fine and counts somewhere nobody reads.
        let handler_counters = pipeline.handler.as_ref().unwrap().counters();
        assert!(
            Arc::ptr_eq(&counters, handler_counters),
            "handler must share the pipeline's counters, not its own"
        );

        // Same for the microphone tap. Without this, a fresh set handed to
        // MicrophoneCapture compiles and counts where nothing reads — which
        // would silently lose exactly the mic drop burst CHR-53 reports.
        //
        // Build the microphone the way the first enable would. Conditional,
        // because a build failure is soft and leaves the slot empty: on a host
        // with no input device, `AVAudioConverter::initFromFormat_toFormat`
        // in `MicrophoneCapture::new` returns nil against a zero-channel
        // native format and `new` returns `Err`. Asserting unconditionally
        // would fail such a host for behaving as designed.
        //
        // Skipping the assertion there costs nothing: an `Err` means no
        // `MicrophoneCapture`, so no tap block was constructed and there is no
        // wrong-capture bug for the skip to hide. The count below is what keeps
        // this honest on every host that DOES have a device.
        let mut slot = pipeline.mic_slot();
        let _ = pipeline.fill_mic_slot(&mut slot);
        if let Some(mic) = slot.as_ref() {
            assert!(
                Arc::ptr_eq(&counters, mic.counters()),
                "mic tap must share the pipeline's counters, not its own"
            );
        }

        // `ptr_eq` on the struct field says nothing about what the tap block's
        // `move` closure captured — a fresh Arc passed to `make_tap_block`
        // leaves the field correct and the block counting into nothing. That
        // is the "wrong capture compiles and silently counts nothing" failure
        // this task exists to prevent, and strong_count is the only cheap
        // signal that reaches inside the block.
        //
        // Three holders always: this local, the pipeline field, the handler
        // ivars. The mic adds exactly two more — the `MicrophoneCapture` field
        // and the tap block's capture — so the expected count is derived from
        // whether the mic exists rather than hardcoded. That keeps this running
        // by DEFAULT on every dev machine, where it is the only assertion that
        // reaches inside the tap block, while staying green on a mic-less host.
        // Do not replace this with a bare `5` and do not move it behind
        // `#[ignore]`: both were tried, and both stop it catching the mutation
        // it exists for on the machines that actually run the suite.
        let expected = 3 + 2 * usize::from(slot.is_some());
        assert_eq!(
            Arc::strong_count(&counters),
            expected,
            "expected {expected} holders of the counters; a lower count means \
             something (most likely the tap block) was handed a different \
             allocation"
        );
        drop(slot);
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

    /// A pipeline whose microphone factory always fails and counts its calls.
    /// Stands in for a host where `AVAudioEngine` setup fails, without
    /// needing audio hardware.
    fn pipeline_with_failing_mic() -> (AudioPipeline, Arc<AtomicUsize>) {
        let dir = tempfile::tempdir().unwrap();
        let config = AudioConfig {
            segment_duration_secs: 30,
            bitrate: 64_000,
            output_dir: dir.path().to_path_buf(),
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        let factory: MicFactory = Box::new(move |_, _| {
            seen.fetch_add(1, Ordering::SeqCst);
            Err(AudioError::Microphone("no input device".into()))
        });
        let (pipeline, _rx) = AudioPipeline::create_with_mic_factory(config, factory).unwrap();
        (pipeline, calls)
    }

    #[test]
    fn create_does_not_build_the_microphone() {
        let (mut pipeline, calls) = pipeline_with_failing_mic();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(pipeline.mic_slot().is_none());
        pipeline.stop().unwrap();
    }

    #[test]
    fn disable_never_builds_the_microphone() {
        let (mut pipeline, calls) = pipeline_with_failing_mic();
        assert_eq!(
            pipeline.set_microphone_enabled(false),
            MicToggleOutcome::Disabled
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        pipeline.stop().unwrap();
    }

    #[test]
    fn failed_build_returns_failed_and_is_retried() {
        let (mut pipeline, calls) = pipeline_with_failing_mic();
        assert!(matches!(
            pipeline.set_microphone_enabled(true),
            MicToggleOutcome::Failed { .. }
        ));
        assert!(matches!(
            pipeline.set_microphone_enabled(true),
            MicToggleOutcome::Failed { .. }
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(pipeline.mic_slot().is_none());
        pipeline.stop().unwrap();
    }

    #[test]
    fn drop_microphone_on_an_empty_slot_is_disabled() {
        let (mut pipeline, calls) = pipeline_with_failing_mic();
        assert_eq!(pipeline.drop_microphone(), MicToggleOutcome::Disabled);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        pipeline.stop().unwrap();
    }

    #[test]
    fn enable_after_stop_fails_without_building() {
        let (mut pipeline, calls) = pipeline_with_failing_mic();
        pipeline.stop().unwrap();
        assert!(matches!(
            pipeline.set_microphone_enabled(true),
            MicToggleOutcome::Failed { .. }
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
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
        assert!(
            pipeline.mic_slot().is_none(),
            "create must not build the mic"
        );

        assert_eq!(
            pipeline.set_microphone_enabled(true),
            MicToggleOutcome::Enabled
        );
        assert!(
            pipeline.mic_slot().as_ref().unwrap().is_running(),
            "mic should run after enable"
        );

        // Enabling again is idempotent — already running, no second start.
        assert_eq!(
            pipeline.set_microphone_enabled(true),
            MicToggleOutcome::Enabled
        );
        assert!(pipeline.mic_slot().as_ref().unwrap().is_running());

        assert_eq!(
            pipeline.set_microphone_enabled(false),
            MicToggleOutcome::Disabled
        );
        assert!(
            !pipeline.mic_slot().as_ref().unwrap().is_running(),
            "mic should not run after disable"
        );

        assert_eq!(pipeline.drop_microphone(), MicToggleOutcome::Disabled);
        assert!(pipeline.mic_slot().is_none(), "drop must empty the slot");

        // The next enable builds a new engine and starts it (design AD 1).
        assert_eq!(
            pipeline.set_microphone_enabled(true),
            MicToggleOutcome::Enabled
        );
        assert!(pipeline.mic_slot().as_ref().unwrap().is_running());
        assert_eq!(pipeline.drop_microphone(), MicToggleOutcome::Disabled);
        assert!(pipeline.mic_slot().is_none());

        pipeline.stop().unwrap();
    }
}
