//! Owns the capture runtime and reconciles it against three independent
//! stop reasons: user pause, system sleep, display sleep.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use chronicle_audio::{AudioPipeline, CHANNEL_COUNT, SAMPLE_RATE};
use chronicle_capture::{CaptureConfig, CaptureError, EngineStatusProbe};
use chronicle_storage::Storage;

use crate::capture_runtime::{CaptureRuntime, CaptureRuntimeError};
use crate::ipc_handler;
use crate::permissions;
use crate::pipeline::counters::PipelineCounters;
use crate::pipeline::sinks::AppMetadataProvider;
use crate::settings;

/// What `reconcile()` decided to do, given the flags and current run state.
#[derive(Debug, PartialEq, Eq)]
enum ReconcileAction {
    Start,
    Stop,
    None,
}

/// Pure decision: capture should run only when none of the three flags is set.
fn decide(
    user_paused: bool,
    system_asleep: bool,
    display_asleep: bool,
    running: bool,
) -> ReconcileAction {
    let should_run = !user_paused && !system_asleep && !display_asleep;
    match (should_run, running) {
        (true, false) => ReconcileAction::Start,
        (false, true) => ReconcileAction::Stop,
        _ => ReconcileAction::None,
    }
}

/// The result of a `reconcile()` call. The boot caller escalates
/// `StartFailed { partial_teardown: true }` to `exit(3)`; other callers log.
#[derive(Debug, PartialEq, Eq)]
pub enum ReconcileOutcome {
    Started,
    Stopped,
    NoChange,
    StartFailed { partial_teardown: bool },
}

/// Drives the capture runtime against three independent stop reasons.
///
/// `AudioPipeline` is deliberately NOT a field. `reconcile()` borrows it as
/// `&'a AudioPipeline` so the borrow checker enforces ADR-009: the audio
/// handler outlives every SCStream registered on it. The `'a` lifetime on
/// the supervisor anchors the runtime's lifetime to the same audio borrow.
pub struct CaptureSupervisor<'a, M: AppMetadataProvider + 'static + ?Sized> {
    user_paused: bool,
    system_asleep: bool,
    display_asleep: bool,
    runtime: Option<CaptureRuntime<'a, M>>,
    storage: Arc<Storage>,
    metadata: Arc<M>,
    counters: Arc<PipelineCounters>,
    ocr_channel_capacity: usize,
    probe_holder: Arc<Mutex<Option<EngineStatusProbe>>>,
    /// IPC-visible mirror of `user_paused`. Written by `set_user_paused`
    /// (Task 6); kept here so the supervisor owns the persistence atom
    /// alongside the runtime. Unread by `reconcile()` — the supervisor's
    /// own `user_paused` field is the source of truth for decisions.
    capture_paused: Arc<AtomicBool>,
    mic_state_atom: Arc<AtomicU8>,
    base_dir: PathBuf,
}

