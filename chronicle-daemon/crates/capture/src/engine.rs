//! Capture engine managing per-display SCStreams.
//!
//! `CaptureEngine` enumerates all connected displays, creates one `SCStream`
//! per display, and delivers frames over a bounded mpsc channel.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::AnyThread;
use objc2::rc::{Retained, autoreleasepool};
use objc2_core_media::CMTime;
use objc2_foundation::{NSArray, NSError, NSInteger};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamErrorCode, SCStreamErrorDomain, SCStreamOutputType,
};
use tokio::sync::mpsc;

use crate::deps::EngineDeps;
use crate::error::{CaptureError, Result};
use crate::handler::CaptureOutputHandler;
use crate::{CaptureConfig, CaptureStatus, CapturedFrame};

// ---------------------------------------------------------------------------
// CoreGraphics FFI for primary display detection
// ---------------------------------------------------------------------------

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGMainDisplayID() -> u32;
}

/// Post-construction lifecycle state of `CaptureEngine`.
///
/// Set to `Running` on successful construction. Transitions via `stop()` to
/// `Stopping` and then either `Idle` (clean teardown) or `Poisoned` (one or
/// more streams failed to stop).
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[repr(u8)]
pub enum EngineState {
    Running = 0,
    Stopping = 1,
    Idle = 2,
    Poisoned = 3,
}

impl EngineState {
    pub(crate) fn from_u8(value: u8) -> Self {
        match value {
            0 => EngineState::Running,
            1 => EngineState::Stopping,
            2 => EngineState::Idle,
            _ => EngineState::Poisoned,
        }
    }
}

/// Multi-display capture engine.
///
/// Call [`CaptureEngine::start`] to enumerate displays, create one `SCStream`
/// per display, and begin delivering `CapturedFrame`s over the returned
/// receiver. Use [`stop`](CaptureEngine::stop) to tear down all streams and
/// [`status`](CaptureEngine::status) to read health counters.
pub struct CaptureEngine<'a> {
    streams: Vec<Retained<SCStream>>,
    handlers: Vec<Retained<CaptureOutputHandler>>,
    display_ids: Vec<u32>,
    _sender: mpsc::Sender<CapturedFrame>,
    frames_captured: Arc<AtomicU64>,
    frames_dropped: Arc<AtomicU64>,
    state: EngineState,
    state_atom: Arc<AtomicU8>,
    active_displays_atom: Arc<AtomicUsize>,
    deps: EngineDeps,
    /// Borrows the `AudioPipeline` for as long as the engine lives. Lets
    /// the borrow checker enforce that callers keep the pipeline alive
    /// until the engine is dropped. Not read in the body — the field's
    /// only job is to keep the borrow alive.
    _audio_token: Option<chronicle_audio::AudioHandlerToken<'a>>,
}

impl<'a> CaptureEngine<'a> {
    /// Enumerate displays and start one capture stream per display.
    ///
    /// Returns the engine and a receiver that delivers `CapturedFrame`s.
    /// The channel has a bounded buffer of `config.channel_buffer_size`.
    ///
    /// # Errors
    ///
    /// * `CaptureError::NoDisplays` -- no displays found
    /// * `CaptureError::ScreenCaptureKit` -- SCK returned an error
    pub fn start(config: CaptureConfig<'a>) -> Result<(Self, mpsc::Receiver<CapturedFrame>)> {
        Self::start_with_deps(config, EngineDeps::default())
    }

