mod capture_runtime;
mod capture_supervisor;
mod ipc_handler;
mod permissions;
mod pipeline;
mod power;
mod provisioning;
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
    // One handle, one channel, one sink for every path below. The sink gates
    // on the handle, so while it is empty every enqueue short-circuits to
    // `Disabled` without touching the channel — identical to the old
    // `NoopTranscriptionSink`. The receiver exists from the moment the channel
    // is created and, on the paths that never spawn the loop, stays alive as an
    // unmoved local until `main` returns, so the channel is never closed behind
    // a live sender. Only the successful-load arm sets the handle and spawns
    // the loop, exactly as before; Task 13 restructures this block.
    let engine_handle = Arc::new(provisioning::EngineHandle::new());
    // Bounded channel (64), matching the audio bridge: drop-on-full.
    let (transcription_tx, transcription_rx) = tokio::sync::mpsc::channel(64);
    let transcription_sink: Arc<dyn pipeline::sinks::TranscriptionSink> =
        Arc::new(pipeline::sinks::TokioTranscriptionSink {
            tx: transcription_tx,
            engine: Arc::clone(&engine_handle),
        });
    let transcribe_handle: Option<tokio::task::JoinHandle<()>> = if model_present {
        // Load the ggml model off the runtime — it's heavy blocking work
        // (seconds for a large variant) and must not stall tokio workers
        // (HEU-480). The cell is already in Loading (set at creation, before
        // the IPC server started).
        let base_dir = storage.base_dir().to_path_buf();
        let load_result = tokio::task::spawn_blocking(move || {
            chronicle_transcription::TranscriptionEngine::load(&base_dir, whisper_variant)
        })
        .await;
        match load_result {
            Ok(Ok(engine)) => {
                let engine: Arc<dyn chronicle_transcription::Transcriber> = Arc::new(engine);
                // Publish the engine BEFORE announcing Ready, in that order.
                // IPC has been serving since `IpcServer::start`, and the Status
                // handler reads the cell on its own task — announcing first
                // opens a window where a client sees `ready` while
                // `is_loaded()` is still false. Nothing can enqueue in that
                // window today (`audio_store_loop` spawns further down), but
                // Task 13 drives swaps while capture is already running, where
                // the same ordering stops being harmless.
                engine_handle.set(Arc::clone(&engine));
                transcription_status.set_ready(whisper_variant);
                // Not the global cancellation token, by design — the loop must
                // outlive it so the shutdown flush still gets transcribed. This flag
                // means "finish the segment you are on, then stop", and only the
                // drain timeout below sets it. See `transcribe_loop`'s contract.
                let handle = tokio::spawn(pipeline::transcribe_loop(
                    engine,
                    Arc::clone(&storage),
                    transcription_rx,
                    Arc::clone(&stop_transcription),
                ));
                log::info!(
                    "transcription engine loaded (variant={}, backend={})",
                    whisper_variant,
                    if cfg!(feature = "transcription-metal") {
                        "metal"
                    } else {
                        "cpu"
                    }
                );
                Some(handle)
            }
            Ok(Err(e)) => {
                log::error!(
                    "transcription engine load failed ({whisper_variant}): {e} — staying idle"
                );
                // TranscriptionError::Display already carries the
                // "model load failed:" prefix — don't double it on the wire.
                transcription_status.set_error(&e.to_string());
                None
            }
            Err(e) => {
                log::error!(
                    "transcription engine load task failed ({whisper_variant}): {e} — staying idle"
                );
                // JoinError::Display forwards arbitrary panic payloads —
                // keep the wire string fixed; the log line above has {e}.
                transcription_status.set_error("model load failed");
                None
            }
        }
    } else {
        // The startup model-presence check above already logged the
        // "model missing → idle" warning.
        None
    };
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

    // --- Event loop: serve mic toggles until a shutdown signal arrives ---
    // Toggles are handled inline, one per iteration. This loop is the only
    // caller of write_mic_setting, which writes a fixed temp path — the
    // one-at-a-time processing here is what keeps that path safe. (The
    // supervisor's `start()` reads but never writes the mic setting when
    // restoring it on boot/resume/wake, so it can't race this loop's write.)
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    // Open the readiness gates: from here the loop drains `mic_rx` and
    // `capture_rx`, so the IPC handler can forward toggles instead of
    // rejecting them.
    mic_ready.store(true, Ordering::Release);
    capture_ready.store(true, Ordering::Release);
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
    // Set when an owned task/thread panicked during shutdown. Those panics are
    // deliberately swallowed mid-sequence so the transcription drain still runs;
    // this flag surfaces them in the exit code once the drain is done.
    let mut shutdown_failed = false;

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
    if let Some(mut handle) = transcribe_handle {
        // Bounds how long we let the queue drain — NOT how long exit takes. Runtime
        // drop blocks until every blocking worker finishes its current task, so
        // worst-case exit is DRAIN_GRACE plus the in-flight whisper call either way;
        // the same delay precedes the `exit(3)` poison handoff. Kept short to leave
        // headroom inside launchd's 20s default ExitTimeOut.
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