impl<'a, M: AppMetadataProvider + 'static + ?Sized> CaptureSupervisor<'a, M> {
    /// Construct a supervisor. `system_asleep` and `display_asleep` start
    /// `false` — only `user_paused` is caller-provided because it's loaded
    /// from persisted settings on boot.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        user_paused: bool,
        storage: Arc<Storage>,
        metadata: Arc<M>,
        counters: Arc<PipelineCounters>,
        ocr_channel_capacity: usize,
        probe_holder: Arc<Mutex<Option<EngineStatusProbe>>>,
        capture_paused: Arc<AtomicBool>,
        mic_state_atom: Arc<AtomicU8>,
        base_dir: PathBuf,
    ) -> Self {
        Self {
            user_paused,
            system_asleep: false,
            display_asleep: false,
            runtime: None,
            storage,
            metadata,
            counters,
            ocr_channel_capacity,
            probe_holder,
            capture_paused,
            mic_state_atom,
            base_dir,
        }
    }

    /// True when none of the three stop flags is set.
    pub fn should_run(&self) -> bool {
        !self.user_paused && !self.system_asleep && !self.display_asleep
    }

    /// Compare the three flags against the current run state and Start, Stop,
    /// or do nothing accordingly.
    ///
    /// Log lines carry only state booleans and the action name — never PII
    /// or captured content.
    pub async fn reconcile(&mut self, audio: &'a AudioPipeline) -> ReconcileOutcome {
        let action = decide(
            self.user_paused,
            self.system_asleep,
            self.display_asleep,
            self.runtime.is_some(),
        );
        log::info!(
            "reconcile: user_paused={} system_asleep={} display_asleep={} -> {:?}",
            self.user_paused,
            self.system_asleep,
            self.display_asleep,
            action,
        );
        match action {
            ReconcileAction::Start => self.start(audio),
            ReconcileAction::Stop => self.stop_capture(audio).await,
            ReconcileAction::None => ReconcileOutcome::NoChange,
        }
    }

    /// Build a fresh `CaptureRuntime` and restore the persisted mic state.
    ///
    /// On success this UNCONDITIONALLY calls `set_microphone_enabled` and
    /// writes `mic_state_atom`, regardless of whether the persisted mic
    /// preference is true or false. This is a small delta from today's boot
    /// (which gates both on `read_mic_setting == true`) and is what makes
    /// `start()` work for both boot and a flag-driven resume: a pause/sleep
    /// teardown disables the mic, and any subsequent Start must re-apply the
    /// user's intent without relying on the caller to remember to do it.
    /// The cold-boot+mic-off case is observably a no-op (the device starts
    /// disabled; `MicState::Off` matches the atom's initial value).
    ///
    /// Failures log and return `StartFailed`; this method never exits the
    /// process. The boot caller decides whether to escalate `partial_teardown`.
    fn start(&mut self, audio: &'a AudioPipeline) -> ReconcileOutcome {
        let capture_config = CaptureConfig {
            audio: audio.token(SAMPLE_RATE, CHANNEL_COUNT),
            ..Default::default()
        };
        match CaptureRuntime::start(
            capture_config,
            Arc::clone(&self.storage),
            Arc::clone(&self.metadata),
            Arc::clone(&self.counters),
            self.ocr_channel_capacity,
        ) {
            Ok(rt) => {
                *self.probe_holder.lock().unwrap() = Some(rt.status_probe());
                self.runtime = Some(rt);
                log::info!("capture runtime started (audio on primary display)");

                // Synchronize the mic to the persisted preference. The pause
                // teardown disables the mic, so a resume must re-apply the
                // user's intent unconditionally. The atom mirrors the result
                // so `Status` reflects reality even when the restore fails.
                let wanted_mic = settings::read_mic_setting(&self.base_dir);
                let outcome = audio.set_microphone_enabled(wanted_mic);
                let state = ipc_handler::map_outcome(outcome, permissions::check_microphone());
                self.mic_state_atom.store(state as u8, Ordering::Release);
                log::info!("restored mic to {wanted_mic}, result={state:?}");

                ReconcileOutcome::Started
            }
            Err(e) => {
                log::error!("capture startup failed: {e}");
                let partial_teardown = matches!(
                    e,
                    CaptureRuntimeError::Engine(CaptureError::PartialTeardown { .. })
                );
                ReconcileOutcome::StartFailed { partial_teardown }
            }
        }
    }

    /// Tear down the current runtime, disable the mic, and flush the system
    /// segment so a later restart doesn't span the off-period.
    ///
    /// Persisted `capture_paused` is NOT written here — pause persistence
    /// belongs to the IPC pause path, not to flag-driven stops (sleep).
    async fn stop_capture(&mut self, audio: &'a AudioPipeline) -> ReconcileOutcome {
        if let Some(rt) = self.runtime.take()
            && let Err(e) = rt.stop().await
        {
            log::error!("CaptureRuntime stop failed: {e}");
        }
        *self.probe_holder.lock().unwrap() = None;

        // ADR-010: SCStream stop alone doesn't release the OS mic indicator;
        // explicit set_microphone_enabled(false) is what releases it.
        let outcome = audio.set_microphone_enabled(false);
        let state = ipc_handler::map_outcome(outcome, permissions::check_microphone());
        self.mic_state_atom.store(state as u8, Ordering::Release);
        log::info!("stop: disabled mic, result={state:?}");

        // Best-effort: finalize the system accumulator's partial segment so a
        // later restart does not produce one segment spanning the off-period.
        if let Err(e) = audio.flush_system_segment() {
            log::error!("failed to flush system segment on stop: {e}");
        }

        ReconcileOutcome::Stopped
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU8};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use chronicle_audio::{AudioConfig, AudioPipeline};
    use chronicle_ipc::MicState;
    use chronicle_storage::Storage;

    use crate::pipeline::counters::PipelineCounters;
    use crate::pipeline::metadata::CachingAppMetadataProvider;

    #[test]
    fn decide_truth_table() {
        // All flags false + not running → should start
        assert_eq!(decide(false, false, false, false), ReconcileAction::Start);

        // All flags false + already running → no change
        assert_eq!(decide(false, false, false, true), ReconcileAction::None);

        // user_paused alone: running → stop
        assert_eq!(decide(true, false, false, true), ReconcileAction::Stop);

        // user_paused alone: not running → no change
        assert_eq!(decide(true, false, false, false), ReconcileAction::None);

        // system_asleep alone: running → stop
        assert_eq!(decide(false, true, false, true), ReconcileAction::Stop);

        // system_asleep alone: not running → no change
        assert_eq!(decide(false, true, false, false), ReconcileAction::None);

        // display_asleep alone: running → stop
        assert_eq!(decide(false, false, true, true), ReconcileAction::Stop);

        // display_asleep alone: not running → no change
        assert_eq!(decide(false, false, true, false), ReconcileAction::None);

        // All three flags set + running → stop
        assert_eq!(decide(true, true, true, true), ReconcileAction::Stop);

        // All three flags set + not running → no change
        assert_eq!(decide(true, true, true, false), ReconcileAction::None);

        // Mix: user_paused + system_asleep, running → stop
        assert_eq!(decide(true, true, false, true), ReconcileAction::Stop);

        // Mix: user_paused + system_asleep, not running → no change
        assert_eq!(decide(true, true, false, false), ReconcileAction::None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reconcile_with_a_stop_flag_set_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();

        // Build the AudioPipeline BEFORE the supervisor — ADR-009 requires
        // the audio handler to outlive every borrow, and the supervisor's
        // `'a` lifetime parameter is anchored to this borrow.
        let audio_staging = dir.path().join("audio-staging");
        std::fs::create_dir_all(&audio_staging).unwrap();
        let audio_config = AudioConfig {
            output_dir: audio_staging,
            ..AudioConfig::default()
        };
        let (audio, _segment_rx) = AudioPipeline::create(audio_config).unwrap();

        let storage = Arc::new(
            Storage::open(chronicle_storage::StorageConfig {
                base_dir: dir.path().to_path_buf(),
                pool_size: 1,
            })
            .await
            .unwrap(),
        );
        let metadata = Arc::new(CachingAppMetadataProvider::with_default_clock(
            chronicle_capture::get_frontmost_app,
            Duration::from_millis(250),
        ));
        let counters = PipelineCounters::new();
        let probe_holder = Arc::new(Mutex::new(None));
        let capture_paused = Arc::new(AtomicBool::new(true));
        let mic_state_atom = Arc::new(AtomicU8::new(MicState::Off as u8));

        let mut supervisor = CaptureSupervisor::new(
            true, // user_paused — keeps decide() returning None
            storage,
            metadata,
            counters,
            1024,
            probe_holder,
            capture_paused,
            mic_state_atom,
            dir.path().to_path_buf(),
        );

        // The stop flag keeps decide() returning None, so reconcile() never
        // touches ScreenCaptureKit — the Start/Stop I/O paths need a real
        // display and are covered by the Task 9 end-to-end verification.
        let outcome = supervisor.reconcile(&audio).await;

        assert_eq!(outcome, ReconcileOutcome::NoChange);
        assert!(
            supervisor.runtime.is_none(),
            "runtime must stay None when decide() returns None"
        );
    }
}
