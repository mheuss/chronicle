use std::path::PathBuf;
use std::sync::Arc;

use chronicle_audio::CompletedSegment;
use chronicle_capture::{CapturedFrame, encode_heif, get_frontmost_app};
use chronicle_ocr::extract_text;
use chronicle_storage::{ScreenshotMetadata, Storage};
use tokio::sync::mpsc;

const HEIF_QUALITY: f64 = 0.65;

/// Receive captured frames, encode to HEIF, store metadata in the database,
/// and forward (row_id, image_path) to the OCR task.
///
/// Runs until the frame channel closes (capture engine stopped).
pub async fn capture_store_loop(
    storage: Arc<Storage>,
    mut frame_rx: mpsc::Receiver<CapturedFrame>,
    ocr_tx: mpsc::Sender<(i64, PathBuf)>,
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
    ocr_tx: &mpsc::Sender<(i64, PathBuf)>,
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
    encode_heif(&frame.image_buffer, &image_path, HEIF_QUALITY)?;

    // 5. Insert DB record — clean up the HEIF file if this fails so we
    //    don't accumulate orphaned files on disk.
    let row_id = match storage
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
        .await
    {
        Ok(id) => id,
        Err(e) => {
            let _ = std::fs::remove_file(&image_path);
            return Err(e.into());
        }
    };

    // 6. Forward to OCR task (best-effort, non-blocking)
    match ocr_tx.try_send((row_id, image_path)) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            log::warn!("OCR channel full — screenshot {row_id} will not be OCR'd");
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            log::warn!("OCR channel closed — screenshot {row_id} will not be OCR'd");
        }
    }

    log::debug!(
        "Stored screenshot {row_id} for display {}",
        frame.display_id
    );

    Ok(())
}

/// Run OCR on stored screenshots and index the extracted text.
///
/// Runs until the OCR channel closes (capture→store loop exited).
pub async fn ocr_loop(
    storage: Arc<Storage>,
    mut ocr_rx: mpsc::Receiver<(i64, PathBuf)>,
) {
    while let Some((row_id, image_path)) = ocr_rx.recv().await {
        let path = image_path.clone();
        let result = tokio::task::spawn_blocking(move || extract_text(&path)).await;

        match result {
            Ok(Ok(text)) => {
                if !text.is_empty()
                    && let Err(e) = storage.update_ocr_text(row_id, text).await
                {
                    log::error!("Failed to store OCR text for screenshot {row_id}: {e}");
                }
            }
            Ok(Err(e)) => {
                log::warn!("OCR failed for screenshot {row_id}: {e}");
            }
            Err(e) => {
                log::error!("OCR task panicked for screenshot {row_id}: {e}");
            }
        }
    }
    log::info!("OCR loop exiting (channel closed)");
}

/// Bridge std::sync::mpsc to tokio::mpsc for audio segments.
///
/// Runs on a dedicated OS thread. Reads from the sync receiver and
/// forwards to the tokio sender via `blocking_send`. Uses blocking_send
/// (not try_send) because audio segments are 30-second recordings —
/// dropping one means a gap in recorded audio.
///
/// Exits when the sync channel closes (audio engine stopped).
///
/// # Panics
///
/// Must be called from a dedicated OS thread, not from within a tokio
/// runtime. `blocking_send` panics if called inside an async context.
pub fn bridge_audio_segments(
    sync_rx: std::sync::mpsc::Receiver<CompletedSegment>,
    async_tx: mpsc::Sender<CompletedSegment>,
) {
    while let Ok(segment) = sync_rx.recv() {
        if async_tx.blocking_send(segment).is_err() {
            log::info!("Audio bridge: tokio channel closed, stopping");
            break;
        }
    }
    log::info!("Audio bridge thread exiting (sync channel closed)");
}

#[cfg(test)]
mod tests {
    use super::*;
    use chronicle_storage::StorageConfig;
    use tempfile::tempdir;

    /// Helper: open a Storage backed by a temp directory.
    async fn temp_storage() -> (Arc<Storage>, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let config = StorageConfig {
            base_dir: dir.path().to_path_buf(),
            pool_size: 2,
        };
        let storage = Arc::new(Storage::open(config).await.unwrap());
        (storage, dir)
    }

    /// Helper: insert a screenshot record so we have a valid row_id.
    async fn insert_test_screenshot(storage: &Storage, image_path: &str, timestamp: i64) -> i64 {
        storage
            .insert_screenshot(ScreenshotMetadata {
                timestamp,
                display_id: "display1".into(),
                app_name: None,
                app_bundle_id: None,
                window_title: None,
                image_path: image_path.into(),
                ocr_text: None,
                phash: None,
                resolution: None,
            })
            .await
            .unwrap()
    }

