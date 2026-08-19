mod capture_runtime;
mod capture_supervisor;
mod ipc_handler;
mod permissions;
mod pipeline;
mod power;
mod provisioning;
mod retention_task;
mod settings;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use arc_swap::ArcSwap;
use chronicle_audio::{AudioConfig, AudioPipeline};
use chronicle_ipc::{CancellationToken, IpcServer};
use chronicle_storage::{Storage, StorageConfig};

use crate::capture_supervisor::{CaptureSupervisor, ReconcileOutcome, StartRetry};

/// Parse a `retention_days` setting from the config table. An unset key uses
/// the 30-day default; an unparseable value also falls back to 30 with a warn.
///
/// The `None` case stays silent: the storage refresher calls this every 30 s,
/// and default configs have no `retention_days` key — logging on each tick
/// would spam at info level forever.
fn parse_retention_days(s: Option<String>) -> u32 {
    match s {
        Some(v) => v.parse::<u32>().unwrap_or_else(|_| {
            log::warn!("settings: invalid retention_days={v:?}, defaulting to 30");
            30
        }),
        None => 30,
    }
}

/// Detached one-shot: after `delay`, nudge the event loop to re-run
/// reconcile. Not cancelled on recovery — a stale nudge hits an idempotent
/// reconcile and is discarded.
fn spawn_start_retry(tx: tokio::sync::mpsc::Sender<()>, delay: std::time::Duration) {
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let _ = tx.send(()).await;
    });
}

/// Handle a completed provision: persist the variant that just started
/// serving (AD-10, "persist on success").
///
/// **Called from the event loop, and once more from the shutdown drain after
/// that loop has exited** — never concurrently, which is the correctness
/// requirement here rather than a style choice. All three settings writers
/// share one fixed temp path (`settings.tmp`) — see the single-writer note at
/// the mic arm below — so strict serialization is the only thing making that
/// safe. Two truly concurrent callers would surface a lost race as
/// `AlreadyExists`, and worse, the loser's cleanup can delete the winner's
/// temp file and break its rename. The drain is safe because `main` is
/// single-threaded past the loop and no other writer can still be running.
/// Extracted as a free function purely so it stays unit-testable.
///
/// A failed persist is surfaced, never swallowed: the swap already happened
/// and the UI is now reporting the new variant as active, so a silent failure
/// here means the daemon quietly reverts on next start — exactly the
/// divergence AD-10 exists to prevent.
fn handle_provision_event(base_dir: &std::path::Path, evt: provisioning::ProvisionEvent) {
    match evt {
        provisioning::ProvisionEvent::Ready { variant } => {
            match settings::write_whisper_model(base_dir, variant) {
                Ok(()) => {
                    log::info!("whisper model '{variant}' provisioned; setting persisted")
                }
                // The io::Error from open/rename carries no path, so name the
                // directory — otherwise an operator gets "No space left on
                // device" twice with nothing saying which file.
                Err(e) => log::error!(
                    "whisper model '{variant}' is serving, but persisting the setting \
                     under {} failed: {e} — the daemon will fall back to the previous \
                     variant on next start",
                    base_dir.display()
                ),
            }
        }
    }
}

/// Decide and start one model switch. Returns the accept/reject answer the
/// caller must reply with, plus the spawned task when one was started.
///
/// **Sync on purpose.** AD-9 says the reply must not wait on the download or
/// the load, and a `fn` returning a `JoinHandle` cannot await the provision
/// *inside this helper* — which is exactly where the slip would otherwise go
/// unnoticed. It does not make the property airtight: the caller can still
/// await the handle it gets back, and no test here would catch that.
/// Extracted from the event-loop arm because no handler-level test can reach
/// that arm — the handler tests supply their own fake loop, so an `await`
/// there would stall the real event loop for the length of a download with
/// the whole suite still green.
fn begin_model_switch(
    provisioner: &Arc<provisioning::ProvisionerContext>,
    variant: chronicle_transcription::ModelVariant,
    still_waiting: &std::sync::atomic::AtomicBool,
    evt_tx: &tokio::sync::mpsc::Sender<provisioning::ProvisionEvent>,
) -> (bool, Option<tokio::task::JoinHandle<()>>) {
    // Checked BEFORE `try_begin`, which is the whole point: `try_begin`
    // commits the entering state and takes the slot synchronously, so a check
    // after it would need a rollback path with no snapshot to roll back to.
    //
    // The requester is gone when the handler's 20s timeout fired — reachable
    // whenever the event loop is tied up by a slow pause/resume or a power
    // reconcile. It has already answered `ok: false`. Starting the download
    // and the engine swap now would do work nobody asked for, report it
    // nowhere, and on success persist a variant the user was told was
    // refused.
    //
    // The residual race — the handler giving up in the microseconds between
    // this load and the send below — leaves the old behaviour, but the window
    // shrinks from twenty seconds to the width of a CAS.
    if !still_waiting.load(std::sync::atomic::Ordering::Acquire) {
        log::warn!(
            "dropping model command for {variant}: the requester stopped waiting \
             and was already told it was rejected"
        );
        return (false, None);
    }
    if !provisioner.try_begin(variant) {
        return (false, None);
    }
    let ctx = Arc::clone(provisioner);
    let evt_tx = evt_tx.clone();
    (
        true,
        Some(tokio::spawn(
            async move { ctx.provision(variant, evt_tx).await },
        )),
    )
}

