use std::path::PathBuf;
use std::sync::Arc;

use chronicle_capture::{CapturedFrame, encode_heif, get_frontmost_app};
use chronicle_storage::{ScreenshotMetadata, Storage};
use tokio::sync::mpsc;

/// Receive captured frames, encode to HEIF, store metadata in the database,
/// and forward (row_id, image_path) to the OCR task.
///
/// Runs until the frame channel closes (capture engine stopped).
pub async fn capture_store_loop(
    storage: Arc<Storage>,
    mut frame_rx: mpsc::Receiver<CapturedFrame>,
    ocr_tx: mpsc::UnboundedSender<(i64, PathBuf)>,
) {
    while let Some(frame) = frame_rx.recv().await {
        if let Err(e) = process_frame(&storage, &frame, &ocr_tx).await {
            log::error!(
                "Failed to process frame (display={}, ts={}): {e}",
                frame.display_id,
                frame.timestamp
            );
        }
    }
    log::info!("Capture→store loop exiting (frame channel closed)");
}

async fn process_frame(
    storage: &Storage,
    frame: &CapturedFrame,
    ocr_tx: &mpsc::UnboundedSender<(i64, PathBuf)>,
) -> anyhow::Result<()> {
    // 1. Grab app metadata (sync, fast)
    let metadata = get_frontmost_app();

    // 2. Build resolution string
    let resolution = format!("{}x{}", frame.width, frame.height);

    // 3. Allocate storage path
    let display_id = frame.display_id.to_string();
    let image_path = storage
        .allocate_screenshot_path(frame.timestamp, &display_id)
        .await?;

    // 4. Encode HEIF (CPU-bound — run off the async executor)
    //
    // encode_heif takes references that aren't Send, so we call it directly.
    // The capture loop is the only consumer and frames arrive at ~0.5 fps,
    // so briefly blocking the task is acceptable.
    encode_heif(&frame.image_buffer, &image_path, 0.65)?;

    // 5. Insert DB record
    let row_id = storage
        .insert_screenshot(ScreenshotMetadata {
            timestamp: frame.timestamp,
            display_id,
            app_name: metadata.app_name,
            app_bundle_id: metadata.app_bundle_id,
            window_title: metadata.window_title,
            image_path: image_path.to_string_lossy().into_owned(),
            ocr_text: None,
            phash: None,
            resolution: Some(resolution),
        })
        .await?;

    // 6. Forward to OCR task (best-effort)
    if ocr_tx.send((row_id, image_path)).is_err() {
        log::warn!("OCR channel closed — screenshot {row_id} will not be OCR'd");
    }

    log::debug!(
        "Stored screenshot {row_id} for display {}",
        frame.display_id
    );

    Ok(())
}
