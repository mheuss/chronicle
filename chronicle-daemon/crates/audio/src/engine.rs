//! AudioEngine — manages SCStream lifecycle for audio capture.
//!
//! Creates and manages an SCStream configured for mic + system audio,
//! spawns an encoding thread, and delivers completed Opus segments
//! over an mpsc channel.

use std::sync::mpsc;
use std::thread::{self, JoinHandle};

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::{autoreleasepool, Retained};
use objc2::AnyThread;
use objc2_foundation::{NSArray, NSError};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamOutputType,
};

use crate::accumulator::SegmentAccumulator;
use crate::handler::{AudioBuffer, AudioOutputHandler};
use crate::{AudioConfig, AudioError, AudioSource, CompletedSegment, Result};

/// Audio capture engine backed by ScreenCaptureKit.
///
/// Manages the full lifecycle: enumerate displays, configure SCStream,
/// register audio output handlers, and run an encoding thread that
/// converts raw PCM into Opus segments.
pub struct AudioEngine {
    config: AudioConfig,
    stream: Option<Retained<SCStream>>,
    handler: Option<Retained<AudioOutputHandler>>,
    encoding_thread: Option<JoinHandle<()>>,
    buffer_sender: Option<mpsc::SyncSender<AudioBuffer>>,
}

impl AudioEngine {
    /// Create a new AudioEngine with the given configuration.
    ///
    /// No ScreenCaptureKit calls are made until `start()`.
    pub fn new(config: AudioConfig) -> Result<Self> {
        Ok(Self {
            config,
            stream: None,
            handler: None,
            encoding_thread: None,
            buffer_sender: None,
        })
    }

    /// Start audio capture and return a receiver for completed segments.
    ///
    /// This will:
    /// 1. Enumerate displays via SCShareableContent
    /// 2. Configure an SCStream for audio capture
    /// 3. Spawn an encoding thread
    /// 4. Start the stream
    pub fn start(&mut self) -> Result<mpsc::Receiver<CompletedSegment>> {
        // Channel for completed segments (encoding thread -> caller).
        let (segment_tx, segment_rx) = mpsc::channel::<CompletedSegment>();

        // Channel for raw audio buffers (SCK callback -> encoding thread).
        let (buffer_tx, buffer_rx) = mpsc::sync_channel::<AudioBuffer>(64);

        // Spawn the encoding thread. It owns the accumulators and reads
        // from buffer_rx until the channel disconnects.
        let encoding_thread = spawn_encoding_thread(
            buffer_rx,
            segment_tx,
            self.config.clone(),
        );

        // Enumerate displays.
        let display = enumerate_first_display()?;

        // Create SCStream configuration.
        let stream_config = autoreleasepool(|_| create_stream_config());

        // Create content filter with the display.
        let filter = autoreleasepool(|_| {
            let empty_windows: Retained<NSArray<_>> = NSArray::new();
            unsafe {
                SCContentFilter::initWithDisplay_excludingWindows(
                    SCContentFilter::alloc(),
                    &display,
                    &empty_windows,
                )
            }
        });

        // Create the SCStream.
        let stream = autoreleasepool(|_| unsafe {
            SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &stream_config,
                None, // no delegate
            )
        });

        // Create the audio output handler wired to the buffer channel.
        let handler = AudioOutputHandler::new(buffer_tx.clone());

        // Create a dispatch queue for sample delivery.
        let queue = DispatchQueue::new("com.chronicle.audio.samples", None);

        // Register the handler for both system audio and microphone.
        autoreleasepool(|_| {
            register_stream_outputs(&stream, &handler, &queue)?;
            Ok::<(), AudioError>(())
        })?;

        // Start capture.
        start_capture(&stream)?;

        self.stream = Some(stream);
        self.handler = Some(handler);
        self.encoding_thread = Some(encoding_thread);
        self.buffer_sender = Some(buffer_tx);

