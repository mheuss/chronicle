//! Atomic counters for the screenshot and audio pipelines.
//!
//! Values are exported via the Status IPC. All fields use `AtomicU64` and
//! `Ordering::Relaxed` — these counters are eventually-consistent
//! observability, never used for synchronization.

#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Live pipeline counters. Cheap to clone via `Arc`.
#[derive(Default)]
pub struct PipelineCounters {
    pub frames_processed: AtomicU64,
    pub frames_failed: AtomicU64,
    pub ocr_enqueued: AtomicU64,
    pub ocr_dropped: AtomicU64,
    pub audio_segments_persisted: AtomicU64,
    pub transcription_enqueued: AtomicU64,
    pub transcription_dropped: AtomicU64,
}

impl PipelineCounters {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn snapshot(&self) -> CountersSnapshot {
        CountersSnapshot {
            frames_processed: self.frames_processed.load(Ordering::Relaxed),
            frames_failed: self.frames_failed.load(Ordering::Relaxed),
            ocr_enqueued: self.ocr_enqueued.load(Ordering::Relaxed),
            ocr_dropped: self.ocr_dropped.load(Ordering::Relaxed),
            audio_segments_persisted: self.audio_segments_persisted.load(Ordering::Relaxed),
            transcription_enqueued: self.transcription_enqueued.load(Ordering::Relaxed),
            transcription_dropped: self.transcription_dropped.load(Ordering::Relaxed),
        }
    }
}

/// Point-in-time snapshot of all counters.
#[derive(Debug, Clone, Copy, Default)]
pub struct CountersSnapshot {
    pub frames_processed: u64,
    pub frames_failed: u64,
    pub ocr_enqueued: u64,
    pub ocr_dropped: u64,
    pub audio_segments_persisted: u64,
    pub transcription_enqueued: u64,
    pub transcription_dropped: u64,
}
