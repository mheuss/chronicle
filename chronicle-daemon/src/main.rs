mod ipc_handler;
mod permissions;
mod pipeline;

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use anyhow::Result;
use arc_swap::ArcSwap;
use chronicle_audio::{AudioConfig, AudioPipeline, CHANNEL_COUNT, SAMPLE_RATE};
use chronicle_capture::{CaptureConfig, CaptureEngine, CaptureError};
use chronicle_ipc::{CancellationToken, IpcServer};
use chronicle_storage::{Storage, StorageConfig};

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

    let metadata_provider = Arc::new(
        crate::pipeline::metadata::CachingAppMetadataProvider::with_default_clock(
            chronicle_capture::get_frontmost_app,
            Duration::from_millis(250),
        ),
    );

    // --- IPC server ---
    let cancel = CancellationToken::new();
    let socket_path = storage.base_dir().join("chronicle.sock");
    let handler = ipc_handler::DaemonHandler::new(
        Arc::clone(&counters),
        Arc::clone(&capture_snapshot),
        Arc::clone(&storage_db_size),
    );
    let _ipc_server = IpcServer::start(&socket_path, handler, cancel.clone()).await?;
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
    // Hold the token locally; CaptureEngine::start consumes it and the
    // borrow ends when `start` returns. After this block, `audio_pipeline`
    // is borrow-free and can be stopped at shutdown.
    let capture_config = CaptureConfig {
        audio: audio_pipeline.token(SAMPLE_RATE, CHANNEL_COUNT, false),
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

    let storage_refresher_storage = Arc::clone(&storage);
    let storage_refresher_size = Arc::clone(&storage_db_size);
    let storage_refresher_cancel = cancel.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(30));
        loop {
            tokio::select! {
                _ = storage_refresher_cancel.cancelled() => break,
                _ = ticker.tick() => {
                    match storage_refresher_storage.status().await {
                        Ok(status) => storage_refresher_size.store(
                            status.db_size_bytes,
                            std::sync::atomic::Ordering::Relaxed,
                        ),
                        Err(e) => log::warn!("storage status refresh failed: {e}"),
                    }
                }
            }
        }
    });

    // --- Shutdown ---
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        res = tokio::signal::ctrl_c() => {
            res?;
            log::info!("SIGINT received, shutting down");
        }
        _ = sigterm.recv() => {
            log::info!("SIGTERM received, shutting down");
        }
    }
    cancel.cancel();

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
