use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use arc_swap::ArcSwap;
use chronicle_capture::EngineState;
use chronicle_ipc::{
    AudioStats, CaptureStats, OcrStats, Request, RequestHandler, Response, StatusData, StorageStats,
};

use crate::pipeline::counters::PipelineCounters;

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

/// Daemon-side request handler.
pub struct DaemonHandler {
    started_at: Instant,
    counters: Arc<PipelineCounters>,
    engine_status: Arc<ArcSwap<CaptureStatusSnapshot>>,
    storage_db_size: Arc<AtomicU64>,
}

impl DaemonHandler {
    pub fn new(
        counters: Arc<PipelineCounters>,
        engine_status: Arc<ArcSwap<CaptureStatusSnapshot>>,
        storage_db_size: Arc<AtomicU64>,
    ) -> Self {
        Self {
            started_at: Instant::now(),
            counters,
            engine_status,
            storage_db_size,
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
                        },
                        ocr: OcrStats {
                            enqueued: c.ocr_enqueued,
                            dropped: c.ocr_dropped,
                        },
                        audio: AudioStats {
                            segments_persisted: c.audio_segments_persisted,
                        },
                        storage: StorageStats {
                            db_size_bytes: self.storage_db_size.load(Ordering::Relaxed),
                        },
                    },
                }
            }
        }
    }
}

/// Map a microphone-toggle outcome to the wire-level mic state. A failed
/// toggle is classified by the current microphone permission: a missing grant
/// (including `NotDetermined`) is reported as `permission_denied` so the user
/// has something actionable; an authorized-but-still-failed toggle is `error`.
// HEU-330: called by the daemon control plane once wired (Task 9).
#[allow(dead_code)]
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
            // Design §3.4: log the reason daemon-side; the IPC response
            // carries only MicState, never internal error detail.
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
        use std::sync::atomic::AtomicU64;

        let counters = crate::pipeline::counters::PipelineCounters::new();
        let snapshot = Arc::new(ArcSwap::from_pointee(CaptureStatusSnapshot::default()));
        let db_size = Arc::new(AtomicU64::new(0));
        let handler = DaemonHandler::new(counters, snapshot, db_size);
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
        use std::sync::atomic::AtomicU64;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixStream;

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("test.sock");
        let cancel = CancellationToken::new();

        let counters = crate::pipeline::counters::PipelineCounters::new();
        let snapshot = Arc::new(ArcSwap::from_pointee(CaptureStatusSnapshot::default()));
        let db_size = Arc::new(AtomicU64::new(0));
        let handler = DaemonHandler::new(counters, snapshot, db_size);
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
}
