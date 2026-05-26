//! Owns the capture runtime and reconciles it against two independent
//! stop reasons: user pause and system sleep.
//!
//! Display sleep is intentionally not a separate flag — see HEU-496 for the
//! Apple-Silicon-specific investigation. The supervisor's two-flag design
//! cleanly accommodates re-adding display sleep later (the existing
//! `set_system_asleep` pattern generalizes to a `set_display_asleep` setter
//! with no other architectural change).

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

/// Pure decision: capture should run only when neither stop flag is set.
fn decide(user_paused: bool, system_asleep: bool, running: bool) -> ReconcileAction {
    let should_run = !user_paused && !system_asleep;
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

/// Drives the capture runtime against two independent stop reasons: user
/// pause and system sleep.
///
/// `AudioPipeline` is deliberately NOT a field. `reconcile()` borrows it as
/// `&'a AudioPipeline` so the borrow checker enforces ADR-009: the audio
/// handler outlives every SCStream registered on it. The `'a` lifetime on
/// the supervisor anchors the runtime's lifetime to the same audio borrow.
pub struct CaptureSupervisor<'a, M: AppMetadataProvider + 'static + ?Sized> {
    user_paused: bool,
    system_asleep: bool,
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
    /// Construct a supervisor. `system_asleep` starts `false` — only
    /// `user_paused` is caller-provided because it's loaded from persisted
    /// settings on boot.
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

    /// True when neither stop flag is set.
    pub fn should_run(&self) -> bool {
        !self.user_paused && !self.system_asleep
    }

    /// Test-only snapshot of the flags as `(user_paused, system_asleep)`.
    /// Used to verify setter side-effects without exposing the fields
    /// publicly.
    #[cfg(test)]
    fn flags_for_test(&self) -> (bool, bool) {
        (self.user_paused, self.system_asleep)
    }

    /// Compare the flags against the current run state and Start, Stop, or
    /// do nothing accordingly.
    ///
    /// Log lines carry only state booleans and the action name — never PII
    /// or captured content.
    pub async fn reconcile(&mut self, audio: &'a AudioPipeline) -> ReconcileOutcome {
        let action = decide(self.user_paused, self.system_asleep, self.runtime.is_some());
        log::info!(
            "reconcile: user_paused={} system_asleep={} -> {:?}",
            self.user_paused,
            self.system_asleep,
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

    /// Flip `user_paused`, persist it, and reconcile. This is the only setter
    /// that writes to disk — the sleep setters are transient. The persistence
    /// path writes a single boolean (`capture_paused`), nothing more.
    pub async fn set_user_paused(&mut self, paused: bool, audio: &'a AudioPipeline) {
        self.user_paused = paused;
        self.capture_paused.store(paused, Ordering::Release);
        settings::write_capture_paused(&self.base_dir, paused);
        log::info!("set_user_paused: paused={paused}");
        self.reconcile(audio).await;
    }

    /// Flip `system_asleep` and reconcile. Transient — never persisted.
    pub async fn set_system_asleep(&mut self, asleep: bool, audio: &'a AudioPipeline) {
        self.system_asleep = asleep;
        log::info!("set_system_asleep: asleep={asleep}");
        self.reconcile(audio).await;
    }

    /// Clear `system_asleep` and reconcile. Transient — never persisted.
    pub async fn set_system_awake(&mut self, audio: &'a AudioPipeline) {
        self.system_asleep = false;
        log::info!("set_system_awake: cleared system_asleep");
        self.reconcile(audio).await;
    }

    /// Stop the runtime deterministically for daemon shutdown. Consumes
    /// `self`, releasing the `&AudioPipeline` borrow so the caller can then
    /// call `audio_pipeline.stop()` (which needs `&mut self`). This is the
    /// ADR-009 borrow-checker dance — see `docs/development/objc2-interop.md`
    /// "Global Shutdown Order".
    ///
    /// Returns `true` if teardown poisoned the engine (the boot caller
    /// escalates this; runtime callers log).
    pub async fn shutdown(mut self) -> bool {
        let Some(rt) = self.runtime.take() else {
            return false;
        };
        match rt.stop().await {
            Ok(()) => {
                log::info!("capture runtime stopped cleanly");
                false
            }
            Err(CaptureRuntimeError::Engine(CaptureError::PartialTeardown {
                survivors,
                stop_errors,
                ..
            })) => {
                log::error!(
                    "runtime stop failed; engine poisoned. survivors={survivors:?} \
                     stop_errors={stop_errors:?}"
                );
                true
            }
            Err(e) => {
                log::error!("runtime stop failed: {e}");
                false
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
        // Both flags false + not running → should start
        assert_eq!(decide(false, false, false), ReconcileAction::Start);

        // Both flags false + already running → no change
        assert_eq!(decide(false, false, true), ReconcileAction::None);

        // user_paused alone: running → stop
        assert_eq!(decide(true, false, true), ReconcileAction::Stop);

        // user_paused alone: not running → no change
        assert_eq!(decide(true, false, false), ReconcileAction::None);

        // system_asleep alone: running → stop
        assert_eq!(decide(false, true, true), ReconcileAction::Stop);

        // system_asleep alone: not running → no change
        assert_eq!(decide(false, true, false), ReconcileAction::None);

        // Both flags set + running → stop
        assert_eq!(decide(true, true, true), ReconcileAction::Stop);

        // Both flags set + not running → no change
        assert_eq!(decide(true, true, false), ReconcileAction::None);
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

    /// Test-only helper: builds the prerequisites for a supervisor. Returns
    /// the temp dir (drop = cleanup), the audio pipeline (caller borrows it
    /// to construct the supervisor so the `'a` lifetime resolves locally),
    /// the `capture_paused` atom, and the rest of the supervisor's args.
    /// Metadata is returned as `Arc<dyn AppMetadataProvider>` so the test
    /// supervisor's `M` parameter resolves without further annotations.
    #[allow(clippy::type_complexity)]
    async fn supervisor_fixtures() -> (
        tempfile::TempDir,
        AudioPipeline,
        Arc<AtomicBool>,
        Arc<Storage>,
        Arc<dyn AppMetadataProvider>,
        Arc<PipelineCounters>,
        Arc<Mutex<Option<EngineStatusProbe>>>,
        Arc<AtomicU8>,
    ) {
        let dir = tempfile::tempdir().unwrap();
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
        let metadata: Arc<dyn AppMetadataProvider> =
            Arc::new(CachingAppMetadataProvider::with_default_clock(
                chronicle_capture::get_frontmost_app,
                Duration::from_millis(250),
            ));
        let counters = PipelineCounters::new();
        let probe_holder = Arc::new(Mutex::new(None));
        let capture_paused = Arc::new(AtomicBool::new(false));
        let mic_state_atom = Arc::new(AtomicU8::new(MicState::Off as u8));

        (
            dir,
            audio,
            capture_paused,
            storage,
            metadata,
            counters,
            probe_holder,
            mic_state_atom,
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_user_paused_persists_and_updates_atom() {
        let (dir, audio, capture_paused, storage, metadata, counters, probe_holder, mic_atom) =
            supervisor_fixtures().await;

        // Construct with user_paused=false. We pre-set system_asleep below so
        // flipping user_paused never crosses into Start.
        let mut supervisor = CaptureSupervisor::new(
            false,
            storage,
            metadata,
            counters,
            1024,
            probe_holder,
            Arc::clone(&capture_paused),
            mic_atom,
            dir.path().to_path_buf(),
        );
        supervisor.set_system_asleep(true, &audio).await;
        // Sanity: persisted paused starts false (missing file => false).
        assert!(!settings::read_capture_paused(dir.path()));

        supervisor.set_user_paused(true, &audio).await;
        assert!(
            settings::read_capture_paused(dir.path()),
            "set_user_paused(true) must persist"
        );
        assert!(
            capture_paused.load(Ordering::Acquire),
            "capture_paused atom must mirror user_paused=true"
        );

        supervisor.set_user_paused(false, &audio).await;
        assert!(
            !settings::read_capture_paused(dir.path()),
            "set_user_paused(false) must persist"
        );
        assert!(
            !capture_paused.load(Ordering::Acquire),
            "capture_paused atom must mirror user_paused=false"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_system_asleep_flips_flag_without_persisting() {
        let (dir, audio, capture_paused, storage, metadata, counters, probe_holder, mic_atom) =
            supervisor_fixtures().await;

        // user_paused stays false. set_system_asleep keeps decide() at None
        // because the system_asleep flag is now set.
        let mut supervisor = CaptureSupervisor::new(
            false,
            storage,
            metadata,
            counters,
            1024,
            probe_holder,
            capture_paused,
            mic_atom,
            dir.path().to_path_buf(),
        );
        assert!(!settings::read_capture_paused(dir.path()));

        supervisor.set_system_asleep(true, &audio).await;

        assert!(
            !supervisor.should_run(),
            "should_run() must be false while system_asleep is set"
        );
        assert!(
            !settings::read_capture_paused(dir.path()),
            "sleep setters MUST NOT persist user_paused"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_system_awake_clears_system_asleep_only() {
        let (dir, audio, capture_paused, storage, metadata, counters, probe_holder, mic_atom) =
            supervisor_fixtures().await;

        // user_paused stays set throughout so reconcile() never starts.
        let mut supervisor = CaptureSupervisor::new(
            true,
            storage,
            metadata,
            counters,
            1024,
            probe_holder,
            capture_paused,
            mic_atom,
            dir.path().to_path_buf(),
        );
        supervisor.set_system_asleep(true, &audio).await;

        // Sanity: both flags set.
        let (u, s) = supervisor.flags_for_test();
        assert!(u && s, "expected both flags set before wake");

        supervisor.set_system_awake(&audio).await;

        let (u, s) = supervisor.flags_for_test();
        assert!(u, "user_paused must be untouched by set_system_awake");
        assert!(!s, "set_system_awake must clear system_asleep");
        assert!(
            !supervisor.should_run(),
            "should_run() must stay false while user_paused holds"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn shutdown_with_no_runtime_is_clean() {
        let (dir, _audio, capture_paused, storage, metadata, counters, probe_holder, mic_atom) =
            supervisor_fixtures().await;

        // user_paused = true => runtime never started.
        let supervisor = CaptureSupervisor::new(
            true,
            storage,
            metadata,
            counters,
            1024,
            probe_holder,
            capture_paused,
            mic_atom,
            dir.path().to_path_buf(),
        );
        assert!(supervisor.runtime.is_none());

        let poisoned = supervisor.shutdown().await;
        assert!(!poisoned, "shutdown with no runtime must report no poison");
    }
}
