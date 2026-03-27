mod pipeline;

use std::sync::Arc;

use anyhow::Result;
use chronicle_capture::{CaptureConfig, CaptureEngine};
use chronicle_storage::{Storage, StorageConfig};

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    log::info!("chronicle-daemon starting");

    // 1. Open storage
    let storage = Arc::new(Storage::open(StorageConfig::default()).await?);

    // 2. Start capture engine
    let (mut engine, frame_rx) = CaptureEngine::start(CaptureConfig::default())?;
    log::info!("Capture engine started");

    // 3. Create OCR channel (unbounded — OCR can fall behind)
    let (ocr_tx, ocr_rx) = tokio::sync::mpsc::unbounded_channel();

    // 4. Spawn Task A (capture→store)
    let store_storage = Arc::clone(&storage);
    let store_handle = tokio::spawn(pipeline::capture_store_loop(
        store_storage,
        frame_rx,
        ocr_tx,
    ));

    // 5. Spawn Task B (OCR)
    let ocr_storage = Arc::clone(&storage);
    let ocr_handle = tokio::spawn(pipeline::ocr_loop(ocr_storage, ocr_rx));

    // 6. Wait for ctrl-c
    tokio::signal::ctrl_c().await?;
    log::info!("Shutdown signal received");

    // 7. Stop capture engine — closes frame channel — Task A drains and exits
    engine.stop()?;
    log::info!("Capture engine stopped");

    // 8. Wait for both tasks to finish
    //    Task A draining drops ocr_tx → Task B drains and exits
    if let Err(e) = store_handle.await {
        log::error!("Capture→store task failed: {e}");
    }
    if let Err(e) = ocr_handle.await {
        log::error!("OCR task failed: {e}");
    }

    log::info!("chronicle-daemon stopped");
    Ok(())
}