    /// Internal entry point that accepts an injectable set of SCK seams.
    ///
    /// Public callers use [`start`](Self::start), which delegates here with
    /// `EngineDeps::default()`. Unit tests substitute alternate implementations
    /// to exercise rollback and failure paths without a real SCK runtime.
    pub(crate) fn start_with_deps(
        config: CaptureConfig<'a>,
        deps: EngineDeps,
    ) -> Result<(Self, mpsc::Receiver<CapturedFrame>)> {
        if config.frame_interval_secs <= 0.0 {
            return Err(CaptureError::ScreenCaptureKit(
                "frame_interval_secs must be positive".into(),
            ));
        }
        if config.channel_buffer_size == 0 {
            return Err(CaptureError::ScreenCaptureKit(
                "channel_buffer_size must be at least 1".into(),
            ));
        }

        let (sender, receiver) = mpsc::channel(config.channel_buffer_size);

        let displays = (deps.enumerate_displays)()?;
        if displays.is_empty() {
            return Err(CaptureError::NoDisplays);
        }

        let frames_captured = Arc::new(AtomicU64::new(0));
        let frames_dropped = Arc::new(AtomicU64::new(0));

        // Build a CMTime for the minimum frame interval.
        // Uses millisecond precision: e.g. 2.0 s -> value=2000, timescale=1000.
        let frame_interval = seconds_to_cmtime(config.frame_interval_secs);

        // Identify the primary display for audio output registration.
        // Fall back to the first display if CGMainDisplayID returns an ID
        // not in the enumerated list (e.g., during display hot-plug).
        let cg_primary = unsafe { CGMainDisplayID() };
        let first_display_id = unsafe { displays[0].displayID() };
        let primary_has_match = displays
            .iter()
            .any(|d| unsafe { d.displayID() } == cg_primary);
        let primary_display_id = if primary_has_match {
            cg_primary
        } else {
            log::warn!(
                "Primary display {cg_primary} not found in enumerated displays, \
                 using first display {first_display_id} for audio"
            );
            first_display_id
        };

        let mut streams: Vec<Retained<SCStream>> = Vec::with_capacity(displays.len());
        let mut handlers: Vec<Retained<CaptureOutputHandler>> = Vec::with_capacity(displays.len());
        let mut started_display_ids: Vec<u32> = Vec::with_capacity(displays.len());

        for display in &displays {
            let display_id = unsafe { display.displayID() };
            let width = unsafe { display.width() } as u32;
            let height = unsafe { display.height() } as u32;

            // TODO: Detect actual retina scale factor per display. External
            // non-retina monitors (common with Mac mini/Studio/Pro) use 1.0x.
            // The screencapturekit wrapper used SCShareableContentInfo::for_filter
            // for this. We need an objc2 equivalent or a CoreGraphics query
            // (CGDisplayScreenSize + pixel dimensions). For now, default to 2.0
            // which is correct for built-in Apple Silicon displays.
            let scale_factor = 2.0;
            let is_primary = display_id == primary_display_id;

            // Build the content filter and stream configuration.
            let (filter, stream_config) = autoreleasepool(|_| {
                let empty_windows: Retained<NSArray<_>> = NSArray::new();
                let filter = unsafe {
                    SCContentFilter::initWithDisplay_excludingWindows(
                        SCContentFilter::alloc(),
                        display,
                        &empty_windows,
                    )
                };

                let sc_config = unsafe { SCStreamConfiguration::new() };
                unsafe {
                    sc_config.setWidth(width as usize);
                    sc_config.setHeight(height as usize);
                    sc_config.setShowsCursor(true);
                    sc_config.setMinimumFrameInterval(frame_interval);
                    // BGRA = 'BGRA' as FourCC
                    sc_config.setPixelFormat(u32::from_be_bytes(*b"BGRA"));
                }

                // Configure audio capture on the primary display.
                if is_primary && let Some(ref audio) = config.audio {
                    unsafe {
                        sc_config.setCapturesAudio(true);
                        sc_config.setSampleRate(audio.sample_rate as isize);
                        sc_config.setChannelCount(audio.channel_count as isize);
                        sc_config.setExcludesCurrentProcessAudio(true);
                    }
                }

                (filter, sc_config)
            });

            // Create the handler wired to our frame channel.
            let handler = CaptureOutputHandler::new(
                sender.clone(),
                display_id,
                scale_factor,
                Arc::clone(&frames_captured),
                Arc::clone(&frames_dropped),
            );

            // Create the SCStream.
            let stream = autoreleasepool(|_| unsafe {
                SCStream::initWithFilter_configuration_delegate(
                    SCStream::alloc(),
                    &filter,
                    &stream_config,
                    None,
                )
            });

            // Register the output handler.
            let queue =
                DispatchQueue::new(&format!("com.chronicle.capture.display-{display_id}"), None);

            autoreleasepool(|_| unsafe {
                stream
                    .addStreamOutput_type_sampleHandlerQueue_error(
                        handler.as_protocol_object(),
                        SCStreamOutputType::Screen,
                        Some(&queue),
                    )
                    .map_err(|e| {
                        CaptureError::ScreenCaptureKit(format!(
                            "failed to add output handler for display {display_id}: {}",
                            e.localizedDescription()
                        ))
                    })
            })?;

            // Register audio handler on the primary display's stream.
            if is_primary && let Some(ref audio) = config.audio {
                // System audio — hard error. Core functionality.
                let system_handler = audio.handler_clone();
                autoreleasepool(|_| unsafe {
                    stream
                        .addStreamOutput_type_sampleHandlerQueue_error(
                            &system_handler,
                            SCStreamOutputType::Audio,
                            Some(audio.queue()),
                        )
                        .map_err(|e| {
                            CaptureError::ScreenCaptureKit(format!(
                                "failed to register audio handler on primary display: {}",
                                e.localizedDescription()
                            ))
                        })
                })?;

                log::info!("Registered audio handler on primary display {display_id}");
            }

            // Start capture.
            if let Err(start_err) = (deps.start_stream)(&stream) {
                let mut stop_errors: Vec<(u32, String)> = Vec::new();
                let mut survivors: Vec<u32> = Vec::new();
                for (started, started_id) in streams.iter().zip(started_display_ids.iter()) {
                    if let Err(stop_err) = (deps.stop_stream)(started) {
                        log::error!(
                            "rollback stop_stream failed for display {started_id}: {stop_err}"
                        );
                        stop_errors.push((*started_id, stop_err));
                        survivors.push(*started_id);
                    }
                }
                let original = CaptureError::ScreenCaptureKit(format!(
                    "failed to start capture on display {display_id}: {start_err}"
                ));
                return if stop_errors.is_empty() {
                    Err(original)
                } else {
                    Err(CaptureError::PartialTeardown {
                        survivors,
                        stop_errors,
                        original: Box::new(original),
                    })
                };
            }

            log::info!(
                "Started capture on display {display_id} ({width}x{height}, scale {scale_factor}{})",
                if is_primary { ", primary" } else { "" }
            );
            streams.push(stream);
            started_display_ids.push(display_id);
            handlers.push(handler);
        }

        let state_atom = Arc::new(AtomicU8::new(EngineState::Running as u8));
        let active_displays_atom = Arc::new(AtomicUsize::new(streams.len()));
        let engine = Self {
            streams,
            handlers,
            display_ids: started_display_ids,
            _sender: sender,
            frames_captured,
            frames_dropped,
            state: EngineState::Running,
            state_atom,
            active_displays_atom,
            deps,
            _audio_token: config.audio,
        };

        Ok((engine, receiver))
    }

