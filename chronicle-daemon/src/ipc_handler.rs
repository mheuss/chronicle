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

/// In-process control message: a mic toggle request plus its reply channel.
pub(crate) struct MicCommand {
    pub enabled: bool,
    pub reply: std::sync::mpsc::SyncSender<chronicle_ipc::MicState>,
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
#[allow(dead_code)] // fields read by Status handler in HEU-242 T8
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
    storage_db_size: Arc<AtomicU64>,
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
}

impl DaemonHandler {
    pub fn new(
        counters: Arc<PipelineCounters>,
        engine_status: Arc<ArcSwap<CaptureStatusSnapshot>>,
        storage_db_size: Arc<AtomicU64>,
        mic_tx: mpsc::Sender<MicCommand>,
        mic_state: Arc<AtomicU8>,
        mic_ready: Arc<AtomicBool>,
    ) -> Self {
        Self {
            started_at: Instant::now(),
            counters,
            engine_status,
            storage_db_size,
            mic_tx,
            mic_state,
            mic_ready,
            mic_reply_timeout: Duration::from_secs(20),
        }
    }
}

fn engine_state_str(state: Option<EngineState>) -> String {
    match state {
        Some(EngineState::Running) => "running",
        Some(EngineState::Stopping) => "stopping",
        Some(EngineState::Idle) => "idle",
        Some(EngineState::Poisoned) => "poisoned",
        None => "unknown",
    }
    .to_string()
}

impl RequestHandler for DaemonHandler {
    fn handle(&self, req: Request) -> Response {
        match req {
            Request::Status => {
                let c = self.counters.snapshot();
                let cap = self.engine_status.load();
                Response::Status {
                    ok: true,
                    data: StatusData {
                        uptime_secs: self.started_at.elapsed().as_secs(),
                        version: env!("CARGO_PKG_VERSION").to_string(),
                        capture: CaptureStats {
                            state: engine_state_str(cap.state),
                            active_displays: cap.active_displays,
                            frames_captured: cap.frames_captured,
                            frames_dropped: cap.frames_dropped,
                            frames_processed: c.frames_processed,
                            frames_failed: c.frames_failed,
                            paused: false,
                        },
                        ocr: OcrStats {
                            enqueued: c.ocr_enqueued,
                            dropped: c.ocr_dropped,
                        },
                        audio: AudioStats {
                            segments_persisted: c.audio_segments_persisted,
                            mic_state: MicState::from_u8(self.mic_state.load(Ordering::Acquire)),
                        },
                        storage: StorageStats {
                            db_size_bytes: self.storage_db_size.load(Ordering::Relaxed),
                            ..Default::default()
                        },
                    },
                }
            }
            // Stub. Real handler lands in HEU-242 T9; HEU-470 adds audio results.
            Request::Search { .. } => Response::Error {
                ok: false,
                code: chronicle_ipc::ErrorCode::InvalidRequest,
                message: "search not yet implemented".to_string(),
            },
            // Stub. Real handler lands in HEU-242 T10.
            Request::GetScreenshot { .. } => Response::Error {
                ok: false,
                code: chronicle_ipc::ErrorCode::InvalidRequest,
                message: "get_screenshot not yet implemented".to_string(),
            },
            // Stub. Real handler lands in HEU-242 T11/T12.
            Request::PauseCapture => Response::Error {
                ok: false,
                code: chronicle_ipc::ErrorCode::InvalidRequest,
                message: "pause_capture not yet implemented".to_string(),
            },
            // Stub. Real handler lands in HEU-242 T11/T12.
            Request::ResumeCapture => Response::Error {
                ok: false,
                code: chronicle_ipc::ErrorCode::InvalidRequest,
                message: "resume_capture not yet implemented".to_string(),
            },
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
        }
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

    #[test]
    fn status_returns_version_and_uptime_and_nested_stats() {
        use arc_swap::ArcSwap;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64};

        let counters = crate::pipeline::counters::PipelineCounters::new();
        let snapshot = Arc::new(ArcSwap::from_pointee(CaptureStatusSnapshot::default()));
        let db_size = Arc::new(AtomicU64::new(0));
        let (mic_tx, _mic_rx) = tokio::sync::mpsc::channel(8);
        let mic_state = Arc::new(AtomicU8::new(0));
        let mic_ready = Arc::new(AtomicBool::new(true));
        let handler = DaemonHandler::new(counters, snapshot, db_size, mic_tx, mic_state, mic_ready);
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
            }
            other => panic!("expected Status response, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn integration_status_round_trip_through_server() {
        use arc_swap::ArcSwap;
        use chronicle_ipc::{CancellationToken, IpcServer};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixStream;

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("test.sock");
        let cancel = CancellationToken::new();

        let counters = crate::pipeline::counters::PipelineCounters::new();
        let snapshot = Arc::new(ArcSwap::from_pointee(CaptureStatusSnapshot::default()));
        let db_size = Arc::new(AtomicU64::new(0));
        let (mic_tx, _mic_rx) = tokio::sync::mpsc::channel(8);
        let mic_state = Arc::new(AtomicU8::new(0));
        let mic_ready = Arc::new(AtomicBool::new(true));
        let handler = DaemonHandler::new(counters, snapshot, db_size, mic_tx, mic_state, mic_ready);
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

        cancel.cancel();
    }

