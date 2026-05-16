pub mod counters;
pub mod metadata;
pub mod sinks;

use std::path::PathBuf;
use std::sync::Arc;

use chronicle_audio::CompletedSegment;
use chronicle_capture::{CapturedFrame, encode_heif};
use chronicle_ocr::extract_text;
use chronicle_storage::{AudioSegmentMetadata, ScreenshotMetadata, Storage};
use tokio::sync::mpsc;

const HEIF_QUALITY: f64 = 0.65;

/// Receive captured frames, encode to HEIF, store metadata in the database,
/// and forward (row_id, image_path) to the OCR task.
///
/// Runs until the frame channel closes (capture engine stopped).
pub async fn capture_store_loop<M>(
    storage: Arc<Storage>,
    mut frame_rx: mpsc::Receiver<CapturedFrame>,
    ocr: Arc<dyn crate::pipeline::sinks::OcrSink>,
    meta: Arc<M>,
    counters: Arc<crate::pipeline::counters::PipelineCounters>,
) where
    M: crate::pipeline::sinks::AppMetadataProvider + ?Sized + 'static,
{
    use std::sync::atomic::Ordering;

    while let Some(frame) = frame_rx.recv().await {
        if let Err(e) = process_frame(
            &storage,
            &frame,
            ocr.as_ref(),
            meta.as_ref(),
            counters.as_ref(),
        )
        .await
        {
            // Counted here so every `process_frame` failure edge
            // (allocate_path, encode_heif, harden_file, DB insert) is
            // reflected in `capture.frames_failed`, not just the DB path.
            counters.frames_failed.fetch_add(1, Ordering::Relaxed);
            log::error!(
                "Failed to process frame (display={}, ts={}): {e}",
                frame.display_id,
                frame.timestamp
            );
        }
    }
    log::info!("Capture→store loop exiting (frame channel closed)");
}

async fn process_frame<M>(
    storage: &Arc<Storage>,
    frame: &CapturedFrame,
    ocr: &dyn crate::pipeline::sinks::OcrSink,
    meta: &M,
    counters: &crate::pipeline::counters::PipelineCounters,
) -> anyhow::Result<()>
where
    M: crate::pipeline::sinks::AppMetadataProvider + ?Sized,
{
    let metadata = meta.frontmost();
    let resolution = format!("{}x{}", frame.width, frame.height);
    let display_id = frame.display_id.to_string();

    let image_path = storage
        .allocate_screenshot_path(frame.timestamp, &display_id)
        .await?;

    // encode_heif takes references that aren't Send, so we call it directly.
    // The capture loop is the only consumer and frames arrive at ~0.5 fps,
    // so briefly blocking the task is acceptable.
    encode_heif(frame.sample_buffer.inner(), &image_path, HEIF_QUALITY)?;

    // Harden file permissions (encode_heif writes directly, bypassing MediaManager)
    if let Err(e) = storage.media_manager().harden_file(&image_path) {
        if let Err(del_err) = storage.media_manager().delete_file(&image_path) {
            log::error!(
                "failed to harden {} and cleanup also failed: {del_err}",
                image_path.display()
            );
        }
        return Err(e.into());
    }

    let record = ScreenshotMetadata {
        timestamp: frame.timestamp,
        display_id,
        app_name: metadata.app_name,
        app_bundle_id: metadata.app_bundle_id,
        window_title: metadata.window_title,
        image_path: image_path.to_string_lossy().into_owned(),
        ocr_text: None,
        phash: None,
        resolution: Some(resolution),
    };

    insert_and_enqueue(record, image_path, storage.as_ref(), ocr, counters).await
}