    /// Stop all active capture streams and collect per-stream teardown errors.
    ///
    /// Idempotent on `Idle` and `Stopping` (returns `Ok(())` without doing
    /// anything). On `Poisoned`, returns `CaptureError::Poisoned` because the
    /// engine cannot be stopped cleanly from a poisoned state. On `Running`,
    /// iterates every stream; any `stop_stream` failure is collected and the
    /// engine transitions to `Poisoned`.
    pub fn stop(&mut self) -> Result<()> {
        match self.state {
            EngineState::Idle | EngineState::Stopping => return Ok(()),
            EngineState::Poisoned => return Err(CaptureError::Poisoned),
            EngineState::Running => {}
        }
        self.set_state(EngineState::Stopping);

        let mut errors: Vec<(u32, String)> = Vec::new();
        for (stream, display_id) in self.streams.iter().zip(self.display_ids.iter()) {
            if let Err(e) = (self.deps.stop_stream)(stream) {
                log::error!("stop_stream failed for display {display_id}: {e}");
                errors.push((*display_id, e));
            }
        }
        self.streams.clear();
        self.handlers.clear();
        self.display_ids.clear();

        if errors.is_empty() {
            self.active_displays_atom.store(0, Ordering::Release);
            self.set_state(EngineState::Idle);
            Ok(())
        } else {
            let survivors: Vec<u32> = errors.iter().map(|(id, _)| *id).collect();
            self.active_displays_atom
                .store(survivors.len(), Ordering::Release);
            self.set_state(EngineState::Poisoned);
            Err(CaptureError::PartialTeardown {
                survivors,
                stop_errors: errors,
                original: Box::new(CaptureError::ScreenCaptureKit("engine stop failed".into())),
            })
        }
    }

