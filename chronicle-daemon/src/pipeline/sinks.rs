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

/// `tokio::mpsc`-backed `TranscriptionSink`, gated on the engine handle:
/// while no engine is loaded, enqueues report `Disabled` (idle, not a drop)
/// — bit-for-bit the old `NoopTranscriptionSink` counter semantics.
///
/// The gate is checked BEFORE `try_send`, so an engine-less sink reports
/// `Disabled` even when its channel is closed. That ordering is the
/// [Catalog] contract: a daemon booting without a model must not count
/// drops, and before this task it could not, because it held a sink with no
/// channel at all.
pub struct TokioTranscriptionSink {
    pub tx: mpsc::Sender<TranscriptionJob>,
    pub engine: std::sync::Arc<crate::provisioning::EngineHandle>,
}

impl TranscriptionSink for TokioTranscriptionSink {
    fn try_enqueue(&self, job: TranscriptionJob) -> TranscriptionEnqueueResult {
        if !self.engine.is_loaded() {
            return TranscriptionEnqueueResult::Disabled;
        }
        match self.tx.try_send(job) {
            Ok(()) => TranscriptionEnqueueResult::Enqueued,
            Err(mpsc::error::TrySendError::Full(_)) => TranscriptionEnqueueResult::ChannelFull,
            Err(mpsc::error::TrySendError::Closed(_)) => TranscriptionEnqueueResult::ChannelClosed,
        }
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

    /// Stand-in engine. Only its presence in the handle matters here — the
    /// sink gates on `is_loaded()` and never transcribes.
    struct StubEngine;

    impl chronicle_transcription::Transcriber for StubEngine {
        fn transcribe(
            &self,
            _pcm_16k_mono: &[f32],
        ) -> Result<chronicle_transcription::Transcript, chronicle_transcription::TranscriptionError>
        {
            unreachable!("never called in sink tests")
        }
        fn variant(&self) -> &str {
            "base"
        }
    }

    /// An `EngineHandle` with an engine already in it.
    fn loaded_handle() -> std::sync::Arc<crate::provisioning::EngineHandle> {
        let handle = std::sync::Arc::new(crate::provisioning::EngineHandle::new());
        handle.set(std::sync::Arc::new(StubEngine));
        handle
    }

    #[test]
    fn tokio_sink_reports_full_then_closed() {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let sink = TokioTranscriptionSink {
            tx,
            engine: loaded_handle(),
        };
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
    fn tokio_sink_disabled_while_handle_empty() {
        let handle = crate::provisioning::EngineHandle::new();
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let sink = TokioTranscriptionSink {
            tx,
            engine: std::sync::Arc::new(handle),
        };
        let job = TranscriptionJob {
            row_id: 1,
            audio_path: PathBuf::from("/tmp/a.opus"),
        };
        assert!(matches!(
            sink.try_enqueue(job),
            TranscriptionEnqueueResult::Disabled
        ));
    }

    #[test]
    fn tokio_sink_enqueues_once_engine_set() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let sink = TokioTranscriptionSink {
            tx,
            engine: loaded_handle(),
        };
        let job = TranscriptionJob {
            row_id: 1,
            audio_path: PathBuf::from("/tmp/a.opus"),
        };
        assert!(matches!(
            sink.try_enqueue(job),
            TranscriptionEnqueueResult::Enqueued
        ));
    }

    #[test]
    fn tokio_sink_disabled_takes_precedence_over_closed_channel() {
        // [Catalog] pipeline.md: "Disabled means no model is loaded, so it is
        // idle, not a drop, and is not counted." A closed channel on an
        // engine-less sink must still report Disabled, not ChannelClosed —
        // otherwise boot-without-a-model would start counting drops the old
        // NoopTranscriptionSink never counted.
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        drop(rx);
        let sink = TokioTranscriptionSink {
            tx,
            engine: std::sync::Arc::new(crate::provisioning::EngineHandle::new()),
        };
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