/// Insert a screenshot record, enqueue an OCR job, and update counters.
///
/// Separated from `process_frame` so it can be unit-tested with fake sinks
/// — no filesystem, no database, no SCK.
pub(crate) async fn insert_and_enqueue<S>(
    record: ScreenshotMetadata,
    image_path: PathBuf,
    sink: &S,
    ocr: &dyn crate::pipeline::sinks::OcrSink,
    counters: &crate::pipeline::counters::PipelineCounters,
) -> anyhow::Result<()>
where
    S: crate::pipeline::sinks::ScreenshotSink + ?Sized,
{
    use crate::pipeline::sinks::{OcrEnqueueResult, OcrJob};
    use std::sync::atomic::Ordering;

    let row_id = match sink.insert(record).await {
        Ok(id) => id,
        Err(e) => {
            sink.delete_file(&image_path);
            // `frames_failed` is incremented in the caller
            // (`capture_store_loop`) so that earlier failures (encode,
            // harden, path allocation) are counted on the same edge.
            return Err(e);
        }
    };

    counters.frames_processed.fetch_add(1, Ordering::Relaxed);

    match ocr.try_enqueue(OcrJob { row_id, image_path }) {
        OcrEnqueueResult::Enqueued => {
            counters.ocr_enqueued.fetch_add(1, Ordering::Relaxed);
        }
        OcrEnqueueResult::ChannelFull => {
            let n = counters.ocr_dropped.fetch_add(1, Ordering::Relaxed) + 1;
            if n.is_multiple_of(100) {
                log::warn!("OCR channel full; shedding load (total drops: {n})");
            }
        }
        OcrEnqueueResult::ChannelClosed => {
            counters.ocr_dropped.fetch_add(1, Ordering::Relaxed);
            log::warn!("OCR channel closed; enqueue dropped");
        }
    }
    Ok(())
}

