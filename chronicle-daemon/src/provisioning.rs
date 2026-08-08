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

    /// The one transition back to `Missing`: `boot()` reconciling the
    /// creation-time entry `main` made from an older presence check against
    /// what boot actually found on disk. Without it, a model deleted during
    /// startup strands the daemon in `Loading` with no engine and no error.
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
///
/// Shortened under `cfg(test)` — the plan's fallback (plan lines 1697-1701),
/// taken after the `start_paused` version of the stall test was found to
/// never reach this timeout at all. Under a paused clock the only timer armed
/// during connect is the 30 s `connect_timeout`, so the runtime jumps the
/// clock there before the loopback socket is ever observed ready, and the
/// test passed on the connect error instead. Real socket I/O and an
/// auto-advancing clock do not compose; a short real timeout does.
#[cfg(not(test))]
const CHUNK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
#[cfg(test)]
const CHUNK_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// statvfs-based free space check, run before a download starts.
pub fn check_disk_space(base_dir: &Path, required_bytes: u64) -> anyhow::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(base_dir.as_os_str().as_bytes())?;
    // SAFETY: `statvfs` is all integer fields, so all-zero is a valid bit
    // pattern for it — there is no reference, enum, or NonZero to invalidate.
    let mut vfs: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c_path` is a valid NUL-terminated string that outlives the
    // call, and `vfs` is a live, correctly-sized, initialized statvfs.
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

/// True when following a redirect would drop from HTTPS to plaintext.
/// `previous` is the redirect chain so far. Checks whether ANY hop was https
/// rather than just the head: the head IS the originally-requested URL in
/// reqwest 0.12, but keying a security decision on that ordering makes the
/// predicate fail OPEN if it ever changes. `any` fails closed instead, and
/// still lets the all-plaintext loopback fixture through. Split out of the
/// client's redirect policy so it can be tested without a TLS origin.
fn is_plaintext_downgrade(previous: &[reqwest::Url], next: &reqwest::Url) -> bool {
    previous.iter().any(|u| u.scheme() == "https") && next.scheme() != "https"
}

/// Stream `url` into `tmp`. Fetch ONLY — no verify, no rename. The caller
/// (`provision()`, Task 13) owns the Verifying and landing steps so every
/// design §2.1 state edge maps to one visible provision() step. Progress
/// lands in the two atomics. Any failure deletes the partial and returns
/// Err with a user-facing message.
///
/// Transport safety comes from three separate things, none of which is the
/// TLS backend on its own: the manifest pins `https` URLs (Task 1), the
/// redirect policy below refuses an HTTPS → plaintext hop, and the pinned
/// SHA-1 in [`finalize_model`] is what actually guards the content. The
/// initial scheme is deliberately NOT rejected here — the tests drive a
/// loopback `http` fixture.
///
/// Cancellation note: the partial is deleted on every `Err`, but a dropped
/// future (shutdown, `abort()`) skips that cleanup. [`cleanup_stale_tmps`] at
/// boot is the backstop for that case.
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
            // `rustls-tls` selects the TLS backend; it does not forbid
            // plaintext. reqwest defaults `https_only` to false, so without
            // this an https manifest URL that 302s to http would be followed
            // in the clear. Reject the DOWNGRADE rather than all plaintext,
            // so the loopback http fixture in tests still works.
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if is_plaintext_downgrade(attempt.previous(), attempt.url()) {
                    // reqwest's Display prints only kind + URL and never the
                    // source, and anyhow!("{e}") drops the chain — so without
                    // this log the refusal fires completely invisibly and is
                    // indistinguishable from a malformed Location header.
                    log::warn!(
                        "refusing an HTTPS → plaintext redirect to {}",
                        attempt.url()
                    );
                    attempt.error("refusing to follow a redirect from HTTPS to plaintext HTTP")
                } else if attempt.previous().len() > 10 {
                    // error(), NOT stop(): stop() returns the 30x itself as
                    // Ok, and error_for_status only rejects 4xx/5xx — so a
                    // redirect loop would "succeed" with the redirect body in
                    // .tmp, and Task 13 would tell the user "checksum
                    // mismatch — update Chronicle" for a CDN misconfiguration.
                    // `> 10` matches reqwest's own default cap, whose chain
                    // also counts the initial URL.
                    log::warn!(
                        "refusing to follow more than 10 redirects, stopped at {}",
                        attempt.url()
                    );
                    attempt.error("too many redirects")
                } else {
                    attempt.follow()
                }
            }))
            .build()?;
        // Design Integration Architecture: offline/DNS/5xx are prefixed with
        // "download failed: " so the banner has a stable lead-in. The reqwest
        // text (and the URL it embeds) still rides along — mapping error
        // kinds to friendlier copy belongs with Task 15's banner work.
        let mut resp = client
            .get(url)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| anyhow::anyhow!("download failed: {e}"))?;
        // A plain GET with no Range header has exactly ONE correct success
        // status, so anything else is a failed download however friendly it
        // looks. Neither of the obvious guards is enough on its own:
        //
        //   - `error_for_status` only rejects 4xx/5xx, so 1xx/3xx pass.
        //   - the redirect policy never sees a 3xx whose status is outside
        //     {301,302,303,307,308}, or that has no resolvable Location —
        //     tower-http returns those as Ok without consulting it
        //     (follow_redirect/mod.rs:292 and :308, tower-http 0.6.11).
        //   - `is_success()` accepts 204/206/203: a 204 (what corporate web
        //     filters and captive portals answer a blocked URL with) decodes
        //     as zero-length and lands an EMPTY .tmp as a complete download.
        //
        // Every one of those ends the same way — Task 13 hashes the wrong
        // bytes and tells the user "checksum mismatch — update Chronicle"
        // when the model is fine and the network is not.
        anyhow::ensure!(
            resp.status() == reqwest::StatusCode::OK,
            "download failed: unexpected HTTP status {}",
            resp.status()
        );
        if let Some(len) = resp.content_length() {
            total_atom.store(len, Ordering::Relaxed);
        }
        // 0600 from birth, not just at finalize: File::create would use
        // 0666 & umask, leaving the partial world-readable for the whole
        // download. Unlinking first is what makes that the normal case —
        // open(2) ignores the mode argument unless O_CREAT actually creates,
        // so reopening a leftover .tmp would silently keep its old
        // permissions. (Not airtight: if the dir is read-only but the file
        // is writable, the unlink fails and the old mode survives. That path
        // cannot finish anyway — finalize's rename would fail too.)
        let _ = std::fs::remove_file(tmp);
        let mut file = {
            use std::os::unix::fs::OpenOptionsExt as _;
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(tmp)?
        };
        loop {
            let chunk = tokio::time::timeout(CHUNK_TIMEOUT, resp.chunk())
                .await
                .map_err(|_| {
                    anyhow::anyhow!("download stalled (no data for {CHUNK_TIMEOUT:?})")
                })??;
            let Some(chunk) = chunk else { break };
            file.write_all(&chunk)?;
            bytes_atom.fetch_add(chunk.len() as u64, Ordering::Relaxed);
        }
        // fsync, not flush: `Write::flush` on a `fs::File` is a no-op (there
        // is no userspace buffer), so without this the caller could rename a
        // 1.5 GB file into place with its bytes still only in the page cache,
        // and a power loss would land a corrupt model at the final path.
        // `verify_sha1` reads back through that same cache, so it would not
        // catch the gap either. On the blocking pool because forcing
        // writeback of a multi-GB file is not a short operation.
        // Both arms get the same banner lead-in. JoinError::Display forwards
        // arbitrary panic payloads, and a bare io::Error reads as
        // "No space left on device (os error 28)" — neither belongs in the
        // UI unprefixed. The real error goes to the log either way.
        match tokio::task::spawn_blocking(move || file.sync_all()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                log::error!("model write failed: {e}");
                anyhow::bail!("download failed: could not write the model file ({e})");
            }
            Err(e) => {
                log::error!("model write task failed: {e}");
                anyhow::bail!("download failed: could not write the model file");
            }
        }
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
pub async fn finalize_model(tmp: &Path, dest: &Path, expected_sha1: &str) -> anyhow::Result<()> {
    let result: anyhow::Result<()> = async {
        use std::os::unix::fs::PermissionsExt; // for Permissions::from_mode
        let tmp_owned = tmp.to_path_buf();
        let expected = expected_sha1.to_string();
        let matches = tokio::task::spawn_blocking(move || {
            chronicle_transcription::verify_sha1(&tmp_owned, &expected)
        })
        .await
        // JoinError::Display forwards arbitrary panic payloads, and this
        // error string reaches the UI banner — keep it fixed.
        .map_err(|e| {
            log::error!("model verification task failed: {e}");
            anyhow::anyhow!("could not verify the downloaded model")
        })??;
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

/// Sent to the main event loop when a provision completes successfully —
/// the loop persists the variant (sole settings writer, AD-10).
#[derive(Debug)]
pub enum ProvisionEvent {
    Ready { variant: ModelVariant },
}

/// Clears the in-flight flag on every exit path, including a panic. Without
/// this an early return anywhere in `provision()` would freeze status in
/// `Downloading`/`Loading` forever, with no way to retry short of a restart.
struct InFlightGuard<'a>(&'a std::sync::atomic::AtomicBool);

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Owns everything a provision needs: the status cell it publishes to, the
/// engine slot it installs into, the data dir, and the one-at-a-time guard.
pub struct ProvisionerContext {
    cell: Arc<TranscriptionStatusCell>,
    handle: Arc<EngineHandle>,
    base_dir: std::path::PathBuf,
    in_flight: std::sync::atomic::AtomicBool,
    /// AD-11: the variant whose *load* failed last. A retry of that same
    /// variant deletes the on-disk file and re-downloads, on the theory that
    /// a file which cannot be loaded is more likely corrupt than the loader
    /// is broken.
    last_load_failed: std::sync::Mutex<Option<ModelVariant>>,
    /// Test seam only — production resolves URLs via `model_info(variant).url`
    /// so the pinned manifest stays the single source of truth.
    url_override: Option<String>,
}

impl ProvisionerContext {
    pub fn new(
        cell: Arc<TranscriptionStatusCell>,
        handle: Arc<EngineHandle>,
        base_dir: &Path,
    ) -> Arc<Self> {
        Arc::new(Self {
            cell,
            handle,
            base_dir: base_dir.to_path_buf(),
            in_flight: std::sync::atomic::AtomicBool::new(false),
            last_load_failed: std::sync::Mutex::new(None),
            url_override: None,
        })
    }

    fn url_for(&self, variant: ModelVariant) -> &str {
        self.url_override
            .as_deref()
            .unwrap_or_else(|| model_info(variant).url)
    }

    /// Reserve the single provisioning slot AND commit the state the caller
    /// is about to report. Returns `false` when one is already running, which
    /// the caller answers as "busy".
    ///
    /// The state commit is synchronous and deliberate (design §2.2): the
    /// `SetWhisperModel` reply carries the state just *entered*, so it must be
    /// visible in the cell before this returns — otherwise the reply, or a
    /// Status request racing it, could still observe the pre-request state.
    ///
    /// Called by whoever owns the reply — the Task 14 event-loop arm, or a
    /// test. **Never by `provision()` itself**; see its precondition.
    pub fn try_begin(&self, variant: ModelVariant) -> bool {
        if self
            .in_flight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        if self.needs_download(variant) {
            self.cell.set_downloading(variant);
        } else {
            // The ATTEMPTED variant, so a failed switch to an already-present
            // variant reports the right name (Task 3 re-review).
            self.cell.set_loading(variant);
        }
        true
    }

    /// True when the walk has to fetch: the file is absent, or it is present
    /// but flagged by AD-11 as the variant whose load just failed (so it is
    /// about to be deleted and re-fetched).
    fn needs_download(&self, variant: ModelVariant) -> bool {
        !chronicle_transcription::model_present(&self.base_dir, variant)
            || *self.last_load_failed.lock().unwrap() == Some(variant)
    }

    /// Drive one full provision: fetch if needed, verify, land, load, swap.
    ///
    /// **Precondition: the caller already won [`try_begin`].** This must NOT
    /// call it again — a second CAS would lose against its own caller's
    /// reservation and bail out with the flag stuck set, freezing status
    /// forever (codex plan review, critical 1). It starts from the committed
    /// entering state and clears the flag on every exit via a drop guard.
    ///
    /// Returns `()`: every failure is reported through the cell, not to the
    /// caller, because the caller already replied to the user.
    pub async fn provision(
        &self,
        variant: ModelVariant,
        evt_tx: tokio::sync::mpsc::Sender<ProvisionEvent>,
    ) {
        let _guard = InFlightGuard(&self.in_flight);

        // AD-11: a file that failed to load last time is more likely corrupt
        // than the loader is broken, so a retry starts clean.
        if chronicle_transcription::model_present(&self.base_dir, variant)
            && *self.last_load_failed.lock().unwrap() == Some(variant)
        {
            let path = model_path(&self.base_dir, variant);
            log::info!("deleting suspect model file after load failure; re-downloading");
            match std::fs::remove_file(&path) {
                // Only now is the file genuinely gone, so only now is the
                // marker stale. Clearing it on a FAILED delete would drop
                // straight through to loading the same corrupt bytes again,
                // with the cell sitting in Downloading and no download run.
                Ok(()) => *self.last_load_failed.lock().unwrap() = None,
                Err(e) => log::warn!("could not delete suspect model file: {e}"),
            }
        }

        if !chronicle_transcription::model_present(&self.base_dir, variant) {
            // Idempotent on top of try_begin's commit, and the re-zero of the
            // progress atomics is wanted on a retry.
            self.cell.set_downloading(variant);
            let dest = model_path(&self.base_dir, variant);
            let tmp = dest.with_extension("bin.tmp");

            if let Err(e) = check_disk_space(&self.base_dir, model_info(variant).size_bytes) {
                log::error!("model download precheck failed ({variant}): {e}");
                self.cell.set_error(&e.to_string());
                return;
            }
            if let Err(e) = download_model(
                self.url_for(variant),
                &tmp,
                self.cell.progress_bytes(),
                self.cell.progress_total(),
            )
            .await
            {
                log::error!("model download failed ({variant}): {e}");
                self.cell.set_error(&e.to_string());
                return;
            }
            // Downloading → Verifying. The window is the span until
            // finalize_model returns; transient by nature, which is why the
            // checksum test asserts the edge executed via its distinctive
            // error rather than by catching the state.
            self.cell.set_verifying();
            if let Err(e) = finalize_model(&tmp, &dest, model_info(variant).sha1).await {
                log::error!("model verification failed ({variant}): {e}");
                self.cell.set_error(&e.to_string());
                return;
            }
        }

        self.cell.set_loading(variant);
        let base_dir = self.base_dir.clone();
        // Off the runtime: loading a ggml model is seconds of blocking work
        // for a large variant and must not stall tokio workers (HEU-480).
        let load = tokio::task::spawn_blocking(move || {
            chronicle_transcription::TranscriptionEngine::load(&base_dir, variant)
        })
        .await;

        match load {
            Ok(Ok(engine)) => {
                let engine: Arc<dyn chronicle_transcription::Transcriber> = Arc::new(engine);
                // Engine BEFORE Ready, always. The Status handler reads the
                // cell on its own task, so the other order opens a window
                // where a client sees `ready` with `is_loaded()` still false.
                // At boot that was harmless; here it is not — this path runs
                // while capture is already flowing.
                self.handle.set(engine);
                self.cell.set_ready(variant);
                // Clear only when the marker names the variant that just
                // loaded. Clearing unconditionally would let
                // `fail A → succeed B → retry A` reload A's corrupt file
                // instead of re-downloading it, breaking the promise the
                // banner just made ("retrying will re-download the file").
                // This is one slot, not per-variant tracking: `fail A →
                // fail C` still loses A's marker. Good enough — the marker
                // only ever accelerates a retry, never gates correctness.
                {
                    let mut marker = self.last_load_failed.lock().unwrap();
                    if *marker == Some(variant) {
                        *marker = None;
                    }
                }
                log::info!("transcription engine swapped (variant={variant})");
                // Best-effort: the loop persists on receipt (AD-10). A closed
                // receiver means shutdown, not a reason to fail the swap.
                if let Err(e) = evt_tx.send(ProvisionEvent::Ready { variant }).await {
                    log::warn!("could not report Ready for persistence: {e}");
                }
            }
            Ok(Err(e)) => {
                log::error!("transcription engine load failed ({variant}): {e}");
                *self.last_load_failed.lock().unwrap() = Some(variant);
                // TranscriptionError::Display already carries the
                // "model load failed:" prefix — don't double it on the wire.
                self.cell
                    .set_error(&format!("{e} — retrying will re-download the file"));
            }
            Err(e) => {
                log::error!("transcription engine load task failed ({variant}): {e}");
                *self.last_load_failed.lock().unwrap() = Some(variant);
                // JoinError::Display forwards arbitrary panic payloads —
                // keep the wire string fixed; the log line above has {e}.
                self.cell
                    .set_error("model load failed — retrying will re-download the file");
            }
        }
    }
}

