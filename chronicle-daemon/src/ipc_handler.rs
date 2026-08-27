use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use chronicle_capture::EngineState;
use chronicle_ipc::{
    AudioStats, CaptureStats, MicState, OcrStats, Request, RequestHandler, Response, StatusData,
    StorageStats,
};
use tokio::sync::mpsc;

use crate::pipeline::counters::PipelineCounters;
use crate::provisioning::TranscriptionStatusCell;

/// Sample rate for the INFO search-latency log: 1 in N successful searches.
/// DEBUG carries every search but is off by default; this sampled INFO line
/// keeps NFR-1 (search p95 < 200ms) observable in production without flooding
/// the logs. See HEU-484.
const SEARCH_SAMPLE_RATE: u64 = 20;

/// Monotonic counter of successful searches, used only to sample the INFO log.
static SEARCH_LOG_COUNT: AtomicU64 = AtomicU64::new(0);

/// Whether the `n`th (0-based) successful search should emit the sampled INFO
/// latency line. Factored out so the cadence is unit-testable.
fn should_sample_search(n: u64) -> bool {
    n.is_multiple_of(SEARCH_SAMPLE_RATE)
}

/// In-process control message: a mic toggle request plus its reply channel.
pub(crate) struct MicCommand {
    pub enabled: bool,
    pub reply: std::sync::mpsc::SyncSender<chronicle_ipc::MicState>,
}

/// In-process control message: a capture pause/resume request plus its
/// reply channel. The reply carries the resulting `paused` flag.
/// Fields are read by the T12 main-loop drain.
pub(crate) struct CaptureCommand {
    pub action: CaptureAction,
    pub reply: std::sync::mpsc::SyncSender<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CaptureAction {
    Pause,
    Resume,
}

/// In-process control message: a model-switch request plus its reply channel.
/// The reply is the event loop's accept/reject decision, not the outcome of
/// the provision — that lands asynchronously in the status cell (AD-9).
///
/// `variant` is a `ModelVariant`, not a `String`: the allow-list check happens
/// in the handler, so nothing downstream can be reached with an unvalidated
/// name (docs/use-cases/transcription.md "Allow-Listed Model Variant").
pub(crate) struct ModelCommand {
    pub variant: chronicle_transcription::ModelVariant,
    pub reply: std::sync::mpsc::SyncSender<bool>,
    /// Cleared by the handler when it stops waiting for the reply.
    ///
    /// A `SyncSender` cannot be probed for a live receiver without consuming
    /// a send, and the verdict is not known until `try_begin` has already
    /// committed state — so liveness needs its own channel. The event loop
    /// reads this **before** committing anything.
    pub still_waiting: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// Engine status as observed by the background refresher. Owned by the
/// daemon; published via `ArcSwap` so the sync `RequestHandler::handle`
/// can read without blocking.
#[derive(Debug, Clone, Default)]
pub struct CaptureStatusSnapshot {
    pub state: Option<EngineState>,
    pub active_displays: usize,
    pub frames_captured: u64,
    pub frames_dropped: u64,
}

/// Cached storage status snapshot. Written every 30s by the storage
/// refresher task in `main()`. Read by `RequestHandler::handle` on every
/// `Status` request — no per-request directory walk.
#[derive(Debug, Clone)]
pub struct StorageStatusSnapshot {
    pub db_size_bytes: u64,
    pub total_disk_usage_bytes: u64,
    pub screenshot_count: u64,
    pub audio_segment_count: u64,
    pub oldest_entry_ms: Option<i64>,
    pub retention_days: u32,
}

impl Default for StorageStatusSnapshot {
    fn default() -> Self {
        Self {
            db_size_bytes: 0,
            total_disk_usage_bytes: 0,
            screenshot_count: 0,
            audio_segment_count: 0,
            oldest_entry_ms: None,
            retention_days: 30,
        }
    }
}

/// Daemon-side request handler.
pub struct DaemonHandler {
    started_at: Instant,
    counters: Arc<PipelineCounters>,
    engine_status: Arc<ArcSwap<CaptureStatusSnapshot>>,
    /// Cached storage status, refreshed every 30s. Read on every `Status`
    /// request — no per-IPC directory walk.
    storage_status: Arc<ArcSwap<StorageStatusSnapshot>>,
    /// Concrete storage handle for direct search / get_screenshot calls.
    storage: Arc<chronicle_storage::Storage>,
    /// Control channel to the `main()` event loop for mic toggles.
    mic_tx: mpsc::Sender<MicCommand>,
    /// Latest microphone state, published by the event loop, read by `Status`.
    mic_state: Arc<AtomicU8>,
    /// Set true once the event loop is draining `mic_tx`. Until then a mic
    /// toggle has no consumer, so the handler rejects it instead of blocking
    /// the caller for the full reply timeout.
    mic_ready: Arc<AtomicBool>,
    /// Backstop for a wedged daemon — a real toggle replies well under 1 s.
    mic_reply_timeout: Duration,
    /// Control channel to the main event loop for capture pause/resume.
    capture_tx: mpsc::Sender<CaptureCommand>,
    /// Latest capture-paused state, read by `Status`.
    capture_paused: Arc<AtomicBool>,
    /// Gate that opens once the main loop starts draining `capture_tx` — and
    /// `model_tx`, which deliberately shares this flag rather than carrying a
    /// third always-equal one. The event loop opens every gate at the same
    /// point, so a separate `model_ready` could only ever disagree by
    /// accident. See the gate-opening block at the start of the event loop in
    /// `main.rs`.
    capture_ready: Arc<AtomicBool>,
    capture_reply_timeout: Duration,
    /// Transcription status cell (HEU-475): boot/provisioner write it, the
    /// `Status` arm reads it.
    transcription_status: Arc<TranscriptionStatusCell>,
    /// Control channel to the main event loop for model switches.
    model_tx: mpsc::Sender<ModelCommand>,
    /// Backstop for a wedged daemon — `try_begin` is a CAS plus one ArcSwap
    /// store, so a real accept/reject replies in microseconds.
    model_reply_timeout: Duration,
}

impl DaemonHandler {
    /// Test accessor: returns the storage handle so tests can seed data and
    /// then exercise the handler against it.
    #[cfg(test)]
    pub(crate) fn storage_for_test(&self) -> Arc<chronicle_storage::Storage> {
        Arc::clone(&self.storage)
    }

    fn send_capture_command(&self, action: CaptureAction) -> Response {
        // Explicit readiness gate — mirrors the mic_ready check in SetMicEnabled.
        if !self.capture_ready.load(Ordering::Acquire) {
            let paused = self.capture_paused.load(Ordering::Acquire);
            return match action {
                CaptureAction::Pause => Response::PauseCapture { ok: false, paused },
                CaptureAction::Resume => Response::ResumeCapture { ok: false, paused },
            };
        }
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel::<bool>(1);
        if self
            .capture_tx
            .try_send(CaptureCommand {
                action,
                reply: reply_tx,
            })
            .is_err()
        {
            let paused = self.capture_paused.load(Ordering::Acquire);
            return match action {
                CaptureAction::Pause => Response::PauseCapture { ok: false, paused },
                CaptureAction::Resume => Response::ResumeCapture { ok: false, paused },
            };
        }
        // Match recv_timeout explicitly so a timeout or sender-drop produces
        // ok:false (with the last known paused state) rather than misleading
        // the UI with ok:true.
        let (ok, paused) =
            match tokio::task::block_in_place(|| reply_rx.recv_timeout(self.capture_reply_timeout))
            {
                Ok(p) => (true, p),
                Err(e) => {
                    log::warn!("capture command reply failed: {e}");
                    (false, self.capture_paused.load(Ordering::Acquire))
                }
            };
        log::info!("capture {:?} -> ok={ok} paused={paused}", action);
        match action {
            CaptureAction::Pause => Response::PauseCapture { ok, paused },
            CaptureAction::Resume => Response::ResumeCapture { ok, paused },
        }
    }

