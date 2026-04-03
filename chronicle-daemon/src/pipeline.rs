use std::path::PathBuf;
use std::sync::Arc;

use chronicle_audio::CompletedSegment;
use chronicle_capture::{CapturedFrame, encode_heif, get_frontmost_app};
use chronicle_ocr::extract_text;
use chronicle_storage::{AudioSegmentMetadata, ScreenshotMetadata, Storage};
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
    encode_heif(frame.sample_buffer.as_ref(), &image_path, HEIF_QUALITY)?;

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

/// Receive completed audio segments, move files from staging to permanent
/// storage, and insert database records.
///
/// Runs until the audio channel closes (bridge thread exited).
pub async fn audio_store_loop(
    storage: Arc<Storage>,
    mut segment_rx: mpsc::Receiver<CompletedSegment>,
) {
    while let Some(segment) = segment_rx.recv().await {
        if let Err(e) = process_audio_segment(&storage, &segment).await {
            log::error!(
                "Failed to store audio segment (source={}, ts={}): {e}",
                segment.source.as_str(),
                segment.start_timestamp
            );
        }
    }
    log::info!("Audio store loop exiting (channel closed)");
}

async fn process_audio_segment(
    storage: &Storage,
    segment: &CompletedSegment,
) -> anyhow::Result<()> {
    // 1. Allocate permanent path via storage (sanitizes source identifier)
    let dest_path = storage
        .allocate_audio_path(segment.start_timestamp, segment.source.as_str())
        .await?;

    // 2. Move from staging to permanent location (atomic rename, same filesystem).
    //    rename(2) on the same filesystem is a metadata-only operation (microseconds).
    //    Both directories are under the Chronicle data dir, so no cross-mount copy.
    std::fs::rename(&segment.path, &dest_path)?;

    // 3. Insert DB record — clean up dest file if insert fails
    match storage
        .insert_audio_segment(AudioSegmentMetadata {
            start_timestamp: segment.start_timestamp,
            end_timestamp: segment.end_timestamp,
            source: segment.source.as_str().to_string(),
            audio_path: dest_path.to_string_lossy().into_owned(),
            transcript: None,
            whisper_model: None,
            language: None,
        })
        .await
    {
        Ok(row_id) => {
            log::debug!(
                "Stored audio segment {row_id} (source={}, ts={})",
                segment.source.as_str(),
                segment.start_timestamp
            );
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&dest_path);
            Err(e.into())
        }
    }
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
    async fn audio_store_loop_stores_segment() {
        use chronicle_audio::{AudioSource, CompletedSegment};

        let (storage, dir) = temp_storage().await;

        let staging_dir = dir.path().join("audio-staging");
        std::fs::create_dir_all(&staging_dir).unwrap();
        let staging_file = staging_dir.join("test_segment.opus");
        std::fs::write(&staging_file, b"fake opus data").unwrap();

        let segment = CompletedSegment {
            source: AudioSource::Microphone,
            path: staging_file.clone(),
            start_timestamp: 1_700_000_000_000,
            end_timestamp: 1_700_000_030_000,
        };

        let (tx, rx) = mpsc::channel(16);
        tx.send(segment).await.unwrap();
        drop(tx);

        audio_store_loop(storage.clone(), rx).await;

        assert!(!staging_file.exists(), "staging file should have been moved");

        let audio = storage.get_audio_segment(1).await.unwrap();
        assert_eq!(audio.start_timestamp, 1_700_000_000_000);
        assert_eq!(audio.end_timestamp, 1_700_000_030_000);
        assert_eq!(audio.source, "mic");
        assert!(audio.transcript.is_none());

        let perm_path = std::path::Path::new(&audio.audio_path);
        assert!(perm_path.exists(), "permanent audio file should exist");
        assert_eq!(std::fs::read(perm_path).unwrap(), b"fake opus data");
    }

    #[tokio::test]
    async fn audio_store_loop_continues_on_missing_file() {
        use chronicle_audio::{AudioSource, CompletedSegment};

        let (storage, dir) = temp_storage().await;

        let staging_dir = dir.path().join("audio-staging");
        std::fs::create_dir_all(&staging_dir).unwrap();
        let good_file = staging_dir.join("good.opus");
        std::fs::write(&good_file, b"good data").unwrap();

        let bad_segment = CompletedSegment {
            source: AudioSource::System,
            path: PathBuf::from("/nonexistent/bad.opus"),
            start_timestamp: 1_700_000_000_000,
            end_timestamp: 1_700_000_030_000,
        };
        let good_segment = CompletedSegment {
            source: AudioSource::Microphone,
            path: good_file,
            start_timestamp: 1_700_000_060_000,
            end_timestamp: 1_700_000_090_000,
        };

        let (tx, rx) = mpsc::channel(16);
        tx.send(bad_segment).await.unwrap();
        tx.send(good_segment).await.unwrap();
        drop(tx);

        audio_store_loop(storage.clone(), rx).await;

        let audio = storage.get_audio_segment(1).await.unwrap();
        assert_eq!(audio.source, "mic");
        assert_eq!(audio.start_timestamp, 1_700_000_060_000);
    }

    #[tokio::test]
    async fn audio_store_loop_exits_on_empty_channel() {
        use chronicle_audio::CompletedSegment;

        let (storage, _dir) = temp_storage().await;
        let (_tx, rx) = mpsc::channel::<CompletedSegment>(16);
        drop(_tx);
        audio_store_loop(storage, rx).await;
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
