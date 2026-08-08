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

/// Transcribe persisted audio segments off the tokio runtime and store the text.
///
/// Heavy work (Opus decode + whisper) runs inside `spawn_blocking` so it never
/// starves tokio workers (cf. HEU-480). Each pulled job is processed to
/// completion before the next `recv`, so at most one whisper call is in flight
/// (sequential). Empty results are skipped so blank rows never reach `audio_fts`.
///
/// **Shutdown contract — this loop must outlive `audio_store_loop`.** It exits
/// when the channel closes (every `TranscriptionSink` clone dropped) or when
/// `stop_after_current` is set, and in the latter case only *after* finishing
/// and persisting the segment already in flight.
///
/// It deliberately does *not* watch the global cancellation token.
/// `audio_pipeline.stop()` flushes the session's final partial segments, and
/// those get persisted and enqueued *after* cancel fires. A loop that broke on
/// cancel would drop that tail un-transcribed, log a spurious "channel closed",
/// and inflate `transcription_dropped` on every clean exit.
///
/// `stop_after_current` is checked at the bottom of the body, never the top, and
/// it exists so `main` never has to drop our `JoinHandle` to stop waiting.
/// Dropping it would detach this task, and runtime drop destroys a detached
/// future without polling it — discarding a transcript whose whisper call the
/// blocking pool is still, separately, being waited on. Do not swap this flag
/// for the global token, and do not let `main`'s timeout consume the handle.
///
/// The engine is resolved from the handle **once per job** and held for that
/// whole job, so a model swap takes effect on the next segment and the
/// transcript is produced and stamped by the same engine (design §3.3). A job
/// that arrives while the handle is empty is skipped as idle — matching the
/// sink's `Disabled` semantics — not treated as a failure.
/// Both `stop_after_current` breaks report this, so the two paths cannot
/// drift. The skip path reaches it only since Task 13 — before that the loop
/// was spawned only after a successful load.
fn warn_abandoned_queue(abandoned: usize) {
    log::warn!(
        "shutdown grace expired; stopping after the current segment — \
         {abandoned} queued segment(s) left untranscribed \
         (transcript = NULL, recoverable by backfill)"
    );
}

