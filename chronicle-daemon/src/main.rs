mod capture_runtime;
mod ipc_handler;
mod permissions;
mod pipeline;
mod settings;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use arc_swap::ArcSwap;
use chronicle_audio::{AudioConfig, AudioPipeline, CHANNEL_COUNT, SAMPLE_RATE};
use chronicle_capture::{CaptureConfig, CaptureError};
use chronicle_ipc::{CancellationToken, IpcServer};
use chronicle_storage::{Storage, StorageConfig};

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
    let capture_paused = Arc::new(AtomicBool::new(settings::read_capture_paused(
        storage.base_dir(),
    )));
    // Whisper model presence check (HEU-421). Logs a startup warning if
    // the configured variant's ggml file isn't on disk; transcription
    // stays idle but the rest of the daemon runs normally.
    let whisper_variant = settings::read_whisper_model(storage.base_dir());
    if !chronicle_transcription::model_present(storage.base_dir(), whisper_variant) {
        let path = chronicle_transcription::model_path(storage.base_dir(), whisper_variant);
        log::warn!(
            "whisper model missing at {} — transcription will stay idle. \
             Run chronicle-daemon/scripts/fetch-whisper-model.sh {} from the repo root to provision.",
            path.display(),
            whisper_variant,
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
    let mut capture_runtime: Option<
        crate::capture_runtime::CaptureRuntime<
            '_,
            crate::pipeline::metadata::CachingAppMetadataProvider<_, _>,
        >,
    > = None;
    let capture_probe_holder: Arc<std::sync::Mutex<Option<chronicle_capture::EngineStatusProbe>>> =
        Arc::new(std::sync::Mutex::new(None));

    if !capture_paused.load(Ordering::Acquire) {
        let capture_config = CaptureConfig {
            audio: audio_pipeline.token(SAMPLE_RATE, CHANNEL_COUNT),
            ..Default::default()
        };
        match crate::capture_runtime::CaptureRuntime::start(
            capture_config,
            Arc::clone(&storage),
            Arc::clone(&metadata_provider),
            Arc::clone(&counters),
            1024, // OCR channel capacity
        ) {
            Ok(rt) => {
                *capture_probe_holder.lock().unwrap() = Some(rt.status_probe());
                capture_runtime = Some(rt);
                log::info!("Capture runtime started (audio on primary display)");
            }
            Err(crate::capture_runtime::CaptureRuntimeError::Engine(
                CaptureError::PartialTeardown {
                    survivors,
                    stop_errors,
                    original,
                },
            )) => {
                log::error!(
                    "capture startup rollback failed; exiting. survivors={survivors:?} \
                     stop_errors={stop_errors:?} original={original}"
                );
                std::process::exit(3);
            }
            Err(e) => return Err(anyhow::anyhow!("capture startup failed: {e}")),
        }
    } else {
        log::info!("Capture is paused (persisted setting); skipping runtime start");
    }

    // Restore the persisted microphone setting only if capture is not paused.
    // A paused daemon must not light up the OS mic indicator at boot — the
    // persisted mic preference is preserved untouched and re-applied on Resume.
    if !capture_paused.load(Ordering::Acquire) && settings::read_mic_setting(storage.base_dir()) {
        let outcome = audio_pipeline.set_microphone_enabled(true);
        let state = ipc_handler::map_outcome(outcome, permissions::check_microphone());
        log::info!("restored persisted mic setting: result={state:?}");
        // A failed restore updates the runtime atom but deliberately leaves the
        // persisted setting untouched so the next daemon start retries it.
        mic_state_atom.store(state as u8, std::sync::atomic::Ordering::Release);
    }

    // Bounded channel (64) with blocking_send — backpressure over data loss
    let (audio_tx, audio_rx) = tokio::sync::mpsc::channel(64);

    // Bridge thread: std::sync::mpsc → tokio::mpsc
    let bridge_handle = std::thread::Builder::new()
        .name("audio-bridge".into())
        .spawn(move || pipeline::bridge_audio_segments(audio_segment_rx, audio_tx))?;

    let audio_storage = Arc::clone(&storage);
    let audio_counters = Arc::clone(&counters);
    // Transcription: load the model once if present; otherwise stay idle with a
    // no-op sink (graceful degradation — capture/OCR/IPC unaffected).
    let (transcription_sink, transcribe_handle): (
        Arc<dyn pipeline::sinks::TranscriptionSink>,
        Option<tokio::task::JoinHandle<()>>,
    ) = if chronicle_transcription::model_present(storage.base_dir(), whisper_variant) {
        // Load the ggml model off the runtime — it's heavy blocking work
        // (seconds for a large variant) and must not stall tokio workers (HEU-480).
        let base_dir = storage.base_dir().to_path_buf();
        let load_result = tokio::task::spawn_blocking(move || {
            chronicle_transcription::TranscriptionEngine::load(&base_dir, whisper_variant)
        })
        .await;
        match load_result {
            Ok(Ok(engine)) => {
                let engine: Arc<dyn chronicle_transcription::Transcriber> = Arc::new(engine);
                // Bounded channel (64), matching the audio bridge: drop-on-full.
                let (tx, rx) = tokio::sync::mpsc::channel(64);
                // No cancellation token by design — the loop drains until the
                // channel closes so the shutdown flush still gets transcribed.
                // See `transcribe_loop`'s shutdown contract.
                let handle =
                    tokio::spawn(pipeline::transcribe_loop(engine, Arc::clone(&storage), rx));
                log::info!(
                    "transcription engine loaded (variant={}, backend={})",
                    whisper_variant,
                    if cfg!(feature = "transcription-metal") {
                        "metal"
                    } else {
                        "cpu"
                    }
                );
                (
                    Arc::new(pipeline::sinks::TokioTranscriptionSink(tx)),
                    Some(handle),
                )
            }
            Ok(Err(e)) => {
                log::error!(
                    "transcription engine load failed ({whisper_variant}): {e} — staying idle"
                );
                (Arc::new(pipeline::sinks::NoopTranscriptionSink), None)
            }
            Err(e) => {
                log::error!(
                    "transcription engine load task panicked ({whisper_variant}): {e} — staying idle"
                );
                (Arc::new(pipeline::sinks::NoopTranscriptionSink), None)
            }
        }
    } else {
        // The startup model-presence check (line ~120) already logged the
        // "model missing → idle" warning.
        (Arc::new(pipeline::sinks::NoopTranscriptionSink), None)
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
    // one-at-a-time processing here is what keeps that path safe. (The startup
    // restore block above also toggles the mic, but deliberately does not
    // persist, so it never races this loop's write.)
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
                let outcome = audio_pipeline.set_microphone_enabled(cmd.enabled);
                let state = ipc_handler::map_outcome(outcome, permissions::check_microphone());
                // NFR-5: every toggle logs the requested state and the result.
                log::info!("mic toggle: requested enabled={}, result={state:?}", cmd.enabled);
                if matches!(state, chronicle_ipc::MicState::On | chronicle_ipc::MicState::Off) {
                    settings::write_mic_setting(
                        storage.base_dir(),
                        state == chronicle_ipc::MicState::On,
                    );
                }
                // The atom always mirrors the latest result so Status is honest.
                mic_state_atom.store(state as u8, std::sync::atomic::Ordering::Release);
                let _ = cmd.reply.send(state); // ignore if the handler already timed out
            }
            Some(cmd) = capture_rx.recv() => {
                let new_paused = match cmd.action {
                    ipc_handler::CaptureAction::Pause => {
                        if let Some(rt) = capture_runtime.take()
                            && let Err(e) = rt.stop().await
                        {
                            log::error!("CaptureRuntime stop on pause failed: {e}");
                        }
                        *capture_probe_holder.lock().unwrap() = None;
                        // ADR-010: SCStream stop alone doesn't release the OS
                        // mic indicator; explicit set_microphone_enabled(false)
                        // is what releases it. Persisted `mic_enabled` is NOT
                        // overwritten — pause is transient and Resume must
                        // restore the user's intent.
                        let outcome = audio_pipeline.set_microphone_enabled(false);
                        let state =
                            ipc_handler::map_outcome(outcome, permissions::check_microphone());
                        // Mirror the result into the runtime atom so `Status`
                        // reports the real mic state during pause (Off on
                        // success; Error/PermissionDenied if the disable
                        // failed). NFR-5 honesty — persisted preference is
                        // untouched, only the runtime view is updated.
                        mic_state_atom
                            .store(state as u8, std::sync::atomic::Ordering::Release);
                        log::info!("pause: disabled mic, result={state:?}");
                        capture_paused.store(true, Ordering::Release);
                        settings::write_capture_paused(storage.base_dir(), true);
                        true
                    }
                    ipc_handler::CaptureAction::Resume => {
                        if capture_runtime.is_none() {
                            let capture_config = CaptureConfig {
                                audio: audio_pipeline.token(SAMPLE_RATE, CHANNEL_COUNT),
                                ..Default::default()
                            };
                            match crate::capture_runtime::CaptureRuntime::start(
                                capture_config,
                                Arc::clone(&storage),
                                Arc::clone(&metadata_provider),
                                Arc::clone(&counters),
                                1024,
                            ) {
                                Ok(rt) => {
                                    *capture_probe_holder.lock().unwrap() =
                                        Some(rt.status_probe());
                                    capture_runtime = Some(rt);
                                    log::info!("resume: CaptureRuntime started");
                                }
                                Err(e) => {
                                    match e {
                                        crate::capture_runtime::CaptureRuntimeError::Engine(
                                            CaptureError::PartialTeardown {
                                                survivors,
                                                stop_errors,
                                                original,
                                            },
                                        ) => {
                                            log::error!(
                                                "resume: PartialTeardown — daemon may have orphan streams. \
                                                 survivors={survivors:?} stop_errors={stop_errors:?} original={original}"
                                            );
                                        }
                                        other => {
                                            log::error!(
                                                "resume: CaptureRuntime start failed: {other}"
                                            );
                                        }
                                    }
                                    // Reply with the current paused state and bail.
                                    let _ = cmd.reply.send(true);
                                    continue;
                                }
                            }
                        }
                        // Restore mic to persisted preference. The atom mirrors
                        // the result so `Status` reflects reality even when the
                        // restore fails (e.g. permission lost mid-session).
                        let wanted_mic = settings::read_mic_setting(storage.base_dir());
                        let outcome = audio_pipeline.set_microphone_enabled(wanted_mic);
                        let state =
                            ipc_handler::map_outcome(outcome, permissions::check_microphone());
                        mic_state_atom
                            .store(state as u8, std::sync::atomic::Ordering::Release);
                        log::info!(
                            "resume: restored mic to {wanted_mic}, result={state:?}"
                        );
                        capture_paused.store(false, Ordering::Release);
                        settings::write_capture_paused(storage.base_dir(), false);
                        false
                    }
                };
                let _ = cmd.reply.send(new_paused);
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
    cancel.cancel();

    // Stop the IPC server before the capture engine: stop accepting
    // requests and await socket cleanup.
    ipc_server.shutdown().await;

    // Stop the capture runtime FIRST (if active) — stops SCStream and joins
    // the downstream store/ocr tasks. Must run before `audio_pipeline.stop()`
    // so the handler Retained ref is released and the audio buffer channel
    // can close. If no runtime is active (paused at shutdown), this is a
    // no-op.
    let mut poisoned = false;
    if let Some(rt) = capture_runtime.take() {
        match rt.stop().await {
            Ok(()) => log::info!("Capture runtime stopped cleanly"),
            Err(crate::capture_runtime::CaptureRuntimeError::Engine(
                CaptureError::PartialTeardown {
                    survivors,
                    stop_errors,
                    ..
                },
            )) => {
                poisoned = true;
                log::error!(
                    "runtime stop failed; entering Poisoned. survivors={survivors:?} \
                     stop_errors={stop_errors:?}"
                );
            }
            Err(e) => log::error!("runtime stop failed: {e}"),
        }
    }
    // The `Option<CaptureRuntime>` binding itself still carries the audio
    // token lifetime in its type. Drop it explicitly so `audio_pipeline`'s
    // immutable borrow is released before we call `stop()` (mutable). The
    // Option is None after `take()`, so this is otherwise a no-op.
    drop(capture_runtime);

    // Stop audio pipeline — encoding thread sees EOF, flushes segments.
    if let Err(e) = audio_pipeline.stop() {
        log::error!("Audio pipeline stop failed: {e}");
    }
    log::info!("Audio pipeline stopped");

    // Wait for bridge thread to finish
    bridge_handle
        .join()
        .map_err(|_| anyhow::anyhow!("audio bridge thread panicked"))?;

    // Wait for the audio store task (the capture/OCR tasks are owned and
    // joined by `CaptureRuntime::stop()` above).
    if let Err(e) = audio_store_handle.await {
        log::error!("Audio store task failed: {e}");
    }

    // Transcription drains LAST, and that ordering is load-bearing.
    // `audio_pipeline.stop()` above flushed the session's final partial segments
    // and `audio_store_handle` just persisted and enqueued them, so the queue is
    // only now complete. Dropping our sink Arc closes the channel — the store
    // task released its clone by finishing — and that close is what tells
    // `transcribe_loop` to drain and exit.
    drop(transcription_sink);
    if let Some(handle) = transcribe_handle {
        // Bounds how long we wait for the queue to drain — NOT how long exit takes.
        // Dropping the `JoinHandle` detaches rather than aborts, and `#[tokio::main]`'s
        // runtime drop blocks until every blocking worker finishes its current task.
        // So worst-case exit is DRAIN_GRACE *plus* one in-flight whisper call, spent
        // after the "stopped" line below; the same delay applies before the `exit(3)`
        // poison handoff. Kept short deliberately, to leave headroom inside launchd's
        // 20s default ExitTimeOut. Undrained rows keep transcript = NULL for a future
        // backfill, same as before.
        const DRAIN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);
        match tokio::time::timeout(DRAIN_GRACE, handle).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => log::error!("transcribe loop task failed: {e}"),
            Err(_) => log::warn!(
                "transcribe queue still draining after {}s — stopping the wait; any \
                 in-flight segment finishes during runtime teardown",
                DRAIN_GRACE.as_secs()
            ),
        }
    }

    log::info!("chronicle-daemon stopped");
    if poisoned {
        log::error!("daemon exiting with code 3 (engine poisoned)");
        std::process::exit(3);
    }
    Ok(())
}
