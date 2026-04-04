use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio_util::sync::CancellationToken;

use crate::{Request, RequestHandler, Response};

/// Maximum line length for incoming requests (64 KB).
const MAX_REQUEST_LINE: u64 = 64 * 1024;

/// Errors from the IPC server.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("failed to bind socket at {path}: {source}")]
    Bind {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// Unix domain socket server for IPC with the Chronicle UI.
///
/// Accepts connections on a Unix socket, reads newline-delimited JSON
/// requests, dispatches to a [`RequestHandler`], and writes JSON responses.
pub struct IpcServer {
    #[allow(dead_code)]
    socket_path: PathBuf,
    #[allow(dead_code)]
    cancel: CancellationToken,
}

impl IpcServer {
    /// Start the IPC server.
    ///
    /// Binds to `socket_path`, spawns an accept loop as a tokio task.
    /// If a stale socket file exists, it is removed before binding.
    /// The server stops when `cancel` is triggered, and the socket file
    /// is cleaned up.
    pub async fn start(
        socket_path: &Path,
        handler: impl RequestHandler,
        cancel: CancellationToken,
    ) -> Result<Self, ServerError> {
        // Clean up stale socket file if present
        if socket_path.exists() {
            // Try connecting to see if a daemon is already running
            match tokio::net::UnixStream::connect(socket_path).await {
                Ok(_) => {
                    // Another daemon is listening — don't clobber it
                    return Err(ServerError::Bind {
                        path: socket_path.to_path_buf(),
                        source: std::io::Error::new(
                            std::io::ErrorKind::AddrInUse,
                            "another daemon is already listening on this socket",
                        ),
                    });
                }
                Err(_) => {
                    // Nobody home — remove the stale file
                    log::info!("Removing stale socket file: {}", socket_path.display());
                    std::fs::remove_file(socket_path).ok();
                }
            }
        }

        let listener = UnixListener::bind(socket_path).map_err(|e| ServerError::Bind {
            path: socket_path.to_path_buf(),
            source: e,
        })?;

        log::info!("IPC server listening on {}", socket_path.display());

        let handler = Arc::new(handler);
        let path = socket_path.to_path_buf();
        let task_cancel = cancel.clone();

        tokio::spawn(async move {
            Self::accept_loop(listener, handler, task_cancel).await;
            // Clean up socket file on shutdown
            std::fs::remove_file(&path).ok();
            log::info!("IPC server stopped");
        });

        Ok(Self {
            socket_path: socket_path.to_path_buf(),
            cancel,
        })
    }

    async fn accept_loop(
        listener: UnixListener,
        handler: Arc<dyn RequestHandler>,
        cancel: CancellationToken,
    ) {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                result = listener.accept() => {
                    match result {
                        Ok((stream, _addr)) => {
                            let conn_handler = Arc::clone(&handler);
                            let conn_cancel = cancel.clone();
                            tokio::spawn(async move {
                                Self::handle_connection(stream, conn_handler, conn_cancel).await;
                            });
                        }
                        Err(e) => {
                            log::error!("IPC accept error: {e}");
                        }
                    }
                }
            }
        }
    }

    async fn handle_connection(
        stream: tokio::net::UnixStream,
        handler: Arc<dyn RequestHandler>,
        cancel: CancellationToken,
    ) {
        let (reader, mut writer) = tokio::io::split(stream);
        // Limit reader to MAX_REQUEST_LINE to prevent unbounded allocation
        let mut buf_reader = BufReader::new(reader.take(MAX_REQUEST_LINE));
        let mut buf = Vec::new();

        loop {
            buf.clear();
            // read_until is cancellation-safe (unlike read_line): partial
            // reads are appended to `buf` and resumed correctly.
            tokio::select! {
                _ = cancel.cancelled() => break,
                result = buf_reader.read_until(b'\n', &mut buf) => {
                    match result {
                        Ok(0) => break, // EOF — client disconnected
                        Ok(_) => {
                            let line = match std::str::from_utf8(&buf) {
                                Ok(s) => s.trim(),
                                Err(_) => {
                                    let resp = Response::Error {
                                        ok: false,
                                        message: "invalid UTF-8".to_string(),
                                    };
                                    let mut json = serde_json::to_string(&resp).unwrap();
                                    json.push('\n');
                                    let _ = writer.write_all(json.as_bytes()).await;
                                    continue;
                                }
                            };

                            let response = match serde_json::from_str::<Request>(line) {
                                Ok(req) => handler.handle(req),
                                Err(e) => Response::Error {
                                    ok: false,
                                    message: format!("invalid request: {e}"),
                                },
                            };

                            let mut resp_json = match serde_json::to_string(&response) {
                                Ok(json) => json,
                                Err(e) => {
                                    log::error!("Failed to serialize response: {e}");
                                    break;
                                }
                            };
                            resp_json.push('\n');

                            if writer.write_all(resp_json.as_bytes()).await.is_err() {
                                break; // Write failed — client disconnected
                            }

                            // Reset the take limit for the next request
                            buf_reader.get_mut().set_limit(MAX_REQUEST_LINE);
                        }
                        Err(e) => {
                            log::error!("IPC read error: {e}");
                            break;
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Request, RequestHandler, Response, StatusData};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    /// Mock handler that always returns a fixed status response.
    struct MockHandler;

    impl RequestHandler for MockHandler {
        fn handle(&self, req: Request) -> Response {
            match req {
                Request::Status => Response::Status {
                    ok: true,
                    data: StatusData {
                        uptime_secs: 42,
                        version: "0.1.0-test".to_string(),
                    },
                },
            }
        }
    }

    #[tokio::test]
    async fn server_responds_to_status_request() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("test.sock");
        let cancel = CancellationToken::new();

        let _server = IpcServer::start(&sock, MockHandler, cancel.clone())
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
        assert_eq!(value["data"]["uptime_secs"], 42);

        cancel.cancel();
    }

    #[tokio::test]
    async fn server_returns_error_for_invalid_json() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("test.sock");
        let cancel = CancellationToken::new();

        let _server = IpcServer::start(&sock, MockHandler, cancel.clone())
            .await
            .unwrap();

        let stream = UnixStream::connect(&sock).await.unwrap();
        let (reader, mut writer) = tokio::io::split(stream);
        let mut buf_reader = BufReader::new(reader);

        writer.write_all(b"not json\n").await.unwrap();

        let mut line = String::new();
        buf_reader.read_line(&mut line).await.unwrap();

        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["type"], "error");
        assert_eq!(value["ok"], false);

        cancel.cancel();
    }

    #[tokio::test]
    async fn server_removes_stale_socket_on_start() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("test.sock");

        // Create a stale socket file (just a regular file, not a real socket)
        std::fs::write(&sock, b"stale").unwrap();
        assert!(sock.exists());

        let cancel = CancellationToken::new();
        let _server = IpcServer::start(&sock, MockHandler, cancel.clone())
            .await
            .unwrap();

        // Should be able to connect — stale file was replaced
        let _stream = UnixStream::connect(&sock).await.unwrap();

        cancel.cancel();
    }

    #[tokio::test]
    async fn server_handles_multiple_requests_per_connection() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("test.sock");
        let cancel = CancellationToken::new();

        let _server = IpcServer::start(&sock, MockHandler, cancel.clone())
            .await
            .unwrap();

        let stream = UnixStream::connect(&sock).await.unwrap();
        let (reader, mut writer) = tokio::io::split(stream);
        let mut buf_reader = BufReader::new(reader);

        // Send two requests on the same connection
        for _ in 0..2 {
            writer.write_all(b"{\"type\":\"status\"}\n").await.unwrap();
            let mut line = String::new();
            buf_reader.read_line(&mut line).await.unwrap();
            let value: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(value["type"], "status");
            line.clear();
        }

        cancel.cancel();
    }
}