    fn set_state(&mut self, next: EngineState) {
        self.state = next;
        self.state_atom.store(next as u8, Ordering::Release);
    }

    /// Return a point-in-time health snapshot.
    pub fn status(&self) -> CaptureStatus {
        CaptureStatus {
            active_displays: self.streams.len(),
            total_frames_captured: self.frames_captured.load(Ordering::Relaxed),
            total_frames_dropped: self.frames_dropped.load(Ordering::Relaxed),
        }
    }

    /// Return the current post-construction state.
    pub fn state(&self) -> EngineState {
        self.state
    }

    /// Return a cloneable probe into the engine's live atomics.
    pub fn status_probe(&self) -> EngineStatusProbe {
        EngineStatusProbe {
            frames_captured: Arc::clone(&self.frames_captured),
            frames_dropped: Arc::clone(&self.frames_dropped),
            state: Arc::clone(&self.state_atom),
            active_displays: Arc::clone(&self.active_displays_atom),
        }
    }
}

/// Lightweight read-only view of the capture engine's live health state.
///
/// Allows a background refresher to observe capture health without
/// borrowing `CaptureEngine` itself (which lives on the main task and
/// isn't `Send` across the refresher boundary).
#[derive(Clone)]
pub struct EngineStatusProbe {
    pub frames_captured: Arc<AtomicU64>,
    pub frames_dropped: Arc<AtomicU64>,
    pub state: Arc<AtomicU8>,
    pub active_displays: Arc<AtomicUsize>,
}

/// One tick's worth of engine status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineStatusSnapshot {
    pub frames_captured: u64,
    pub frames_dropped: u64,
    pub state: EngineState,
    pub active_displays: usize,
}

impl EngineStatusProbe {
    /// Load all four values. `Relaxed` for counters (monotonically
    /// increasing), `Acquire` for state/active_displays (they change in
    /// lockstep with engine transitions and readers want to see a coherent
    /// snapshot — a cache-line refresh per tick is fine).
    pub fn snapshot(&self) -> EngineStatusSnapshot {
        EngineStatusSnapshot {
            frames_captured: self.frames_captured.load(Ordering::Relaxed),
            frames_dropped: self.frames_dropped.load(Ordering::Relaxed),
            state: EngineState::from_u8(self.state.load(Ordering::Acquire)),
            active_displays: self.active_displays.load(Ordering::Acquire),
        }
    }
}

impl Drop for CaptureEngine<'_> {
    fn drop(&mut self) {
        for stream in &self.streams {
            if let Err(e) = (self.deps.stop_stream)(stream) {
                log::warn!("Failed to stop stream on drop: {e}");
            }
        }
        self.streams.clear();
        self.handlers.clear();
        self.display_ids.clear();
    }
}

// ---------------------------------------------------------------------------
// SCK helpers
// ---------------------------------------------------------------------------

/// Enumerate all connected displays via SCShareableContent.
///
/// Uses a synchronous channel + block2 callback to bridge the async SCK API.
pub(crate) fn enumerate_displays() -> Result<Vec<Retained<SCDisplay>>> {
    let (tx, rx) =
        std::sync::mpsc::sync_channel::<std::result::Result<Vec<Retained<SCDisplay>>, String>>(1);

    let block = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
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
            let mut result = Vec::with_capacity(displays.len());
            for i in 0..displays.len() {
                result.push(displays.objectAtIndex(i));
            }
            let _ = tx.send(Ok(result));
        },
    );

    unsafe {
        SCShareableContent::getShareableContentWithCompletionHandler(&block);
    }

    match rx.recv_timeout(std::time::Duration::from_secs(10)) {
        Ok(result) => result.map_err(CaptureError::ScreenCaptureKit),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(CaptureError::ScreenCaptureKit(
            "display enumeration timed out".into(),
        )),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(
            CaptureError::ScreenCaptureKit("display enumeration channel closed".into()),
        ),
    }
}

