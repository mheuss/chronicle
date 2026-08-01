//! Whisper-model provisioning (HEU-475).
//!
//! Phase 1 scope: the status cell the IPC `Status` handler reads, and the
//! per-variant on-disk report. The download state machine, `EngineHandle`,
//! and `SetWhisperModel` wiring land in Phase 2.

// Phase 1 consumes only new/stats/set_loading/set_ready/set_error; the
// other setters and progress accessors are wired in Phase 2 (Tasks 12/13).
// Task 4 narrows this to per-item allows on the still-unconsumed items.
#![allow(dead_code)]

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use arc_swap::ArcSwap;
use chronicle_ipc::{ModelEntry, TranscriptionState, TranscriptionStats};
use chronicle_transcription::{
    ModelVariant, SUPPORTED_VARIANTS, model_info, model_path, parse_variant,
};

/// Inner snapshot published through the ArcSwap. Progress lives in separate
/// atomics so the 1 Hz-polled counters never require a snapshot swap.
#[derive(Debug, Clone)]
struct Snapshot {
    state: TranscriptionState,
    variant: ModelVariant,
    loaded_variant: Option<String>,
    error: Option<String>,
}

/// Shared transcription status, written by boot/provisioner, read by the
/// IPC handler. Same ArcSwap pattern as `CaptureStatusSnapshot`.
pub struct TranscriptionStatusCell {
    snapshot: ArcSwap<Snapshot>,
    download_bytes: AtomicU64,
    download_total: AtomicU64,
}

impl TranscriptionStatusCell {
    pub fn new(variant: ModelVariant) -> Arc<Self> {
        Arc::new(Self {
            snapshot: ArcSwap::from_pointee(Snapshot {
                state: TranscriptionState::Missing,
                variant,
                loaded_variant: None,
                error: None,
            }),
            download_bytes: AtomicU64::new(0),
            download_total: AtomicU64::new(0),
        })
    }

    /// Assemble the wire stats: snapshot + progress atomics + a fresh
    /// on-disk report (3 stat() calls — negligible at UI polling rates).
    pub fn stats(&self, base_dir: &Path) -> TranscriptionStats {
        let snap = self.snapshot.load();
        let downloading = snap.state == TranscriptionState::Downloading;
        // Total is unknown (0) until the response headers arrive — publish
        // None rather than Some(0) so the UI shows an indeterminate bar
        // instead of dividing by zero (wire contract in chronicle-ipc).
        let total = self.download_total.load(Ordering::Relaxed);
        TranscriptionStats {
            state: snap.state,
            variant: snap.variant.as_str().to_string(),
            loaded_variant: snap.loaded_variant.clone(),
            error: snap.error.clone(),
            download_bytes: downloading.then(|| self.download_bytes.load(Ordering::Relaxed)),
            download_total_bytes: (downloading && total > 0).then_some(total),
            models: models_report(base_dir),
        }
    }

    pub fn progress_bytes(&self) -> &AtomicU64 {
        &self.download_bytes
    }

    pub fn progress_total(&self) -> &AtomicU64 {
        &self.download_total
    }

    fn update(&self, mut f: impl FnMut(&Snapshot) -> Snapshot) {
        // rcu retries on a concurrent store, so no writer can be lost.
        // Runtime writers are serialized today (boot completes before the
        // event loop starts; provisions serialize on the in-flight CAS) —
        // this is cheap insurance against a future writer breaking that.
        self.snapshot.rcu(|s| f(s.as_ref()));
    }

    pub fn set_missing(&self) {
        self.update(|s| Snapshot {
            state: TranscriptionState::Missing,
            error: None,
            ..s.clone()
        });
    }

    /// Takes `ModelVariant`, not `&str` — every caller already holds one,
    /// and the allow-list guarantee lives in the type (no panic path here).
    pub fn set_downloading(&self, variant: ModelVariant) {
        self.download_bytes.store(0, Ordering::Relaxed);
        self.download_total.store(0, Ordering::Relaxed);
        self.update(move |s| Snapshot {
            state: TranscriptionState::Downloading,
            variant,
            error: None,
            ..s.clone()
        });
    }

    pub fn set_verifying(&self) {
        self.update(|s| Snapshot {
            state: TranscriptionState::Verifying,
            error: None,
            ..s.clone()
        });
    }

