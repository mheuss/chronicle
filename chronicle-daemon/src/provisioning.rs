//! Whisper-model provisioning (HEU-475 Phase 1, HEU-589 Phase 2).
//!
//! Holds the status cell the IPC `Status` handler reads, the per-variant
//! on-disk report, and [`EngineHandle`] — the shared slot the sink gates on,
//! and which `transcribe_loop` resolves per segment (Task 11).
//!
//! Task 12 adds the download primitives: [`check_disk_space`],
//! [`cleanup_stale_tmps`], [`download_model`] (fetch), and [`finalize_model`]
//! (verify + land). They are deliberately separate steps rather than one
//! `fetch_and_install` — the provisioner publishes a distinct status between
//! fetching and verifying, so each design §2.1 state edge maps to one call.
//! The state machine that sequences them, and the `SetWhisperModel` wiring,
//! land in Tasks 13–14.

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

    // Task 12's `download_model` takes the atomics as parameters, so it is
    // `provision()` (Task 13) that reads them off the cell to pass down.
    #[allow(dead_code)] // consumed by the provisioner in Phase 2 (Task 13)
    pub fn progress_bytes(&self) -> &AtomicU64 {
        &self.download_bytes
    }

    #[allow(dead_code)] // consumed by the provisioner in Phase 2 (Task 13)
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

    #[allow(dead_code)] // consumed by the provisioner in Phase 2 (Task 13)
    pub fn set_missing(&self) {
        self.update(|s| Snapshot {
            state: TranscriptionState::Missing,
            error: None,
            ..s.clone()
        });
    }

    /// Takes `ModelVariant`, not `&str` — every caller already holds one,
    /// and the allow-list guarantee lives in the type (no panic path here).
    #[allow(dead_code)] // consumed by the provisioner in Phase 2 (Task 13)
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

    #[allow(dead_code)] // consumed by the provisioner in Phase 2 (Task 13)
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
                    // A path we can't use must not read like "absent" with
                    // nothing in the log. Two ways that happens: EACCES/EIO on
                    // an existing file, and a path that exists but isn't a
                    // regular file — a directory named ggml-<variant>.bin
                    // returns Ok(meta) with is_file() false, so it would slip
                    // past an Err-only check.
                    //
                    // debug!, not warn!: this runs on every Status request, so
                    // a persistent bad path would repeat at the UI's poll rate.
                    match res {
                        Ok(_) => log::debug!(
                            "models_report: {} exists but is not a regular file",
                            path.display()
                        ),
                        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                            log::debug!("models_report: stat {} failed: {e}", path.display())
                        }
                        Err(_) => {}
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

/// Margin on top of the advertised size before we start a download (NFR-3).
const DISK_MARGIN_BYTES: u64 = 512 * 1024 * 1024;