/// Start the SCStream capture, blocking until the completion handler fires.
///
/// Times out after 10 seconds to avoid hanging indefinitely if SCK
/// never invokes the completion handler.
pub(crate) fn start_stream(stream: &SCStream) -> std::result::Result<(), String> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<Option<String>>(1);

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

    match rx.recv_timeout(std::time::Duration::from_secs(10)) {
        Ok(None) => Ok(()),
        Ok(Some(err)) => Err(err),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err("start capture timed out".into()),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            Err("start capture completion handler channel closed".into())
        }
    }
}

/// True when a stop-completion error means the stream was already stopped —
/// the goal state, reached before we asked. macOS tears down secondary-display
/// streams on system sleep faster than our reconcile Stop runs, so this is a
/// routine race, not a teardown failure. Treating it as one would count the
/// stream as a PartialTeardown survivor and falsely poison the engine.
fn is_benign_stop_error(is_stream_domain: bool, code: NSInteger) -> bool {
    is_stream_domain && code == SCStreamErrorCode::AttemptToStopStreamState.0
}

/// Stop the SCStream capture, blocking until the completion handler fires.
///
/// Times out after 5 seconds. This is shorter than start because stop runs
/// in the Drop path — a hung stop would make the daemon unkillable.
pub(crate) fn stop_stream(stream: &SCStream) -> std::result::Result<(), String> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<Option<String>>(1);

    let block = RcBlock::new(move |error: *mut NSError| {
        if error.is_null() {
            let _ = tx.send(None);
            return;
        }
        // SAFETY: SCK passes a valid NSError when non-null; read-only access
        // inside the completion callback.
        let (is_stream_domain, code) =
            unsafe { (&*(*error).domain() == SCStreamErrorDomain, (*error).code()) };
        if is_benign_stop_error(is_stream_domain, code) {
            log::info!("stop_stream: stream already stopped; treating as stopped");
            let _ = tx.send(None);
        } else {
            let desc = unsafe { (*error).localizedDescription() };
            let _ = tx.send(Some(desc.to_string()));
        }
    });

    unsafe {
        stream.stopCaptureWithCompletionHandler(Some(&block));
    }

    match rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(None) => Ok(()),
        Ok(Some(err)) => Err(err),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err("stop capture timed out".into()),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            Err("stop capture completion handler channel closed".into())
        }
    }
}