    /// Carries the variant being loaded so a switch to an already-on-disk
    /// variant reports the ATTEMPTED variant — without it, a failed switch
    /// would render "Switch to base failed — still using base" (§2.2 copy).
    pub fn set_loading(&self, variant: ModelVariant) {
        self.update(move |s| Snapshot {
            state: TranscriptionState::Loading,
            variant,
            error: None,
            ..s.clone()
        });
    }

    /// Ready: `loaded_variant` becomes the serving variant.
    pub fn set_ready(&self, loaded: ModelVariant) {
        self.update(move |_| Snapshot {
            state: TranscriptionState::Ready,
            variant: loaded,
            loaded_variant: Some(loaded.as_str().to_string()),
            error: None,
        });
    }

    /// Error: keeps `loaded_variant` untouched — a failed switch must keep
    /// reporting the engine that is still serving (design §2.1 invariant 1).
    pub fn set_error(&self, msg: &str) {
        self.update(|s| Snapshot {
            state: TranscriptionState::Error,
            error: Some(msg.to_string()),
            ..s.clone()
        });
    }
}

/// Per-variant on-disk report: actual size when the file exists, advertised
/// manifest size otherwise. Always all allow-listed variants, in order.
pub fn models_report(base_dir: &Path) -> Vec<ModelEntry> {
    SUPPORTED_VARIANTS
        .iter()
        .map(|name| {
            let variant = parse_variant(name).expect("SUPPORTED_VARIANTS entries parse");
            let path = model_path(base_dir, variant);
            match std::fs::metadata(&path) {
                Ok(meta) if meta.is_file() => ModelEntry {
                    variant: (*name).to_string(),
                    downloaded: true,
                    size_bytes: meta.len(),
                },
                res => {
                    // EACCES/EIO on an existing file must not read like
                    // "absent" with nothing in the log.
                    if let Err(e) = res
                        && e.kind() != std::io::ErrorKind::NotFound
                    {
                        log::debug!("models_report: stat {} failed: {e}", path.display());
                    }
                    ModelEntry {
                        variant: (*name).to_string(),
                        downloaded: false,
                        size_bytes: model_info(variant).size_bytes,
                    }
                }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chronicle_ipc::TranscriptionState;
    use chronicle_transcription::{MODELS_SUBDIR, parse_variant};
    use tempfile::tempdir;

    #[test]
    fn cell_defaults_to_missing_with_variant() {
        let cell = TranscriptionStatusCell::new(parse_variant("base").unwrap());
        let stats = cell.stats(std::path::Path::new("/nonexistent"));
        assert_eq!(stats.state, TranscriptionState::Missing);
        assert_eq!(stats.variant, "base");
        assert_eq!(stats.loaded_variant, None);
        assert_eq!(stats.models.len(), 3, "always all allow-listed variants");
        assert!(stats.models.iter().all(|m| !m.downloaded));
    }

    #[test]
    fn models_report_uses_actual_size_when_downloaded() {
        let dir = tempdir().unwrap();
        let models = dir.path().join(MODELS_SUBDIR);
        std::fs::create_dir_all(&models).unwrap();
        std::fs::write(models.join("ggml-base.bin"), vec![0u8; 1234]).unwrap();

        let cell = TranscriptionStatusCell::new(parse_variant("base").unwrap());
        let stats = cell.stats(dir.path());
        let base = stats.models.iter().find(|m| m.variant == "base").unwrap();
        assert!(base.downloaded);
        assert_eq!(base.size_bytes, 1234, "actual on-disk size");
        let small = stats.models.iter().find(|m| m.variant == "small").unwrap();
        assert!(!small.downloaded);
        assert_eq!(
            small.size_bytes,
            chronicle_transcription::model_info(parse_variant("small").unwrap()).size_bytes,
            "advertised size when absent"
        );
    }

    #[test]
    fn set_ready_reports_loaded_variant() {
        let cell = TranscriptionStatusCell::new(parse_variant("base").unwrap());
        cell.set_ready(parse_variant("base").unwrap());
        let stats = cell.stats(std::path::Path::new("/nonexistent"));
        assert_eq!(stats.state, TranscriptionState::Ready);
        assert_eq!(stats.loaded_variant.as_deref(), Some("base"));
        assert_eq!(stats.error, None);
    }

    #[test]
    fn set_error_keeps_previous_loaded_variant() {
        let cell = TranscriptionStatusCell::new(parse_variant("base").unwrap());
        cell.set_ready(parse_variant("base").unwrap());
        cell.set_error("boom");
        let stats = cell.stats(std::path::Path::new("/nonexistent"));
        assert_eq!(stats.state, TranscriptionState::Error);
        assert_eq!(stats.error.as_deref(), Some("boom"));
        assert_eq!(
            stats.loaded_variant.as_deref(),
            Some("base"),
            "failed switch must not erase the serving engine (design invariant 1)"
        );
    }

    #[test]
    fn set_loading_reports_loading_without_progress() {
        // Boot with a file present enters Loading (design §2.1) — Task 4
        // calls this before the spawn_blocking load, while IPC is already
        // serving Status. The state must be visible and carry no download
        // progress fields.
        let cell = TranscriptionStatusCell::new(parse_variant("base").unwrap());
        cell.set_loading(parse_variant("small").unwrap());
        let stats = cell.stats(std::path::Path::new("/nonexistent"));
        assert_eq!(stats.state, TranscriptionState::Loading);
        assert_eq!(
            stats.variant, "small",
            "loading reports the ATTEMPTED variant"
        );
        assert_eq!(stats.download_bytes, None);
        assert_eq!(stats.error, None);
    }

    #[test]
    fn download_progress_appears_only_while_downloading() {
        let cell = TranscriptionStatusCell::new(parse_variant("base").unwrap());
        cell.set_downloading(parse_variant("base").unwrap());
        cell.progress_bytes()
            .store(10, std::sync::atomic::Ordering::Relaxed);
        cell.progress_total()
            .store(100, std::sync::atomic::Ordering::Relaxed);
        let stats = cell.stats(std::path::Path::new("/nonexistent"));
        assert_eq!(stats.state, TranscriptionState::Downloading);
        assert_eq!(stats.download_bytes, Some(10));
        assert_eq!(stats.download_total_bytes, Some(100));

        cell.set_ready(parse_variant("base").unwrap());
        let stats = cell.stats(std::path::Path::new("/nonexistent"));
        assert_eq!(stats.download_bytes, None);
    }

    #[test]
    fn set_downloading_rezeroes_progress_and_hides_unknown_total() {
        // Retry path (AD-11): a second begin must not show the failed
        // attempt's stale bytes as instant progress.
        let cell = TranscriptionStatusCell::new(parse_variant("base").unwrap());
        cell.set_downloading(parse_variant("base").unwrap());
        cell.progress_bytes()
            .store(10, std::sync::atomic::Ordering::Relaxed);
        cell.progress_total()
            .store(100, std::sync::atomic::Ordering::Relaxed);
        cell.set_downloading(parse_variant("base").unwrap());
        let stats = cell.stats(std::path::Path::new("/nonexistent"));
        assert_eq!(stats.download_bytes, Some(0));
        assert_eq!(
            stats.download_total_bytes, None,
            "total is None until Content-Length arrives — never Some(0)"
        );
    }

    #[test]
    fn set_verifying_hides_progress() {
        let cell = TranscriptionStatusCell::new(parse_variant("base").unwrap());
        cell.set_downloading(parse_variant("base").unwrap());
        cell.progress_bytes()
            .store(10, std::sync::atomic::Ordering::Relaxed);
        cell.progress_total()
            .store(100, std::sync::atomic::Ordering::Relaxed);
        cell.set_verifying();
        let stats = cell.stats(std::path::Path::new("/nonexistent"));
        assert_eq!(stats.state, TranscriptionState::Verifying);
        assert_eq!(stats.download_bytes, None);
        assert_eq!(stats.download_total_bytes, None);
    }

    #[test]
    fn set_missing_clears_error() {
        let cell = TranscriptionStatusCell::new(parse_variant("base").unwrap());
        cell.set_error("boom");
        cell.set_missing();
        let stats = cell.stats(std::path::Path::new("/nonexistent"));
        assert_eq!(stats.state, TranscriptionState::Missing);
        assert_eq!(stats.error, None, "error is set iff state == Error");
    }
}