/// Startup path: clear stale partials, then load the configured variant if
/// its file is already on disk. Never downloads — first-run provisioning is
/// user-initiated via `SetWhisperModel` (design §2.1).
///
/// **Awaited in `main` before `audio_store_loop` spawns**, which preserves
/// today's no-loss ordering (codex I2).
///
/// This function owns the cell's startup state outright, rather than trusting
/// the creation-time entry `main` made. `main` decides `Loading` vs `Missing`
/// from a presence check taken before `IpcServer::start` and before
/// `supervisor.reconcile().await` — hundreds of milliseconds earlier. If the
/// file disappears in that window, deferring to that stale decision would
/// leave the daemon reporting `Loading` forever: no engine, no error, and no
/// way back until a `SetWhisperModel` arrives. Re-publishing here costs one
/// `ArcSwap` store and closes that hole.
pub async fn boot(
    base_dir: &Path,
    variant: ModelVariant,
    cell: &Arc<TranscriptionStatusCell>,
    handle: &Arc<EngineHandle>,
) {
    cleanup_stale_tmps(base_dir);

    if !chronicle_transcription::model_present(base_dir, variant) {
        // Reconciles the cell with what boot ACTUALLY found. Normally a
        // no-op — `main` already published Missing and logged the
        // "model missing → idle" warning — but it is the one transition back
        // to Missing in the whole state machine, and it is what keeps a file
        // deleted during startup from stranding the daemon in Loading.
        //
        // In that case `main` took the PRESENT branch and logged nothing, so
        // without the warning below the operator would get a Missing banner
        // with no explanation anywhere in the log. Only fires on the real
        // transition; the ordinary no-op stays quiet.
        if cell.stats(base_dir).state != TranscriptionState::Missing {
            log::warn!(
                "whisper model \"{variant}\" was present at startup but is gone now — \
                 transcription is off; the rest of Chronicle runs normally"
            );
        }
        cell.set_missing();
        return;
    }

    // Re-assert Loading for the same reason: `main` set it from an older
    // presence check, and on the "absent then created" ordering it would not
    // have set it at all. That ordering also leaves `main`'s "not downloaded"
    // warning in the log contradicting a successful load — the
    // "transcription engine loaded" line below is what settles it.
    cell.set_loading(variant);
    let owned = base_dir.to_path_buf();
    let load = tokio::task::spawn_blocking(move || {
        chronicle_transcription::TranscriptionEngine::load(&owned, variant)
    })
    .await;

    match load {
        Ok(Ok(engine)) => {
            let engine: Arc<dyn chronicle_transcription::Transcriber> = Arc::new(engine);
            // Engine before Ready — see provision()'s note on the same order.
            handle.set(engine);
            cell.set_ready(variant);
            log::info!(
                "transcription engine loaded (variant={}, backend={})",
                variant,
                if cfg!(feature = "transcription-metal") {
                    "metal"
                } else {
                    "cpu"
                }
            );
        }
        Ok(Err(e)) => {
            log::error!("transcription engine load failed ({variant}): {e} — staying idle");
            cell.set_error(&e.to_string());
        }
        Err(e) => {
            log::error!("transcription engine load task failed ({variant}): {e} — staying idle");
            cell.set_error("model load failed");
        }
    }
}