    /// Path to the OCR test fixture with known text.
    fn sample_text_image() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("crates/ocr/tests/fixtures/sample-text.png")
    }

    /// Path to the OCR test fixture with no text (blank).
    fn blank_image() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("crates/ocr/tests/fixtures/blank.png")
    }

    #[tokio::test]
    async fn ocr_loop_stores_extracted_text() {
        let (storage, _dir) = temp_storage().await;
        let image_path = sample_text_image();
        let row_id = insert_test_screenshot(&storage, image_path.to_str().unwrap(), 1_700_000_000_000).await;

        let (ocr_tx, ocr_rx) = mpsc::channel(32);
        ocr_tx.try_send((row_id, image_path)).unwrap();
        drop(ocr_tx); // close channel so loop exits after processing

        ocr_loop(storage.clone(), ocr_rx).await;

        let screenshot = storage.get_screenshot(row_id).await.unwrap();
        assert!(
            screenshot.ocr_text.is_some(),
            "expected OCR text to be stored for screenshot with known text"
        );
        let text = screenshot.ocr_text.unwrap();
        assert!(
            !text.is_empty(),
            "expected non-empty OCR text"
        );
    }

    #[tokio::test]
    async fn ocr_loop_skips_empty_text() {
        let (storage, _dir) = temp_storage().await;
        let image_path = blank_image();
        let row_id = insert_test_screenshot(&storage, image_path.to_str().unwrap(), 1_700_000_001_000).await;

        let (ocr_tx, ocr_rx) = mpsc::channel(32);
        ocr_tx.try_send((row_id, image_path)).unwrap();
        drop(ocr_tx);

        ocr_loop(storage.clone(), ocr_rx).await;

        let screenshot = storage.get_screenshot(row_id).await.unwrap();
        assert!(
            screenshot.ocr_text.is_none(),
            "expected no OCR text stored for blank image, got: {:?}",
            screenshot.ocr_text
        );
    }

    #[tokio::test]
    async fn ocr_loop_continues_on_missing_image() {
        let (storage, _dir) = temp_storage().await;
        let missing_path = PathBuf::from("/nonexistent/image.png");
        let row_id_bad = insert_test_screenshot(&storage, "/nonexistent/image.png", 1_700_000_002_000).await;

        let image_path = sample_text_image();
        let row_id_good = insert_test_screenshot(&storage, image_path.to_str().unwrap(), 1_700_000_003_000).await;

        let (ocr_tx, ocr_rx) = mpsc::channel(32);
        // Send bad path first, then good path
        ocr_tx.try_send((row_id_bad, missing_path)).unwrap();
        ocr_tx.try_send((row_id_good, image_path)).unwrap();
        drop(ocr_tx);

        ocr_loop(storage.clone(), ocr_rx).await;

        // Bad one should have no OCR text
        let bad = storage.get_screenshot(row_id_bad).await.unwrap();
        assert!(bad.ocr_text.is_none(), "no OCR text for missing image");

        // Good one should still be processed
        let good = storage.get_screenshot(row_id_good).await.unwrap();
        assert!(
            good.ocr_text.is_some(),
            "OCR text should be stored even after a previous failure"
        );
    }

    #[tokio::test]
    async fn ocr_loop_exits_on_empty_channel() {
        let (storage, _dir) = temp_storage().await;
        let (_ocr_tx, ocr_rx) = mpsc::channel(32);
        drop(_ocr_tx); // close immediately

        // Should return promptly without hanging
        ocr_loop(storage, ocr_rx).await;
    }

    #[tokio::test]
    async fn bridge_audio_segments_forwards_to_tokio_channel() {
        use chronicle_audio::AudioSource;

        let (sync_tx, sync_rx) = std::sync::mpsc::channel::<CompletedSegment>();
        let (async_tx, mut async_rx) = mpsc::channel::<CompletedSegment>(16);

        let bridge = std::thread::Builder::new()
            .name("test-bridge".into())
            .spawn(move || bridge_audio_segments(sync_rx, async_tx))
            .unwrap();

        sync_tx
            .send(CompletedSegment {
                source: AudioSource::Microphone,
                path: PathBuf::from("/tmp/test.opus"),
                start_timestamp: 1_700_000_000_000,
                end_timestamp: 1_700_000_030_000,
            })
            .unwrap();

        let received = async_rx.recv().await.unwrap();
        assert_eq!(received.source, AudioSource::Microphone);
        assert_eq!(received.start_timestamp, 1_700_000_000_000);
        assert_eq!(received.end_timestamp, 1_700_000_030_000);
        assert_eq!(received.path, PathBuf::from("/tmp/test.opus"));

        drop(sync_tx);
        bridge.join().unwrap();
        assert!(async_rx.recv().await.is_none());
    }
}