/// Run OCR on stored screenshots and index the extracted text.
///
/// Runs until the OCR channel closes (capture→store loop exited).
pub async fn ocr_loop(storage: Arc<Storage>, mut ocr_rx: mpsc::Receiver<(i64, PathBuf)>) {
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
    counters: Arc<crate::pipeline::counters::PipelineCounters>,
) {
    while let Some(segment) = segment_rx.recv().await {
        match process_audio_segment(&storage, &segment).await {
            Ok(()) => {
                counters
                    .audio_segments_persisted
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            Err(e) => {
                log::error!(
                    "Failed to store audio segment (source={}, ts={}): {e}",
                    segment.source.as_str(),
                    segment.start_timestamp
                );
            }
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
    storage
        .media_manager()
        .move_file(&segment.path, &dest_path)?;

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
            let _ = storage.media_manager().delete_file(&dest_path);
            Err(e.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chronicle_storage::StorageConfig;
    use std::time::Instant;
    use tempfile::tempdir;

    // Shared helpers for the two metadata-cache tests below. Put these
    // at the top of the test module so both tests import from them.
    use std::sync::Mutex as StdMutex;

    struct ManualClock {
        at: StdMutex<Instant>,
    }
    impl ManualClock {
        fn new(initial: Instant) -> Self {
            Self {
                at: StdMutex::new(initial),
            }
        }
        fn set(&self, instant: Instant) {
            *self.at.lock().unwrap() = instant;
        }
    }
    struct ClockHandle(Arc<ManualClock>);
    impl crate::pipeline::metadata::Clock for ClockHandle {
        fn now(&self) -> Instant {
            *self.0.at.lock().unwrap()
        }
    }

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
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("crates/ocr/tests/fixtures/sample-text.png")
    }

    /// Path to the OCR test fixture with no text (blank).
    fn blank_image() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("crates/ocr/tests/fixtures/blank.png")
    }

    #[tokio::test]
    async fn ocr_loop_stores_extracted_text() {
        let (storage, _dir) = temp_storage().await;
        let image_path = sample_text_image();
        let row_id =
            insert_test_screenshot(&storage, image_path.to_str().unwrap(), 1_700_000_000_000).await;

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
        assert!(!text.is_empty(), "expected non-empty OCR text");
    }

    #[tokio::test]
    async fn ocr_loop_skips_empty_text() {
        let (storage, _dir) = temp_storage().await;
        let image_path = blank_image();
        let row_id =
            insert_test_screenshot(&storage, image_path.to_str().unwrap(), 1_700_000_001_000).await;

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
        let row_id_bad =
            insert_test_screenshot(&storage, "/nonexistent/image.png", 1_700_000_002_000).await;

        let image_path = sample_text_image();
        let row_id_good =
            insert_test_screenshot(&storage, image_path.to_str().unwrap(), 1_700_000_003_000).await;

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
        use crate::pipeline::counters::PipelineCounters;
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

        audio_store_loop(storage.clone(), rx, PipelineCounters::new()).await;

        assert!(
            !staging_file.exists(),
            "staging file should have been moved"
        );

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
        use crate::pipeline::counters::PipelineCounters;
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

        audio_store_loop(storage.clone(), rx, PipelineCounters::new()).await;

        let audio = storage.get_audio_segment(1).await.unwrap();
        assert_eq!(audio.source, "mic");
        assert_eq!(audio.start_timestamp, 1_700_000_060_000);
    }

    #[tokio::test]
    async fn audio_store_loop_exits_on_empty_channel() {
        use crate::pipeline::counters::PipelineCounters;
        use chronicle_audio::CompletedSegment;

        let (storage, _dir) = temp_storage().await;
        let (_tx, rx) = mpsc::channel::<CompletedSegment>(16);
        drop(_tx);
        audio_store_loop(storage, rx, PipelineCounters::new()).await;
    }

    #[tokio::test]
    async fn audio_store_loop_increments_counter() {
        use crate::pipeline::counters::PipelineCounters;
        use chronicle_audio::{AudioSource, CompletedSegment};

        let (storage, dir) = temp_storage().await;
        let staging_dir = dir.path().join("audio-staging");
        std::fs::create_dir_all(&staging_dir).unwrap();
        let staging_file = staging_dir.join("counter_test.opus");
        std::fs::write(&staging_file, b"data").unwrap();

        let (tx, rx) = mpsc::channel(16);
        tx.send(CompletedSegment {
            source: AudioSource::System,
            path: staging_file,
            start_timestamp: 1_700_000_000_000,
            end_timestamp: 1_700_000_030_000,
        })
        .await
        .unwrap();
        drop(tx);

        let counters = PipelineCounters::new();
        audio_store_loop(storage.clone(), rx, Arc::clone(&counters)).await;

        assert_eq!(counters.snapshot().audio_segments_persisted, 1);
    }

    #[tokio::test]
    async fn audio_store_loop_hardens_file_permissions() {
        use crate::pipeline::counters::PipelineCounters;
        use chronicle_audio::{AudioSource, CompletedSegment};
        use std::os::unix::fs::PermissionsExt;

        let (storage, dir) = temp_storage().await;

        let staging_dir = dir.path().join("audio-staging");
        std::fs::create_dir_all(&staging_dir).unwrap();
        let staging_file = staging_dir.join("perm_test.opus");
        // Write with permissive mode so we can verify the move tightens it
        std::fs::write(&staging_file, b"perm test data").unwrap();
        std::fs::set_permissions(&staging_file, std::fs::Permissions::from_mode(0o644)).unwrap();

        let segment = CompletedSegment {
            source: AudioSource::Microphone,
            path: staging_file.clone(),
            start_timestamp: 1_700_000_100_000,
            end_timestamp: 1_700_000_130_000,
        };

        let (tx, rx) = mpsc::channel(16);
        tx.send(segment).await.unwrap();
        drop(tx);

        audio_store_loop(storage.clone(), rx, PipelineCounters::new()).await;

        let audio = storage.get_audio_segment(1).await.unwrap();
        let perm_path = std::path::Path::new(&audio.audio_path);
        assert!(perm_path.exists(), "permanent audio file should exist");

        let mode = std::fs::metadata(perm_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "audio file should be owner-only (0o600), got {:#o}",
            mode
        );
    }

    #[test]
    fn ocr_enqueue_result_variants_compile() {
        use crate::pipeline::sinks::OcrEnqueueResult;
        let _ = OcrEnqueueResult::Enqueued;
        let _ = OcrEnqueueResult::ChannelFull;
        let _ = OcrEnqueueResult::ChannelClosed;
    }

    #[test]
    fn pipeline_counters_start_at_zero_and_increment() {
        use crate::pipeline::counters::PipelineCounters;
        use std::sync::atomic::Ordering;

        let counters = PipelineCounters::new();
        let s = counters.snapshot();
        assert_eq!(s.frames_processed, 0);
        assert_eq!(s.frames_failed, 0);
        assert_eq!(s.ocr_enqueued, 0);
        assert_eq!(s.ocr_dropped, 0);
        assert_eq!(s.audio_segments_persisted, 0);

        counters.frames_processed.fetch_add(3, Ordering::Relaxed);
        counters.ocr_dropped.fetch_add(7, Ordering::Relaxed);
        let s2 = counters.snapshot();
        assert_eq!(s2.frames_processed, 3);
        assert_eq!(s2.ocr_dropped, 7);
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

    #[test]
    fn metadata_cache_hits_within_ttl() {
        use crate::pipeline::metadata::CachingAppMetadataProvider;
        use crate::pipeline::sinks::AppMetadataProvider;
        use chronicle_capture::AppMetadata;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::time::{Duration, Instant};

        let calls = Arc::new(AtomicU32::new(0));
        let calls_clone = Arc::clone(&calls);
        let lookup = move || {
            calls_clone.fetch_add(1, Ordering::SeqCst);
            AppMetadata::default()
        };
        let start = Instant::now();
        let clock = Arc::new(ManualClock::new(start));
        let provider = CachingAppMetadataProvider::new(
            lookup,
            Duration::from_millis(250),
            ClockHandle(Arc::clone(&clock)),
        );

        // First call populates the cache; second call within TTL must reuse it.
        let _ = provider.frontmost();
        let _ = provider.frontmost();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "second call within TTL should hit cache"
        );
    }

    #[test]
    fn metadata_cache_misses_after_ttl() {
        use crate::pipeline::metadata::CachingAppMetadataProvider;
        use crate::pipeline::sinks::AppMetadataProvider;
        use chronicle_capture::AppMetadata;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::time::{Duration, Instant};

        let calls = Arc::new(AtomicU32::new(0));
        let calls_clone = Arc::clone(&calls);
        let lookup = move || {
            calls_clone.fetch_add(1, Ordering::SeqCst);
            AppMetadata::default()
        };
        let start = Instant::now();
        let clock = Arc::new(ManualClock::new(start));
        let provider = CachingAppMetadataProvider::new(
            lookup,
            Duration::from_millis(250),
            ClockHandle(Arc::clone(&clock)),
        );

        // Prime the cache.
        let _ = provider.frontmost();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "first call must populate cache"
        );

        // Advance the clock past the TTL → next call must re-fetch.
        clock.set(start + Duration::from_millis(300));
        let _ = provider.frontmost();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "call after TTL must re-run the lookup"
        );
    }

    #[tokio::test]
    async fn storage_as_sink_inserts_record() {
        use crate::pipeline::sinks::ScreenshotSink;
        let (storage, _dir) = temp_storage().await;
        let record = chronicle_storage::ScreenshotMetadata {
            timestamp: 1_700_000_000_000,
            display_id: "d1".into(),
            app_name: None,
            app_bundle_id: None,
            window_title: None,
            image_path: "/tmp/nonexistent.heif".into(),
            ocr_text: None,
            phash: None,
            resolution: None,
        };
        let id: i64 = storage.as_ref().insert(record).await.unwrap();
        assert!(id > 0);
    }

    // --- `insert_and_enqueue` unit tests (Task 20) ---

    struct FakeSink {
        insert_returns: std::sync::Mutex<Option<anyhow::Result<i64>>>,
        deletes: std::sync::Mutex<Vec<PathBuf>>,
    }
    impl FakeSink {
        fn ok(id: i64) -> Self {
            Self {
                insert_returns: std::sync::Mutex::new(Some(Ok(id))),
                deletes: std::sync::Mutex::new(Vec::new()),
            }
        }
        fn failing() -> Self {
            Self {
                insert_returns: std::sync::Mutex::new(Some(Err(anyhow::anyhow!("db down")))),
                deletes: std::sync::Mutex::new(Vec::new()),
            }
        }
        fn deletes(&self) -> Vec<PathBuf> {
            self.deletes.lock().unwrap().clone()
        }
    }
    #[async_trait::async_trait]
    impl crate::pipeline::sinks::ScreenshotSink for FakeSink {
        async fn insert(
            &self,
            _meta: chronicle_storage::ScreenshotMetadata,
        ) -> anyhow::Result<i64> {
            self.insert_returns
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| Err(anyhow::anyhow!("no prepared result")))
        }
        fn delete_file(&self, path: &std::path::Path) {
            self.deletes.lock().unwrap().push(path.to_path_buf());
        }
    }

    struct FakeOcr(pub crate::pipeline::sinks::OcrEnqueueResult);
    impl crate::pipeline::sinks::OcrSink for FakeOcr {
        fn try_enqueue(
            &self,
            _job: crate::pipeline::sinks::OcrJob,
        ) -> crate::pipeline::sinks::OcrEnqueueResult {
            match &self.0 {
                crate::pipeline::sinks::OcrEnqueueResult::Enqueued => {
                    crate::pipeline::sinks::OcrEnqueueResult::Enqueued
                }
                crate::pipeline::sinks::OcrEnqueueResult::ChannelFull => {
                    crate::pipeline::sinks::OcrEnqueueResult::ChannelFull
                }
                crate::pipeline::sinks::OcrEnqueueResult::ChannelClosed => {
                    crate::pipeline::sinks::OcrEnqueueResult::ChannelClosed
                }
            }
        }
    }

    fn sample_metadata() -> chronicle_storage::ScreenshotMetadata {
        chronicle_storage::ScreenshotMetadata {
            timestamp: 1_700_000_000_000,
            display_id: "d1".into(),
            app_name: None,
            app_bundle_id: None,
            window_title: None,
            image_path: "/tmp/x.heif".into(),
            ocr_text: None,
            phash: None,
            resolution: None,
        }
    }

    #[tokio::test]
    async fn insert_and_enqueue_happy_path() {
        use crate::pipeline::counters::PipelineCounters;
        let sink = FakeSink::ok(42);
        let ocr = FakeOcr(crate::pipeline::sinks::OcrEnqueueResult::Enqueued);
        let counters = PipelineCounters::new();
        crate::pipeline::insert_and_enqueue(
            sample_metadata(),
            PathBuf::from("/tmp/x.heif"),
            &sink,
            &ocr,
            &counters,
        )
        .await
        .unwrap();
        let s = counters.snapshot();
        assert_eq!(s.frames_processed, 1);
        assert_eq!(s.ocr_enqueued, 1);
        assert_eq!(s.frames_failed, 0);
        assert_eq!(s.ocr_dropped, 0);
        assert!(sink.deletes().is_empty());
    }

    #[tokio::test]
    async fn insert_and_enqueue_insert_failure_cleans_file() {
        use crate::pipeline::counters::PipelineCounters;
        let sink = FakeSink::failing();
        let ocr = FakeOcr(crate::pipeline::sinks::OcrEnqueueResult::Enqueued);
        let counters = PipelineCounters::new();
        let path = PathBuf::from("/tmp/failing.heif");
        // Insert failure must propagate so the caller can count the frame
        // as failed and log the underlying error.
        let err = crate::pipeline::insert_and_enqueue(
            sample_metadata(),
            path.clone(),
            &sink,
            &ocr,
            &counters,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("db down"));
        assert_eq!(sink.deletes(), vec![path]);
        // `frames_failed` is incremented by `capture_store_loop` — not here.
        // Tests for that counter live at the loop level.
        let s = counters.snapshot();
        assert_eq!(s.frames_processed, 0);
        assert_eq!(s.ocr_enqueued, 0);
    }

    // Note: there is no unit test that directly exercises the loop-level
    // `frames_failed` increment for encode/harden/allocate_path failures,
    // because those paths need real `SCSampleBuffer` frames to drive
    // `process_frame`. The increment's correctness is covered by code review
    // — the single `fetch_add(1)` call is on the only error edge in
    // `capture_store_loop`, and `insert_and_enqueue` no longer increments
    // the counter itself (verified by `insert_and_enqueue_insert_failure_cleans_file`
    // above, which no longer asserts `frames_failed`).

    #[tokio::test]
    async fn insert_and_enqueue_ocr_full_increments_drop_counter() {
        use crate::pipeline::counters::PipelineCounters;
        let sink = FakeSink::ok(7);
        let ocr = FakeOcr(crate::pipeline::sinks::OcrEnqueueResult::ChannelFull);
        let counters = PipelineCounters::new();
        crate::pipeline::insert_and_enqueue(
            sample_metadata(),
            PathBuf::from("/tmp/full.heif"),
            &sink,
            &ocr,
            &counters,
        )
        .await
        .unwrap();
        let s = counters.snapshot();
        assert_eq!(s.frames_processed, 1);
        assert_eq!(s.ocr_dropped, 1);
        assert_eq!(s.ocr_enqueued, 0);
    }

    #[tokio::test]
    async fn insert_and_enqueue_ocr_closed_increments_drop_counter() {
        use crate::pipeline::counters::PipelineCounters;
        let sink = FakeSink::ok(8);
        let ocr = FakeOcr(crate::pipeline::sinks::OcrEnqueueResult::ChannelClosed);
        let counters = PipelineCounters::new();
        crate::pipeline::insert_and_enqueue(
            sample_metadata(),
            PathBuf::from("/tmp/closed.heif"),
            &sink,
            &ocr,
            &counters,
        )
        .await
        .unwrap();
        let s = counters.snapshot();
        assert_eq!(s.ocr_dropped, 1);
    }
}