/// Refused-connection default for [`ProvisionerContext::new_for_test`].
/// NEVER the real manifest URL: a mutation that made `provision()` download
/// when it shouldn't would otherwise pull 148 MB from HuggingFace inside the
/// unit suite. A test that wants a working server passes its own fixture port.
#[cfg(test)]
const NO_NETWORK_URL: &str = "http://127.0.0.1:1/unreachable";

/// Test constructors and accessors. At module scope rather than inside
/// `mod tests` because `main.rs`'s persist tests build a context too.
#[cfg(test)]
impl ProvisionerContext {
    /// Returns an `Arc` because the progress test clones it into a spawned
    /// provision task.
    pub(crate) fn new_for_test(base_dir: &Path) -> Arc<Self> {
        Self::new_for_test_with_url(base_dir, NO_NETWORK_URL)
    }

    pub(crate) fn new_for_test_with_url(base_dir: &Path, url: &str) -> Arc<Self> {
        Arc::new(Self {
            cell: TranscriptionStatusCell::new(parse_variant("base").unwrap()),
            handle: Arc::new(EngineHandle::new()),
            base_dir: base_dir.to_path_buf(),
            in_flight: std::sync::atomic::AtomicBool::new(false),
            last_load_failed: std::sync::Mutex::new(None),
            url_override: Some(url.to_string()),
        })
    }

