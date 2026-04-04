use std::time::Instant;

use chronicle_ipc::{Request, RequestHandler, Response, StatusData};

/// Daemon-side request handler.
///
/// Maps IPC requests to responses using daemon state. Currently only
/// handles `Status` — new arms are added as downstream tickets land.
pub struct DaemonHandler {
    started_at: Instant,
}

impl DaemonHandler {
    pub fn new() -> Self {
        Self {
            started_at: Instant::now(),
        }
    }
}

impl RequestHandler for DaemonHandler {
    fn handle(&self, req: Request) -> Response {
        match req {
            Request::Status => Response::Status {
                ok: true,
                data: StatusData {
                    uptime_secs: self.started_at.elapsed().as_secs(),
                    version: env!("CARGO_PKG_VERSION").to_string(),
                },
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chronicle_ipc::{Request, RequestHandler, Response};

    #[test]
    fn status_returns_version_and_uptime() {
        let handler = DaemonHandler::new();
        std::thread::sleep(std::time::Duration::from_millis(10));

        let resp = handler.handle(Request::Status);
        match resp {
            Response::Status { ok, data } => {
                assert!(ok);
                assert_eq!(data.version, env!("CARGO_PKG_VERSION"));
                assert!(data.uptime_secs < 5, "uptime should be near zero in test");
            }
            other => panic!("expected Status response, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn integration_status_round_trip_through_server() {
        use chronicle_ipc::{CancellationToken, IpcServer};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixStream;

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("test.sock");
        let cancel = CancellationToken::new();

        let handler = DaemonHandler::new();
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
        // Uptime should be very small in a test
        assert!(value["data"]["uptime_secs"].as_u64().unwrap() < 5);

        cancel.cancel();
    }
}