/// Arm or clear the start-retry budget from a reconcile outcome (HEU-575):
/// `StartFailed` arms the next backoff step (or reports an exhausted
/// budget); every other outcome ends the incident and restores the budget.
fn note_reconcile_outcome(
    outcome: &ReconcileOutcome,
    start_retry: &mut StartRetry,
    retry_tx: &tokio::sync::mpsc::Sender<()>,
) {
    match outcome {
        ReconcileOutcome::StartFailed { .. } => {
            if let Some(delay) = start_retry.arm() {
                log::warn!(
                    "capture start failed; retrying in {}s (attempt {}/{})",
                    delay.as_secs(),
                    start_retry.attempt(),
                    StartRetry::MAX_ATTEMPTS,
                );
                spawn_start_retry(retry_tx.clone(), delay);
            } else {
                log::error!(
                    "capture start failed; retry budget exhausted — capture stays \
                     down until the next power or IPC event"
                );
            }
        }
        _ => start_retry.reset(),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    log::info!("chronicle-daemon starting");

    // --- Permission preflight ---
    let _mic_status = permissions::preflight()?;

    // --- Storage ---
    let storage = Arc::new(Storage::open(StorageConfig::default()).await?);

    // --- Startup orphan sweep ---
    match storage.sweep_orphans().await {
        Ok(stats) if stats.bytes_freed > 0 => {
            log::info!("Startup orphan sweep freed {} bytes", stats.bytes_freed);
        }
        Ok(_) => {}
        Err(e) => {
            log::error!("Startup orphan sweep failed: {e}");
        }
    }

    // --- Pipeline observability primitives (constructed before IPC server
    // so `DaemonHandler::new` can borrow them). The metadata provider is
    // kept here as well so the `capture_store_loop` spawn can pass it in.
    let counters = crate::pipeline::counters::PipelineCounters::new();
    let capture_snapshot = Arc::new(ArcSwap::from_pointee(
        crate::ipc_handler::CaptureStatusSnapshot::default(),
    ));
    let storage_status_snapshot = Arc::new(ArcSwap::from_pointee(
        crate::ipc_handler::StorageStatusSnapshot::default(),
    ));

    let metadata_provider = Arc::new(
        crate::pipeline::metadata::CachingAppMetadataProvider::with_default_clock(
            chronicle_capture::get_frontmost_app,
            Duration::from_millis(250),
        ),
    );

    // Prime the storage status snapshot BEFORE the IPC server starts. The
    // refresher task below ticks every 30 s; without this prime, the first
    // `Status` request that lands during boot would see Default zeros. First
    // load cost is one directory walk; subsequent reads are O(1) ArcSwap reads.
    {
        let snapshot = match storage.status().await {
            Ok(s) => crate::ipc_handler::StorageStatusSnapshot {
                db_size_bytes: s.db_size_bytes,
                total_disk_usage_bytes: s.total_disk_usage_bytes,
                screenshot_count: s.screenshot_count,
                audio_segment_count: s.audio_segment_count,
                oldest_entry_ms: s.oldest_entry,
                retention_days: parse_retention_days(
                    storage.get_config("retention_days").await.ok().flatten(),
                ),
            },
            Err(e) => {
                log::warn!("initial storage status read failed: {e}");
                crate::ipc_handler::StorageStatusSnapshot::default()
            }
        };
        storage_status_snapshot.store(Arc::new(snapshot));
    }

    // --- IPC server ---
    let cancel = CancellationToken::new();
    let socket_path = storage.base_dir().join("chronicle.sock");
    // Control channel: the IPC handler forwards mic toggles to the event loop
    // below; the shared atom carries the latest result back for `Status`.
    let (mic_tx, mut mic_rx) = tokio::sync::mpsc::channel::<ipc_handler::MicCommand>(8);
    let mic_state_atom = Arc::new(std::sync::atomic::AtomicU8::new(
        chronicle_ipc::MicState::Off as u8,
    ));
    // Readiness gate: the handler rejects mic toggles until the event loop
    // below starts draining `mic_rx`. Opened just before the loop begins.
    let mic_ready = Arc::new(AtomicBool::new(false));
    // Capture pause/resume control channel. The handler forwards Pause/Resume
    // requests here; the event loop below drains them.
    let (capture_tx, mut capture_rx) = tokio::sync::mpsc::channel::<ipc_handler::CaptureCommand>(8);
    let initial_capture_paused = settings::read_capture_paused(storage.base_dir());
    let capture_paused = Arc::new(AtomicBool::new(initial_capture_paused));
    // Model-switch control channel (HEU-475). The handler forwards
    // SetWhisperModel here; the event loop decides accept/reject and spawns
    // the provision.
    let (model_tx, mut model_rx) = tokio::sync::mpsc::channel::<ipc_handler::ModelCommand>(8);
    // Provision completions flow back so the event loop — and only the event
    // loop — persists the variant. See `handle_provision_event`.
    let (provision_evt_tx, mut provision_evt_rx) =
        tokio::sync::mpsc::channel::<provisioning::ProvisionEvent>(8);
    // Power observer (HEU-284). Sleep/wake transitions arrive on `power_rx`
    // and drive the supervisor's sleep flags. The observer thread is not
    // joined (design §4); dropping the handle is fine — process exit reaps
    // the thread. If registration fails, capture continues normally without
    // sleep/wake handling.
    let (power_tx, mut power_rx) = tokio::sync::mpsc::channel::<power::PowerEvent>(8);
    match power::spawn_power_observer(power_tx) {
        Ok(_handle) => log::info!("power observer started"),
        Err(e) => log::warn!(
            "power observer failed to start: {e} — sleep/wake handling disabled, \
             capture continues normally"
        ),
    }
    // Start-retry channel (HEU-575). When a wake/resume reconcile fails to
    // start capture — typically racing display re-registration right after
    // wake — a detached sleeper sends one unit event here to re-run
    // reconcile. Stale nudges after recovery are harmless: reconcile is
    // idempotent.
    let (retry_tx, mut retry_rx) = tokio::sync::mpsc::channel::<()>(4);
    let mut start_retry = StartRetry::new();
    // Whisper model status cell (HEU-475). Created — and, when the model is
    // on disk, put into Loading — BEFORE the IPC server below serves its
    // first Status, so the UI never sees Missing for a model that exists
    // (design §2.1: boot, file present → Loading). The engine-load block
    // finishes the story with ready/error. Missing model → transcription
    // stays idle, the rest of the daemon runs normally, and the UI shows why.
    let whisper_variant = settings::read_whisper_model(storage.base_dir());
    let model_present = chronicle_transcription::model_present(storage.base_dir(), whisper_variant);
    let transcription_status = crate::provisioning::TranscriptionStatusCell::new(whisper_variant);
    if model_present {
        transcription_status.set_loading(whisper_variant);
    } else {
        // Purely descriptive: name the variant so the log says which model is
        // missing, and promise nothing. Phase 1 has no in-app remediation —
        // the banner is deliberately CTA-less and the download button lands in
        // Phase 2 — so "enable it from the menu bar app" would be false today.
        // The fetch script stays out of user-facing strings (HEU-485).
        log::warn!(
            "whisper model \"{}\" is not downloaded — transcription is off; \
             the rest of Chronicle runs normally",
            whisper_variant.as_str()
        );
    }
    let capture_ready = Arc::new(AtomicBool::new(false));
    let handler = ipc_handler::DaemonHandler::new(
        Arc::clone(&counters),
        Arc::clone(&capture_snapshot),
        Arc::clone(&storage_status_snapshot),
        Arc::clone(&storage),
        mic_tx,
        Arc::clone(&mic_state_atom),
        Arc::clone(&mic_ready),
        capture_tx,
        Arc::clone(&capture_paused),
        Arc::clone(&capture_ready),
        Arc::clone(&transcription_status),
        model_tx,
    );
    let ipc_server = IpcServer::start(&socket_path, handler, cancel.clone()).await?;
    log::info!("IPC server started");

    // --- Audio pipeline (create first — capture engine needs the handler) ---
    let audio_staging_dir = storage.base_dir().join("audio-staging");
    std::fs::create_dir_all(&audio_staging_dir)?;

    let audio_config = AudioConfig {
        output_dir: audio_staging_dir,
        ..AudioConfig::default()
    };
    let (mut audio_pipeline, audio_segment_rx) = AudioPipeline::create(audio_config)?;
    log::info!("Audio pipeline created");

    // --- Screen capture pipeline (with audio on primary display) ---
    //
    // HEU-242: `CaptureRuntime` wraps the engine + capture_store_loop +
    // ocr_loop as one atomic unit so pause/resume cycle them together. If
    // the persisted `capture_paused` is true, we boot with no runtime; the
    // 1Hz refresher publishes a default snapshot until Resume rebuilds one.
    //
    // The runtime borrows the audio token, which borrows `audio_pipeline`.
    // `set_microphone_enabled(&self)` only takes a shared borrow, so the
    // event loop below can still toggle the mic while the runtime runs;
    // the pipeline is still stopped at shutdown.
    let capture_probe_holder: Arc<std::sync::Mutex<Option<chronicle_capture::EngineStatusProbe>>> =
        Arc::new(std::sync::Mutex::new(None));

    let mut supervisor: CaptureSupervisor<
        '_,
        crate::pipeline::metadata::CachingAppMetadataProvider<_, _>,
    > = CaptureSupervisor::new(
        initial_capture_paused,
        Arc::clone(&storage),
        Arc::clone(&metadata_provider),
        Arc::clone(&counters),
        1024, // OCR channel capacity
        Arc::clone(&capture_probe_holder),
        Arc::clone(&capture_paused),
        Arc::clone(&mic_state_atom),
        storage.base_dir().to_path_buf(),
    );

    if initial_capture_paused {
        log::info!("Capture is paused (persisted setting); skipping runtime start");
    }

    match supervisor.reconcile(&audio_pipeline).await {
        ReconcileOutcome::StartFailed {
            partial_teardown: true,
        } => {
            log::error!("capture startup rollback failed; exiting");
            std::process::exit(3);
        }
        ReconcileOutcome::StartFailed {
            partial_teardown: false,
        } => {
            return Err(anyhow::anyhow!(
                "capture boot aborted (underlying engine error logged by \
                 CaptureSupervisor::start)"
            ));
        }
        _ => {}
    }

    // Bounded channel (64) with blocking_send — backpressure over data loss
    let (audio_tx, audio_rx) = tokio::sync::mpsc::channel(64);

    // Bridge thread: std::sync::mpsc → tokio::mpsc
    let bridge_handle = std::thread::Builder::new()
        .name("audio-bridge".into())
        .spawn(move || pipeline::bridge_audio_segments(audio_segment_rx, audio_tx))?;

    let audio_storage = Arc::clone(&storage);
    let audio_counters = Arc::clone(&counters);
    // Transcription: load the model once if present; otherwise stay idle
    // (graceful degradation — capture/OCR/IPC unaffected).
    // Set only when the shutdown drain grace expires; tells `transcribe_loop` to
    // stop after the segment it is already working on.
    let stop_transcription = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let engine_handle = Arc::new(provisioning::EngineHandle::new());
    // Awaited here, before `audio_store_loop` spawns below, so nothing can be
    // enqueued while the engine slot is still being filled — the no-loss
    // ordering this block has always had.
    let boot_load_failed = provisioning::boot(
        storage.base_dir(),
        whisper_variant,
        &transcription_status,
        &engine_handle,
    )
    .await;
    // Built AFTER boot, over the same cell and handle. boot() must finish
    // before the event loop can drive a provision, or the two would race for
    // the engine slot — `boot().await` above is what guarantees that, and it
    // is load-bearing now rather than incidental.
    //
    // That ordering is also why `boot_load_failed` has to be threaded through
    // rather than left to the provisioner to discover: boot owns the only
    // load that happens before this line exists, so a corrupt model at
    // startup is invisible to AD-11 unless boot hands the verdict over.
    let provisioner = provisioning::ProvisionerContext::new(
        Arc::clone(&transcription_status),
        Arc::clone(&engine_handle),
        storage.base_dir(),
        boot_load_failed,
    );
    // One handle, one channel, one sink, one loop — unconditionally. "No
    // model" is just an empty handle: the sink gates on it, so every enqueue
    // short-circuits to `Disabled` without touching the channel, identical to
    // the old `NoopTranscriptionSink` (design §3.4). Spawning the loop even
    // with no engine is what lets a mid-session `SetWhisperModel` start
    // transcribing without a restart — there is no loop to create later.
    // Bounded channel (64), matching the audio bridge: drop-on-full.
    let (transcription_tx, transcription_rx) = tokio::sync::mpsc::channel(64);
    let transcription_sink: Arc<dyn pipeline::sinks::TranscriptionSink> =
        Arc::new(pipeline::sinks::TokioTranscriptionSink {
            tx: transcription_tx,
            engine: Arc::clone(&engine_handle),
        });
    // Not the global cancellation token, by design — the loop must outlive it
    // so the shutdown flush still gets transcribed. `stop_transcription` means
    // "finish the segment you are on, then stop", and only the drain timeout
    // below sets it. See `transcribe_loop`'s contract.
    let transcribe_handle = tokio::spawn(pipeline::transcribe_loop(
        Arc::clone(&engine_handle),
        Arc::clone(&storage),
        transcription_rx,
        Arc::clone(&stop_transcription),
    ));
    let audio_store_handle = tokio::spawn(pipeline::audio_store_loop(
        audio_storage,
        audio_rx,
        Arc::clone(&transcription_sink),
        audio_counters,
    ));

    // Capture status refresher (1 Hz). Reads engine atomics via the probe
    // holder so the loop works across pause/resume cycles — when the
    // runtime is absent (paused, or paused-on-boot), publish the default
    // snapshot and the `Status` handler reports state="paused".
    let refresher_cancel = cancel.clone();
    let refresher_snapshot = Arc::clone(&capture_snapshot);
    let refresher_probe = Arc::clone(&capture_probe_holder);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                _ = refresher_cancel.cancelled() => break,
                _ = ticker.tick() => {
                    let snapshot = if let Some(probe) =
                        refresher_probe.lock().unwrap().as_ref()
                    {
                        let snap = probe.snapshot();
                        crate::ipc_handler::CaptureStatusSnapshot {
                            state: Some(snap.state),
                            active_displays: snap.active_displays,
                            frames_captured: snap.frames_captured,
                            frames_dropped: snap.frames_dropped,
                        }
                    } else {
                        // No active runtime → paused or paused-on-boot.
                        crate::ipc_handler::CaptureStatusSnapshot::default()
                    };
                    refresher_snapshot.store(Arc::new(snapshot));
                }
            }
        }
    });

    let storage_refresher_storage = Arc::clone(&storage);
    let storage_refresher_snapshot = Arc::clone(&storage_status_snapshot);
    let storage_refresher_cancel = cancel.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(30));
        // First tick fires immediately; skip it so we don't double-prime
        // (the boot prime above already wrote a fresh snapshot).
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = storage_refresher_cancel.cancelled() => break,
                _ = ticker.tick() => {
                    match storage_refresher_storage.status().await {
                        Ok(s) => {
                            let retention = parse_retention_days(
                                storage_refresher_storage
                                    .get_config("retention_days").await.ok().flatten(),
                            );
                            let snap = crate::ipc_handler::StorageStatusSnapshot {
                                db_size_bytes: s.db_size_bytes,
                                total_disk_usage_bytes: s.total_disk_usage_bytes,
                                screenshot_count: s.screenshot_count,
                                audio_segment_count: s.audio_segment_count,
                                oldest_entry_ms: s.oldest_entry,
                                retention_days: retention,
                            };
                            storage_refresher_snapshot.store(Arc::new(snap));
                        }
                        Err(e) => log::warn!("storage status refresh failed: {e}"),
                    }
                }
            }
        }
    });

    // Retention cleanup. Spawned, never awaited on the startup path: HEU-547
    // was a 249-second startup stall and that shape must not come back.
    //
    // The handle is deliberately dropped. HEU-630 adds the shutdown join and
    // the stop flag that lets an in-flight run end between batches; until then
    // `cancel` only stops the loop between runs, so quitting during a long run
    // waits for that run to finish.
    let cleanup_ops = Arc::new(retention_task::StorageCleanupOps::new(Arc::clone(&storage)));
    let cleanup_cancel = cancel.clone();
    // The loop returns `Result`, and dropping the handle discards it. That is
    // intentional for now — HEU-630 keeps the handle, joins it at the end of
    // teardown, and sets `shutdown_failed` on an Err. Do not "simplify" the
    // return type away in the meantime.
    tokio::spawn(retention_task::run_cleanup_loop(
        cleanup_ops,
        cleanup_cancel,
    ));

    // --- Event loop: serve mic toggles until a shutdown signal arrives ---
    // Toggles are handled inline, one per iteration.
    //
    // **The single-writer note.** All three settings writers run from this
    // loop — `write_mic_setting` here, `write_capture_paused` via
    // `set_user_paused` in the capture arm, and `write_whisper_model` via
    // `handle_provision_event`. They share one fixed temp path
    // (`settings.tmp`), so one-at-a-time processing is the only thing keeping
    // that safe: concurrently, a lost race surfaces as `AlreadyExists`, and
    // the loser's cleanup can delete the winner's temp file and break its
    // rename.
    //
    // The one call outside this loop is `handle_provision_event` in the
    // shutdown drain below, which runs after the loop has exited and cannot
    // overlap it. `handle_provision_event`'s doc points here for exactly this
    // reasoning, so it must keep naming all three writers — and this note
    // must keep naming that exception.
    //
    // (The supervisor's `start()` reads but never writes the mic setting when
    // restoring it on boot/resume/wake, so it can't race this loop's write.)
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    // Open the readiness gates: from here the loop drains `mic_rx`,
    // `capture_rx` and `model_rx`, so the IPC handler can forward toggles
    // instead of rejecting them. All gates open together, which is why the
    // model path reuses `capture_ready` rather than carrying a third
    // always-equal flag.
    mic_ready.store(true, Ordering::Release);
    capture_ready.store(true, Ordering::Release);
    // The provision currently in flight, if any. At most one is ever doing
    // real work — the provisioner's in-flight CAS guarantees it — so the
    // latest handle is the only one worth holding, and shutdown waits on it
    // below. (Strictly, `InFlightGuard` drops a hair before the task itself
    // finishes, so a new switch can win the slot while the old handle is
    // technically still unfinished. Harmless: by then the superseded task has
    // already sent its event and has nothing left but to return.)
    let mut provision_task: Option<tokio::task::JoinHandle<()>> = None;
    loop {
        tokio::select! {
            res = tokio::signal::ctrl_c() => {
                res?;
                log::info!("SIGINT received, shutting down");
                break;
            }
            _ = sigterm.recv() => {
                log::info!("SIGTERM received, shutting down");
                break;
            }
            Some(cmd) = mic_rx.recv() => {
                // Always persist the user's intent so the supervisor's next Start
                // restores it.
                settings::write_mic_setting(storage.base_dir(), cmd.enabled);
                let state = if supervisor.should_run() {
                    let outcome = audio_pipeline.set_microphone_enabled(cmd.enabled);
                    let s = ipc_handler::map_outcome(outcome, permissions::check_microphone());
                    log::info!("mic toggle: requested enabled={}, result={s:?}", cmd.enabled);
                    s
                } else {
                    // Capture is stopped (pause or sleep) — record the preference but do
                    // not engage the device. The mic genuinely is not running.
                    log::info!(
                        "mic toggle: requested enabled={} while capture stopped — \
                         preference saved, device left off",
                        cmd.enabled,
                    );
                    chronicle_ipc::MicState::Off
                };
                mic_state_atom.store(state as u8, std::sync::atomic::Ordering::Release);
                let _ = cmd.reply.send(state);
            }
            Some(cmd) = capture_rx.recv() => {
                let (paused, outcome) = match cmd.action {
                    ipc_handler::CaptureAction::Pause => {
                        (true, supervisor.set_user_paused(true, &audio_pipeline).await)
                    }
                    ipc_handler::CaptureAction::Resume => {
                        (false, supervisor.set_user_paused(false, &audio_pipeline).await)
                    }
                };
                note_reconcile_outcome(&outcome, &mut start_retry, &retry_tx);
                let _ = cmd.reply.send(paused);
            }
            Some(cmd) = model_rx.recv() => {
                // Accept = the provisioner slot was free. try_begin commits
                // the entering state (Downloading/Loading) synchronously, so
                // the reply below already carries it (§2.2). Reply
                // immediately (AD-9); the spawned provision() does the slow
                // work. try_begin runs HERE and not in the IPC handler so
                // accept/reject is serialized with everything else this loop
                // owns.
                // A switch to the ALREADY-loaded variant is accepted and
                // reloaded rather than short-circuited. Deliberate: AD-11's
                // retry-after-load-failure works by re-requesting the same
                // variant, so a "same variant → no-op" guard would have to be
                // Ready-gated to avoid breaking it. The cost is a brief ~2x
                // model RAM while both engines are alive — noted for
                // finish-branch triage rather than guarded here.
                let (accepted, task) = begin_model_switch(
                    &provisioner,
                    cmd.variant,
                    &cmd.still_waiting,
                    &provision_evt_tx,
                );
                // Load-bearing, not defensive noise: a rejection means a
                // provision IS in flight, so `provision_task` is holding its
                // live handle. Assigning unconditionally would overwrite it
                // with None, detaching the running task and reopening the
                // shutdown bug below — for the "user clicks twice" case.
                if task.is_some() {
                    provision_task = task;
                }
                let _ = cmd.reply.send(accepted);
            }
            Some(evt) = provision_evt_rx.recv() => {
                handle_provision_event(storage.base_dir(), evt);
            }
            Some(evt) = power_rx.recv() => {
                log::info!("power event: {evt:?}");
                let outcome = match evt {
                    power::PowerEvent::SystemSleep =>
                        supervisor.set_system_asleep(true, &audio_pipeline).await,
                    power::PowerEvent::SystemWake =>
                        supervisor.set_system_awake(&audio_pipeline).await,
                };
                note_reconcile_outcome(&outcome, &mut start_retry, &retry_tx);
            }
            Some(()) = retry_rx.recv() => {
                log::info!(
                    "start retry firing (attempt {}/{})",
                    start_retry.attempt(),
                    StartRetry::MAX_ATTEMPTS,
                );
                let outcome = supervisor.reconcile(&audio_pipeline).await;
                if outcome == ReconcileOutcome::Started {
                    log::info!("capture recovered on retry");
                }
                note_reconcile_outcome(&outcome, &mut start_retry, &retry_tx);
            }
        }
    }
    // Set when an owned task/thread panic is OBSERVED during shutdown — for
    // the provision join below, the panic itself may have happened much
    // earlier and only surfaces here, since nothing clears `provision_task`
    // when a provision completes. Those panics are deliberately swallowed
    // mid-sequence so the rest of the shutdown still runs; this flag surfaces
    // them in the exit code once it is done. Declared here rather than
    // further down because the provision wait is now the first step that can
    // observe one.
    let mut shutdown_failed = false;
    // Drain any control commands that raced this shutdown signal: reply now
    // so the IPC handler returns immediately instead of waiting out its
    // timeout. For mic, sending `MicState::Error` maps to `ok:false`.
    mic_rx.close();
    while let Ok(cmd) = mic_rx.try_recv() {
        let _ = cmd.reply.send(chronicle_ipc::MicState::Error);
    }
    // Drop the reply senders without sending — the handler's `recv_timeout`
    // returns Err and maps to `ok:false`, matching the mic drain semantics.
    capture_rx.close();
    while let Ok(cmd) = capture_rx.try_recv() {
        drop(cmd.reply);
    }
    // Explicit `false` rather than a dropped sender: a model switch that
    // raced shutdown was genuinely not accepted, and saying so immediately
    // beats making the UI wait out the 20s timeout.
    model_rx.close();
    while let Ok(cmd) = model_rx.try_recv() {
        let _ = cmd.reply.send(false);
    }
    // Wait for an in-flight provision BEFORE closing its event channel. It
    // holds a `provision_evt_tx` clone, so closing first would turn a Ready
    // it is about to emit into a dropped persist: the model file landed, the
    // engine swapped, and the next boot still comes up on the old variant
    // with nothing user-visible. That is the AD-10 divergence arriving
    // through the back door.
    //
    // Bounded, because the alternative is waiting out a 1.5 GB download
    // inside launchd's exit window — but the right bound depends on which
    // phase we interrupt, and the two are not symmetric:
    //
    //   Downloading — abandoning costs a partial `.tmp`, which
    //     `cleanup_stale_tmps` sweeps at the next boot. Cheap. 2s.
    //   Verifying — the rename has NOT happened yet (`finalize_model` hashes
    //     first, renames after), so the `.tmp` is still there and abandoning
    //     costs exactly what it costs above. The longer grace is not about
    //     what is lost here — it is that finishing the hash *reaches* the
    //     expensive phase below, and re-downloading to get back to it is the
    //     waste worth avoiding.
    //   Loading — now the file has downloaded, verified, and landed. There is
    //     no `.tmp` left to sweep, and the ONLY thing abandoning costs is the
    //     setting: the model sits on disk unused and the next boot comes up on
    //     the old variant. That is the same silent divergence as above,
    //     reached through a narrower door. The remaining work is one bounded
    //     load whose blocking half we pay for regardless (see below), so give
    //     it the same grace as the transcription drain.
    //
    // The state is sampled once, so a download that completes just after the
    // read gets the short bound applied to a load. That is the original bug
    // with a ~2s window instead of an unbounded one — strictly better, and
    // not worth a re-check loop, but it is a window, not a guarantee.
    //
    // `abort()` does not stop a `spawn_blocking` load that has already
    // started — runtime drop waits for it either way. What abort buys is
    // OVERLAP: the load finishes alongside the rest of shutdown instead of
    // serializing ahead of it. That is the argument for aborting rather than
    // awaiting, and it is why the longer grace below costs little in practice.
    if let Some(mut task) = provision_task {
        const DOWNLOAD_GRACE: std::time::Duration = std::time::Duration::from_secs(2);
        const LAND_GRACE: std::time::Duration = std::time::Duration::from_secs(5);
        let grace = match transcription_status.stats(storage.base_dir()).state {
            chronicle_ipc::TranscriptionState::Downloading => DOWNLOAD_GRACE,
            _ => LAND_GRACE,
        };
        match tokio::time::timeout(grace, &mut task).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                // The provision panicked. Nothing was persisted either way,
                // but say so rather than counting it as a clean finish —
                // the transcription drain below treats this shape the same.
                shutdown_failed = true;
                log::error!("model provisioning task failed: {e}");
            }
            Err(_) => {
                task.abort();
                // Name the consequence the operator actually gets, which
                // differs by phase: a swept partial, or a model that landed
                // but whose setting never persisted.
                let cost = if grace == DOWNLOAD_GRACE {
                    "any partial download is swept at next start"
                } else {
                    "if the model had already landed, the setting did not \
                     persist and the next start will use the previous variant"
                };
                log::warn!(
                    "model provisioning still running after {}s at shutdown; \
                     abandoning it — {cost}",
                    grace.as_secs()
                );
            }
        }
    }
    // Now safe to close: anything the provision emitted is already buffered.
    provision_evt_rx.close();
    while let Ok(evt) = provision_evt_rx.try_recv() {
        handle_provision_event(storage.base_dir(), evt);
    }
    // Discard any power events that raced shutdown — no reply channel to
    // satisfy. The observer thread itself is not joined; process exit reaps
    // it via CFRunLoopRun().
    power_rx.close();
    while power_rx.try_recv().is_ok() {}
    // Same for pending start-retry nudges; their detached sleepers die with
    // the runtime.
    retry_rx.close();
    while retry_rx.try_recv().is_ok() {}
    cancel.cancel();

    // Stop the IPC server before the capture engine: stop accepting
    // requests and await socket cleanup.
    ipc_server.shutdown().await;

    // Stop the capture runtime FIRST (if active) — stops SCStream and joins
    // the downstream store/ocr tasks. Must run before `audio_pipeline.stop()`
    // so the handler Retained ref is released and the audio buffer channel
    // can close. If no runtime is active (paused at shutdown), this is a
    // no-op.
    //
    // shutdown() returns true only when the engine is poisoned
    // (PartialTeardown) — escalates to exit(3) below. Consumes the
    // supervisor, which releases the &audio_pipeline borrow so
    // audio_pipeline.stop() (which needs &mut self) can run next.
    let poisoned = supervisor.shutdown().await;

    // Stop audio pipeline — encoding thread sees EOF, flushes segments.
    if let Err(e) = audio_pipeline.stop() {
        log::error!("Audio pipeline stop failed: {e}");
    }
    log::info!("Audio pipeline stopped");

    // Wait for bridge thread to finish
    // Deliberately not `?`. An early return here would skip `drop(transcription_sink)`
    // and the drain block below, detaching the transcription task and discarding an
    // in-flight transcript — the exact failure the drain path exists to prevent. This
    // is the last place that invariant could still be violated.
    // `shutdown_failed` still surfaces the panic in the exit code afterwards.
    if bridge_handle.join().is_err() {
        shutdown_failed = true;
        log::error!("audio bridge thread panicked; continuing shutdown");
    }

    // Wait for the audio store task (the capture/OCR tasks are owned and
    // joined by `CaptureRuntime::stop()` above).
    if let Err(e) = audio_store_handle.await {
        shutdown_failed = true;
        log::error!("Audio store task failed: {e}");
    }

    // Transcription drains LAST, and that ordering is load-bearing.
    // `audio_pipeline.stop()` above flushed the session's final partial segments
    // and `audio_store_handle` just persisted and enqueued them, so the queue is
    // only now complete. Dropping our sink Arc closes the channel — the store
    // task released its clone by finishing — and that close is what tells
    // `transcribe_loop` to drain and exit.
    drop(transcription_sink);
    {
        // The loop always exists now (Task 13), so this is no longer an
        // `if let Some`. Every timing and borrow property below is unchanged.
        let mut handle = transcribe_handle;
        // Bounds how long we let the queue drain — NOT how long exit takes. Runtime
        // drop blocks until every blocking worker finishes its current task, so
        // worst-case exit is DRAIN_GRACE plus the in-flight whisper call either way;
        // the same delay precedes the `exit(3)` poison handoff. Kept short to leave
        // headroom inside launchd's 20s default ExitTimeOut — which this was
        // never the sole consumer of, and is less so now that the provision
        // wait above can add up to 5s. The unbounded steps between them
        // (`ipc_server.shutdown`, `supervisor.shutdown`, the bridge join, the
        // audio-store await) are the ones with no ceiling; these two explicit
        // waits total 10s of the 20s and are the part we actually control.
        const DRAIN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);
        // Borrow the handle — do NOT let `timeout` consume it. Dropping a JoinHandle
        // detaches the task, and runtime drop then destroys its future *without
        // polling it*, while the blocking pool separately waits out the whisper call
        // whose result now has nowhere to go. That discards a transcript we already
        // paid for. Borrowing lets us stop the drain and still await the write, which
        // costs no extra wall-clock because that wait was happening regardless.
        match tokio::time::timeout(DRAIN_GRACE, &mut handle).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                shutdown_failed = true;
                log::error!("transcribe loop task failed: {e}");
            }
            Err(_) => {
                // Unbounded await, bounded in practice: the loop breaks after the one
                // segment it is already running, and reports how many it abandoned.
                stop_transcription.store(true, std::sync::atomic::Ordering::Relaxed);
                if let Err(e) = handle.await {
                    shutdown_failed = true;
                    log::error!("transcribe loop task failed: {e}");
                }
            }
        }
    }

    log::info!("chronicle-daemon stopped");
    if poisoned {
        log::error!("daemon exiting with code 3 (engine poisoned)");
        std::process::exit(3);
    }
    if shutdown_failed {
        anyhow::bail!("shutdown completed with task failures (see log)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ready_event_persists_variant() {
        let dir = tempfile::tempdir().unwrap();
        handle_provision_event(
            dir.path(),
            provisioning::ProvisionEvent::Ready {
                variant: chronicle_transcription::parse_variant("small").unwrap(),
            },
        );
        assert_eq!(settings::read_whisper_model(dir.path()).as_str(), "small");
    }

    #[tokio::test]
    async fn begin_model_switch_spawns_and_rejects_a_second_while_busy() {
        // Covers the event-loop arm, which no handler test can reach — the
        // handler tests supply their own fake loop.
        //
        // What makes the busy rejection deterministic is that `try_begin`'s
        // CAS runs synchronously BEFORE the spawn, so the slot is taken the
        // moment the first call returns — not the listener. The listener is
        // belt-and-braces for the day this gains `flavor = "multi_thread"`:
        // it accepts nothing, so a polled provision would park in the HTTP
        // read rather than failing fast and freeing the slot.
        let dir = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let ctx = provisioning::ProvisionerContext::new_for_test_with_url(
            dir.path(),
            &format!("http://127.0.0.1:{port}/x"),
        );
        let (evt_tx, _evt_rx) = tokio::sync::mpsc::channel(4);

        let waiting = std::sync::atomic::AtomicBool::new(true);
        let (accepted, task) = begin_model_switch(
            &ctx,
            chronicle_transcription::parse_variant("small").unwrap(),
            &waiting,
            &evt_tx,
        );
        assert!(accepted, "the slot was free");
        let task = task.expect("an accepted switch starts a task");
        // Tautological on this current-thread runtime — nothing has polled
        // the task yet. Kept because it stops being free the moment someone
        // adds `flavor = "multi_thread"`, and it documents the shape.
        assert!(!task.is_finished(), "the task has not been polled yet");

        let (accepted2, task2) = begin_model_switch(
            &ctx,
            chronicle_transcription::parse_variant("base").unwrap(),
            &waiting,
            &evt_tx,
        );
        assert!(!accepted2, "one at a time — the slot is taken");
        assert!(task2.is_none(), "a rejected switch starts no task");

        task.abort();
    }

    // The handler's 20s timeout answers `ok: false` but leaves the command in
    // the channel. When a long pause/resume or power reconcile finally frees
    // the loop, it must NOT start the work: the UI was told the request was
    // rejected, so a multi-gigabyte download and an engine swap would run
    // with nobody watching and nobody informed — the silent-divergence shape
    // this branch spent three tasks closing, arriving through the one door
    // that bypasses the reply.
    #[tokio::test]
    async fn an_abandoned_model_command_starts_no_work() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = provisioning::ProvisionerContext::new_for_test(dir.path());
        let (evt_tx, _evt_rx) = tokio::sync::mpsc::channel(4);

        let abandoned = std::sync::atomic::AtomicBool::new(false);
        let (accepted, task) = begin_model_switch(
            &ctx,
            chronicle_transcription::parse_variant("small").unwrap(),
            &abandoned,
            &evt_tx,
        );
        assert!(!accepted, "nobody is waiting for this answer");
        assert!(task.is_none(), "and no download may start");

        // The slot must be left free, not consumed — otherwise one abandoned
        // command would wedge every later switch behind a phantom provision.
        let still_waiting = std::sync::atomic::AtomicBool::new(true);
        let (accepted2, task2) = begin_model_switch(
            &ctx,
            chronicle_transcription::parse_variant("base").unwrap(),
            &still_waiting,
            &evt_tx,
        );
        assert!(
            accepted2,
            "the abandoned command must not have taken the slot"
        );
        if let Some(t) = task2 {
            t.abort();
        }
    }

    #[tokio::test]
    async fn failed_provision_leaves_settings_untouched() {
        // A failing switch emits no event and must not move the persisted
        // variant — after a restart the daemon boots the last WORKING one
        // (AD-10). This drives the real provisioner rather than asserting on
        // the absence of a call, so a provision that wrongly emitted Ready on
        // failure would be caught here too.
        let dir = tempfile::tempdir().unwrap();
        settings::write_whisper_model(
            dir.path(),
            chronicle_transcription::parse_variant("base").unwrap(),
        )
        .unwrap();
        let ctx = provisioning::ProvisionerContext::new_for_test_with_url(
            dir.path(),
            "http://127.0.0.1:1/x", // refused connection → provision fails
        );
        let (evt_tx, mut evt_rx) = tokio::sync::mpsc::channel(4);
        let small = chronicle_transcription::parse_variant("small").unwrap();
        // provision() documents that the caller must already have won
        // try_begin; honour it so this test models the production pairing
        // rather than a shape no real call site uses.
        assert!(ctx.try_begin(small));
        ctx.provision(small, evt_tx).await;
        assert!(
            evt_rx.try_recv().is_err(),
            "no event from a failed provision"
        );
        assert_eq!(
            settings::read_whisper_model(dir.path()).as_str(),
            "base",
            "settings unchanged after a failed switch"
        );
    }

    fn failed() -> ReconcileOutcome {
        ReconcileOutcome::StartFailed {
            partial_teardown: false,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn start_failed_arms_a_retry_nudge() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(4);
        let mut retry = StartRetry::new();

        note_reconcile_outcome(&failed(), &mut retry, &tx);

        assert_eq!(retry.attempt(), 1, "StartFailed must consume one attempt");
        // Paused-clock sleeps auto-advance to the earliest deadline, so these
        // bracket the 5 s backoff deterministically — and a never-spawned
        // sleeper fails the second assert instead of hanging a recv().await.
        tokio::time::sleep(Duration::from_secs(4)).await;
        assert!(
            rx.try_recv().is_err(),
            "nudge must not fire before the 5s backoff"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(
            rx.try_recv().is_ok(),
            "nudge must fire once the backoff elapses"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn exhausted_budget_arms_nothing() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(4);
        let mut retry = StartRetry::new();
        while retry.arm().is_some() {}
        assert_eq!(retry.attempt(), StartRetry::MAX_ATTEMPTS);

        note_reconcile_outcome(&failed(), &mut retry, &tx);

        assert_eq!(
            retry.attempt(),
            StartRetry::MAX_ATTEMPTS,
            "an exhausted budget must not grow"
        );
        // sleep(), not advance(): going idle auto-advances to the EARLIEST
        // deadline first, so a wrongly-spawned 5 s sleeper delivers its nudge
        // before this 120 s sleep completes and try_recv catches it. advance()
        // would bump the clock before the sleeper is first polled, hiding it.
        tokio::time::sleep(Duration::from_secs(120)).await;
        assert!(
            rx.try_recv().is_err(),
            "no sleeper may be spawned once the budget is exhausted"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn non_failure_outcomes_reset_the_budget() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<()>(4);

        for outcome in [
            ReconcileOutcome::Started,
            ReconcileOutcome::Stopped,
            ReconcileOutcome::NoChange,
        ] {
            let mut retry = StartRetry::new();
            let _ = retry.arm();
            let _ = retry.arm();
            assert_eq!(retry.attempt(), 2);

            note_reconcile_outcome(&outcome, &mut retry, &tx);

            assert_eq!(
                retry.attempt(),
                0,
                "{outcome:?} must end the incident and restore the budget"
            );
        }
    }
}