pub async fn transcribe_loop(
    engine: Arc<crate::provisioning::EngineHandle>,
    storage: Arc<Storage>,
    mut rx: mpsc::Receiver<crate::pipeline::sinks::TranscriptionJob>,
    stop_after_current: Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::atomic::Ordering;

    while let Some(job) = rx.recv().await {
        // Resolve per job and hold for the whole job: the transcript is
        // produced AND stamped by the engine that ran it — a swap mid-queue
        // affects the next job, never the current one (design §3.3).
        let Some(engine) = engine.get() else {
            // Benign race with the sink's is_loaded gate: idle, not failing —
            // same "Disabled" semantics, no counter.
            log::debug!(
                "transcription skipped (no engine) for segment {}",
                job.row_id
            );
            // NOT redundant with the check at the bottom of the body: the
            // `continue` below returns straight to `rx.recv()`, so the bottom
            // check never runs on this path. Every iteration must observe the
            // flag, so it is repeated here. Deleting this reintroduces a
            // shutdown hang when the handle is empty.
            if stop_after_current.load(Ordering::Relaxed) {
                // Same warning as the bottom break. Unreachable before Task
                // 13, which spawned the loop only after a successful load —
                // now the loop always runs, so an operator whose grace
                // expires with an empty handle would otherwise get no signal
                // that a queue was abandoned.
                warn_abandoned_queue(rx.len());
                break;
            }
            continue;
        };
        // Clone a separate Arc for the blocking closure; the resolved `engine`
        // stays owned so `engine.variant()` is usable after the await — it is
        // the per-job engine, not the handle, which this binding shadows.
        let engine_for_blocking = Arc::clone(&engine);
        let path = job.audio_path.clone();
        let result = tokio::task::spawn_blocking(move || {
            let pcm = chronicle_transcription::decode_opus_16k_mono(&path)?;
            engine_for_blocking.transcribe(&pcm)
        })
        .await;

        match result {
            Ok(Ok(transcript)) => {
                // Trim guards the seam, not this impl: `TranscriptionEngine` already
                // trims via `concat_segment_text`, but the `Transcriber` trait
                // promises nothing. Trim once and use it for both the check and the
                // write, so a future impl returning "  hi  " can neither slip a
                // whitespace-only row nor a padded one into `audio_fts` (BR-4).
                let text = transcript.text.trim();
                // if/else rather than `continue`: the stop check at the bottom of the
                // loop must run on every iteration, including skipped segments.
                if text.is_empty() {
                    log::debug!(
                        "transcription skipped (no usable speech) for segment {}",
                        job.row_id
                    );
                } else {
                    let text = text.to_string();
                    if let Err(e) = storage
                        .update_transcript_full(
                            job.row_id,
                            text,
                            engine.variant().to_string(),
                            transcript.language,
                        )
                        .await
                    {
                        log::error!("failed to store transcript for segment {}: {e}", job.row_id);
                    }
                }
            }
            Ok(Err(e)) => log::warn!("transcription failed for segment {}: {e}", job.row_id),
            // Neutral "failed": `JoinError`'s Display names the cause ("panicked
            // with message …" / "was cancelled"), and the shutdown contract above
            // means a cancellation is never observed here — no branch to log it.
            Err(e) => log::error!("transcription task failed for segment {}: {e}", job.row_id),
        }

        // Checked *after* the write, never before it: `main` sets this when the
        // shutdown grace expires, and the whole point is that the segment already
        // in flight still gets persisted. Breaking here (rather than `main`
        // dropping our JoinHandle) is what keeps that write from being discarded.
        if stop_after_current.load(Ordering::Relaxed) {
            warn_abandoned_queue(rx.len());
            break;
        }
    }
    log::info!("Transcribe loop exiting");
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
    transcription: Arc<dyn crate::pipeline::sinks::TranscriptionSink>,
    counters: Arc<crate::pipeline::counters::PipelineCounters>,
) {
    use crate::pipeline::sinks::{TranscriptionEnqueueResult, TranscriptionJob};
    use std::sync::atomic::Ordering;

    // Latches the "worker is gone" ERROR — the condition is permanent once it
    // happens, so it is worth exactly one line, not one per segment.
    let mut worker_gone_reported = false;

    while let Some(segment) = segment_rx.recv().await {
        match process_audio_segment(&storage, &segment).await {
            Ok((row_id, dest_path)) => {
                counters
                    .audio_segments_persisted
                    .fetch_add(1, Ordering::Relaxed);
                match transcription.try_enqueue(TranscriptionJob {
                    row_id,
                    audio_path: dest_path,
                }) {
                    TranscriptionEnqueueResult::Enqueued => {
                        counters
                            .transcription_enqueued
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    TranscriptionEnqueueResult::ChannelFull => {
                        let n = counters
                            .transcription_dropped
                            .fetch_add(1, Ordering::Relaxed)
                            + 1;
                        if n.is_multiple_of(100) {
                            log::warn!("transcription channel full; shedding (total drops: {n})");
                        }
                    }
                    TranscriptionEnqueueResult::ChannelClosed => {
                        counters
                            .transcription_dropped
                            .fetch_add(1, Ordering::Relaxed);
                        // `main` holds the only other sink handle and drops it last
                        // during shutdown, so a closed channel here means
                        // `transcribe_loop` itself died. Nothing gets transcribed for
                        // the rest of the process lifetime — not a benign race. That
                        // also makes the condition permanent, so latch the log: at
                        // ~4 segments/min this would otherwise emit thousands of
                        // identical ERROR lines a day.
                        if !worker_gone_reported {
                            worker_gone_reported = true;
                            log::error!(
                                "transcription worker is gone; segment {row_id} and every \
                                 later segment will go untranscribed (logged once)"
                            );
                        }
                    }
                    TranscriptionEnqueueResult::Disabled => {} // no model — idle, not a drop
                }
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
) -> anyhow::Result<(i64, PathBuf)> {
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
            Ok((row_id, dest_path))
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

    /// A transcription sink that reports `Disabled` for every job, which is
    /// what these tests want: they exercise the audio-store path, not
    /// transcription. Replaces the old `NoopTranscriptionSink` — a gated
    /// `TokioTranscriptionSink` over an empty `EngineHandle` has the same
    /// behavior, because the gate short-circuits before the channel is
    /// touched. The receiver is dropped immediately for the same reason.
    fn disabled_sink() -> Arc<dyn sinks::TranscriptionSink> {
        let (tx, _rx) = mpsc::channel(1);
        Arc::new(sinks::TokioTranscriptionSink {
            tx,
            engine: Arc::new(crate::provisioning::EngineHandle::new()),
        })
    }

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

        audio_store_loop(
            storage.clone(),
            rx,
            disabled_sink(),
            PipelineCounters::new(),
        )
        .await;

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

    #[derive(Default)]
    struct RecordingTranscriptionSink {
        jobs: std::sync::Mutex<Vec<crate::pipeline::sinks::TranscriptionJob>>,
    }
    impl crate::pipeline::sinks::TranscriptionSink for RecordingTranscriptionSink {
        fn try_enqueue(
            &self,
            job: crate::pipeline::sinks::TranscriptionJob,
        ) -> crate::pipeline::sinks::TranscriptionEnqueueResult {
            self.jobs.lock().unwrap().push(job);
            crate::pipeline::sinks::TranscriptionEnqueueResult::Enqueued
        }
    }

    #[tokio::test]
    async fn audio_store_loop_enqueues_permanent_path() {
        use crate::pipeline::counters::PipelineCounters;
        use chronicle_audio::{AudioSource, CompletedSegment};
        use std::sync::atomic::Ordering;

        let (storage, dir) = temp_storage().await;
        let staging_dir = dir.path().join("audio-staging");
        std::fs::create_dir_all(&staging_dir).unwrap();
        let staging_file = staging_dir.join("enqueue_test.opus");
        std::fs::write(&staging_file, b"fake opus data").unwrap();

        let segment = CompletedSegment {
            source: AudioSource::Microphone,
            path: staging_file,
            start_timestamp: 1_700_000_000_000,
            end_timestamp: 1_700_000_030_000,
        };

        let counters = PipelineCounters::new();
        let sink = Arc::new(RecordingTranscriptionSink::default());
        let (tx, rx) = mpsc::channel(4);
        tx.send(segment).await.unwrap();
        drop(tx);
        audio_store_loop(storage.clone(), rx, sink.clone(), Arc::clone(&counters)).await;

        // Copy out of the guard before awaiting so we don't hold the lock
        // across the DB call (clippy::await_holding_lock).
        let (job_count, enqueued_row_id, enqueued_path) = {
            let jobs = sink.jobs.lock().unwrap();
            (jobs.len(), jobs[0].row_id, jobs[0].audio_path.clone())
        };
        assert_eq!(job_count, 1);
        // The enqueued path is the persisted (permanent) path, and it exists.
        let seg = storage.get_audio_segment(enqueued_row_id).await.unwrap();
        assert_eq!(enqueued_path.to_string_lossy(), seg.audio_path);
        assert!(enqueued_path.exists());
        assert_eq!(counters.transcription_enqueued.load(Ordering::Relaxed), 1);
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

        audio_store_loop(
            storage.clone(),
            rx,
            disabled_sink(),
            PipelineCounters::new(),
        )
        .await;

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
        audio_store_loop(storage, rx, disabled_sink(), PipelineCounters::new()).await;
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
        audio_store_loop(storage.clone(), rx, disabled_sink(), Arc::clone(&counters)).await;

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

        audio_store_loop(
            storage.clone(),
            rx,
            disabled_sink(),
            PipelineCounters::new(),
        )
        .await;

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

    // --- `transcribe_loop` tests (Task 9) ---

    /// A `stop_after_current` flag that is never set — the normal case for tests
    /// that only care about draining to channel close.
    fn never_stop() -> Arc<std::sync::atomic::AtomicBool> {
        Arc::new(std::sync::atomic::AtomicBool::new(false))
    }

    /// Lets a test park a job *inside* `transcribe` — signalling when it has
    /// entered, then blocking until released. `transcribe` runs on a
    /// `spawn_blocking` thread, so the release side is a std channel (blocking
    /// there is correct); the entered side is a oneshot so the async test can
    /// await it without occupying a runtime worker.
    struct TranscribeGate {
        entered: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        release: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    }

    struct FakeTranscriber {
        text: String,
        variant: String,
        gate: Option<TranscribeGate>,
    }
    impl chronicle_transcription::Transcriber for FakeTranscriber {
        fn transcribe(
            &self,
            _pcm: &[f32],
        ) -> Result<chronicle_transcription::Transcript, chronicle_transcription::TranscriptionError>
        {
            if let Some(gate) = &self.gate {
                if let Some(tx) = gate.entered.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                // take() first so the guard is released before we block.
                let rx = gate.release.lock().unwrap().take();
                if let Some(rx) = rx {
                    let _ = rx.recv();
                }
            }
            Ok(chronicle_transcription::Transcript {
                text: self.text.clone(),
                language: Some("en".into()),
            })
        }
        fn variant(&self) -> &str {
            &self.variant
        }
    }

    /// A `FakeTranscriber` already installed in an `EngineHandle` — the shape
    /// `transcribe_loop` takes now that it resolves per segment.
    fn handle_with(engine: FakeTranscriber) -> Arc<crate::provisioning::EngineHandle> {
        let handle = Arc::new(crate::provisioning::EngineHandle::new());
        handle.set(Arc::new(engine));
        handle
    }

    /// The common case: a fake engine reporting variant "base", no gate.
    fn base_engine(text: &str) -> FakeTranscriber {
        FakeTranscriber {
            text: text.into(),
            variant: "base".into(),
            gate: None,
        }
    }

    /// A fake engine reporting an arbitrary variant, no gate.
    fn engine_variant(text: &str, variant: &str) -> FakeTranscriber {
        FakeTranscriber {
            text: text.into(),
            variant: variant.into(),
            gate: None,
        }
    }

    // Persist a row whose audio_path points at a real .opus file. Returns the
    // TempDir so the caller keeps it (and the file) alive for the test's duration.
    async fn persist_segment_with_opus()
    -> (Arc<Storage>, i64, std::path::PathBuf, tempfile::TempDir) {
        use chronicle_audio::OggOpusEncoder;
        let (storage, dir) = temp_storage().await;
        let path = dir.path().join("seg.opus");
        let samples: Vec<f32> = (0..48_000)
            .map(|i| (i as f32 * 220.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.2)
            .collect();
        OggOpusEncoder::new(1, 24_000, opus::Application::Voip)
            .encode_to_file(&samples, &path)
            .unwrap();
        let row_id = storage
            .insert_audio_segment(chronicle_storage::AudioSegmentMetadata {
                start_timestamp: 1_700_000_000_000,
                end_timestamp: 1_700_000_030_000,
                source: "mic".into(),
                audio_path: path.to_string_lossy().into_owned(),
                transcript: None,
                whisper_model: None,
                language: None,
            })
            .await
            .unwrap();
        (storage, row_id, path, dir)
    }

    /// The point of resolving per segment: a swap mid-queue must affect the
    /// NEXT job, never the one already in flight, and each transcript must be
    /// stamped by the engine that actually produced it (design §3.3).
    #[tokio::test]
    async fn transcribe_loop_swap_mid_queue_stamps_each_engine() {
        let (storage, row_a, opus_path, _dir) = persist_segment_with_opus().await;
        let row_b = storage
            .insert_audio_segment(chronicle_storage::AudioSegmentMetadata {
                start_timestamp: 1_700_000_030_000,
                end_timestamp: 1_700_000_060_000,
                source: "mic".into(),
                audio_path: opus_path.to_string_lossy().into_owned(),
                transcript: None,
                whisper_model: None,
                language: None,
            })
            .await
            .unwrap();

        let handle = handle_with(base_engine("first"));
        let (tx, rx) = mpsc::channel(4);
        let loop_handle = tokio::spawn(transcribe_loop(
            Arc::clone(&handle),
            storage.clone(),
            rx,
            never_stop(),
        ));
        tokio::task::yield_now().await;

        tx.send(crate::pipeline::sinks::TranscriptionJob {
            row_id: row_a,
            audio_path: opus_path.clone(),
        })
        .await
        .unwrap();
        // Wait until job A is persisted, then swap engines and send job B.
        let mut persisted = false;
        for _ in 0..200 {
            if storage
                .get_audio_segment(row_a)
                .await
                .unwrap()
                .transcript
                .is_some()
            {
                persisted = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        // Distinguishes a slow machine from a real swap-hit-the-wrong-job bug:
        // without this, a timeout falls through and surfaces as row A stamped
        // "small", which reads like a product defect.
        assert!(persisted, "timed out waiting for row A to persist");
        handle.set(Arc::new(engine_variant("second", "small")));
        tx.send(crate::pipeline::sinks::TranscriptionJob {
            row_id: row_b,
            audio_path: opus_path.clone(),
        })
        .await
        .unwrap();
        drop(tx);
        loop_handle.await.unwrap();

        let a = storage.get_audio_segment(row_a).await.unwrap();
        let b = storage.get_audio_segment(row_b).await.unwrap();
        assert_eq!(a.whisper_model.as_deref(), Some("base"));
        assert_eq!(b.whisper_model.as_deref(), Some("small"));
        assert_eq!(a.transcript.as_deref(), Some("first"));
        assert_eq!(b.transcript.as_deref(), Some("second"));
    }

    /// The other half of design §3.3, which the between-jobs swap test above
    /// cannot reach: a job already INSIDE `transcribe` must be stamped by the
    /// engine that ran it, even if the handle is swapped while it is in
    /// flight. Without this, an implementation that resolved twice — once for
    /// `transcribe`, once fresh at the `variant()` write site — passes the
    /// test above and still mis-stamps in production, where segments arrive
    /// every ~30s and a large-model whisper call can span a Settings change.
    #[tokio::test]
    async fn transcribe_loop_swap_while_in_flight_stamps_the_running_engine() {
        let (storage, row_id, opus_path, _dir) = persist_segment_with_opus().await;
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();

        let handle = handle_with(FakeTranscriber {
            text: "first".into(),
            variant: "base".into(),
            gate: Some(TranscribeGate {
                entered: std::sync::Mutex::new(Some(entered_tx)),
                release: std::sync::Mutex::new(Some(release_rx)),
            }),
        });
        let (tx, rx) = mpsc::channel(4);
        let loop_handle = tokio::spawn(transcribe_loop(
            Arc::clone(&handle),
            storage.clone(),
            rx,
            never_stop(),
        ));
        tokio::task::yield_now().await;

        tx.send(crate::pipeline::sinks::TranscriptionJob {
            row_id,
            audio_path: opus_path,
        })
        .await
        .unwrap();

        // Block until the job is parked inside `transcribe`, then swap.
        // Bounded on purpose: the oneshot sender lives in the gate, inside the
        // engine Arc this test still holds, so nothing drops it if `transcribe`
        // is never reached (a decode failure returns before it). A bare await
        // would wedge the suite instead of reporting.
        tokio::time::timeout(std::time::Duration::from_secs(10), entered_rx)
            .await
            .expect("timed out waiting for the job to enter transcribe")
            .expect("job never entered transcribe");
        handle.set(Arc::new(engine_variant("second", "small")));
        // Self-contained: without this, a no-op `set` would leave the row
        // reading "base" and the test would pass having proven nothing.
        assert_eq!(handle.get().unwrap().variant(), "small");
        release_tx.send(()).expect("release channel closed");

        drop(tx);
        loop_handle.await.unwrap();

        let seg = storage.get_audio_segment(row_id).await.unwrap();
        assert_eq!(
            seg.whisper_model.as_deref(),
            Some("base"),
            "in-flight job must keep the engine that ran it, not the swapped-in one"
        );
        assert_eq!(seg.transcript.as_deref(), Some("first"));
    }

    /// Benign race with the sink's `is_loaded` gate: a job can reach the loop
    /// with the handle empty. That is idle, not failing — skip it, do not
    /// error and do not write a row.
    #[tokio::test]
    async fn transcribe_loop_skips_job_when_handle_empty() {
        let (storage, row_id, opus_path, _dir) = persist_segment_with_opus().await;
        let handle = Arc::new(crate::provisioning::EngineHandle::new());
        let (tx, rx) = mpsc::channel(4);
        tx.send(crate::pipeline::sinks::TranscriptionJob {
            row_id,
            audio_path: opus_path,
        })
        .await
        .unwrap();
        drop(tx);
        transcribe_loop(handle, storage.clone(), rx, never_stop()).await;
        let seg = storage.get_audio_segment(row_id).await.unwrap();
        assert!(seg.transcript.is_none(), "no engine → skipped, not errored");
    }

    #[tokio::test]
    async fn transcribe_loop_writes_transcript() {
        let (storage, row_id, opus_path, _dir) = persist_segment_with_opus().await;
        let engine = handle_with(base_engine("the quick brown fox"));
        let (tx, rx) = mpsc::channel(4);
        tx.send(crate::pipeline::sinks::TranscriptionJob {
            row_id,
            audio_path: opus_path,
        })
        .await
        .unwrap();
        drop(tx);
        transcribe_loop(engine, storage.clone(), rx, never_stop()).await;

        let seg = storage.get_audio_segment(row_id).await.unwrap();
        assert_eq!(seg.transcript.as_deref(), Some("the quick brown fox"));
        assert_eq!(seg.whisper_model.as_deref(), Some("base"));
        assert_eq!(seg.language.as_deref(), Some("en"));
    }

    #[tokio::test]
    async fn transcribe_loop_skips_empty_transcript() {
        let (storage, row_id, opus_path, _dir) = persist_segment_with_opus().await;
        let engine = handle_with(base_engine(""));
        let (tx, rx) = mpsc::channel(4);
        tx.send(crate::pipeline::sinks::TranscriptionJob {
            row_id,
            audio_path: opus_path,
        })
        .await
        .unwrap();
        drop(tx);
        transcribe_loop(engine, storage.clone(), rx, never_stop()).await;

        let seg = storage.get_audio_segment(row_id).await.unwrap();
        assert!(seg.transcript.is_none());
    }

    #[tokio::test]
    async fn transcribe_loop_continues_on_missing_file() {
        let (storage, row_id, _opus_path, _dir) = persist_segment_with_opus().await;
        let engine = handle_with(base_engine("x"));
        let (tx, rx) = mpsc::channel(4);
        tx.send(crate::pipeline::sinks::TranscriptionJob {
            row_id,
            audio_path: "/no/such.opus".into(),
        })
        .await
        .unwrap();
        drop(tx);
        transcribe_loop(engine, storage.clone(), rx, never_stop()).await;
        let seg = storage.get_audio_segment(row_id).await.unwrap();
        assert!(seg.transcript.is_none());
    }

    /// `audio_pipeline.stop()` flushes the session's final partial segments, so they
    /// are enqueued *after* shutdown has begun. Jobs handed to an already-running
    /// loop must therefore all be transcribed, with only the channel close ending it.
    ///
    /// Scope, stated honestly: this pins the loop-side contract only. The `main`-side
    /// ordering it exists to protect — sink Arc dropped after `audio_store_handle`
    /// resolves — is not exercised here and is carried by `transcribe_loop`'s doc
    /// comment plus `docs/use-cases/pipeline.md`. Reintroducing an early-break would
    /// change this signature and break compilation, which is the real backstop.
    #[tokio::test]
    async fn transcribe_loop_drains_all_queued_before_exit() {
        let (storage, row_id, opus_path, _dir) = persist_segment_with_opus().await;
        // A second row over the same audio file stands in for the tail segment
        // that the shutdown flush persists last.
        let tail_id = storage
            .insert_audio_segment(chronicle_storage::AudioSegmentMetadata {
                start_timestamp: 1_700_000_030_000,
                end_timestamp: 1_700_000_060_000,
                source: "system".into(),
                audio_path: opus_path.to_string_lossy().into_owned(),
                transcript: None,
                whisper_model: None,
                language: None,
            })
            .await
            .unwrap();
        let engine = handle_with(base_engine("drained"));
        let (tx, rx) = mpsc::channel(4);
        let loop_handle = tokio::spawn(transcribe_loop(engine, storage.clone(), rx, never_stop()));
        // `tokio::spawn` only queues the task, and a `send` into a channel with free
        // capacity completes without yielding — so without this the loop would never
        // be polled before the sends land, and the test would silently degrade into
        // "tokio buffers items across close". Yielding hands the current-thread
        // scheduler to the loop so it is genuinely parked on `recv` first.
        tokio::task::yield_now().await;
        for id in [row_id, tail_id] {
            tx.send(crate::pipeline::sinks::TranscriptionJob {
                row_id: id,
                audio_path: opus_path.clone(),
            })
            .await
            .unwrap();
            // Let the loop pick the job up, so the next one is enqueued into a loop
            // that is already mid-drain.
            tokio::task::yield_now().await;
        }
        // Closing the channel is what ends the loop — mirrors `main` dropping its
        // sink Arc once `audio_store_handle` has finished.
        drop(tx);
        loop_handle.await.unwrap();

        for id in [row_id, tail_id] {
            let seg = storage.get_audio_segment(id).await.unwrap();
            assert_eq!(
                seg.transcript.as_deref(),
                Some("drained"),
                "every queued job must be transcribed before the loop exits (segment {id})"
            );
        }
    }

    /// When the shutdown grace expires, `main` sets `stop_after_current` instead of
    /// dropping our `JoinHandle`. The segment already in flight must still be
    /// **persisted** — discarding a transcript the process already waited for is the
    /// bug this flag exists to prevent — and the loop must then exit rather than
    /// drain the rest.
    #[tokio::test]
    async fn transcribe_loop_persists_current_job_then_stops() {
        let (storage, row_id, opus_path, _dir) = persist_segment_with_opus().await;
        let abandoned_id = storage
            .insert_audio_segment(chronicle_storage::AudioSegmentMetadata {
                start_timestamp: 1_700_000_030_000,
                end_timestamp: 1_700_000_060_000,
                source: "system".into(),
                audio_path: opus_path.to_string_lossy().into_owned(),
                transcript: None,
                whisper_model: None,
                language: None,
            })
            .await
            .unwrap();
        let engine = handle_with(base_engine("persisted"));
        let (tx, rx) = mpsc::channel(4);
        for id in [row_id, abandoned_id] {
            tx.send(crate::pipeline::sinks::TranscriptionJob {
                row_id: id,
                audio_path: opus_path.clone(),
            })
            .await
            .unwrap();
        }
        // Already set when the loop starts: it must still finish and write job one.
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(true));
        // Sender stays alive, so only the flag can end this loop. If the flag were
        // checked before the write (or the loop broke on it immediately), the first
        // assert fails; if it were ignored the loop never returns — the timeout turns
        // that regression into a clean failure instead of a wedged test suite.
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            transcribe_loop(engine, storage.clone(), rx, stop),
        )
        .await
        .expect("loop ignored stop_after_current");

        let done = storage.get_audio_segment(row_id).await.unwrap();
        assert_eq!(
            done.transcript.as_deref(),
            Some("persisted"),
            "the in-flight segment must be persisted before stopping"
        );
        let skipped = storage.get_audio_segment(abandoned_id).await.unwrap();
        assert!(
            skipped.transcript.is_none(),
            "queued segments past the stop point stay NULL for backfill"
        );
    }

    /// An empty transcript must not bypass the stop check. This is why the skip path
    /// is `if/else` rather than `continue`: end-of-session tails are frequently
    /// silent, so a realistic shutdown yields a *run* of empty results, and a
    /// `continue` would jump straight back to `recv` past the flag — defeating the
    /// grace bound in precisely the case it exists for.
    #[tokio::test]
    async fn transcribe_loop_stops_after_empty_transcript() {
        let (storage, row_id, opus_path, _dir) = persist_segment_with_opus().await;
        let queued_id = storage
            .insert_audio_segment(chronicle_storage::AudioSegmentMetadata {
                start_timestamp: 1_700_000_030_000,
                end_timestamp: 1_700_000_060_000,
                source: "system".into(),
                audio_path: opus_path.to_string_lossy().into_owned(),
                transcript: None,
                whisper_model: None,
                language: None,
            })
            .await
            .unwrap();
        // Whitespace only — trims to empty, so every job takes the skip path.
        let engine = handle_with(base_engine("   "));
        let (tx, rx) = mpsc::channel(4);
        for id in [row_id, queued_id] {
            tx.send(crate::pipeline::sinks::TranscriptionJob {
                row_id: id,
                audio_path: opus_path.clone(),
            })
            .await
            .unwrap();
        }
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(true));
        // Sender stays alive, so the flag is the only way out. Restore `continue` on
        // the empty path and this times out.
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            transcribe_loop(engine, storage.clone(), rx, stop),
        )
        .await
        .expect("empty transcript must not bypass stop_after_current");

        for id in [row_id, queued_id] {
            let seg = storage.get_audio_segment(id).await.unwrap();
            assert!(
                seg.transcript.is_none(),
                "empty transcripts are never written (segment {id})"
            );
        }
    }
}
