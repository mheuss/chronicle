//! Owns the capture runtime and reconciles it against four stop reasons:
//! user pause, system sleep, a missing Screen Recording grant, and a stale
//! Screen Recording read.
//!
//! Display sleep is intentionally not a separate flag — see CHR-57 for the
//! Apple-Silicon-specific investigation. Re-adding it later is one more flag
//! and a `set_display_asleep` setter on the `set_system_asleep` pattern.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chronicle_audio::{AudioPipeline, CHANNEL_COUNT, SAMPLE_RATE};
use chronicle_capture::{CaptureConfig, CaptureError, EngineStatusProbe};
use chronicle_storage::Storage;

use crate::capture_runtime::{CaptureRuntime, CaptureRuntimeError, engine_poisoned};
use crate::ipc_handler;
use crate::mic_gate;
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

/// Pure decision over the run predicate and the current run state. The
/// predicate itself is defined once — `CaptureSupervisor::should_run` — so a
/// future flag (e.g. CHR-57's `display_asleep`) is added in exactly one
/// place.
fn decide(should_run: bool, running: bool) -> ReconcileAction {
    match (should_run, running) {
        (true, false) => ReconcileAction::Start,
        (false, true) => ReconcileAction::Stop,
        _ => ReconcileAction::None,
    }
}

/// The result of a `reconcile()` call. `route_reconcile_outcome` in `main.rs`
/// escalates `StopPoisoned`, and `StartFailed { partial_teardown: true }`
/// before the first successful start, to `exit(3)`; otherwise callers log and
/// retry.
///
/// `must_use`: discarding an outcome silently skips the StartRetry arm/reset
/// bookkeeping (`note_reconcile_outcome`) — exactly the bug class CHR-136
/// exists to prevent. Tests that genuinely don't care use `let _ =`.
#[must_use]
#[derive(Debug, PartialEq, Eq)]
pub enum ReconcileOutcome {
    Started,
    Stopped,
    NoChange,
    StartFailed {
        partial_teardown: bool,
    },
    /// A stop that left a stream possibly running.
    StopPoisoned,
}

/// A copy of the supervisor's four stop flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Gates {
    pub user_paused: bool,
    pub system_asleep: bool,
    pub sr_denied: bool,
    pub sr_stale: bool,
    /// The supervisor's idle epoch (design §1.2).
    pub idle_epoch: u64,
}

/// Retry budget for failed capture starts at boot, wake, or resume.
///
/// A start that fails at wake+~2 s usually races display re-registration —
/// SCK enumerates a transient, non-startable display (Task 20 runs 1–2 saw
/// transient IDs 146–151 and 4/4 wake-start failures at realistic cadence).
/// A login launch can land in the same window, so boot retries too.
/// Schedule: 5, 10, 20, 30, 30 s — at most `MAX_ATTEMPTS` armed retries per
/// incident. Any non-StartFailed reconcile outcome ends the incident; the
/// caller resets the budget.
#[derive(Default)]
pub struct StartRetry {
    attempts: u32,
}

impl StartRetry {
    pub const MAX_ATTEMPTS: u32 = 5;

    pub fn new() -> Self {
        Self::default()
    }

    /// Consume one attempt and return the delay before the next retry, or
    /// `None` when the budget is exhausted.
    pub fn arm(&mut self) -> Option<Duration> {
        if self.attempts >= Self::MAX_ATTEMPTS {
            return None;
        }
        self.attempts += 1;
        let secs = (5u64 << (self.attempts - 1)).min(30);
        Some(Duration::from_secs(secs))
    }

    /// Attempts armed so far in this incident (for log lines).
    pub fn attempt(&self) -> u32 {
        self.attempts
    }

    /// End the incident: restore the full budget.
    pub fn reset(&mut self) {
        self.attempts = 0;
    }
}