    /// Accept or reject a model switch. Never waits for the download or the
    /// load (AD-9) — the reply says only whether the provisioner took the
    /// job, and progress is observed through `Status`.
    ///
    /// `status` is re-read from the cell AFTER the reply arrives, never
    /// echoed from the request: the event loop commits the entering state
    /// inside `try_begin` before it answers, so this read cannot return the
    /// stale pre-request state (design §2.2).
    ///
    /// It is NOT a promise of `Downloading`/`Loading` specifically. On the
    /// accept path the spawned provision is already running and may have
    /// reached `Verifying`, `Ready` or `Error` before this read — a disk
    /// precheck failure gets there almost immediately. The wire contract is
    /// the honest one: whatever the state is after the decision.
    fn set_whisper_model(&self, variant: &str) -> Response {
        let stats = || self.transcription_status.stats(self.storage.base_dir());

        // The allow-list is the path-traversal guard, and this is the only
        // place the wire string can cross it.
        let Some(variant) = chronicle_transcription::parse_variant(variant) else {
            log::warn!("rejected SetWhisperModel: unknown variant");
            return Response::SetWhisperModel {
                ok: false,
                status: stats(),
            };
        };
        // Same reasoning as the mic gate: before the event loop starts there
        // is no consumer, so reject now rather than blocking the caller for
        // the full reply timeout.
        if !self.capture_ready.load(Ordering::Acquire) {
            return Response::SetWhisperModel {
                ok: false,
                status: stats(),
            };
        }
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel::<bool>(1);
        let still_waiting = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        if self
            .model_tx
            .try_send(ModelCommand {
                variant,
                reply: reply_tx,
                still_waiting: std::sync::Arc::clone(&still_waiting),
            })
            .is_err()
        {
            return Response::SetWhisperModel {
                ok: false,
                status: stats(),
            };
        }
        // handle() is sync but runs inside the async IPC server;
        // block_in_place keeps the runtime healthy. The 20s timeout is a
        // wedged-daemon backstop — try_begin is a CAS plus one ArcSwap store,
        // so a real accept/reject replies in microseconds.
        let ok =
            match tokio::task::block_in_place(|| reply_rx.recv_timeout(self.model_reply_timeout)) {
                Ok(accepted) => accepted,
                Err(e) => {
                    // Withdraw the request before answering. The command is
                    // still sitting in the channel, and without this the loop
                    // would pick it up whenever it frees up and start a
                    // download the reply below is about to call rejected.
                    still_waiting.store(false, std::sync::atomic::Ordering::Release);
                    log::warn!("model command reply failed: {e} — request withdrawn");
                    false
                }
            };
        log::info!("set whisper model {variant} -> ok={ok}");
        Response::SetWhisperModel {
            ok,
            status: stats(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        counters: Arc<PipelineCounters>,
        engine_status: Arc<ArcSwap<CaptureStatusSnapshot>>,
        storage_status: Arc<ArcSwap<StorageStatusSnapshot>>,
        storage: Arc<chronicle_storage::Storage>,
        mic_tx: mpsc::Sender<MicCommand>,
        mic_state: Arc<AtomicU8>,
        mic_ready: Arc<AtomicBool>,
        capture_tx: mpsc::Sender<CaptureCommand>,
        capture_paused: Arc<AtomicBool>,
        capture_ready: Arc<AtomicBool>,
        transcription_status: Arc<TranscriptionStatusCell>,
        model_tx: mpsc::Sender<ModelCommand>,
    ) -> Self {
        Self {
            started_at: Instant::now(),
            counters,
            engine_status,
            storage_status,
            storage,
            mic_tx,
            mic_state,
            mic_ready,
            mic_reply_timeout: Duration::from_secs(20),
            capture_tx,
            capture_paused,
            capture_ready,
            capture_reply_timeout: Duration::from_secs(20),
            transcription_status,
            model_tx,
            model_reply_timeout: Duration::from_secs(20),
        }
    }
}

fn engine_state_str(state: Option<EngineState>, paused: bool) -> &'static str {
    if paused {
        return "paused";
    }
    match state {
        Some(EngineState::Running) => "running",
        Some(EngineState::Stopping) => "stopping",
        Some(EngineState::Idle) => "idle",
        Some(EngineState::Poisoned) => "poisoned",
        None => "unknown",
    }
}

impl RequestHandler for DaemonHandler {
    fn handle(&self, req: Request) -> Response {
        match req {
            Request::Status => {
                let c = self.counters.snapshot();
                let cap = self.engine_status.load();
                let storage = self.storage_status.load();
                let paused = self.capture_paused.load(Ordering::Acquire);
                Response::Status {
                    ok: true,
                    data: StatusData {
                        uptime_secs: self.started_at.elapsed().as_secs(),
                        version: env!("CARGO_PKG_VERSION").to_string(),
                        capture: CaptureStats {
                            state: engine_state_str(cap.state, paused).to_string(),
                            active_displays: cap.active_displays,
                            frames_captured: cap.frames_captured,
                            frames_dropped: cap.frames_dropped,
                            frames_processed: c.frames_processed,
                            frames_failed: c.frames_failed,
                            paused,
                        },
                        ocr: OcrStats {
                            enqueued: c.ocr_enqueued,
                            dropped: c.ocr_dropped,
                        },
                        audio: AudioStats {
                            segments_persisted: c.audio_segments_persisted,
                            transcription_enqueued: c.transcription_enqueued,
                            transcription_dropped: c.transcription_dropped,
                            mic_state: MicState::from_u8(self.mic_state.load(Ordering::Acquire)),
                        },
                        storage: StorageStats {
                            db_size_bytes: storage.db_size_bytes,
                            total_disk_usage_bytes: storage.total_disk_usage_bytes,
                            screenshot_count: storage.screenshot_count,
                            audio_segment_count: storage.audio_segment_count,
                            oldest_entry_ms: storage.oldest_entry_ms,
                            retention_days: storage.retention_days,
                            media_served: c.media_served,
                            media_absent: c.media_absent,
                        },
                        transcription: self.transcription_status.stats(self.storage.base_dir()),
                    },
                }
            }
            Request::Search {
                query,
                limit,
                offset,
            } => {
                let trimmed = query.trim();
                if trimmed.is_empty() {
                    return Response::Search {
                        ok: true,
                        hits: vec![],
                    };
                }
                // Server-side clamp prevents a buggy or malicious caller from
                // requesting huge result sets.
                let limit = (limit.min(200)) as usize;
                let offset = offset as usize;
                let query = trimmed.to_string();
                let storage = Arc::clone(&self.storage);
                // NFR-5: capture the start time BEFORE the blocking call so the
                // elapsed time we log covers everything the caller waits for.
                //
                // That window widened in HEU-624: it now spans the query, the
                // row->hit mapping, and one `stat` per returned hit (up to the
                // 200 clamped above). Measured on a warm local SSD: a full
                // 200-hit page costs 0.2-0.65 ms of stats, against NFR-1's
                // 200 ms p95 budget — well under 1%. A cold cache is slower;
                // 13k paths across many directories measured 268 ms, so treat
                // this as an order of magnitude, not a guarantee. The
                // sampled INFO line below therefore reports end-to-end search
                // latency, which is what a user actually experiences, not the
                // storage query alone.
                let started = Instant::now();
                let counters = Arc::clone(&self.counters);
                let result = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current()
                        .block_on(async {
                            storage
                                .search(
                                    &query,
                                    chronicle_storage::SearchFilter::ScreenOnly,
                                    limit,
                                    offset,
                                )
                                .await
                        })
                        .map(|rows| {
                            let hits: Vec<chronicle_ipc::SearchHit> =
                                rows.into_iter().map(search_hit_from_storage).collect();
                            for h in &hits {
                                count_media_presence(&counters, &h.image_path);
                            }
                            hits
                        })
                });
                match result {
                    Ok(hits) => {
                        let elapsed = started.elapsed();
                        let hit_count = hits.len();
                        // [Security] Log the query LENGTH, not the query content.
                        // The query is whatever the user typed and may contain PII
                        // or other sensitive context they were searching for. The
                        // length is computed inside the log calls (not hoisted) so the
                        // O(n) char count is skipped on the common path — DEBUG off and
                        // no sample this round.
                        log::debug!(
                            "search q_len={} limit={limit} offset={offset} -> {hit_count} hits in {elapsed:?}",
                            query.chars().count()
                        );
                        // HEU-484: DEBUG is off by default, so NFR-1 (search p95 <
                        // 200ms) isn't observable in production. Emit a sampled INFO
                        // line (1 in SEARCH_SAMPLE_RATE) — light enough for INFO,
                        // dense enough to compute p95 offline. Same PII-safe fields,
                        // plus an integer `latency_us` (microseconds, not Duration's
                        // unit-variable `{:?}`) so the percentile is trivial to compute
                        // offline; µs avoids truncating sub-millisecond searches to 0.
                        // The counter sits in this Ok arm by design: failed searches
                        // (rare, not meaningfully timeable) are intentionally excluded
                        // from the sample population. `Relaxed` is sufficient — the
                        // counter is a standalone tally with no happens-before tie to
                        // other state, and `fetch_add` is atomic so no count is lost.
                        let n = SEARCH_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
                        if should_sample_search(n) {
                            let latency_us = elapsed.as_micros();
                            log::info!(
                                "search latency sample: q_len={} hits={hit_count} latency_us={latency_us}",
                                query.chars().count()
                            );
                        }
                        Response::Search { ok: true, hits }
                    }
                    Err(e) => Response::Error {
                        ok: false,
                        code: chronicle_ipc::ErrorCode::InvalidRequest,
                        // [Security] Use Display, not Debug, so we don't leak the
                        // wrapping enum variant name or any internal file paths.
                        message: format!("search failed: {e}"),
                    },
                }
            }
            Request::GetScreenshot { id } => {
                let storage = Arc::clone(&self.storage);
                let counters = Arc::clone(&self.counters);
                let result = tokio::task::block_in_place(|| {
                    let got = tokio::runtime::Handle::current()
                        .block_on(async { storage.get_screenshot_opt(id).await });
                    if let Ok(Some(s)) = &got {
                        count_media_presence(&counters, &s.image_path);
                    }
                    got
                });
                match result {
                    Ok(Some(s)) => Response::GetScreenshot {
                        ok: true,
                        hit: Some(chronicle_ipc::SearchHit {
                            id: s.id,
                            source: chronicle_ipc::SearchHitSource::Screen,
                            timestamp_ms: s.timestamp,
                            app_name: s.app_name,
                            app_bundle_id: s.app_bundle_id,
                            window_title: s.window_title,
                            image_path: s.image_path,
                            snippet: String::new(),
                            rank: 0.0,
                        }),
                    },
                    Ok(None) => Response::GetScreenshot {
                        ok: true,
                        hit: None,
                    },
                    Err(e) => Response::Error {
                        ok: false,
                        code: chronicle_ipc::ErrorCode::InvalidRequest,
                        // [Security] Use Display, not Debug, so we don't leak
                        // internal enum variant names or file paths.
                        message: format!("get_screenshot failed: {e}"),
                    },
                }
            }
            Request::PauseCapture => self.send_capture_command(CaptureAction::Pause),
            Request::ResumeCapture => self.send_capture_command(CaptureAction::Resume),
            Request::SetMicEnabled { enabled } => {
                // The event loop is the only consumer of `mic_tx`, and it does
                // not start until daemon startup finishes. A toggle arriving in
                // that window has no consumer, so reject it now rather than
                // blocking the caller for the full reply timeout.
                if !self.mic_ready.load(Ordering::Acquire) {
                    return Response::SetMicEnabled {
                        ok: false,
                        state: MicState::Error,
                    };
                }
                let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel::<MicState>(1);
                if self
                    .mic_tx
                    .try_send(MicCommand {
                        enabled,
                        reply: reply_tx,
                    })
                    .is_err()
                {
                    return Response::SetMicEnabled {
                        ok: false,
                        state: MicState::Error,
                    };
                }
                // handle() is sync but runs inside the async IPC server;
                // block_in_place keeps the runtime healthy. The 20s timeout is
                // a wedged-daemon backstop — a real toggle replies in well
                // under a second.
                let state =
                    tokio::task::block_in_place(|| reply_rx.recv_timeout(self.mic_reply_timeout))
                        .unwrap_or(MicState::Error);
                // `ok` and `state` must agree: the toggle succeeded only if the
                // mic is now On or Off; Error/PermissionDenied mean it did not.
                let ok = matches!(state, MicState::On | MicState::Off);
                Response::SetMicEnabled { ok, state }
            }
            Request::SetWhisperModel { variant } => self.set_whisper_model(&variant),
        }
    }
}

/// Record one served media path and whether its file was absent.
///
/// Call only from inside a `block_in_place` closure — this does blocking
/// filesystem I/O, and `handler.handle` runs synchronously on a tokio worker
/// (see `crates/ipc/src/server.rs:223`), so a stat placed after a closure
/// returns would block the runtime thread.
///
/// [Security] Counts only. The path is never logged: it embeds capture
/// timestamps and app identifiers, the same reason the search handler logs
/// query length rather than content.
fn count_media_presence(counters: &PipelineCounters, path: &str) {
    counters.media_served.fetch_add(1, Ordering::Relaxed);
    if crate::media_presence::media_is_absent(std::path::Path::new(path)) {
        counters.media_absent.fetch_add(1, Ordering::Relaxed);
    }
}

fn search_hit_from_storage(r: chronicle_storage::SearchResult) -> chronicle_ipc::SearchHit {
    let screenshot = match r.source {
        chronicle_storage::SearchSource::Screen(s) => s,
        chronicle_storage::SearchSource::Audio(_) => {
            // HEU-242 is screen-only. ScreenOnly filter guarantees this
            // branch is unreachable. Defensive panic in case the
            // contract is violated later.
            unreachable!("HEU-242 search filter is ScreenOnly; audio hit returned");
        }
    };
    chronicle_ipc::SearchHit {
        id: screenshot.id,
        source: chronicle_ipc::SearchHitSource::Screen,
        timestamp_ms: screenshot.timestamp,
        app_name: screenshot.app_name,
        app_bundle_id: screenshot.app_bundle_id,
        window_title: screenshot.window_title,
        image_path: screenshot.image_path,
        snippet: r.snippet,
        rank: r.rank,
    }
}

/// Map a microphone-toggle outcome to the wire-level mic state. A failed
/// toggle is classified by the current microphone permission: a missing grant
/// (including `NotDetermined`) is reported as `permission_denied` so the user
/// has something actionable; an authorized-but-still-failed toggle is `error`.
pub(crate) fn map_outcome(
    outcome: chronicle_audio::MicToggleOutcome,
    mic_permission: crate::permissions::MicrophoneStatus,
) -> chronicle_ipc::MicState {
    use crate::permissions::MicrophoneStatus;
    use chronicle_audio::MicToggleOutcome;
    use chronicle_ipc::MicState;
    match outcome {
        MicToggleOutcome::Enabled => MicState::On,
        MicToggleOutcome::Disabled => MicState::Off,
        MicToggleOutcome::Failed { reason } => {
            // HEU-330: log the reason daemon-side; the IPC response carries
            // only MicState, never internal error detail.
            log::warn!("microphone toggle failed: {reason}");
            match mic_permission {
                MicrophoneStatus::Denied
                | MicrophoneStatus::Restricted
                | MicrophoneStatus::NotDetermined => MicState::PermissionDenied,
                MicrophoneStatus::Authorized => MicState::Error,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::MicrophoneStatus;
    use chronicle_audio::MicToggleOutcome;
    use chronicle_ipc::{MicState, Request, RequestHandler, Response};

    #[test]
    fn search_sampling_fires_every_nth_from_first() {
        // 1-in-SEARCH_SAMPLE_RATE, with the very first search (n=0) sampled.
        assert!(should_sample_search(0));
        assert!(should_sample_search(SEARCH_SAMPLE_RATE));
        assert!(should_sample_search(SEARCH_SAMPLE_RATE * 3));
        assert!(!should_sample_search(1));
        assert!(!should_sample_search(SEARCH_SAMPLE_RATE - 1));
        assert!(!should_sample_search(SEARCH_SAMPLE_RATE + 1));
    }

    #[test]
    fn storage_status_snapshot_default_has_zeroes() {
        let snap = StorageStatusSnapshot::default();
        assert_eq!(snap.db_size_bytes, 0);
        assert_eq!(snap.total_disk_usage_bytes, 0);
        assert_eq!(snap.screenshot_count, 0);
        assert_eq!(snap.audio_segment_count, 0);
        assert_eq!(snap.oldest_entry_ms, None);
        assert_eq!(snap.retention_days, 30);
    }

    #[test]
    fn map_outcome_enabled_returns_on() {
        assert_eq!(
            map_outcome(MicToggleOutcome::Enabled, MicrophoneStatus::Authorized),
            MicState::On
        );
    }

    #[test]
    fn map_outcome_disabled_returns_off() {
        assert_eq!(
            map_outcome(MicToggleOutcome::Disabled, MicrophoneStatus::Authorized),
            MicState::Off
        );
    }

    #[test]
    fn map_outcome_failed_denied_returns_permission_denied() {
        assert_eq!(
            map_outcome(
                MicToggleOutcome::Failed {
                    reason: "denied".to_string()
                },
                MicrophoneStatus::Denied
            ),
            MicState::PermissionDenied,
        );
    }

    #[test]
    fn map_outcome_failed_restricted_returns_permission_denied() {
        assert_eq!(
            map_outcome(
                MicToggleOutcome::Failed {
                    reason: "restricted".to_string()
                },
                MicrophoneStatus::Restricted
            ),
            MicState::PermissionDenied,
        );
    }

    #[test]
    fn map_outcome_failed_not_determined_returns_permission_denied() {
        assert_eq!(
            map_outcome(
                MicToggleOutcome::Failed {
                    reason: "not determined".to_string()
                },
                MicrophoneStatus::NotDetermined
            ),
            MicState::PermissionDenied,
        );
    }

    #[test]
    fn map_outcome_failed_authorized_returns_error() {
        assert_eq!(
            map_outcome(
                MicToggleOutcome::Failed {
                    reason: "pipeline error".to_string()
                },
                MicrophoneStatus::Authorized
            ),
            MicState::Error,
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn status_returns_version_and_uptime_and_nested_stats() {
        let (handler, _mic_rx, _atom, _dir) = handler_with_mic_channel(8, true).await;
        std::thread::sleep(std::time::Duration::from_millis(10));

        let resp = handler.handle(Request::Status);
        match resp {
            Response::Status { ok, data } => {
                assert!(ok);
                assert_eq!(data.version, env!("CARGO_PKG_VERSION"));
                assert!(data.uptime_secs < 5, "uptime should be near zero in test");
                assert_eq!(data.capture.frames_captured, 0);
                assert_eq!(data.ocr.enqueued, 0);
                assert_eq!(data.audio.segments_persisted, 0);
                assert_eq!(data.storage.db_size_bytes, 0);
                assert!(!data.capture.paused);
                assert_eq!(data.capture.state, "unknown");
            }
            other => panic!("expected Status response, got: {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn status_includes_transcription_block() {
        let (handler, _mic_rx, _mic_atom, _cap_rx, _cap_paused, _ss, cell, _dir) =
            handler_with_full_channels(8, true).await;
        // Cell starts Missing unconditionally; the fresh temp base_dir means
        // no variant reports downloaded.
        match handler.handle(Request::Status) {
            Response::Status { data, .. } => {
                assert_eq!(
                    data.transcription.state,
                    chronicle_ipc::TranscriptionState::Missing
                );
                assert_eq!(data.transcription.variant, "base");
                assert_eq!(data.transcription.models.len(), 3);
            }
            other => panic!("expected Status response, got: {other:?}"),
        }
        // The handler must read the SHARED cell, not a private copy — a
        // regression to an inline cell would pin Status on Missing forever.
        cell.set_ready(chronicle_transcription::parse_variant("base").unwrap());
        match handler.handle(Request::Status) {
            Response::Status { data, .. } => {
                assert_eq!(
                    data.transcription.state,
                    chronicle_ipc::TranscriptionState::Ready
                );
                assert_eq!(data.transcription.loaded_variant.as_deref(), Some("base"));
            }
            other => panic!("expected Status response, got: {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn integration_status_round_trip_through_server() {
        use chronicle_ipc::{CancellationToken, IpcServer};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixStream;

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("test.sock");
        let cancel = CancellationToken::new();

        let (handler, _mic_rx, _atom, _dir) = handler_with_mic_channel(8, true).await;
        let _server = IpcServer::start(&sock, handler, cancel.clone())
            .await
            .unwrap();

        let stream = UnixStream::connect(&sock).await.unwrap();
        let (reader, mut writer) = tokio::io::split(stream);
        let mut buf_reader = BufReader::new(reader);

        writer.write_all(b"{\"type\":\"status\"}\n").await.unwrap();

        let mut line = String::new();
        buf_reader.read_line(&mut line).await.unwrap();

        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["type"], "status");
        assert_eq!(value["ok"], true);
        assert_eq!(value["data"]["version"], env!("CARGO_PKG_VERSION"));
        assert!(value["data"]["uptime_secs"].as_u64().unwrap() < 5);
        assert!(value["data"]["capture"].is_object());
        assert!(value["data"]["ocr"].is_object());
        assert!(value["data"]["audio"].is_object());
        assert!(value["data"]["storage"].is_object());
        assert!(value["data"]["transcription"].is_object());

        cancel.cancel();
    }

    /// Build a `DaemonHandler` with a control channel of the given capacity.
    /// `ready` sets the readiness gate; pass `true` to exercise post-startup
    /// behavior. Returns the handler and the receiver so a test can play the
    /// daemon loop's role (consume `MicCommand`, reply on `cmd.reply`).
    ///
    /// Delegates to `handler_with_full_channels` and drops the capture-side
    /// return values; tests that don't care about pause/resume use this
    /// thinner shape. `async` because the inner helper opens a real
    /// `Storage` — callers must use `#[tokio::test]`.
    async fn handler_with_mic_channel(
        capacity: usize,
        ready: bool,
    ) -> (
        DaemonHandler,
        tokio::sync::mpsc::Receiver<MicCommand>,
        Arc<std::sync::atomic::AtomicU8>,
        tempfile::TempDir,
    ) {
        let (
            handler,
            mic_rx,
            mic_state,
            _capture_rx,
            _capture_paused,
            _storage_status,
            _transcription_status,
            dir,
        ) = handler_with_full_channels(capacity, ready).await;
        (handler, mic_rx, mic_state, dir)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_mic_enabled_success() {
        let (handler, mut mic_rx, _atom, _dir) = handler_with_mic_channel(8, true).await;

        // Play the daemon loop: receive the command, reply with On.
        tokio::spawn(async move {
            let cmd = mic_rx.recv().await.expect("command should arrive");
            assert!(cmd.enabled);
            let _ = cmd.reply.send(MicState::On);
        });

        let resp = handler.handle(Request::SetMicEnabled { enabled: true });
        match resp {
            // A successful toggle reports ok: true alongside the On state.
            Response::SetMicEnabled { ok, state } => {
                assert!(ok);
                assert_eq!(state, MicState::On);
            }
            other => panic!("expected SetMicEnabled response, got: {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_mic_enabled_not_ready_returns_error() {
        // mic_ready = false: the event loop is not yet draining the channel.
        // The handler rejects the toggle immediately — no try_send, no block.
        // Helper opens a real Storage asynchronously, so this runs under tokio
        // even though the handler arm returns early.
        let (handler, _mic_rx, _atom, _dir) = handler_with_mic_channel(8, false).await;

        let resp = handler.handle(Request::SetMicEnabled { enabled: true });
        match resp {
            Response::SetMicEnabled { ok, state } => {
                assert!(!ok, "ok must be false when the toggle was rejected");
                assert_eq!(state, MicState::Error);
            }
            other => panic!("expected SetMicEnabled response, got: {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_mic_enabled_channel_closed() {
        let (handler, mic_rx, _atom, _dir) = handler_with_mic_channel(8, true).await;
        // Drop the receiver: try_send fails, handle early-returns Error.
        drop(mic_rx);

        let resp = handler.handle(Request::SetMicEnabled { enabled: true });
        match resp {
            // A failed try_send reports ok: false alongside the Error state.
            Response::SetMicEnabled { ok, state } => {
                assert!(!ok);
                assert_eq!(state, MicState::Error);
            }
            other => panic!("expected SetMicEnabled response, got: {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_mic_enabled_channel_full() {
        let (handler, _mic_rx, _atom, _dir) = handler_with_mic_channel(1, true).await;
        // Pre-fill the capacity-1 channel with one unconsumed command so the
        // handler's try_send fails.
        let (pre_reply, _pre_rx) = std::sync::mpsc::sync_channel(1);
        handler
            .mic_tx
            .try_send(MicCommand {
                enabled: true,
                reply: pre_reply,
            })
            .expect("first send fills the channel");

        let resp = handler.handle(Request::SetMicEnabled { enabled: true });
        match resp {
            Response::SetMicEnabled { state, .. } => assert_eq!(state, MicState::Error),
            other => panic!("expected SetMicEnabled response, got: {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_mic_enabled_reply_timeout() {
        let (mut handler, mut mic_rx, _atom, _dir) = handler_with_mic_channel(8, true).await;
        handler.mic_reply_timeout = std::time::Duration::from_millis(50);

        // Receive the command but never reply — keep it alive so the reply
        // sender is not dropped; the handler must hit the timeout.
        tokio::spawn(async move {
            let cmd = mic_rx.recv().await.expect("command should arrive");
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            drop(cmd);
        });

        let resp = handler.handle(Request::SetMicEnabled { enabled: true });
        match resp {
            Response::SetMicEnabled { state, .. } => assert_eq!(state, MicState::Error),
            other => panic!("expected SetMicEnabled response, got: {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_mic_enabled_reply_sender_dropped() {
        let (handler, mut mic_rx, _atom, _dir) = handler_with_mic_channel(8, true).await;

        // Receive the command and drop the reply sender without sending —
        // recv_timeout sees a disconnected channel and the handler maps it
        // to Error.
        tokio::spawn(async move {
            let cmd = mic_rx.recv().await.expect("command should arrive");
            drop(cmd.reply);
        });

        let resp = handler.handle(Request::SetMicEnabled { enabled: true });
        match resp {
            Response::SetMicEnabled { state, .. } => assert_eq!(state, MicState::Error),
            other => panic!("expected SetMicEnabled response, got: {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn status_includes_mic_state() {
        let (handler, _mic_rx, atom, _dir) = handler_with_mic_channel(8, true).await;
        atom.store(MicState::On as u8, Ordering::Release);

        let resp = handler.handle(Request::Status);
        match resp {
            Response::Status { data, .. } => assert_eq!(data.audio.mic_state, MicState::On),
            other => panic!("expected Status response, got: {other:?}"),
        }
    }

    /// Build a `DaemonHandler` with the full constructor signature. Returns the
    /// receivers and the shared atoms so a test can play the daemon loop's role
    /// (consume `MicCommand` / `CaptureCommand`, flip `capture_paused`) and
    /// inspect the storage status snapshot. `capture_ready` defaults to `true`.
    ///
    /// `async` because opening a real `Storage` requires the tokio runtime —
    /// callers must use `#[tokio::test]` (sync `#[test]` cannot reach it).
    async fn handler_with_full_channels(
        capacity: usize,
        mic_ready: bool,
    ) -> (
        DaemonHandler,
        tokio::sync::mpsc::Receiver<MicCommand>,
        Arc<std::sync::atomic::AtomicU8>,
        tokio::sync::mpsc::Receiver<CaptureCommand>,
        Arc<AtomicBool>,
        Arc<ArcSwap<StorageStatusSnapshot>>,
        Arc<TranscriptionStatusCell>,
        tempfile::TempDir,
    ) {
        // Drops the model receiver, the same way `handler_with_mic_channel`
        // drops the capture side — the twelve callers of this helper predate
        // the model channel and none of them care about it. Keeping the
        // widening confined to the inner helper avoids rewriting a
        // twelve-way positional destructure at every one of those sites.
        let (handler, mic_rx, mic_state, cap_rx, cap_paused, ss, tcell, _model_rx, dir) =
            handler_with_full_channels_and_capture_ready(capacity, mic_ready, true).await;
        (
            handler, mic_rx, mic_state, cap_rx, cap_paused, ss, tcell, dir,
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_whisper_model_accepted_reports_entered_state() {
        let (handler, mut model_rx, cell, _dir) = handler_with_model_channel(8, true).await;
        tokio::spawn(async move {
            let cmd = model_rx.recv().await.expect("command should arrive");
            assert_eq!(cmd.variant.as_str(), "small");
            cell.set_downloading(cmd.variant);
            let _ = cmd.reply.send(true);
        });
        let resp = handler.handle(Request::SetWhisperModel {
            variant: "small".into(),
        });
        match resp {
            Response::SetWhisperModel { ok, status } => {
                assert!(ok);
                assert_eq!(
                    status.state,
                    chronicle_ipc::TranscriptionState::Downloading,
                    "reply carries the state just entered, not the stale one"
                );
                assert_eq!(status.variant, "small", "status read from cell, not echo");
            }
            other => panic!("expected SetWhisperModel response, got: {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_whisper_model_rejects_unknown_variant() {
        // The allow-list is the path-traversal guard, so this must be
        // rejected without ever reaching the event loop.
        let (handler, mut model_rx, _cell, _dir) = handler_with_model_channel(8, true).await;
        let resp = handler.handle(Request::SetWhisperModel {
            variant: "tiny".into(),
        });
        match resp {
            Response::SetWhisperModel { ok, .. } => assert!(!ok),
            other => panic!("expected SetWhisperModel response, got: {other:?}"),
        }
        assert!(
            model_rx.try_recv().is_err(),
            "an unknown variant must never reach the provisioner"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_whisper_model_rejects_traversal_attempt() {
        // Same guard, stated in the shape that makes it a security control
        // rather than a typo check.
        let (handler, mut model_rx, _cell, _dir) = handler_with_model_channel(8, true).await;
        let resp = handler.handle(Request::SetWhisperModel {
            variant: "../../etc/passwd".into(),
        });
        match resp {
            Response::SetWhisperModel { ok, .. } => assert!(!ok),
            other => panic!("expected SetWhisperModel response, got: {other:?}"),
        }
        assert!(
            model_rx.try_recv().is_err(),
            "never reaches a filesystem path"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_whisper_model_not_ready_rejects() {
        let (handler, _model_rx, _cell, _dir) = handler_with_model_channel(8, false).await;
        let resp = handler.handle(Request::SetWhisperModel {
            variant: "base".into(),
        });
        match resp {
            Response::SetWhisperModel { ok, .. } => assert!(!ok),
            other => panic!("expected SetWhisperModel response, got: {other:?}"),
        }
    }

    // The producer half of the abandoned-command guard. `begin_model_switch`
    // is tested for "given a cleared flag, start nothing"; this pins that the
    // handler actually clears it, so a refactor cannot silently drop the
    // withdrawal and leave every test green while a timed-out request starts
    // a multi-gigabyte download.
    //
    // Driven by dropping the reply sender rather than waiting out the 20s
    // timeout: `recv_timeout` returns `Err(Disconnected)` immediately, and
    // both errors run the same withdrawal line.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_handler_that_gives_up_withdraws_the_request() {
        let (handler, mut model_rx, _cell, _dir) = handler_with_model_channel(8, true).await;
        let (flag_tx, flag_rx) = std::sync::mpsc::channel();
        tokio::spawn(async move {
            while let Some(cmd) = model_rx.recv().await {
                // Keep the flag, discard the reply channel — the loop is
                // "still busy" and the handler will stop waiting.
                let _ = flag_tx.send(std::sync::Arc::clone(&cmd.still_waiting));
                drop(cmd.reply);
            }
        });

        let resp = handler.handle(Request::SetWhisperModel {
            variant: "base".into(),
        });
        assert!(
            matches!(resp, Response::SetWhisperModel { ok: false, .. }),
            "a request nobody answered is a rejection"
        );

        let flag = flag_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the loop received the command");
        assert!(
            !flag.load(std::sync::atomic::Ordering::Acquire),
            "the handler must withdraw the request it just called rejected"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_whisper_model_reply_does_not_wait_on_the_loop() {
        // Named for what it measures: handler-to-loop round-trip latency.
        // It does NOT pin AD-9's no-wait-on-provisioning property — the fake
        // loop below never provisions, so the bound would pass against a
        // handler that waited out a real download. That property is
        // structural in `begin_model_switch`'s signature — a sync `fn`
        // returning a `JoinHandle` — and no test pins it. The previous name
        // ("...before_any_provisioning_completes") was a false signpost
        // pointing here.
        let (handler, mut model_rx, cell, _dir) = handler_with_model_channel(8, true).await;
        tokio::spawn(async move {
            while let Some(cmd) = model_rx.recv().await {
                cell.set_downloading(cmd.variant);
                let _ = cmd.reply.send(true);
            }
        });
        let started = std::time::Instant::now();
        let resp = handler.handle(Request::SetWhisperModel {
            variant: "base".into(),
        });
        assert!(matches!(resp, Response::SetWhisperModel { ok: true, .. }));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "accept/reject reply must be immediate"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_whisper_model_rejected_still_reports_current_status() {
        // A rejection is not an error response: the UI needs the current
        // transcription block to re-render from, whatever the answer was.
        let (handler, mut model_rx, cell, _dir) = handler_with_model_channel(8, true).await;
        cell.set_ready(chronicle_transcription::parse_variant("base").unwrap());
        tokio::spawn(async move {
            let cmd = model_rx.recv().await.expect("command should arrive");
            let _ = cmd.reply.send(false); // slot busy
        });
        let resp = handler.handle(Request::SetWhisperModel {
            variant: "small".into(),
        });
        match resp {
            Response::SetWhisperModel { ok, status } => {
                assert!(!ok);
                assert_eq!(status.state, chronicle_ipc::TranscriptionState::Ready);
                assert_eq!(
                    status.loaded_variant.as_deref(),
                    Some("base"),
                    "a rejected switch still reports the engine that is serving"
                );
            }
            other => panic!("expected SetWhisperModel response, got: {other:?}"),
        }
    }

    /// Handler plus the model channel and the status cell, so a test can play
    /// the event loop's real role: commit the entering state (which production
    /// does inside `try_begin`, §2.2) BEFORE replying.
    async fn handler_with_model_channel(
        capacity: usize,
        ready: bool,
    ) -> (
        DaemonHandler,
        tokio::sync::mpsc::Receiver<ModelCommand>,
        Arc<TranscriptionStatusCell>,
        tempfile::TempDir,
    ) {
        let (handler, _mic_rx, _mic_state, _cap_rx, _cap_paused, _ss, tcell, model_rx, dir) =
            handler_with_full_channels_and_capture_ready(capacity, true, ready).await;
        (handler, model_rx, tcell, dir)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn search_empty_query_returns_empty_hits_not_error() {
        let (handler, _mic_rx, _mic_atom, _cap_rx, _cap_paused, _ss, _tcell, _dir) =
            handler_with_full_channels(8, true).await;
        let resp = handler.handle(Request::Search {
            query: "   ".into(),
            limit: 10,
            offset: 0,
        });
        match resp {
            Response::Search { ok, hits } => {
                assert!(ok);
                assert!(hits.is_empty());
            }
            other => panic!("expected Search response, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn search_with_no_data_returns_empty_hits() {
        let (handler, _mic_rx, _mic_atom, _cap_rx, _cap_paused, _ss, _tcell, _dir) =
            handler_with_full_channels(8, true).await;
        let resp = handler.handle(Request::Search {
            query: "nothing".into(),
            limit: 10,
            offset: 0,
        });
        match resp {
            Response::Search { ok, hits } => {
                assert!(ok);
                assert!(hits.is_empty());
            }
            other => panic!("expected Search response, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn search_limit_is_clamped_to_max() {
        let (handler, _mic_rx, _mic_atom, _cap_rx, _cap_paused, _ss, _tcell, _dir) =
            handler_with_full_channels(8, true).await;
        let resp = handler.handle(Request::Search {
            query: "any".into(),
            limit: 1_000_000,
            offset: 0,
        });
        assert!(matches!(resp, Response::Search { ok: true, .. }));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_screenshot_unknown_id_returns_hit_none() {
        let (handler, _mic_rx, _mic_atom, _cap_rx, _cap_paused, _ss, _tcell, _dir) =
            handler_with_full_channels(8, true).await;
        let resp = handler.handle(Request::GetScreenshot { id: 999_999 });
        match resp {
            Response::GetScreenshot { ok, hit } => {
                assert!(ok);
                assert!(hit.is_none(), "expected None for unknown id, got {hit:?}");
            }
            other => panic!("expected GetScreenshot response, got {other:?}"),
        }
    }

    /// Insert one screenshot row with a chosen path. Returns its id.
    async fn insert_screenshot_at(handler: &DaemonHandler, image_path: &str) -> i64 {
        handler
            .storage_for_test()
            .insert_screenshot(chronicle_storage::ScreenshotMetadata {
                timestamp: 1_700_000_000_000,
                display_id: "display1".into(),
                app_name: None,
                app_bundle_id: None,
                window_title: None,
                image_path: image_path.into(),
                ocr_text: Some("findable marker text".into()),
                phash: None,
                resolution: None,
            })
            .await
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_screenshot_counts_nothing_when_the_row_is_missing() {
        // Pins the `Ok(Some(_))` guard. Count unconditionally instead and a
        // lookup for a row that does not exist inflates `media_served` with no
        // path ever having been served — and the suite would stay green.
        let (handler, _mic_rx, _mic_atom, _cap_rx, _cap_paused, _ss, _tcell, _dir) =
            handler_with_full_channels(8, true).await;

        let _ = handler.handle(Request::GetScreenshot { id: 999_999 });

        let c = handler.counters.snapshot();
        assert_eq!(c.media_served, 0, "no row means no path was served");
        assert_eq!(c.media_absent, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_screenshot_counts_an_absent_media_file() {
        let (handler, _mic_rx, _mic_atom, _cap_rx, _cap_paused, _ss, _tcell, _dir) =
            handler_with_full_channels(8, true).await;
        let media = tempfile::tempdir().unwrap();
        let missing = media.path().join("gone.heif");
        let id = insert_screenshot_at(&handler, missing.to_str().unwrap()).await;

        let _ = handler.handle(Request::GetScreenshot { id });

        let c = handler.counters.snapshot();
        assert_eq!(c.media_served, 1);
        assert_eq!(c.media_absent, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_screenshot_counts_a_present_file_as_served_only() {
        let (handler, _mic_rx, _mic_atom, _cap_rx, _cap_paused, _ss, _tcell, _dir) =
            handler_with_full_channels(8, true).await;
        let media = tempfile::tempdir().unwrap();
        let present = media.path().join("present.heif");
        std::fs::write(&present, b"x").unwrap();
        let id = insert_screenshot_at(&handler, present.to_str().unwrap()).await;

        let _ = handler.handle(Request::GetScreenshot { id });

        let c = handler.counters.snapshot();
        assert_eq!(c.media_served, 1);
        assert_eq!(
            c.media_absent, 0,
            "a present file must never count as absent"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn search_counts_every_hit_it_returns() {
        // Without this test, Search counting can be omitted entirely and the
        // suite stays green — the GetScreenshot tests never touch that path.
        let (handler, _mic_rx, _mic_atom, _cap_rx, _cap_paused, _ss, _tcell, _dir) =
            handler_with_full_channels(8, true).await;
        let media = tempfile::tempdir().unwrap();
        let present = media.path().join("here.heif");
        std::fs::write(&present, b"x").unwrap();
        insert_screenshot_at(&handler, present.to_str().unwrap()).await;
        insert_screenshot_at(&handler, media.path().join("gone.heif").to_str().unwrap()).await;

        let resp = handler.handle(Request::Search {
            query: "findable".into(),
            limit: 50,
            offset: 0,
        });
        let hits = match resp {
            Response::Search { hits, .. } => hits.len(),
            other => panic!("expected Search response, got {other:?}"),
        };
        assert_eq!(
            hits, 2,
            "fixture must match, or the counts below prove nothing"
        );

        let c = handler.counters.snapshot();
        assert_eq!(c.media_served, 2);
        assert_eq!(c.media_absent, 1);

        // The counters -> StatusData hop. Nothing else covers it: Task 2 tested
        // the counters, Task 3 the struct's serde, Task 4 the Swift decoder —
        // the assignment between them was the one link with no test, and
        // hardcoding both fields to 0 left the whole suite green. The values
        // here are deliberately distinguishable (2 vs 1), so a swapped
        // assignment fails rather than passing on matching zeros.
        match handler.handle(Request::Status) {
            Response::Status { data, .. } => {
                assert_eq!(data.storage.media_served, 2, "served must reach the wire");
                assert_eq!(data.storage.media_absent, 1, "absent must reach the wire");
            }
            other => panic!("expected Status response, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn get_screenshot_existing_id_returns_full_hit() {
        let (handler, _mic_rx, _mic_atom, _cap_rx, _cap_paused, _ss, _tcell, _dir) =
            handler_with_full_channels(8, true).await;
        let storage = handler.storage_for_test();

        let meta = chronicle_storage::ScreenshotMetadata {
            timestamp: 1_700_000_000_000,
            display_id: "display1".into(),
            app_name: Some("Terminal".into()),
            app_bundle_id: Some("com.apple.Terminal".into()),
            window_title: Some("zsh".into()),
            image_path: "/tmp/x.heif".into(),
            ocr_text: Some("hello".into()),
            phash: None,
            resolution: None,
        };
        let id = storage.insert_screenshot(meta).await.unwrap();

        let resp = handler.handle(Request::GetScreenshot { id });
        match resp {
            Response::GetScreenshot { ok, hit: Some(h) } => {
                assert!(ok);
                assert_eq!(h.id, id);
                assert_eq!(h.app_name, Some("Terminal".into()));
                assert_eq!(h.image_path, "/tmp/x.heif");
            }
            other => panic!("expected GetScreenshot with Some(hit), got {other:?}"),
        }
    }

    /// Like [`handler_with_full_channels`] but also accepts a `capture_ready`
    /// flag so tests can simulate the pre-startup state where the main loop has
    /// not yet begun draining `capture_tx`.
    ///
    /// Returns the backing `TempDir` so the caller can hold it for the test's
    /// scope. Dropping it cleans up the storage directory; previously the
    /// helper called `dir.keep()` and leaked one tempdir per test run.
    async fn handler_with_full_channels_and_capture_ready(
        capacity: usize,
        mic_ready: bool,
        capture_ready: bool,
    ) -> (
        DaemonHandler,
        tokio::sync::mpsc::Receiver<MicCommand>,
        Arc<std::sync::atomic::AtomicU8>,
        tokio::sync::mpsc::Receiver<CaptureCommand>,
        Arc<AtomicBool>,
        Arc<ArcSwap<StorageStatusSnapshot>>,
        Arc<TranscriptionStatusCell>,
        tokio::sync::mpsc::Receiver<ModelCommand>,
        tempfile::TempDir,
    ) {
        let counters = crate::pipeline::counters::PipelineCounters::new();
        let cap_snapshot = Arc::new(ArcSwap::from_pointee(CaptureStatusSnapshot::default()));
        let storage_status = Arc::new(ArcSwap::from_pointee(StorageStatusSnapshot::default()));
        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(
            chronicle_storage::Storage::open(chronicle_storage::StorageConfig {
                base_dir: dir.path().to_path_buf(),
                pool_size: 1,
            })
            .await
            .unwrap(),
        );
        let (mic_tx, mic_rx) = tokio::sync::mpsc::channel(capacity);
        let mic_state = Arc::new(std::sync::atomic::AtomicU8::new(0));
        let mic_ready_atom = Arc::new(AtomicBool::new(mic_ready));
        let (capture_tx, capture_rx) = tokio::sync::mpsc::channel(capacity);
        let capture_paused = Arc::new(AtomicBool::new(false));
        let capture_ready_atom = Arc::new(AtomicBool::new(capture_ready));
        let transcription_status =
            TranscriptionStatusCell::new(chronicle_transcription::DEFAULT_VARIANT);
        let (model_tx, model_rx) = tokio::sync::mpsc::channel(capacity);
        let handler = DaemonHandler::new(
            counters,
            cap_snapshot,
            Arc::clone(&storage_status),
            storage,
            mic_tx,
            Arc::clone(&mic_state),
            mic_ready_atom,
            capture_tx,
            Arc::clone(&capture_paused),
            capture_ready_atom,
            Arc::clone(&transcription_status),
            model_tx,
        );
        (
            handler,
            mic_rx,
            mic_state,
            capture_rx,
            capture_paused,
            storage_status,
            transcription_status,
            model_rx,
            dir,
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pause_capture_not_ready_returns_error_immediately() {
        // capture_ready = false: main loop hasn't started draining yet.
        let (handler, _mic_rx, _mic_atom, _cap_rx, _cap_paused, _ss, _tcell, _model_rx, _dir) =
            handler_with_full_channels_and_capture_ready(8, true, false).await;
        let resp = handler.handle(Request::PauseCapture);
        match resp {
            Response::PauseCapture { ok, paused } => {
                assert!(!ok, "ok must be false when capture_ready is false");
                assert!(!paused, "paused should reflect current (false) state");
            }
            other => panic!("expected PauseCapture, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pause_capture_round_trip_through_channel() {
        let (mut handler, _mic_rx, _mic_atom, mut capture_rx, _capture_paused, _ss, _tcell, _dir) =
            handler_with_full_channels(8, true).await;
        handler.capture_reply_timeout = std::time::Duration::from_millis(500);

        tokio::spawn(async move {
            let cmd = capture_rx.recv().await.expect("command should arrive");
            assert!(matches!(cmd.action, CaptureAction::Pause));
            let _ = cmd.reply.send(true);
        });

        let resp = handler.handle(Request::PauseCapture);
        match resp {
            Response::PauseCapture { ok, paused } => {
                assert!(ok);
                assert!(paused);
            }
            other => panic!("expected PauseCapture, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resume_capture_round_trip_through_channel() {
        let (mut handler, _mic_rx, _mic_atom, mut capture_rx, _cap_paused, _ss, _tcell, _dir) =
            handler_with_full_channels(8, true).await;
        handler.capture_reply_timeout = std::time::Duration::from_millis(500);

        tokio::spawn(async move {
            let cmd = capture_rx.recv().await.expect("command should arrive");
            assert!(matches!(cmd.action, CaptureAction::Resume));
            let _ = cmd.reply.send(false);
        });

        let resp = handler.handle(Request::ResumeCapture);
        assert!(matches!(
            resp,
            Response::ResumeCapture {
                ok: true,
                paused: false
            }
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pause_capture_reply_timeout_returns_error_with_cached_paused() {
        // Override capture_reply_timeout to a short value so the test runs fast.
        // The spawned task receives the CaptureCommand but never replies, forcing
        // recv_timeout into the Err branch. The handler should return ok:false
        // with the current capture_paused value.
        let (mut handler, _mic_rx, _mic_atom, mut capture_rx, capture_paused, _ss, _tcell, _dir) =
            handler_with_full_channels(8, true).await;
        handler.capture_reply_timeout = std::time::Duration::from_millis(50);

        // Seed the cached paused state so the assertion is meaningful (not just zero).
        capture_paused.store(true, std::sync::atomic::Ordering::Release);

        // Receive but never reply — keep the command alive so the reply sender
        // is not dropped; the handler must hit the timeout, not the disconnected
        // path.
        tokio::spawn(async move {
            let cmd = capture_rx.recv().await.expect("command should arrive");
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            drop(cmd);
        });

        let resp = handler.handle(Request::PauseCapture);
        match resp {
            Response::PauseCapture { ok, paused } => {
                assert!(!ok, "ok must be false on reply timeout");
                assert!(paused, "paused should reflect cached value (true)");
            }
            other => panic!("expected PauseCapture, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn status_includes_paused_field_and_storage_snapshot_fields() {
        let (handler, _mic_rx, _atom, _capture_rx, capture_paused, storage_status, _tcell, _dir) =
            handler_with_full_channels(8, true).await;

        storage_status.store(Arc::new(StorageStatusSnapshot {
            db_size_bytes: 1024,
            total_disk_usage_bytes: 8192,
            screenshot_count: 50,
            audio_segment_count: 5,
            oldest_entry_ms: Some(1_700_000_000_000),
            retention_days: 14,
        }));
        capture_paused.store(true, std::sync::atomic::Ordering::Release);

        let resp = handler.handle(Request::Status);
        let data = match resp {
            Response::Status { data, .. } => data,
            other => panic!("expected Status response, got: {other:?}"),
        };
        assert!(data.capture.paused, "paused should be true");
        assert_eq!(
            data.capture.state, "paused",
            "state string should be 'paused' when paused"
        );
        assert_eq!(data.storage.db_size_bytes, 1024);
        assert_eq!(data.storage.total_disk_usage_bytes, 8192);
        assert_eq!(data.storage.screenshot_count, 50);
        assert_eq!(data.storage.audio_segment_count, 5);
        assert_eq!(data.storage.oldest_entry_ms, Some(1_700_000_000_000));
        assert_eq!(data.storage.retention_days, 14);
    }
}