    /// Simulate a provision already running, without starting one.
    pub(crate) fn mark_in_flight(&self) {
        self.in_flight.store(true, Ordering::Release);
    }

    /// Production never reaches through the context for these — `main`
    /// already holds both Arcs and passes them in — so they are test-only
    /// rather than carrying a dead-code marker.
    pub(crate) fn cell(&self) -> &Arc<TranscriptionStatusCell> {
        &self.cell
    }

    pub(crate) fn handle(&self) -> &Arc<EngineHandle> {
        &self.handle
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
                            // 15s, not an hour: 30x the 500ms cfg(test)
                            // CHUNK_TIMEOUT, so the real test is unchanged
                            // even under heavy scheduling starvation, but a
                            // build that MUTATES the timeout away gets a
                            // truncated-body error after 15s instead of
                            // wedging the whole suite with no output —
                            // libtest has no default per-test timeout.
                            tokio::time::sleep(std::time::Duration::from_secs(15)).await;
                        }
                        None => sock.write_all(&body).await.unwrap(),
                    }
                });
            }
        });
        port
    }

    /// Like `http_fixture`, but writes `split` bytes, waits for `gate`, then
    /// writes the rest. Lets a test observe progress mid-body. Serves exactly
    /// one connection, because the gate is single-use.
    async fn http_fixture_gated(
        body: Vec<u8>,
        split: usize,
        gate: tokio::sync::oneshot::Receiver<()>,
    ) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await; // consume request
            let head = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n", body.len());
            sock.write_all(head.as_bytes()).await.unwrap();
            sock.write_all(&body[..split]).await.unwrap();
            sock.flush().await.unwrap();
            let _ = gate.await;
            sock.write_all(&body[split..]).await.unwrap();
        });
        port
    }

    /// Answers every request with a 302 pointing at itself — a redirect loop.
    /// `connection: close` so each hop is a fresh accept.
    async fn http_fixture_redirect_loop() -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let _ = sock.read(&mut buf).await;
                    let _ = sock
                        .write_all(
                            b"HTTP/1.1 302 Found\r\nlocation: /again\r\n\
                              content-length: 0\r\nconnection: close\r\n\r\n",
                        )
                        .await;
                });
            }
        });
        port
    }

    /// Answers every request with `raw` verbatim — status line, headers and
    /// body — so a test can drive statuses reqwest/tower-http treat specially.
    async fn http_fixture_raw(raw: &'static str) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let _ = sock.read(&mut buf).await;
                    let _ = sock.write_all(raw.as_bytes()).await;
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

    #[tokio::test]
    async fn download_errors_on_stalled_transfer() {
        // Real clock, short CHUNK_TIMEOUT. The `start_paused` form of this
        // test never reached the chunk timeout at all — the paused clock
        // jumped to the 30 s connect deadline before the loopback socket was
        // observed ready, so it passed on a connect error, and deleting the
        // timeout wrapper entirely left it green.
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
        let err = result.expect_err("stall must time out, not hang");
        assert!(
            err.to_string().contains("stalled"),
            "must fail on the chunk-stall timeout, not on connect or a \
             truncated body — got: {err}"
        );
        assert!(!tmp.exists(), "partial cleaned up on failure");
    }

    #[tokio::test]
    async fn download_publishes_progress_before_the_body_completes() {
        // Pins STREAMING, not merely "the bytes arrived". A buffered
        // implementation (`resp.bytes().await`, the whole 1.5 GB in memory)
        // cannot publish a byte count until the entire body is in hand — and
        // the fixture withholds the second half until this test observes the
        // first. So a buffered impl never satisfies the wait below and fails
        // on the bounded loop instead of passing silently, which is what the
        // single-chunk happy-path test above cannot detect.
        let dir = tempdir().unwrap();
        let body: Vec<u8> = (0..4096u32).map(|i| i as u8).collect();
        let split = 1024usize;
        let (gate_tx, gate_rx) = tokio::sync::oneshot::channel();
        let port = http_fixture_gated(body.clone(), split, gate_rx).await;
        let tmp = dir.path().join(MODELS_SUBDIR).join("ggml-base.bin.tmp");
        let bytes = Arc::new(AtomicU64::new(0));
        let total = Arc::new(AtomicU64::new(0));

        let url = format!("http://127.0.0.1:{port}/model");
        let (b2, t2, tmp2) = (Arc::clone(&bytes), Arc::clone(&total), tmp.clone());
        let task = tokio::spawn(async move { download_model(&url, &tmp2, &b2, &t2).await });

        // Bounded so a buffered implementation FAILS rather than hanging.
        // The loop also watches the task: its 1000ms budget outlives the
        // 500ms CHUNK_TIMEOUT, so a starved poll could see the download die
        // of a stall and then blame streaming for it. Break on a finished
        // task and report the real error instead.
        let mut saw_partial = false;
        let mut died_early = false;
        for _ in 0..200 {
            if bytes.load(Ordering::Relaxed) as usize >= split {
                saw_partial = true;
                break;
            }
            if task.is_finished() {
                died_early = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            !died_early,
            "download ended before publishing progress: {:?}",
            task.await.unwrap()
        );
        assert!(
            saw_partial,
            "no progress published before the body completed — not streaming"
        );
        assert_eq!(
            total.load(Ordering::Relaxed),
            body.len() as u64,
            "Content-Length surfaces as soon as the headers land"
        );

        gate_tx.send(()).unwrap();
        task.await.unwrap().unwrap();
        assert_eq!(std::fs::read(&tmp).unwrap(), body, "fetch only — no rename");
        assert_eq!(bytes.load(Ordering::Relaxed), body.len() as u64);
    }

    #[tokio::test]
    async fn download_errors_on_a_redirect_loop() {
        // Covers the redirect-cap branch of the policy closure, which the
        // predicate test below does not reach. The distinguishing signal is
        // Err vs Ok, not the message: reqwest's `stop()` hands the 30x back
        // as a SUCCESS and `error_for_status` only rejects 4xx/5xx, so the
        // buggy form returns Ok(()) with the redirect body sitting in .tmp —
        // which Task 13 would then report to the user as "checksum mismatch
        // — update Chronicle" for what is really a CDN misconfiguration.
        let dir = tempdir().unwrap();
        let port = http_fixture_redirect_loop().await;
        let tmp = dir.path().join(MODELS_SUBDIR).join("ggml-base.bin.tmp");
        let result = download_model(
            &format!("http://127.0.0.1:{port}/start"),
            &tmp,
            &AtomicU64::new(0),
            &AtomicU64::new(0),
        )
        .await;
        assert!(
            result.is_err(),
            "a redirect loop must fail, not land the 302 body as a download"
        );
        assert!(!tmp.exists(), "no partial left behind");
    }

    #[tokio::test]
    async fn download_errors_on_any_non_200_status() {
        // Five statuses that each slip past a *different* guard and would
        // otherwise land as a completed download, then surface to the user
        // as "checksum mismatch — update Chronicle".
        //
        // The message is asserted, not just is_err(): these fixtures are
        // hand-built, and a malformed response or an RST would also produce
        // Err. The string is ours, so it is a stable signal that the status
        // check is what fired — unlike download_errors_on_a_redirect_loop,
        // where the message comes from reqwest.
        for raw in [
            // Past the redirect policy: 300 is not a followed status, so
            // tower-http returns it without consulting the policy at all.
            "HTTP/1.1 300 Multiple Choices\r\nlocation: /elsewhere\r\n\
             content-length: 3\r\nconnection: close\r\n\r\nxxx",
            // Past the redirect policy: a followed status with no Location.
            "HTTP/1.1 302 Found\r\ncontent-length: 3\r\n\
             connection: close\r\n\r\nxxx",
            // Past is_success(): decodes as zero-length, so it would have
            // landed an EMPTY .tmp. This is what corporate web filters and
            // captive portals answer a blocked URL with — the realistic one.
            "HTTP/1.1 204 No Content\r\nconnection: close\r\n\r\n",
            // Past is_success(): no Range was sent, so a 206 is a server
            // bug that would land a truncated body as complete.
            "HTTP/1.1 206 Partial Content\r\ncontent-length: 3\r\n\
             connection: close\r\n\r\nxxx",
            // Past is_success(): a transforming proxy's modified payload.
            "HTTP/1.1 203 Non-Authoritative Information\r\n\
             content-length: 3\r\nconnection: close\r\n\r\nxxx",
        ] {
            let dir = tempdir().unwrap();
            let port = http_fixture_raw(raw).await;
            let tmp = dir.path().join(MODELS_SUBDIR).join("ggml-base.bin.tmp");
            let err = download_model(
                &format!("http://127.0.0.1:{port}/x"),
                &tmp,
                &AtomicU64::new(0),
                &AtomicU64::new(0),
            )
            .await
            .expect_err(raw);
            assert!(
                err.to_string().contains("unexpected HTTP status"),
                "must fail on the status guard, not transport — got {err} for: {raw}"
            );
            assert!(!tmp.exists(), "no partial left behind for: {raw}");
        }
    }

    #[tokio::test]
    async fn download_resets_a_leftover_partials_permissions() {
        // open(2) ignores the mode argument unless O_CREAT actually creates
        // the file, so without the unlink in download_model a leftover .tmp
        // keeps its old mode for the whole download — a world-readable
        // 1.5 GB partial. Deleting that one line leaves every other test in
        // this file green, which is why this exists.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let body = b"fake model bytes".to_vec();
        let port = http_fixture(body.clone(), None).await;
        let tmp = dir.path().join(MODELS_SUBDIR).join("ggml-base.bin.tmp");
        std::fs::create_dir_all(tmp.parent().unwrap()).unwrap();
        std::fs::write(&tmp, b"stale").unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644)).unwrap();

        download_model(
            &format!("http://127.0.0.1:{port}/model"),
            &tmp,
            &AtomicU64::new(0),
            &AtomicU64::new(0),
        )
        .await
        .unwrap();

        let mode = std::fs::metadata(&tmp).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "a reopened partial must not keep its old mode");
        assert_eq!(std::fs::read(&tmp).unwrap(), body, "stale bytes replaced");
    }

    #[test]
    fn redirect_policy_rejects_only_https_to_plaintext() {
        // `rustls-tls` picks the TLS backend; it does not forbid plaintext,
        // and reqwest defaults `https_only` to false. This predicate is what
        // actually stops an https manifest URL from being 302'd into the
        // clear, so it is pinned directly — standing up a TLS origin just to
        // exercise the policy closure would not be worth it.
        let https = reqwest::Url::parse("https://example.com/ggml-base.bin").unwrap();
        let http = reqwest::Url::parse("http://example.com/ggml-base.bin").unwrap();
        assert!(
            is_plaintext_downgrade(std::slice::from_ref(&https), &http),
            "https → http must be refused"
        );
        assert!(
            !is_plaintext_downgrade(std::slice::from_ref(&https), &https),
            "https → https is a normal redirect"
        );
        assert!(
            !is_plaintext_downgrade(std::slice::from_ref(&http), &http),
            "http → http is the loopback fixture's own path"
        );
        assert!(
            !is_plaintext_downgrade(&[], &http),
            "an empty chain is the initial request, not a redirect"
        );
    }

    // ---- Task 13: provisioner state machine ----

    #[tokio::test]
    async fn try_begin_rejects_while_in_flight() {
        let dir = tempdir().unwrap();
        let ctx = ProvisionerContext::new_for_test(dir.path());
        ctx.mark_in_flight(); // simulate a running operation
        let accepted = ctx.try_begin(parse_variant("base").unwrap());
        assert!(!accepted, "second operation must be rejected while busy");
    }

    #[tokio::test]
    async fn try_begin_commits_entering_state_synchronously() {
        // Design §2.2 response contract: the SetWhisperModel reply carries
        // the state just ENTERED. try_begin commits it before returning, so
        // the event loop's reply (and the handler's cell re-read) can never
        // observe the stale pre-request state.
        let dir = tempdir().unwrap();
        // File absent → entering state is Downloading.
        let ctx = ProvisionerContext::new_for_test(dir.path());
        assert!(ctx.try_begin(parse_variant("small").unwrap()));
        let stats = ctx.cell().stats(dir.path());
        assert_eq!(stats.state, TranscriptionState::Downloading);
        assert_eq!(stats.variant, "small");

        // File present → entering state is Loading.
        let dir2 = tempdir().unwrap();
        let models = dir2.path().join(MODELS_SUBDIR);
        std::fs::create_dir_all(&models).unwrap();
        std::fs::write(models.join("ggml-base.bin"), b"present").unwrap();
        let ctx2 = ProvisionerContext::new_for_test(dir2.path());
        assert!(ctx2.try_begin(parse_variant("base").unwrap()));
        assert_eq!(
            ctx2.cell().stats(dir2.path()).state,
            TranscriptionState::Loading
        );
    }

    #[tokio::test]
    async fn provision_download_failure_sets_error_and_emits_no_ready() {
        let dir = tempdir().unwrap();
        // Port 1 refuses connections → download fails fast.
        let ctx = ProvisionerContext::new_for_test_with_url(dir.path(), "http://127.0.0.1:1/x");
        let (evt_tx, mut evt_rx) = tokio::sync::mpsc::channel(4);
        ctx.provision(parse_variant("base").unwrap(), evt_tx).await;
        let stats = ctx.cell().stats(dir.path());
        assert_eq!(stats.state, TranscriptionState::Error);
        assert!(
            stats.error.unwrap().contains("download failed"),
            "generic network failures use the design's 'download failed: …' message"
        );
        assert!(evt_rx.try_recv().is_err(), "no Ready event on failure");
    }

    #[tokio::test]
    async fn provision_checksum_mismatch_walks_verifying_into_error() {
        // Fixture bytes cannot match the manifest digest, so this drives the
        // real Downloading → Verifying → Error path (design §2.1): the
        // distinctive checksum message only exists on the Verifying edge.
        let dir = tempdir().unwrap();
        let port = http_fixture(b"definitely not the model".to_vec(), None).await;
        let ctx = ProvisionerContext::new_for_test_with_url(
            dir.path(),
            &format!("http://127.0.0.1:{port}/x"),
        );
        let (evt_tx, mut evt_rx) = tokio::sync::mpsc::channel(4);
        ctx.provision(parse_variant("base").unwrap(), evt_tx).await;
        let stats = ctx.cell().stats(dir.path());
        assert_eq!(stats.state, TranscriptionState::Error);
        assert!(stats.error.unwrap().contains("checksum mismatch"));
        assert!(
            evt_rx.try_recv().is_err(),
            "no Ready event on verify failure"
        );
        let models = dir.path().join(MODELS_SUBDIR);
        assert!(!models.join("ggml-base.bin").exists(), "nothing landed");
        assert!(
            !models.join("ggml-base.bin.tmp").exists(),
            "partial deleted"
        );
    }

    #[tokio::test]
    async fn try_begin_then_provision_walks_to_terminal_state() {
        // The production pairing (Task 14 event loop): the loop wins
        // try_begin — committing the entering state — then spawns
        // provision(). provision() must run the walk WITHOUT re-CASing the
        // in-flight flag (it would lose against its own caller's
        // reservation and bail with status stuck) and must clear the flag
        // on exit so the next request can begin.
        let dir = tempdir().unwrap();
        let port = http_fixture(b"definitely not the model".to_vec(), None).await;
        let ctx = ProvisionerContext::new_for_test_with_url(
            dir.path(),
            &format!("http://127.0.0.1:{port}/x"),
        );
        let (evt_tx, _evt_rx) = tokio::sync::mpsc::channel(4);
        assert!(ctx.try_begin(parse_variant("base").unwrap()));
        ctx.provision(parse_variant("base").unwrap(), evt_tx).await;
        let stats = ctx.cell().stats(dir.path());
        assert_eq!(
            stats.state,
            TranscriptionState::Error,
            "walk reached its terminal state despite the caller-held begin"
        );
        assert!(
            ctx.try_begin(parse_variant("base").unwrap()),
            "in-flight flag cleared on exit — the slot frees up"
        );
    }

    #[tokio::test]
    async fn failed_switch_leaves_old_engine_serving() {
        // Design §3.6: "Failed switch leaves the old engine serving."
        let dir = tempdir().unwrap();
        let ctx = ProvisionerContext::new_for_test_with_url(dir.path(), "http://127.0.0.1:1/x");
        ctx.handle().set(Arc::new(StubEngine("base")));
        ctx.cell().set_ready(parse_variant("base").unwrap());

        let (evt_tx, _evt_rx) = tokio::sync::mpsc::channel(4);
        ctx.provision(parse_variant("small").unwrap(), evt_tx).await;

        let stats = ctx.cell().stats(dir.path());
        assert_eq!(stats.state, TranscriptionState::Error);
        assert_eq!(
            stats.loaded_variant.as_deref(),
            Some("base"),
            "status keeps reporting the serving engine"
        );
        assert_eq!(
            ctx.handle().get().unwrap().variant(),
            "base",
            "the old engine is still installed in the handle"
        );
    }

    #[tokio::test]
    async fn status_reports_advancing_progress_mid_download() {
        // Design §3.6 / codex S2 (automated half) + NFR-4: while a download
        // runs, cell.stats() answers normally and shows advancing bytes.
        let dir = tempdir().unwrap();
        // 10 bytes arrive, then the fixture stalls — freezing the download
        // in the Downloading state for the poll below. The poll budget
        // (300ms) stays under the cfg(test) CHUNK_TIMEOUT (500ms) so the
        // stall cannot flip the state to Error mid-poll.
        let port = http_fixture(vec![7u8; 1024], Some(10)).await;
        let ctx = ProvisionerContext::new_for_test_with_url(
            dir.path(),
            &format!("http://127.0.0.1:{port}/x"),
        );
        let (evt_tx, _evt_rx) = tokio::sync::mpsc::channel(4);
        let ctx2 = Arc::clone(&ctx);
        let task =
            tokio::spawn(
                async move { ctx2.provision(parse_variant("base").unwrap(), evt_tx).await },
            );

        let mut observed = None;
        let mut last = None;
        for _ in 0..60 {
            let stats = ctx.cell().stats(dir.path());
            if stats.state == TranscriptionState::Downloading
                && stats.download_bytes.unwrap_or(0) >= 10
            {
                observed = Some(stats);
                break;
            }
            // A terminal state means the walk died; stop polling and report
            // what actually happened rather than "never observed progress",
            // which would point at the wrong thing entirely.
            if stats.state == TranscriptionState::Error {
                last = Some(stats);
                break;
            }
            last = Some(stats);
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let stats = observed.unwrap_or_else(|| {
            panic!("never observed advancing download progress; last seen: {last:?}")
        });
        assert_eq!(
            stats.download_total_bytes,
            Some(1024),
            "Content-Length surfaced"
        );
        task.abort(); // stalled fixture never completes; test is done
    }

    #[tokio::test]
    async fn provision_load_failure_on_existing_garbage_marks_error() {
        let dir = tempdir().unwrap();
        let models = dir.path().join(MODELS_SUBDIR);
        std::fs::create_dir_all(&models).unwrap();
        std::fs::write(models.join("ggml-base.bin"), b"not a ggml model").unwrap();
        let ctx = ProvisionerContext::new_for_test(dir.path());
        let (evt_tx, _evt_rx) = tokio::sync::mpsc::channel(4);
        ctx.provision(parse_variant("base").unwrap(), evt_tx).await;
        let stats = ctx.cell().stats(dir.path());
        assert_eq!(stats.state, TranscriptionState::Error);
        assert!(
            !ctx.handle().is_loaded(),
            "no engine appears from a bad file"
        );
    }

    #[tokio::test]
    async fn retry_after_load_failure_deletes_suspect_file() {
        let dir = tempdir().unwrap();
        let models = dir.path().join(MODELS_SUBDIR);
        std::fs::create_dir_all(&models).unwrap();
        std::fs::write(models.join("ggml-base.bin"), b"garbage").unwrap();
        let ctx = ProvisionerContext::new_for_test_with_url(dir.path(), "http://127.0.0.1:1/x");
        let (evt_tx, _evt_rx) = tokio::sync::mpsc::channel(4);
        // First attempt: load fails on garbage → Error, file still there.
        ctx.provision(parse_variant("base").unwrap(), evt_tx.clone())
            .await;
        assert!(
            models.join("ggml-base.bin").exists(),
            "the FIRST failure must leave the file alone — without this, an \
             implementation that deletes on every entry also passes below"
        );
        // Retry: AD-11 — suspect file is deleted, then re-download starts
        // (and fails here on the dead URL, which is fine for the assertion).
        ctx.provision(parse_variant("base").unwrap(), evt_tx).await;
        assert!(
            !models.join("ggml-base.bin").exists(),
            "retry after load failure must delete the corrupt file (AD-11)"
        );
    }

    #[tokio::test]
    async fn boot_with_no_model_reports_missing_and_loads_nothing() {
        // main.rs decides Loading-vs-Missing from a presence check taken
        // hundreds of ms earlier, before IpcServer::start and
        // supervisor.reconcile(). Simulate the stale decision: the cell says
        // Loading, the file is gone. boot() must reconcile it, or the daemon
        // reports Loading forever with no engine and no error.
        let dir = tempdir().unwrap();
        let cell = TranscriptionStatusCell::new(parse_variant("base").unwrap());
        let handle = Arc::new(EngineHandle::new());
        cell.set_loading(parse_variant("base").unwrap());

        boot(dir.path(), parse_variant("base").unwrap(), &cell, &handle).await;

        let stats = cell.stats(dir.path());
        assert_eq!(
            stats.state,
            TranscriptionState::Missing,
            "boot must publish what it actually found, not inherit main's stale check"
        );
        assert!(!handle.is_loaded(), "no engine from an absent file");
    }

    #[tokio::test]
    async fn boot_with_unloadable_model_reports_error_and_loads_nothing() {
        let dir = tempdir().unwrap();
        let models = dir.path().join(MODELS_SUBDIR);
        std::fs::create_dir_all(&models).unwrap();
        std::fs::write(models.join("ggml-base.bin"), b"not a ggml model").unwrap();
        let cell = TranscriptionStatusCell::new(parse_variant("base").unwrap());
        let handle = Arc::new(EngineHandle::new());

        boot(dir.path(), parse_variant("base").unwrap(), &cell, &handle).await;

        let stats = cell.stats(dir.path());
        assert_eq!(stats.state, TranscriptionState::Error);
        assert!(stats.error.is_some(), "the banner needs a reason");
        assert!(!handle.is_loaded(), "no engine appears from a bad file");
    }

    #[tokio::test]
    async fn boot_clears_stale_partials() {
        // A .tmp survives a crash mid-download. Nothing else sweeps it, so
        // deleting boot's cleanup_stale_tmps call must fail a test.
        let dir = tempdir().unwrap();
        let models = dir.path().join(MODELS_SUBDIR);
        std::fs::create_dir_all(&models).unwrap();
        std::fs::write(models.join("ggml-base.bin.tmp"), b"partial").unwrap();
        let cell = TranscriptionStatusCell::new(parse_variant("base").unwrap());
        let handle = Arc::new(EngineHandle::new());

        boot(dir.path(), parse_variant("base").unwrap(), &cell, &handle).await;

        assert!(
            !models.join("ggml-base.bin.tmp").exists(),
            "boot must sweep partials left by a crashed download"
        );
    }

    #[test]
    fn precheck_fails_when_disk_cannot_fit() {
        let dir = tempdir().unwrap();
        let err = check_disk_space(dir.path(), u64::MAX).unwrap_err();
        assert!(err.to_string().contains("free disk space"));
    }

    #[test]
    fn precheck_passes_on_a_healthy_disk() {
        // Without this, an always-Err implementation passes the test above
        // and bricks every download with a green suite. Requiring 0 bytes
        // still has to clear DISK_MARGIN_BYTES, so this also pins that the
        // margin arithmetic doesn't reject a healthy disk. It does NOT pin
        // the field choice — f_bfree or f_bsize would pass here too.
        // Assumes 512 MiB free on the tempdir filesystem.
        let dir = tempdir().unwrap();
        check_disk_space(dir.path(), 0).expect("a temp dir on a working disk has 512 MiB free");
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
