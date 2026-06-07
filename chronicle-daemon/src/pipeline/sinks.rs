//! Trait abstractions over the screenshot pipeline's side effects.
//!
//! These traits let `insert_and_enqueue` be unit-tested without touching
//! SQLite or the OCR channel. Production code uses the concrete impls
//! (`ScreenshotSink` for `Storage`, `TokioOcrSink` for
//! `mpsc::Sender<(i64, PathBuf)>`).

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use chronicle_capture::AppMetadata;
use chronicle_storage::{ScreenshotMetadata, Storage};
use tokio::sync::mpsc;

/// Persists screenshot metadata and cleans up staged files on failure.
#[async_trait]
pub trait ScreenshotSink: Send + Sync {
    async fn insert(&self, meta: ScreenshotMetadata) -> anyhow::Result<i64>;
    fn delete_file(&self, path: &Path);
}

/// Forwards OCR jobs downstream. Implementations must not block.
pub trait OcrSink: Send + Sync {
    fn try_enqueue(&self, job: OcrJob) -> OcrEnqueueResult;
}

/// A screenshot ready for OCR processing.
pub struct OcrJob {
    pub row_id: i64,
    pub image_path: PathBuf,
}

/// Result of attempting to enqueue an OCR job.
pub enum OcrEnqueueResult {
    Enqueued,
    ChannelFull,
    ChannelClosed,
}

/// Looks up the frontmost application. Implementations are expected to be
/// fast — the pipeline calls this on every frame.
pub trait AppMetadataProvider: Send + Sync {
    fn frontmost(&self) -> AppMetadata;
}

/// `tokio::mpsc`-backed `OcrSink`.
pub struct TokioOcrSink(pub mpsc::Sender<(i64, PathBuf)>);

impl OcrSink for TokioOcrSink {
    fn try_enqueue(&self, job: OcrJob) -> OcrEnqueueResult {
        match self.0.try_send((job.row_id, job.image_path)) {
            Ok(()) => OcrEnqueueResult::Enqueued,
            Err(mpsc::error::TrySendError::Full(_)) => OcrEnqueueResult::ChannelFull,
            Err(mpsc::error::TrySendError::Closed(_)) => OcrEnqueueResult::ChannelClosed,
        }
    }
}

/// Forwards transcription jobs downstream. Implementations must not block.
pub trait TranscriptionSink: Send + Sync {
    fn try_enqueue(&self, job: TranscriptionJob) -> TranscriptionEnqueueResult;
}

/// A persisted audio segment ready for transcription. Carries the **permanent**
/// path (post-move), never the staging path.
pub struct TranscriptionJob {
    pub row_id: i64,
    pub audio_path: PathBuf,
}

/// Result of attempting to enqueue a transcription job.
pub enum TranscriptionEnqueueResult {
    Enqueued,
    ChannelFull,
    ChannelClosed,
    /// No model loaded — transcription is idle; not a drop.
    Disabled,
}

/// `tokio::mpsc`-backed `TranscriptionSink` (used when a model is loaded).
pub struct TokioTranscriptionSink(pub mpsc::Sender<TranscriptionJob>);

impl TranscriptionSink for TokioTranscriptionSink {
    fn try_enqueue(&self, job: TranscriptionJob) -> TranscriptionEnqueueResult {
        match self.0.try_send(job) {
            Ok(()) => TranscriptionEnqueueResult::Enqueued,
            Err(mpsc::error::TrySendError::Full(_)) => TranscriptionEnqueueResult::ChannelFull,
            Err(mpsc::error::TrySendError::Closed(_)) => TranscriptionEnqueueResult::ChannelClosed,
        }
    }
}

/// No-op sink used when no model is present — enqueues are silently dropped as
/// `Disabled` (not counted, since transcription is idle, not failing).
pub struct NoopTranscriptionSink;

impl TranscriptionSink for NoopTranscriptionSink {
    fn try_enqueue(&self, _job: TranscriptionJob) -> TranscriptionEnqueueResult {
        TranscriptionEnqueueResult::Disabled
    }
}

#[async_trait]
impl ScreenshotSink for Storage {
    async fn insert(&self, meta: ScreenshotMetadata) -> anyhow::Result<i64> {
        Ok(self.insert_screenshot(meta).await?)
    }

    fn delete_file(&self, path: &Path) {
        if let Err(e) = self.media_manager().delete_file(path) {
            log::warn!("failed to delete staged screenshot {}: {e}", path.display());
        }
    }
}

#[cfg(test)]
mod transcription_sink_tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn tokio_sink_reports_full_then_closed() {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let sink = TokioTranscriptionSink(tx);
        let job = || TranscriptionJob {
            row_id: 1,
            audio_path: PathBuf::from("/tmp/a.opus"),
        };

        assert!(matches!(
            sink.try_enqueue(job()),
            TranscriptionEnqueueResult::Enqueued
        ));
        assert!(matches!(
            sink.try_enqueue(job()),
            TranscriptionEnqueueResult::ChannelFull
        ));
        drop(rx);
        assert!(matches!(
            sink.try_enqueue(job()),
            TranscriptionEnqueueResult::ChannelClosed
        ));
    }

    #[test]
    fn noop_sink_reports_disabled() {
        let sink = NoopTranscriptionSink;
        let job = TranscriptionJob {
            row_id: 1,
            audio_path: PathBuf::from("/tmp/a.opus"),
        };
        assert!(matches!(
            sink.try_enqueue(job),
            TranscriptionEnqueueResult::Disabled
        ));
    }
}
