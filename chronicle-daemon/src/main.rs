mod pipeline;

use std::sync::Arc;

use anyhow::Result;
use chronicle_audio::{AudioConfig, AudioEngine};
use chronicle_capture::{CaptureConfig, CaptureEngine};
use chronicle_storage::{Storage, StorageConfig};

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    log::info!("chronicle-daemon starting");

    // --- Storage ---
    let storage = Arc::new(Storage::open(StorageConfig::default()).await?);

    // --- Screen capture pipeline ---
    let (mut engine, frame_rx) = CaptureEngine::start(CaptureConfig::default())?;
    log::info!("Capture engine started");

    let (ocr_tx, ocr_rx) = tokio::sync::mpsc::channel(1024);

    let store_storage = Arc::clone(&storage);
    let store_handle = tokio::spawn(pipeline::capture_store_loop(
        store_storage,
        frame_rx,
        ocr_tx,
    ));

    let ocr_storage = Arc::clone(&storage);
    let ocr_handle = tokio::spawn(pipeline::ocr_loop(ocr_storage, ocr_rx));

    // --- Audio pipeline ---
    let audio_staging_dir = storage.base_dir().join("audio-staging");
    std::fs::create_dir_all(&audio_staging_dir)?;

    let audio_config = AudioConfig {
        output_dir: audio_staging_dir,
        ..AudioConfig::default()
    };
    let mut audio_engine = AudioEngine::new(audio_config)?;
    let audio_segment_rx = audio_engine.start()?;
    log::info!("Audio engine started");

    // Bounded channel (64) with blocking_send — backpressure over data loss
    let (audio_tx, audio_rx) = tokio::sync::mpsc::channel(64);

    // Bridge thread: std::sync::mpsc → tokio::mpsc
    let bridge_handle = std::thread::Builder::new()
        .name("audio-bridge".into())
        .spawn(move || pipeline::bridge_audio_segments(audio_segment_rx, audio_tx))
        ?;

    let audio_storage = Arc::clone(&storage);
    let audio_store_handle = tokio::spawn(pipeline::audio_store_loop(audio_storage, audio_rx));

    // --- Shutdown ---
    tokio::signal::ctrl_c().await?;
    log::info!("Shutdown signal received");

    // Stop capture engine — closes frame channel so Task A drains and exits
    if let Err(e) = engine.stop() {
        log::error!("Capture engine stop failed: {e}");
    }
    drop(engine);
    log::info!("Capture engine stopped");

    // Stop audio engine — stops SCStream, flushes remaining segments, closes
    // sync channel. Bridge thread sees close, forwards remaining, exits.
    if let Err(e) = audio_engine.stop() {
        log::error!("Audio engine stop failed: {e}");
    }
    log::info!("Audio engine stopped");

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