        Ok(segment_rx)
    }

    /// Stop audio capture, flush remaining segments, and clean up.
    pub fn stop(&mut self) -> Result<()> {
        // Stop the SCStream.
        if let Some(ref stream) = self.stream {
            stop_capture(stream)?;
        }

        // Drop the stream and handler. This disconnects the buffer sender
        // held by the SCK handler, so the encoding thread's recv() will
        // eventually return Err.
        self.stream = None;
        self.handler = None;

        // Drop our copy of the sender too.
        self.buffer_sender = None;

        // Wait for the encoding thread to finish. It will flush
        // accumulators before exiting.
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
        config.sample_rate,
        config.segment_duration_secs,
        config.bitrate,
        opus::Application::Voip, // Speech-optimized for mic
        &config.output_dir,
        segment_tx.clone(),
    );

    let mut sys_acc = SegmentAccumulator::new(
        AudioSource::System,
        config.sample_rate,
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

/// Enumerate displays and return the first one.
fn enumerate_first_display() -> Result<Retained<SCDisplay>> {
    let (tx, rx) = mpsc::sync_channel::<std::result::Result<Retained<SCDisplay>, String>>(1);

    let block = RcBlock::new(move |content: *mut SCShareableContent, error: *mut NSError| {
        if !error.is_null() {
            let desc = unsafe { (*error).localizedDescription() };
            let _ = tx.send(Err(desc.to_string()));
            return;
        }

        if content.is_null() {
            let _ = tx.send(Err("SCShareableContent was null".into()));
            return;
        }

        let content = unsafe { &*content };
        let displays = unsafe { content.displays() };
        if displays.is_empty() {
            let _ = tx.send(Err("no displays found".into()));
            return;
        }

        let display = displays.objectAtIndex(0);
        let _ = tx.send(Ok(display));
    });

    unsafe {
        SCShareableContent::getShareableContentWithCompletionHandler(&block);
    }

    rx.recv()
        .map_err(|_| AudioError::ScreenCaptureKit("display enumeration channel closed".into()))?
        .map_err(AudioError::ScreenCaptureKit)
}

/// Create and configure the SCStreamConfiguration for audio capture.
fn create_stream_config() -> Retained<SCStreamConfiguration> {
    let config = unsafe { SCStreamConfiguration::new() };
    unsafe {
        // Enable audio capture.
        config.setCapturesAudio(true);
        config.setCaptureMicrophone(true);
        config.setExcludesCurrentProcessAudio(true);

        // Audio format: 48kHz mono.
        config.setSampleRate(48_000);
        config.setChannelCount(1);

        // Minimize video overhead — we only need audio.
        config.setShowsCursor(false);
        config.setWidth(1);
        config.setHeight(1);
    }
    config
}

/// Register the audio output handler for system audio and microphone streams.
fn register_stream_outputs(
    stream: &SCStream,
    handler: &AudioOutputHandler,
    queue: &DispatchQueue,
) -> Result<()> {
    let protocol_obj = handler.as_protocol_object();

    unsafe {
        stream
            .addStreamOutput_type_sampleHandlerQueue_error(
                protocol_obj,
                SCStreamOutputType::Audio,
                Some(queue),
            )
            .map_err(|e| {
                AudioError::ScreenCaptureKit(format!(
                    "failed to add system audio output: {}",
                    e.localizedDescription()
                ))
            })?;
    }

    // Microphone output may fail if mic permission is denied.
    // Log a warning but don't fail start().
    let mic_result = unsafe {
        stream.addStreamOutput_type_sampleHandlerQueue_error(
            protocol_obj,
            SCStreamOutputType::Microphone,
            Some(queue),
        )
    };

    if let Err(e) = mic_result {
        log::warn!(
            "failed to add microphone output (permission denied?): {}",
            e.localizedDescription()
        );
    }

    Ok(())
}

/// Start the SCStream capture, blocking until the completion handler fires.
fn start_capture(stream: &SCStream) -> Result<()> {
    let (tx, rx) = mpsc::sync_channel::<Option<String>>(1);

    let block = RcBlock::new(move |error: *mut NSError| {
        if error.is_null() {
            let _ = tx.send(None);
        } else {
            let desc = unsafe { (*error).localizedDescription() };
            let _ = tx.send(Some(desc.to_string()));
        }
    });

    unsafe {
        stream.startCaptureWithCompletionHandler(Some(&block));
    }

    match rx.recv() {
        Ok(None) => Ok(()),
        Ok(Some(err)) => Err(AudioError::ScreenCaptureKit(format!(
            "start capture failed: {err}"
        ))),
        Err(_) => Err(AudioError::ScreenCaptureKit(
            "start capture completion handler channel closed".into(),
        )),
    }
}

/// Stop the SCStream capture, blocking until the completion handler fires.
fn stop_capture(stream: &SCStream) -> Result<()> {
    let (tx, rx) = mpsc::sync_channel::<Option<String>>(1);

    let block = RcBlock::new(move |error: *mut NSError| {
        if error.is_null() {
            let _ = tx.send(None);
        } else {
            let desc = unsafe { (*error).localizedDescription() };
            let _ = tx.send(Some(desc.to_string()));
        }
    });

    unsafe {
        stream.stopCaptureWithCompletionHandler(Some(&block));
    }

    match rx.recv() {
        Ok(None) => Ok(()),
        Ok(Some(err)) => Err(AudioError::ScreenCaptureKit(format!(
            "stop capture failed: {err}"
        ))),
        Err(_) => Err(AudioError::ScreenCaptureKit(
            "stop capture completion handler channel closed".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn new_stores_config_without_sck_calls() {
        let config = AudioConfig {
            segment_duration_secs: 15,
            bitrate: 32_000,
            sample_rate: 48_000,
            output_dir: PathBuf::from("/tmp/audio-test"),
        };

        let engine = AudioEngine::new(config.clone()).unwrap();

        assert_eq!(engine.config.segment_duration_secs, 15);
        assert_eq!(engine.config.bitrate, 32_000);
        assert!(engine.stream.is_none());
        assert!(engine.handler.is_none());
        assert!(engine.encoding_thread.is_none());
        assert!(engine.buffer_sender.is_none());
    }

    #[test]
    fn encoding_loop_dispatches_buffers_to_correct_accumulator() {
        let dir = tempfile::tempdir().unwrap();
        let config = AudioConfig {
            segment_duration_secs: 1,
            bitrate: 64_000,
            sample_rate: 48_000,
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
            sample_rate: 48_000,
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
            sample_rate: 48_000,
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
    fn stop_without_start_is_noop() {
        let config = AudioConfig::default();
        let mut engine = AudioEngine::new(config).unwrap();

        // Stopping before starting should not panic or error.
        let result = engine.stop();
        assert!(result.is_ok());
    }
}
