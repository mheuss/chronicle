mod capture_runtime;
mod ipc_handler;
mod permissions;
mod pipeline;
mod settings;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Result;
use arc_swap::ArcSwap;
use chronicle_audio::{AudioConfig, AudioPipeline, CHANNEL_COUNT, SAMPLE_RATE};
use chronicle_capture::{CaptureConfig, CaptureEngine, CaptureError};
use chronicle_ipc::{CancellationToken, IpcServer};
use chronicle_storage::{Storage, StorageConfig};

/// Parse a `retention_days` setting from the config table. An unset key uses
/// the 30-day default; an unparseable value also falls back to 30 with a warn.
fn parse_retention_days(s: Option<String>) -> u32 {
    match s {
        Some(v) => v.parse::<u32>().unwrap_or_else(|_| {
            log::warn!("settings: invalid retention_days={v:?}, defaulting to 30");
            30
        }),
        None => {
            log::info!("settings: retention_days not configured, defaulting to 30");
            30
        }
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
    let storage_db_size = Arc::new(AtomicU64::new(0));
    // The richer snapshot lives alongside the old atom until Task 8 swaps the
    // Status read path over to it. The refresher below populates both in lockstep.
    let storage_status_snapshot = Arc::new(ArcSwap::from_pointee(
        crate::ipc_handler::StorageStatusSnapshot::default(),
    ));

    let metadata_provider = Arc::new(
        crate::pipeline::metadata::CachingAppMetadataProvider::with_default_clock(
            chronicle_capture::get_frontmost_app,
            Duration::from_millis(250),
        ),
    );

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
    let handler = ipc_handler::DaemonHandler::new(
        Arc::clone(&counters),
        Arc::clone(&capture_snapshot),
        Arc::clone(&storage_db_size),
        mic_tx,
        Arc::clone(&mic_state_atom),
        Arc::clone(&mic_ready),
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
    // CaptureEngine::start consumes the token, but the engine holds the
    // `AudioHandlerToken` for its whole life. `set_microphone_enabled(&self)`
    // only borrows `audio_pipeline` shared, so the event loop below can toggle
    // the mic while the engine runs; the pipeline is still stopped at shutdown.
    let capture_config = CaptureConfig {
        audio: audio_pipeline.token(SAMPLE_RATE, CHANNEL_COUNT),
        ..Default::default()
    };
    let (mut engine, frame_rx) = match CaptureEngine::start(capture_config) {
        Ok(pair) => pair,
        Err(CaptureError::PartialTeardown {
            survivors,
            stop_errors,
            original,
        }) => {
            log::error!(
                "capture startup rollback failed; exiting. survivors={survivors:?} \
                 stop_errors={stop_errors:?} original={original}"
            );
            std::process::exit(3);
        }
        Err(e) => return Err(e.into()),
    };
    log::info!("Capture engine started (audio on primary display)");

    // Restore the persisted microphone setting. The mic is off by default;
    // only an explicit prior "on" turns it back on at startup.
    if settings::read_mic_setting(storage.base_dir()) {
        let outcome = audio_pipeline.set_microphone_enabled(true);
        let state = ipc_handler::map_outcome(outcome, permissions::check_microphone());
        log::info!("restored persisted mic setting: result={state:?}");
        // A failed restore updates the runtime atom but deliberately leaves the
        // persisted setting untouched so the next daemon start retries it.
        mic_state_atom.store(state as u8, std::sync::atomic::Ordering::Release);
    }

    let (ocr_tx, ocr_rx) = tokio::sync::mpsc::channel(1024);

    let ocr_sink: Arc<dyn crate::pipeline::sinks::OcrSink> =
        Arc::new(crate::pipeline::sinks::TokioOcrSink(ocr_tx));

    let store_storage = Arc::clone(&storage);
    let store_counters = Arc::clone(&counters);
    let store_meta = Arc::clone(&metadata_provider);
    let store_handle = tokio::spawn(pipeline::capture_store_loop(
        store_storage,
        frame_rx,
        ocr_sink,
        store_meta,
        store_counters,
    ));

    let ocr_storage = Arc::clone(&storage);
    let ocr_handle = tokio::spawn(pipeline::ocr_loop(ocr_storage, ocr_rx));

    // Bounded channel (64) with blocking_send — backpressure over data loss
    let (audio_tx, audio_rx) = tokio::sync::mpsc::channel(64);

    // Bridge thread: std::sync::mpsc → tokio::mpsc
    let bridge_handle = std::thread::Builder::new()
        .name("audio-bridge".into())
        .spawn(move || pipeline::bridge_audio_segments(audio_segment_rx, audio_tx))?;

    let audio_storage = Arc::clone(&storage);
    let audio_counters = Arc::clone(&counters);
    let audio_store_handle = tokio::spawn(pipeline::audio_store_loop(
        audio_storage,
        audio_rx,
        audio_counters,
    ));

    // Capture status refresher (1 Hz). Reads engine atomics so state and
    // active_displays reflect shutdown/poisoning transitions.
    let probe = engine.status_probe();
    let refresher_cancel = cancel.clone();
    let refresher_snapshot = Arc::clone(&capture_snapshot);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                _ = refresher_cancel.cancelled() => break,
                _ = ticker.tick() => {
                    let snap = probe.snapshot();
                    let snapshot = crate::ipc_handler::CaptureStatusSnapshot {
                        state: Some(snap.state),
                        active_displays: snap.active_displays,
                        frames_captured: snap.frames_captured,
                        frames_dropped: snap.frames_dropped,
                    };
                    refresher_snapshot.store(Arc::new(snapshot));
                }
            }
        }
    });

    // Prime the storage status snapshot before the refresher task spawns, so the
    // snapshot has real data when the first 30-second tick eventually arrives.
    // First-load cost is one directory walk; subsequent reads are O(1) ArcSwap reads.
    {
        let snapshot = match storage.status().await {
            Ok(s) => {
                // Keep the legacy atom in sync at boot so Status doesn't lie
                // for the first 30 seconds. The refresher (below) keeps both
                // in sync after that. Removed in T8 when DaemonHandler::new
                // switches to read from the snapshot.
                storage_db_size.store(s.db_size_bytes, Ordering::Relaxed);
                crate::ipc_handler::StorageStatusSnapshot {
                    db_size_bytes: s.db_size_bytes,
                    total_disk_usage_bytes: s.total_disk_usage_bytes,
                    screenshot_count: s.screenshot_count,
                    audio_segment_count: s.audio_segment_count,
                    oldest_entry_ms: s.oldest_entry,
                    retention_days: parse_retention_days(
                        storage.get_config("retention_days").await.ok().flatten(),
                    ),
                }
            }
            Err(e) => {
                log::warn!("initial storage status read failed: {e}");
                crate::ipc_handler::StorageStatusSnapshot::default()
            }
        };
        storage_status_snapshot.store(Arc::new(snapshot));
    }

    let storage_refresher_storage = Arc::clone(&storage);
    let storage_refresher_snapshot = Arc::clone(&storage_status_snapshot);
    let storage_refresher_size = Arc::clone(&storage_db_size); // kept for now; removed in Task 8
    let storage_refresher_cancel = cancel.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(30));
        // First tick fires immediately; skip it so we don't double-prime.
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
                            // Keep the old atom in sync until Task 8 removes it.
                            storage_refresher_size.store(s.db_size_bytes, Ordering::Relaxed);
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
    // Open the readiness gate: from here the loop drains `mic_rx`, so the IPC
    // handler can forward mic toggles instead of rejecting them.
    mic_ready.store(true, Ordering::Release);
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
        }
    }
    // Drain any mic command that raced this shutdown signal: reply Error now so
    // the IPC handler returns immediately instead of waiting out its timeout.
    mic_rx.close();
    while let Ok(cmd) = mic_rx.try_recv() {
        let _ = cmd.reply.send(chronicle_ipc::MicState::Error);
    }
    cancel.cancel();

    // Stop the IPC server before the capture engine: stop accepting
    // requests and await socket cleanup.
    ipc_server.shutdown().await;

    // Stop capture engine FIRST — stops SCStream, no more audio callbacks.
    // Must drop before audio_pipeline.stop() so the handler Retained ref
    // is released and the buffer channel can close.
    let stop_result = engine.stop();
    let mut poisoned = false;
    match &stop_result {
        Ok(()) => log::info!("Capture engine stopped cleanly"),
        Err(CaptureError::PartialTeardown {
            survivors,
            stop_errors,
            ..
        }) => {
            poisoned = true;
            log::error!(
                "engine stop failed; entering Poisoned. survivors={survivors:?} \
                 stop_errors={stop_errors:?}"
            );
        }
        Err(e) => log::error!("engine stop failed: {e}"),
    }
    drop(engine);
    log::info!("Capture engine stopped");

    // Stop audio pipeline — encoding thread sees EOF, flushes segments.
    if let Err(e) = audio_pipeline.stop() {
        log::error!("Audio pipeline stop failed: {e}");
    }
    log::info!("Audio pipeline stopped");

    // Wait for bridge thread to finish
    bridge_handle
        .join()
        .map_err(|_| anyhow::anyhow!("audio bridge thread panicked"))?;

    // Wait for all async tasks to finish
    if let Err(e) = store_handle.await {
        log::error!("Capture→store task failed: {e}");
    }
    if let Err(e) = ocr_handle.await {
        log::error!("OCR task failed: {e}");
    }
    if let Err(e) = audio_store_handle.await {
        log::error!("Audio store task failed: {e}");
    }

    log::info!("chronicle-daemon stopped");
    if poisoned {
        log::error!("daemon exiting with code 3 (engine poisoned)");
        std::process::exit(3);
    }
    Ok(())
}