/// Drives the capture runtime against four stop reasons: user pause, system
/// sleep, a missing Screen Recording grant, and a stale Screen Recording read.
///
/// `AudioPipeline` is deliberately NOT a field. `reconcile()` borrows it as
/// `&'a AudioPipeline` so the borrow checker enforces ADR-009: the audio
/// handler outlives every SCStream registered on it. The `'a` lifetime on
/// the supervisor anchors the runtime's lifetime to the same audio borrow.
pub struct CaptureSupervisor<'a, M: AppMetadataProvider + 'static + ?Sized> {
    user_paused: bool,
    system_asleep: bool,
    /// Screen Recording is not authorized, per the last helper read (design §1.2).
    sr_denied: bool,
    /// Set on every pause or sleep. Cleared only by an Authorized read.
    /// A resume or wake therefore never starts on a read taken before the idle period.
    sr_stale: bool,
    /// IPC-visible mirror of `sr_denied`.
    sr_denied_mirror: Arc<AtomicBool>,
    /// Adds 1 on every entry to Idle, so a read taken before it can be told apart.
    idle_epoch: u64,
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
    /// Drop counters handed to every engine this supervisor builds.
    ///
    /// Owned by `run` and cloned in here, because `start()` constructs a
    /// fresh `CaptureEngine` on every resume. An engine-owned counter would
    /// reset each cycle, making the reporter's deltas lossy and its totals
    /// non-monotonic.
    capture_drops: Arc<chronicle_capture::CaptureDropCounters>,
    on_stop_poisoned: fn(),
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
        capture_drops: Arc<chronicle_capture::CaptureDropCounters>,
    ) -> Self {
        Self {
            user_paused,
            system_asleep: false,
            sr_denied: false,
            // A boot with capture paused starts stale.
            sr_stale: user_paused,
            sr_denied_mirror: Arc::new(AtomicBool::new(false)),
            idle_epoch: 0,
            runtime: None,
            storage,
            metadata,
            counters,
            ocr_channel_capacity,
            probe_holder,
            capture_paused,
            mic_state_atom,
            base_dir,
            capture_drops,
            on_stop_poisoned: || {},
        }
    }

    /// Run `hook` as soon as a runtime stop is known to be poisoned, before
    /// the stop waits on its downstream tasks.
    pub fn with_on_stop_poisoned(mut self, hook: fn()) -> Self {
        self.on_stop_poisoned = hook;
        self
    }

    /// Set the boot Screen Recording read and the IPC-visible mirror.
    pub fn with_screen_recording_gate(mut self, denied: bool, mirror: Arc<AtomicBool>) -> Self {
        mirror.store(denied, Ordering::Release);
        self.sr_denied = denied;
        self.sr_denied_mirror = mirror;
        self
    }

    /// Build the config for one engine start.
    ///
    /// Extracted from `start` so a test can assert the counters are carried.
    /// `CaptureConfig::default()` hands out a *fresh* counter set, so building
    /// this config from the default would silently give each rebuilt engine
    /// its own counters — the exact loss this design exists to remove.
    fn capture_config<'t>(
        &self,
        audio: Option<chronicle_audio::AudioHandlerToken<'t>>,
    ) -> CaptureConfig<'t> {
        // Every field spelled out rather than `..Default::default()`: the
        // default hands out a *fresh* counter set, so a future field added
        // above would quietly reintroduce the per-engine reset this method
        // exists to prevent. The scalars come from named constants rather than
        // `CaptureConfig::default()`, which would allocate a counter set solely
        // to throw it away on every engine start.
        CaptureConfig {
            frame_interval_secs: chronicle_capture::DEFAULT_FRAME_INTERVAL_SECS,
            channel_buffer_size: chronicle_capture::DEFAULT_CHANNEL_BUFFER_SIZE,
            audio,
            drop_counters: Arc::clone(&self.capture_drops),
        }
    }

    /// True when no stop flag is set.
    pub fn should_run(&self) -> bool {
        !self.user_paused && !self.system_asleep && !self.sr_denied && !self.sr_stale
    }

    /// A copy of the four stop flags.
    pub fn gates(&self) -> Gates {
        Gates {
            user_paused: self.user_paused,
            system_asleep: self.system_asleep,
            sr_denied: self.sr_denied,
            sr_stale: self.sr_stale,
            idle_epoch: self.idle_epoch,
        }
    }

    /// True when a capture runtime exists.
    pub fn is_running(&self) -> bool {
        self.runtime.is_some()
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
        let action = decide(self.should_run(), self.runtime.is_some());
        log::info!(
            "reconcile: user_paused={} system_asleep={} sr_denied={} sr_stale={} -> {:?}",
            self.user_paused,
            self.system_asleep,
            self.sr_denied,
            self.sr_stale,
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
    /// On success this always runs the mic gate on the persisted preference
    /// and writes `mic_state_atom`, whether the preference is true or false.
    /// That is what makes `start()` work for both boot and a flag-driven
    /// resume: a pause/sleep teardown disables the mic, and any later Start
    /// must re-apply the user's intent without relying on the caller. The gate
    /// may clear the preference when the grant is missing (design §1.2), so
    /// `start()` is a settings writer. It must run on the event loop task, or in
    /// the boot `reconcile` before the loop starts.
    ///
    /// Failures log and return `StartFailed`; this method never exits the
    /// process. `route_reconcile_outcome` decides whether to escalate
    /// `partial_teardown`.
    fn start(&mut self, audio: &'a AudioPipeline) -> ReconcileOutcome {
        let capture_config = self.capture_config(audio.token(SAMPLE_RATE, CHANNEL_COUNT));
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

                // Apply the gate to the persisted preference. The pause
                // teardown disables the mic, so a resume re-applies the
                // user's intent. The atom mirrors the result so `Status`
                // reflects reality even when the restore is refused.
                let wanted_mic = settings::read_mic_setting(&self.base_dir);
                let state = mic_gate::reapply_mic_gate(
                    audio,
                    &self.base_dir,
                    permissions::fresh_microphone_status,
                    true,
                    &self.mic_state_atom,
                );
                log::info!("restored mic to {wanted_mic}, result={state:?}");

                ReconcileOutcome::Started
            }
            Err(e) => {
                log::error!("capture startup failed: {e}");
                let CaptureRuntimeError::Engine(engine_err) = &e;
                let partial_teardown = engine_poisoned(engine_err);
                ReconcileOutcome::StartFailed { partial_teardown }
            }
        }
    }

    /// Set `user_paused`, persist it, and mark the read stale on a pause.
    /// Doesn't reconcile. The caller can publish the new state first.
    pub fn mark_user_paused(&mut self, paused: bool) {
        self.user_paused = paused;
        if paused {
            self.sr_stale = true;
            self.idle_epoch += 1;
        }
        self.capture_paused.store(paused, Ordering::Release);
        settings::write_capture_paused(&self.base_dir, paused);
        log::info!("set_user_paused: paused={paused}");
    }

    /// Flip `user_paused`, persist it, and reconcile. This and
    /// `mark_user_paused` are the only setters that write to disk — the sleep
    /// setters are transient. The persistence
    /// path writes a single boolean (`capture_paused`), nothing more.
    ///
    /// Returns the reconcile outcome so the event loop can arm a retry when
    /// a resume's Start fails (CHR-136).
    pub async fn set_user_paused(
        &mut self,
        paused: bool,
        audio: &'a AudioPipeline,
    ) -> ReconcileOutcome {
        self.mark_user_paused(paused);
        self.reconcile(audio).await
    }

    /// Set `system_asleep` and mark the read stale on a sleep. Transient.
    pub fn mark_system_asleep(&mut self, asleep: bool) {
        self.system_asleep = asleep;
        if asleep {
            self.sr_stale = true;
            self.idle_epoch += 1;
        }
        log::info!("set_system_asleep: asleep={asleep}");
    }

    /// Flip `system_asleep` and reconcile. Transient — never persisted.
    pub async fn set_system_asleep(
        &mut self,
        asleep: bool,
        audio: &'a AudioPipeline,
    ) -> ReconcileOutcome {
        self.mark_system_asleep(asleep);
        self.reconcile(audio).await
    }

    /// Clear `system_asleep`. Transient.
    pub fn mark_system_awake(&mut self) {
        self.system_asleep = false;
        log::info!("set_system_awake: cleared system_asleep");
    }

    /// Clear `system_asleep` and reconcile. Transient — never persisted.
    ///
    /// Returns the reconcile outcome so the event loop can arm a retry when
    /// the wake's Start fails (CHR-136).
    pub async fn set_system_awake(&mut self, audio: &'a AudioPipeline) -> ReconcileOutcome {
        self.mark_system_awake();
        self.reconcile(audio).await
    }

    /// Set or clear the Screen Recording gate and reconcile. Transient, never
    /// persisted. Clearing it also clears `sr_stale`, because only an
    /// Authorized read clears the gate (design §3 Step 1).
    pub async fn set_sr_denied(
        &mut self,
        denied: bool,
        audio: &'a AudioPipeline,
    ) -> ReconcileOutcome {
        self.sr_denied = denied;
        if !denied {
            self.sr_stale = false;
        }
        self.sr_denied_mirror.store(denied, Ordering::Release);
        log::info!("set_sr_denied: denied={denied}");
        self.reconcile(audio).await
    }

    /// Stop the runtime deterministically for daemon shutdown. Consumes
    /// `self`, releasing the `&AudioPipeline` borrow so the caller can then
    /// call `audio_pipeline.stop()` (which needs `&mut self`). This is the
    /// ADR-009 borrow-checker dance — see `docs/development/objc2-interop.md`
    /// "Global Shutdown Order".
    ///
    /// Returns `true` if teardown poisoned the engine; `run` escalates this to
    /// `exit(3)`.
    pub async fn shutdown(mut self) -> bool {
        let Some(rt) = self.runtime.take() else {
            return false;
        };
        match rt.stop(self.on_stop_poisoned).await {
            Ok(()) => {
                log::info!("capture runtime stopped cleanly");
                false
            }
            Err(CaptureRuntimeError::Engine(ref e)) if engine_poisoned(e) => {
                if let CaptureError::PartialTeardown {
                    survivors,
                    stop_errors,
                    ..
                } = e
                {
                    log::error!(
                        "runtime stop failed; engine poisoned. survivors={survivors:?} \
                         stop_errors={stop_errors:?}"
                    );
                } else {
                    log::error!("runtime stop failed; engine poisoned: {e}");
                }
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
        let mut poisoned = false;
        if let Some(rt) = self.runtime.take()
            && let Err(e) = rt.stop(self.on_stop_poisoned).await
        {
            log::error!("CaptureRuntime stop failed: {e}");
            let CaptureRuntimeError::Engine(engine_err) = &e;
            poisoned = engine_poisoned(engine_err);
        }
        *self.probe_holder.lock().unwrap() = None;

        // ADR-010: SCStream stop alone doesn't release the OS mic indicator;
        // explicit set_microphone_enabled(false) is what releases it.
        let outcome = audio.set_microphone_enabled(false);
        let state = ipc_handler::map_outcome(outcome);
        self.mic_state_atom.store(state as u8, Ordering::Release);
        log::info!("stop: disabled mic, result={state:?}");

        // Best-effort: finalize the system accumulator's partial segment so a
        // later restart does not produce one segment spanning the off-period.
        if let Err(e) = audio.flush_system_segment() {
            log::error!("failed to flush system segment on stop: {e}");
        }

        if poisoned {
            ReconcileOutcome::StopPoisoned
        } else {
            ReconcileOutcome::Stopped
        }
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
    fn start_retry_backoff_schedule_caps_at_five() {
        let mut r = StartRetry::new();
        let delays: Vec<u64> = std::iter::from_fn(|| r.arm().map(|d| d.as_secs())).collect();
        assert_eq!(delays, vec![5, 10, 20, 30, 30]);
        assert_eq!(r.arm(), None, "budget exhausted after MAX_ATTEMPTS");
    }

    #[test]
    fn start_retry_reset_restores_budget() {
        let mut r = StartRetry::new();
        assert_eq!(r.arm().unwrap().as_secs(), 5);
        assert_eq!(r.arm().unwrap().as_secs(), 10);
        r.reset();
        assert_eq!(r.attempt(), 0);
        assert_eq!(
            r.arm().unwrap().as_secs(),
            5,
            "reset must restore the schedule"
        );
    }

    #[test]
    fn decide_truth_table() {
        // Should run + not running → start
        assert_eq!(decide(true, false), ReconcileAction::Start);

        // Should run + already running → no change
        assert_eq!(decide(true, true), ReconcileAction::None);

        // Should not run + running → stop
        assert_eq!(decide(false, true), ReconcileAction::Stop);

        // Should not run + not running → no change
        assert_eq!(decide(false, false), ReconcileAction::None);
    }

    #[tokio::test]
    async fn supervisor_config_carries_its_own_counters() {
        let (dir, _audio, capture_paused, storage, metadata, counters, probe_holder, mic_atom) =
            supervisor_fixtures().await;
        let drops = Arc::new(chronicle_capture::CaptureDropCounters::default());

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
            Arc::clone(&drops),
        );

        // Two configs, as two successive starts would build.
        let first = supervisor.capture_config(None);
        let second = supervisor.capture_config(None);

        first.drop_counters.full.fetch_add(3, Ordering::Relaxed);
        second.drop_counters.closed.fetch_add(4, Ordering::Relaxed);

        // Two configs share one allocation, so a rebuilt engine keeps
        // accumulating rather than restarting at zero.
        // The mutation this pins: swap `Arc::clone(&self.capture_drops)` in
        // `capture_config` for `defaults.drop_counters`. Each config then gets
        // its own set, these read zero, and `ptr_eq` below fails — the whole
        // failure this task exists to prevent. (Deleting the field outright
        // would not compile: the literal has no `..Default::default()`.)
        assert_eq!(drops.snapshot().full, 3);
        assert_eq!(drops.snapshot().closed, 4);
        assert!(Arc::ptr_eq(&drops, &first.drop_counters));
        assert!(Arc::ptr_eq(&drops, &second.drop_counters));
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
            Arc::new(chronicle_capture::CaptureDropCounters::default()),
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
    async fn gated_reconcile_starts_nothing() {
        let (dir, audio, capture_paused, storage, metadata, counters, probe_holder, mic_atom) =
            supervisor_fixtures().await;
        let mirror = Arc::new(AtomicBool::new(false));
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
            Arc::new(chronicle_capture::CaptureDropCounters::default()),
        )
        .with_screen_recording_gate(true, Arc::clone(&mirror));

        assert!(
            mirror.load(Ordering::Acquire),
            "the mirror starts at the boot read"
        );
        assert!(!supervisor.should_run());
        assert_eq!(
            supervisor.reconcile(&audio).await,
            ReconcileOutcome::NoChange
        );
        assert!(supervisor.runtime.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resume_and_wake_wait_for_a_fresh_read() {
        let (dir, audio, capture_paused, storage, metadata, counters, probe_holder, mic_atom) =
            supervisor_fixtures().await;
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
            Arc::new(chronicle_capture::CaptureDropCounters::default()),
        )
        .with_screen_recording_gate(false, Arc::new(AtomicBool::new(false)));

        assert_eq!(
            supervisor.set_user_paused(true, &audio).await,
            ReconcileOutcome::NoChange
        );
        assert!(supervisor.gates().sr_stale, "a pause marks the read stale");
        assert_eq!(
            supervisor.gates().idle_epoch,
            1,
            "a pause starts a new epoch"
        );
        assert_eq!(
            supervisor.set_user_paused(false, &audio).await,
            ReconcileOutcome::NoChange
        );
        assert!(supervisor.runtime.is_none(), "no start before a fresh read");

        assert_eq!(
            supervisor.set_system_asleep(true, &audio).await,
            ReconcileOutcome::NoChange
        );
        assert!(supervisor.gates().sr_stale);
        assert_eq!(
            supervisor.gates().idle_epoch,
            2,
            "a sleep starts a new epoch"
        );
        assert_eq!(
            supervisor.set_system_awake(&audio).await,
            ReconcileOutcome::NoChange
        );
        assert!(supervisor.runtime.is_none(), "no start before a fresh read");

        // An Authorized read while asleep clears the stale flag and starts
        // nothing, because the sleep flag still holds.
        assert_eq!(
            supervisor.set_system_asleep(true, &audio).await,
            ReconcileOutcome::NoChange
        );
        assert_eq!(
            supervisor.set_sr_denied(false, &audio).await,
            ReconcileOutcome::NoChange
        );
        assert_eq!(
            supervisor.gates(),
            Gates {
                system_asleep: true,
                idle_epoch: 3,
                ..Gates::default()
            }
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn mark_changes_the_gates_without_reconciling() {
        let (dir, _audio, capture_paused, storage, metadata, counters, probe_holder, mic_atom) =
            supervisor_fixtures().await;
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
            Arc::new(chronicle_capture::CaptureDropCounters::default()),
        );
        supervisor.mark_system_asleep(true);
        assert_eq!(
            supervisor.gates(),
            Gates {
                system_asleep: true,
                sr_stale: true,
                idle_epoch: 1,
                ..Gates::default()
            }
        );
        assert!(supervisor.runtime.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn boot_paused_starts_stale() {
        let (dir, audio, capture_paused, storage, metadata, counters, probe_holder, mic_atom) =
            supervisor_fixtures().await;
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
            Arc::new(chronicle_capture::CaptureDropCounters::default()),
        )
        .with_screen_recording_gate(false, Arc::new(AtomicBool::new(false)));

        assert!(
            supervisor.gates().sr_stale,
            "a paused boot is an entry to Idle"
        );
        assert_eq!(
            supervisor.set_user_paused(false, &audio).await,
            ReconcileOutcome::NoChange
        );
        assert!(supervisor.runtime.is_none(), "no start before a fresh read");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn clearing_the_gate_clears_stale_and_the_mirror() {
        let (dir, audio, capture_paused, storage, metadata, counters, probe_holder, mic_atom) =
            supervisor_fixtures().await;
        let mirror = Arc::new(AtomicBool::new(false));
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
            Arc::new(chronicle_capture::CaptureDropCounters::default()),
        )
        .with_screen_recording_gate(true, Arc::clone(&mirror));
        // user_paused stays true, so neither call below reaches ScreenCaptureKit.
        assert_eq!(
            supervisor.set_user_paused(true, &audio).await,
            ReconcileOutcome::NoChange
        );

        assert_eq!(
            supervisor.set_sr_denied(false, &audio).await,
            ReconcileOutcome::NoChange
        );
        let g = supervisor.gates();
        assert!(!g.sr_denied && !g.sr_stale);
        assert!(!mirror.load(Ordering::Acquire));

        assert_eq!(
            supervisor.set_sr_denied(true, &audio).await,
            ReconcileOutcome::NoChange
        );
        assert!(mirror.load(Ordering::Acquire));
        assert_eq!(
            supervisor.gates(),
            Gates {
                user_paused: true,
                sr_denied: true,
                idle_epoch: 1,
                ..Gates::default()
            },
            "a denial sets the gate and leaves sr_stale alone"
        );
        assert!(!supervisor.should_run());
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
            Arc::new(chronicle_capture::CaptureDropCounters::default()),
        );
        let _ = supervisor.set_system_asleep(true, &audio).await;
        // Sanity: persisted paused starts false (missing file => false).
        assert!(!settings::read_capture_paused(dir.path()));

        let _ = supervisor.set_user_paused(true, &audio).await;
        assert!(
            settings::read_capture_paused(dir.path()),
            "set_user_paused(true) must persist"
        );
        assert!(
            capture_paused.load(Ordering::Acquire),
            "capture_paused atom must mirror user_paused=true"
        );

        let _ = supervisor.set_user_paused(false, &audio).await;
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
            Arc::new(chronicle_capture::CaptureDropCounters::default()),
        );
        assert!(!settings::read_capture_paused(dir.path()));
        // Positive coverage of the run predicate: no stop flag set must mean
        // run. This is the assertion that catches an inverted flag when
        // CHR-57 extends should_run() with display_asleep.
        assert!(supervisor.should_run(), "no stop flag set must mean run");

        let outcome = supervisor.set_system_asleep(true, &audio).await;

        assert_eq!(
            outcome,
            ReconcileOutcome::NoChange,
            "sleeping with nothing running must reconcile to NoChange"
        );
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
            Arc::new(chronicle_capture::CaptureDropCounters::default()),
        );
        let _ = supervisor.set_system_asleep(true, &audio).await;

        // Sanity: both flags set.
        let (u, s) = supervisor.flags_for_test();
        assert!(u && s, "expected both flags set before wake");

        let _ = supervisor.set_system_awake(&audio).await;

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
            Arc::new(chronicle_capture::CaptureDropCounters::default()),
        );
        assert!(supervisor.runtime.is_none());

        let poisoned = supervisor.shutdown().await;
        assert!(!poisoned, "shutdown with no runtime must report no poison");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn is_running_false_without_runtime() {
        let (dir, _audio, capture_paused, storage, metadata, counters, probe_holder, mic_atom) =
            supervisor_fixtures().await;

        let supervisor = CaptureSupervisor::new(
            false,
            storage,
            metadata,
            counters,
            1024,
            probe_holder,
            capture_paused,
            mic_atom,
            dir.path().to_path_buf(),
            Arc::new(chronicle_capture::CaptureDropCounters::default()),
        );

        assert!(supervisor.should_run(), "no stop flag is set");
        assert!(!supervisor.is_running(), "no runtime has been started");
    }
}