    /// Build a `DaemonHandler` with a control channel of the given capacity.
    /// `ready` sets the readiness gate; pass `true` to exercise post-startup
    /// behavior. Returns the handler and the receiver so a test can play the
    /// daemon loop's role (consume `MicCommand`, reply on `cmd.reply`).
    fn handler_with_mic_channel(
        capacity: usize,
        ready: bool,
    ) -> (
        DaemonHandler,
        tokio::sync::mpsc::Receiver<MicCommand>,
        Arc<std::sync::atomic::AtomicU8>,
    ) {
        use arc_swap::ArcSwap;
        use std::sync::atomic::{AtomicBool, AtomicU64};

        let counters = crate::pipeline::counters::PipelineCounters::new();
        let snapshot = Arc::new(ArcSwap::from_pointee(CaptureStatusSnapshot::default()));
        let db_size = Arc::new(AtomicU64::new(0));
        let (mic_tx, mic_rx) = tokio::sync::mpsc::channel(capacity);
        let mic_state = Arc::new(std::sync::atomic::AtomicU8::new(0));
        let mic_ready = Arc::new(AtomicBool::new(ready));
        let handler = DaemonHandler::new(
            counters,
            snapshot,
            db_size,
            mic_tx,
            Arc::clone(&mic_state),
            mic_ready,
        );
        (handler, mic_rx, mic_state)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_mic_enabled_success() {
        let (handler, mut mic_rx, _atom) = handler_with_mic_channel(8, true);

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

    #[test]
    fn set_mic_enabled_not_ready_returns_error() {
        // mic_ready = false: the event loop is not yet draining the channel.
        // The handler must reject the toggle immediately — no try_send, no
        // block — so this is a plain #[test] (it returns before block_in_place).
        let (handler, _mic_rx, _atom) = handler_with_mic_channel(8, false);

        let resp = handler.handle(Request::SetMicEnabled { enabled: true });
        match resp {
            Response::SetMicEnabled { ok, state } => {
                assert!(!ok, "ok must be false when the toggle was rejected");
                assert_eq!(state, MicState::Error);
            }
            other => panic!("expected SetMicEnabled response, got: {other:?}"),
        }
    }

    #[test]
    fn set_mic_enabled_channel_closed() {
        let (handler, mic_rx, _atom) = handler_with_mic_channel(8, true);
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

    #[test]
    fn set_mic_enabled_channel_full() {
        let (handler, _mic_rx, _atom) = handler_with_mic_channel(1, true);
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
        let (mut handler, mut mic_rx, _atom) = handler_with_mic_channel(8, true);
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
        let (handler, mut mic_rx, _atom) = handler_with_mic_channel(8, true);

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

    #[test]
    fn status_includes_mic_state() {
        let (handler, _mic_rx, atom) = handler_with_mic_channel(8, true);
        atom.store(MicState::On as u8, Ordering::Release);

        let resp = handler.handle(Request::Status);
        match resp {
            Response::Status { data, .. } => assert_eq!(data.audio.mic_state, MicState::On),
            other => panic!("expected Status response, got: {other:?}"),
        }
    }
}
