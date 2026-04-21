use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use arc_swap::ArcSwap;
use chronicle_capture::EngineState;
use chronicle_ipc::{
    AudioStats, CaptureStats, OcrStats, Request, RequestHandler, Response,
    StatusData, StorageStats,
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

#[cfg(test)]
mod tests {
    use super::*;
    use chronicle_ipc::{Request, RequestHandler, Response};

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
        use std::sync::Arc;
        use std::sync::atomic::AtomicU64;
        use chronicle_ipc::{CancellationToken, IpcServer};
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
