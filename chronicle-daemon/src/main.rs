mod ipc_handler;
mod permissions;
mod pipeline;

use std::sync::Arc;

use anyhow::Result;
use chronicle_audio::{AudioConfig, AudioPipeline, CHANNEL_COUNT, SAMPLE_RATE};
use chronicle_capture::{AudioOutputConfig, CaptureConfig, CaptureEngine};
use chronicle_storage::{Storage, StorageConfig};

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    log::info!("chronicle-daemon starting");

    // --- Permission preflight ---
    let _mic_status = permissions::preflight()?;

    // --- Storage ---
    let storage = Arc::new(Storage::open(StorageConfig::default()).await?);

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
    let capture_config = CaptureConfig {
        audio: Some(AudioOutputConfig {
            handler: audio_pipeline
                .handler()
                .ok_or_else(|| anyhow::anyhow!("audio handler unavailable"))?,
            queue: audio_pipeline.queue(),
            sample_rate: SAMPLE_RATE,
            channel_count: CHANNEL_COUNT,
            capture_microphone: false, // HEU-329: mic off by default
        }),
        ..Default::default()
    };
    let (mut engine, frame_rx) = CaptureEngine::start(capture_config)?;
    log::info!("Capture engine started (audio on primary display)");

    let (ocr_tx, ocr_rx) = tokio::sync::mpsc::channel(1024);

    let store_storage = Arc::clone(&storage);
    let store_handle = tokio::spawn(pipeline::capture_store_loop(
        store_storage,
        frame_rx,
        ocr_tx,
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
    let audio_store_handle = tokio::spawn(pipeline::audio_store_loop(audio_storage, audio_rx));

    // --- Shutdown ---
    tokio::signal::ctrl_c().await?;
    log::info!("Shutdown signal received");

    // Stop capture engine FIRST — stops SCStream, no more audio callbacks.
    // Must drop before audio_pipeline.stop() so the handler Retained ref
    // is released and the buffer channel can close.
    if let Err(e) = engine.stop() {
        log::error!("Capture engine stop failed: {e}");
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
    Ok(())
}