/// Convert a fractional-seconds interval into a `CMTime`.
///
/// Uses millisecond precision (timescale = 1000) so values like 0.5 s or 2.0 s
/// are represented accurately.
fn seconds_to_cmtime(secs: f64) -> CMTime {
    let millis = (secs * 1000.0).round() as i64;
    unsafe { CMTime::new(millis, 1000) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deps::EngineDeps;

    #[test]
    fn seconds_to_cmtime_converts_correctly() {
        let time = seconds_to_cmtime(2.0);
        // CMTime is packed — copy fields to locals to avoid unaligned access.
        let value = { time.value };
        let timescale = { time.timescale };
        assert_eq!(value, 2000);
        assert_eq!(timescale, 1000);
    }

    #[test]
    fn seconds_to_cmtime_fractional() {
        let time = seconds_to_cmtime(0.5);
        let value = { time.value };
        let timescale = { time.timescale };
        assert_eq!(value, 500);
        assert_eq!(timescale, 1000);
    }

    #[test]
    fn already_stopped_error_is_benign() {
        assert!(is_benign_stop_error(
            true,
            SCStreamErrorCode::AttemptToStopStreamState.0
        ));
    }

    #[test]
    fn other_stop_errors_are_not_benign() {
        // Same domain, different code — a real stop failure.
        assert!(!is_benign_stop_error(
            true,
            SCStreamErrorCode::InternalError.0
        ));
        // Right code, wrong domain — not SCK's already-stopped signal.
        assert!(!is_benign_stop_error(
            false,
            SCStreamErrorCode::AttemptToStopStreamState.0
        ));
    }

    #[test]
    fn engine_state_variants_compile_and_compare() {
        assert_eq!(EngineState::Running, EngineState::Running);
        assert_ne!(EngineState::Running, EngineState::Idle);
        assert_ne!(EngineState::Stopping, EngineState::Poisoned);
    }

    #[test]
    fn engine_deps_default_uses_real_implementations() {
        // Compile-time check: Default::default() builds.
        let _deps = crate::deps::EngineDeps::default();
    }

    #[test]
    fn start_with_deps_rejects_empty_display_list() {
        fn empty_displays() -> Result<Vec<Retained<SCDisplay>>> {
            Ok(Vec::new())
        }
        fn unreachable_start(_: &SCStream) -> std::result::Result<(), String> {
            unreachable!("start should not be called when there are no displays")
        }
        fn unreachable_stop(_: &SCStream) -> std::result::Result<(), String> {
            unreachable!("stop should not be called when there are no displays")
        }

        let deps = crate::deps::EngineDeps {
            enumerate_displays: empty_displays,
            start_stream: unreachable_start,
            stop_stream: unreachable_stop,
        };
        let result = CaptureEngine::start_with_deps(CaptureConfig::default(), deps);
        assert!(matches!(result, Err(CaptureError::NoDisplays)));
    }

    // Rollback tests run only on macOS where the SCK framework links.
    // The `EngineDeps` seam intercepts before any ObjC call reaches SCK;
    // the streams constructed inside the loop are never started because
    // fake `start_stream` fires first.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore] // Requires real SCDisplay enumeration, which needs Screen Recording TCC.
    fn rollback_reports_partial_teardown_when_stop_fails() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static START_CALLS: AtomicUsize = AtomicUsize::new(0);
        static STOP_CALLS: AtomicUsize = AtomicUsize::new(0);

        fn fail_on_third_start(_: &SCStream) -> std::result::Result<(), String> {
            let n = START_CALLS.fetch_add(1, Ordering::SeqCst);
            if n == 2 {
                Err("injected failure on 3rd stream".into())
            } else {
                Ok(())
            }
        }
        fn fail_on_first_stop(_: &SCStream) -> std::result::Result<(), String> {
            let n = STOP_CALLS.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                Err("injected stop failure".into())
            } else {
                Ok(())
            }
        }

        let deps = crate::deps::EngineDeps {
            enumerate_displays: crate::engine::enumerate_displays,
            start_stream: fail_on_third_start,
            stop_stream: fail_on_first_stop,
        };
        let err = CaptureEngine::start_with_deps(CaptureConfig::default(), deps)
            .err()
            .expect("rollback should fail");
        match err {
            CaptureError::PartialTeardown {
                survivors,
                stop_errors,
                ..
            } => {
                assert_eq!(survivors.len(), 1, "expected one survivor");
                assert_eq!(stop_errors.len(), 1, "expected one stop error");
            }
            other => panic!("expected PartialTeardown, got {other:?}"),
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore] // Requires real SCDisplay enumeration, which needs Screen Recording TCC.
    fn rollback_no_error_when_all_stops_succeed() {
        // When rollback's stop_stream calls all succeed, the caller should
        // see the original `ScreenCaptureKit` start error — not a
        // `PartialTeardown` — because no streams survived.
        use std::sync::atomic::{AtomicUsize, Ordering};
        static START_CALLS: AtomicUsize = AtomicUsize::new(0);

        fn fail_on_second_start(_: &SCStream) -> std::result::Result<(), String> {
            let n = START_CALLS.fetch_add(1, Ordering::SeqCst);
            if n == 1 {
                Err("injected failure on 2nd stream".into())
            } else {
                Ok(())
            }
        }
        fn always_stop_ok(_: &SCStream) -> std::result::Result<(), String> {
            Ok(())
        }

        let deps = crate::deps::EngineDeps {
            enumerate_displays: crate::engine::enumerate_displays,
            start_stream: fail_on_second_start,
            stop_stream: always_stop_ok,
        };
        let err = CaptureEngine::start_with_deps(CaptureConfig::default(), deps)
            .err()
            .expect("rollback should surface the start error");
        assert!(
            matches!(err, CaptureError::ScreenCaptureKit(_)),
            "clean rollback should return ScreenCaptureKit, got {err:?}"
        );
    }

    fn test_engine_with_state(state: EngineState) -> CaptureEngine<'static> {
        let (sender, _rx) = mpsc::channel(1);
        CaptureEngine {
            streams: Vec::new(),
            handlers: Vec::new(),
            display_ids: Vec::new(),
            _sender: sender,
            frames_captured: Arc::new(AtomicU64::new(0)),
            frames_dropped: Arc::new(AtomicU64::new(0)),
            state,
            state_atom: Arc::new(AtomicU8::new(state as u8)),
            active_displays_atom: Arc::new(AtomicUsize::new(0)),
            deps: EngineDeps::default(),
            _audio_token: None,
        }
    }

    #[test]
    fn stop_on_idle_is_idempotent() {
        let mut engine = test_engine_with_state(EngineState::Idle);
        engine.stop().expect("stop on Idle must be idempotent");
        assert_eq!(engine.state, EngineState::Idle);
    }

    #[test]
    fn stop_on_poisoned_returns_poisoned_err() {
        let mut engine = test_engine_with_state(EngineState::Poisoned);
        let err = engine.stop().expect_err("stop on Poisoned must error");
        assert!(matches!(err, CaptureError::Poisoned));
        assert_eq!(engine.state, EngineState::Poisoned);
    }

    #[test]
    fn stop_success_transitions_to_idle() {
        // Empty streams vec → no deps.stop_stream calls → clean transition.
        let mut engine = test_engine_with_state(EngineState::Running);
        engine
            .stop()
            .expect("stop on Running with no streams must succeed");
        assert_eq!(engine.state, EngineState::Idle);
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore] // Requires real SCStream (TCC) to populate the streams vec.
    fn stop_failure_transitions_to_poisoned() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static STOP_CALLS: AtomicUsize = AtomicUsize::new(0);
        fn always_fails(_: &SCStream) -> std::result::Result<(), String> {
            STOP_CALLS.fetch_add(1, Ordering::SeqCst);
            Err("injected stop failure".into())
        }

        // Use the real start path to get a populated engine, then swap deps
        // so the next stop() call fails deterministically.
        let (mut engine, _rx) = CaptureEngine::start(CaptureConfig::default())
            .expect("engine must start on macOS with TCC granted");
        engine.deps = EngineDeps {
            stop_stream: always_fails,
            ..EngineDeps::default()
        };

        let err = engine
            .stop()
            .expect_err("stop must fail when deps.stop_stream fails");
        assert!(matches!(err, CaptureError::PartialTeardown { .. }));
        assert_eq!(engine.state, EngineState::Poisoned);
    }

    #[test]
    fn status_probe_reads_live_state_and_counters() {
        use crate::{EngineState, EngineStatusProbe};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};

        let captured = Arc::new(AtomicU64::new(42));
        let dropped = Arc::new(AtomicU64::new(3));
        let state = Arc::new(AtomicU8::new(EngineState::Running as u8));
        let active = Arc::new(AtomicUsize::new(2));
        let probe = EngineStatusProbe {
            frames_captured: Arc::clone(&captured),
            frames_dropped: Arc::clone(&dropped),
            state: Arc::clone(&state),
            active_displays: Arc::clone(&active),
        };
        let snap = probe.snapshot();
        assert_eq!(snap.frames_captured, 42);
        assert_eq!(snap.frames_dropped, 3);
        assert_eq!(snap.state, EngineState::Running);
        assert_eq!(snap.active_displays, 2);

        // Simulate a transition to `Idle`; probe must observe it.
        state.store(EngineState::Idle as u8, Ordering::Release);
        active.store(0, Ordering::Release);
        let snap2 = probe.snapshot();
        assert_eq!(snap2.state, EngineState::Idle);
        assert_eq!(snap2.active_displays, 0);
    }
}