/// Per-chunk stall timeout. A whole-request timeout would falsely kill a
/// healthy 1.5 GB transfer; a stalled chunk read is the real failure signal.
const CHUNK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// statvfs-based free space check, run before a download starts.
#[allow(dead_code)] // called by provision() in Phase 2 (Task 13)
pub fn check_disk_space(base_dir: &Path, required_bytes: u64) -> anyhow::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(base_dir.as_os_str().as_bytes())?;
    let mut vfs: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c_path` is a valid NUL-terminated string that outlives the
    // call, and `vfs` is a live, correctly-sized, zeroed statvfs.
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut vfs) };
    anyhow::ensure!(
        rc == 0,
        "statvfs failed: {}",
        std::io::Error::last_os_error()
    );
    let free = (vfs.f_bavail as u64).saturating_mul(vfs.f_frsize as u64);
    let needed = required_bytes.saturating_add(DISK_MARGIN_BYTES);
    anyhow::ensure!(
        free >= needed,
        "not enough free disk space: need {:.1} GB, have {:.1} GB",
        needed as f64 / 1e9,
        free as f64 / 1e9,
    );
    Ok(())
}

/// Delete any `*.tmp` partials in the models dir (startup + failure paths).
#[allow(dead_code)] // called by boot() in Phase 2 (Task 13)
pub fn cleanup_stale_tmps(base_dir: &Path) {
    let models = base_dir.join(chronicle_transcription::MODELS_SUBDIR);
    let Ok(entries) = std::fs::read_dir(&models) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.extension().is_some_and(|e| e == "tmp") {
            if let Err(e) = std::fs::remove_file(&p) {
                log::warn!("failed to remove stale partial {}: {e}", p.display());
            } else {
                log::info!("removed stale partial download {}", p.display());
            }
        }
    }
}

/// Stream `url` into `tmp`. Fetch ONLY — no verify, no rename. The caller
/// (`provision()`, Task 13) owns the Verifying and landing steps so every
/// design §2.1 state edge maps to one visible provision() step. Progress
/// lands in the two atomics. Any failure deletes the partial and returns
/// Err with a user-facing message.
///
/// Transport safety is upstream of this function, not in it: the manifest
/// pins `https` URLs (Task 1) and the client is built with rustls and no
/// native-tls fallback, so there is no downgrade path in production. The
/// scheme is deliberately not rejected here — the tests drive a loopback
/// `http` fixture.
#[allow(dead_code)] // called by provision() in Phase 2 (Task 13)
pub async fn download_model(
    url: &str,
    tmp: &Path,
    bytes_atom: &AtomicU64,
    total_atom: &AtomicU64,
) -> anyhow::Result<()> {
    use std::io::Write as _;
    if let Some(parent) = tmp.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let result: anyhow::Result<()> = async {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(30))
            .build()?;
        // Design Integration Architecture: offline/DNS/5xx surface as
        // "download failed: …" — never reqwest's raw internal error text.
        let mut resp = client
            .get(url)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| anyhow::anyhow!("download failed: {e}"))?;
        if let Some(len) = resp.content_length() {
            total_atom.store(len, Ordering::Relaxed);
        }
        let mut file = std::fs::File::create(tmp)?;
        loop {
            let chunk = tokio::time::timeout(CHUNK_TIMEOUT, resp.chunk())
                .await
                .map_err(|_| anyhow::anyhow!("download stalled (no data for 60s)"))??;
            let Some(chunk) = chunk else { break };
            file.write_all(&chunk)?;
            bytes_atom.fetch_add(chunk.len() as u64, Ordering::Relaxed);
        }
        file.flush()?;
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    result
}

/// Verify `tmp` against the pinned digest, chmod 0600, rename into `dest`.
/// Called by `provision()` immediately AFTER it publishes `Verifying`. Every
/// failure path deletes the partial; a mismatch reports the rotation-aware
/// message (NFR-1, Integration Architecture).
///
/// Async purely to own the `spawn_blocking` the hash needs: `sha1` 0.10 has
/// no aarch64 backend (x86 → SHA-NI, loongarch64 → asm, everything else →
/// `soft`), so digesting the 1.5 GB `medium` model is seconds of CPU. Run
/// direct from an await path it would stall a tokio worker and starve the
/// 1 Hz IPC status poll that renders `Verifying`. Keeping the blocking hop
/// inside means a caller cannot forget it. Hashing happens BEFORE the
/// rename, so the verified bytes are provably the bytes that get loaded.
#[allow(dead_code)] // called by provision() in Phase 2 (Task 13)
pub async fn finalize_model(tmp: &Path, dest: &Path, expected_sha1: &str) -> anyhow::Result<()> {
    let result: anyhow::Result<()> = async {
        use std::os::unix::fs::PermissionsExt; // for Permissions::from_mode
        let tmp_owned = tmp.to_path_buf();
        let expected = expected_sha1.to_string();
        let matches = tokio::task::spawn_blocking(move || {
            chronicle_transcription::verify_sha1(&tmp_owned, &expected)
        })
        .await??;
        anyhow::ensure!(
            matches,
            "checksum mismatch — upstream may have rotated the model; update Chronicle"
        );
        std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(0o600))?;
        std::fs::rename(tmp, dest)?;
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    result
}

/// Shared slot for the live engine. `None` until first provision; then only
/// ever REPLACED, never cleared — a failed switch keeps the old engine
/// (design §2.1 invariant 1). RwLock, not ArcSwap: ArcSwap cannot hold
/// `Arc<dyn Trait>` without a sized wrapper, and at one read per ~30 s
/// segment, lock cost is irrelevant (AD-3).
pub struct EngineHandle(std::sync::RwLock<Option<Arc<dyn chronicle_transcription::Transcriber>>>);

impl EngineHandle {
    pub fn new() -> Self {
        Self(std::sync::RwLock::new(None))
    }

    pub fn get(&self) -> Option<Arc<dyn chronicle_transcription::Transcriber>> {
        self.0.read().unwrap().clone()
    }

    /// Install `engine`, returning nothing. The previous engine's destructor
    /// runs AFTER the write guard is released: dropping the last `Arc` to a
    /// real engine is `whisper_free` on up to 1.5 GB plus Metal teardown, and
    /// `is_loaded()` is read from `try_enqueue` on a tokio worker — holding
    /// the lock across that free would stall the worker for its duration.
    /// The guard temporary dies at the end of the `replace` statement, so
    /// `previous` drops with the lock free.
    pub fn set(&self, engine: Arc<dyn chronicle_transcription::Transcriber>) {
        let previous = self.0.write().unwrap().replace(engine);
        drop(previous);
    }

    pub fn is_loaded(&self) -> bool {
        self.0.read().unwrap().is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chronicle_ipc::TranscriptionState;
    use chronicle_transcription::{MODELS_SUBDIR, parse_variant};
    use tempfile::tempdir;

    /// Stand-in engine, tagged so tests can tell two instances apart.
    struct StubEngine(&'static str);

    impl chronicle_transcription::Transcriber for StubEngine {
        fn transcribe(
            &self,
            _pcm_16k_mono: &[f32],
        ) -> Result<chronicle_transcription::Transcript, chronicle_transcription::TranscriptionError>
        {
            unreachable!("never called in EngineHandle tests")
        }
        fn variant(&self) -> &str {
            self.0
        }
    }

    #[test]
    fn engine_handle_starts_empty() {
        let handle = EngineHandle::new();
        assert!(!handle.is_loaded());
        assert!(handle.get().is_none());
    }

    #[test]
    fn engine_handle_set_replaces_and_never_clears() {
        // The slot is "only ever REPLACED, never cleared" (design §2.1
        // invariant 1). Pins that a second `set` swaps the engine rather than
        // being ignored, and that the handle stays loaded across the swap —
        // the path Task 12/13 drive on every model switch.
        let handle = EngineHandle::new();

        handle.set(Arc::new(StubEngine("base")));
        assert!(handle.is_loaded());
        assert_eq!(handle.get().unwrap().variant(), "base");

        handle.set(Arc::new(StubEngine("small")));
        assert!(handle.is_loaded());
        assert_eq!(handle.get().unwrap().variant(), "small");
    }

    #[test]
    fn engine_handle_set_drops_previous_engine_outside_the_lock() {
        // A replaced engine's destructor must NOT run under the write lock:
        // dropping the last Arc to a real engine is whisper_free on up to
        // 1.5 GB plus Metal teardown, and `is_loaded()` is read from
        // `try_enqueue` on a tokio worker, so a blocking read there stalls
        // that worker for the duration.
        //
        // The probe's Drop reports whether the lock was free when it ran.
        // `try_read` rather than `read` on purpose — if the guard were still
        // held, a blocking read would hang the test run instead of failing it.
        use std::sync::atomic::AtomicBool;

        struct LockProbe {
            handle: Arc<EngineHandle>,
            lock_was_free: Arc<AtomicBool>,
        }
        impl chronicle_transcription::Transcriber for LockProbe {
            fn transcribe(
                &self,
                _pcm_16k_mono: &[f32],
            ) -> Result<
                chronicle_transcription::Transcript,
                chronicle_transcription::TranscriptionError,
            > {
                unreachable!("never called")
            }
            fn variant(&self) -> &str {
                "probe"
            }
        }
        impl Drop for LockProbe {
            fn drop(&mut self) {
                self.lock_was_free
                    .store(self.handle.0.try_read().is_ok(), Ordering::SeqCst);
            }
        }

        let handle = Arc::new(EngineHandle::new());
        let lock_was_free = Arc::new(AtomicBool::new(false));
        handle.set(Arc::new(LockProbe {
            handle: Arc::clone(&handle),
            lock_was_free: Arc::clone(&lock_was_free),
        }));

        // Replacing drops the LockProbe, whose Drop probes the same lock.
        handle.set(Arc::new(StubEngine("base")));

        assert!(
            lock_was_free.load(Ordering::SeqCst),
            "previous engine's destructor ran while the write lock was held"
        );
        assert_eq!(handle.get().unwrap().variant(), "base");
    }

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

    /// Minimal HTTP/1.1 fixture: serves `body` for any GET, optionally
    /// stalling forever after `stall_after` bytes. Returns the bound port.
    async fn http_fixture(body: Vec<u8>, stall_after: Option<usize>) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let body = body.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let _ = sock.read(&mut buf).await; // consume request
                    let head = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n", body.len());
                    sock.write_all(head.as_bytes()).await.unwrap();
                    match stall_after {
                        Some(n) => {
                            sock.write_all(&body[..n]).await.unwrap();
                            // Hold the socket open, never sending the rest.
                            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                        }
                        None => sock.write_all(&body).await.unwrap(),
                    }
                });
            }
        });
        port
    }

    /// sha1 of `body`, computed in-test so no digest is hardcoded.
    fn sha1_hex(body: &[u8]) -> String {
        use sha1::{Digest, Sha1};
        let mut h = Sha1::new();
        h.update(body);
        format!("{:x}", h.finalize())
    }

    #[tokio::test]
    async fn download_streams_body_to_tmp_with_progress() {
        let dir = tempdir().unwrap();
        let body = b"fake model bytes".to_vec();
        let port = http_fixture(body.clone(), None).await;
        let tmp = dir.path().join(MODELS_SUBDIR).join("ggml-base.bin.tmp");
        let bytes = AtomicU64::new(0);
        let total = AtomicU64::new(0);
        download_model(
            &format!("http://127.0.0.1:{port}/model"),
            &tmp,
            &bytes,
            &total,
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(&tmp).unwrap(), body, "fetch only — no rename");
        assert_eq!(bytes.load(Ordering::Relaxed), body.len() as u64);
        assert_eq!(
            total.load(Ordering::Relaxed),
            body.len() as u64,
            "Content-Length"
        );
    }

    #[tokio::test]
    async fn finalize_verifies_chmods_and_renames() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let tmp = dir.path().join("ggml-base.bin.tmp");
        let body = b"fake model bytes";
        std::fs::write(&tmp, body).unwrap();
        let expected = sha1_hex(body);
        let dest = dir.path().join("ggml-base.bin");
        finalize_model(&tmp, &dest, &expected).await.unwrap();
        assert!(!tmp.exists());
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        let mode = std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[tokio::test]
    async fn finalize_rejects_checksum_mismatch() {
        let dir = tempdir().unwrap();
        let tmp = dir.path().join("ggml-base.bin.tmp");
        std::fs::write(&tmp, b"corrupt").unwrap();
        let dest = dir.path().join("ggml-base.bin");
        let err = finalize_model(&tmp, &dest, "465707469ff3a37a2b9b8d8f89f2f99de7299dac")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("checksum mismatch"));
        assert!(!dest.exists());
        assert!(!tmp.exists(), "suspect partial deleted");
    }

    #[tokio::test(start_paused = true)]
    async fn download_errors_on_stalled_transfer() {
        let dir = tempdir().unwrap();
        let port = http_fixture(vec![0u8; 1024], Some(10)).await;
        let tmp = dir.path().join(MODELS_SUBDIR).join("ggml-base.bin.tmp");
        let result = download_model(
            &format!("http://127.0.0.1:{port}/x"),
            &tmp,
            &AtomicU64::new(0),
            &AtomicU64::new(0),
        )
        .await;
        assert!(result.is_err(), "stall must time out, not hang");
        assert!(!tmp.exists(), "partial cleaned up on failure");
    }

    #[test]
    fn precheck_fails_when_disk_cannot_fit() {
        let dir = tempdir().unwrap();
        let err = check_disk_space(dir.path(), u64::MAX).unwrap_err();
        assert!(err.to_string().contains("free disk space"));
    }

    #[test]
    fn cleanup_removes_stale_tmp_only() {
        let dir = tempdir().unwrap();
        let models = dir.path().join(MODELS_SUBDIR);
        std::fs::create_dir_all(&models).unwrap();
        std::fs::write(models.join("ggml-base.bin.tmp"), b"stale").unwrap();
        std::fs::write(models.join("ggml-base.bin"), b"real").unwrap();
        cleanup_stale_tmps(dir.path());
        assert!(!models.join("ggml-base.bin.tmp").exists());
        assert!(models.join("ggml-base.bin").exists());
    }
}
