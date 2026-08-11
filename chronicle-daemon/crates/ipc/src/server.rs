use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::{ErrorCode, Request, RequestHandler, Response};

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
    #[error("failed to set permissions on socket at {path}: {source}")]
    Permissions {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// Remove the socket file, logging any failure other than "not found".
fn remove_socket_file(path: &Path) {
    if let Err(e) = std::fs::remove_file(path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        log::warn!("failed to remove socket file {}: {e}", path.display());
    }
}

/// Set the socket file to owner-only (`0600`). On failure, remove the
/// just-created socket file so a failed start leaves no under-permissioned
/// socket on disk, then surface the error.
fn harden_socket_permissions(path: &Path) -> Result<(), ServerError> {
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        remove_socket_file(path);
        return Err(ServerError::Permissions {
            path: path.to_path_buf(),
            source: e,
        });
    }
    Ok(())
}

/// Unix domain socket server for IPC with the Chronicle UI.
///
/// Accepts connections on a Unix socket, reads newline-delimited JSON
/// requests, dispatches to a [`RequestHandler`], and writes JSON responses.
pub struct IpcServer {
    /// Child of the caller's token; cancelling it stops only this server.
    server_token: CancellationToken,
    /// Accept-loop task handle. `shutdown()` takes it, so a later `Drop`
    /// finds `None` and skips its cancellation.
    accept_handle: Option<JoinHandle<()>>,
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
                    remove_socket_file(socket_path);
                }
            }
        }

        let listener = UnixListener::bind(socket_path).map_err(|e| ServerError::Bind {
            path: socket_path.to_path_buf(),
            source: e,
        })?;

        harden_socket_permissions(socket_path)?;

        log::info!("IPC server listening on {}", socket_path.display());

        let handler = Arc::new(handler);
        let path = socket_path.to_path_buf();
        let server_token = cancel.child_token();
        let task_cancel = server_token.clone();

        let accept_handle = tokio::spawn(async move {
            Self::accept_loop(listener, handler, task_cancel).await;
            // Clean up socket file on shutdown
            remove_socket_file(&path);
            log::info!("IPC server stopped");
        });

        Ok(Self {
            server_token,
            accept_handle: Some(accept_handle),
        })
    }

    /// Stop the server: cancel the accept loop and await its exit, which
    /// guarantees the socket file has been removed before this returns.
    ///
    /// In-flight connection tasks are signalled to stop via the shared
    /// cancellation token but are not awaited — `shutdown()` may return
    /// while a connection handler is still finishing.
    pub async fn shutdown(mut self) {
        self.server_token.cancel();
        if let Some(handle) = self.accept_handle.take()
            && let Err(e) = handle.await
        {
            log::warn!("IPC accept-loop task did not exit cleanly: {e}");
        }
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
                            // Detect oversized request (hit the Take limit
                            // without finding a newline delimiter)
                            let hit_limit = buf.len() as u64 == MAX_REQUEST_LINE
                                && !buf.ends_with(b"\n");
                            if hit_limit {
                                log::warn!("IPC request exceeded max line length");
                                let resp = Response::Error {
                                    ok: false,
                                    code: ErrorCode::RequestTooLarge,
                                    message: "request too large".to_string(),
                                };
                                if let Ok(mut json) = serde_json::to_string(&resp) {
                                    json.push('\n');
                                    let _ = writer.write_all(json.as_bytes()).await;
                                }
                                break; // close connection — framing is desynced
                            }

                            let line = match std::str::from_utf8(&buf) {
                                Ok(s) => s.trim(),
                                Err(_) => {
                                    let resp = Response::Error {
                                        ok: false,
                                        code: ErrorCode::InvalidUtf8,
                                        message: "invalid UTF-8".to_string(),
                                    };
                                    let mut json = serde_json::to_string(&resp).unwrap();
                                    json.push('\n');
                                    let _ = writer.write_all(json.as_bytes()).await;
                                    // Reset the take limit before continuing
                                    buf_reader.get_mut().set_limit(MAX_REQUEST_LINE);
                                    continue;
                                }
                            };

                            let response = match serde_json::from_str::<Request>(line) {
                                Ok(req) => handler.handle(req),
                                Err(e) => {
                                    log::warn!("Invalid IPC request: {e}");
                                    Response::Error {
                                        ok: false,
                                        code: ErrorCode::InvalidRequest,
                                        message: "invalid request".to_string(),
                                    }
                                }
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

impl Drop for IpcServer {
    fn drop(&mut self) {
        // Best-effort: cancel the accept loop. Cannot await here, so socket
        // cleanup happens asynchronously; shutdown() is the deterministic path.
        if self.accept_handle.is_some() {
            self.server_token.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AudioStats, CaptureStats, MicState, OcrStats, Request, RequestHandler, Response,
        StatusData, StorageStats, TranscriptionStats,
    };
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
                        capture: CaptureStats::default(),
                        ocr: OcrStats::default(),
                        audio: AudioStats::default(),
                        storage: StorageStats::default(),
                        transcription: TranscriptionStats::default(),
                    },
                },
                Request::SetMicEnabled { .. } => Response::SetMicEnabled {
                    ok: true,
                    state: MicState::On,
                },
                Request::Search { .. } => Response::Search {
                    ok: true,
                    hits: vec![],
                },
                Request::GetScreenshot { .. } => Response::GetScreenshot {
                    ok: true,
                    hit: None,
                },
                Request::PauseCapture => Response::PauseCapture {
                    ok: true,
                    paused: false,
                },
                Request::ResumeCapture => Response::ResumeCapture {
                    ok: true,
                    paused: false,
                },
                Request::SetWhisperModel { .. } => Response::SetWhisperModel {
                    ok: true,
                    status: TranscriptionStats::default(),
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
    async fn server_responds_to_set_mic_enabled_request() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("test.sock");
        let cancel = CancellationToken::new();

        let _server = IpcServer::start(&sock, MockHandler, cancel.clone())
            .await
            .unwrap();

        let stream = UnixStream::connect(&sock).await.unwrap();
        let (reader, mut writer) = tokio::io::split(stream);
        let mut buf_reader = BufReader::new(reader);

        writer
            .write_all(b"{\"type\":\"set_mic_enabled\",\"enabled\":true}\n")
            .await
            .unwrap();

        let mut line = String::new();
        buf_reader.read_line(&mut line).await.unwrap();

        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["type"], "set_mic_enabled");
        assert_eq!(value["ok"], true);
        assert!(value["state"].is_string(), "state should be present");

        cancel.cancel();
    }

    #[tokio::test]
    async fn server_responds_to_set_whisper_model_request() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("test.sock");
        let cancel = CancellationToken::new();

        let _server = IpcServer::start(&sock, MockHandler, cancel.clone())
            .await
            .unwrap();

        let stream = UnixStream::connect(&sock).await.unwrap();
        let (reader, mut writer) = tokio::io::split(stream);
        let mut buf_reader = BufReader::new(reader);

        writer
            .write_all(b"{\"type\":\"set_whisper_model\",\"variant\":\"small\"}\n")
            .await
            .unwrap();

        let mut line = String::new();
        buf_reader.read_line(&mut line).await.unwrap();

        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["type"], "set_whisper_model");
        assert_eq!(value["ok"], true);
        assert!(
            value["status"].is_object(),
            "the reply carries the transcription block, not just a flag"
        );

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
        assert_eq!(value["code"], "invalid_request");

        cancel.cancel();
    }

    #[tokio::test]
    async fn server_error_for_oversized_request_has_code() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("test.sock");
        let cancel = CancellationToken::new();
        let _server = IpcServer::start(&sock, MockHandler, cancel.clone())
            .await
            .unwrap();

        let stream = UnixStream::connect(&sock).await.unwrap();
        let (reader, mut writer) = tokio::io::split(stream);
        let mut buf_reader = BufReader::new(reader);

        // More than MAX_REQUEST_LINE (64 KiB), with no newline delimiter.
        // The write may fail with BrokenPipe because the server closes the
        // connection after reading exactly 64 KiB and sending the error.
        let big = vec![b'x'; 64 * 1024 + 1];
        let _ = writer.write_all(&big).await;

        let mut line = String::new();
        buf_reader.read_line(&mut line).await.unwrap();
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["type"], "error");
        assert_eq!(value["code"], "request_too_large");

        cancel.cancel();
    }

    #[tokio::test]
    async fn server_error_for_invalid_utf8_has_code() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("test.sock");
        let cancel = CancellationToken::new();
        let _server = IpcServer::start(&sock, MockHandler, cancel.clone())
            .await
            .unwrap();

        let stream = UnixStream::connect(&sock).await.unwrap();
        let (reader, mut writer) = tokio::io::split(stream);
        let mut buf_reader = BufReader::new(reader);

        // Invalid UTF-8 byte, then a newline delimiter.
        writer.write_all(&[0xff, b'\n']).await.unwrap();

        let mut line = String::new();
        buf_reader.read_line(&mut line).await.unwrap();
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["type"], "error");
        assert_eq!(value["code"], "invalid_utf8");

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
    async fn server_socket_has_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("test.sock");
        let cancel = CancellationToken::new();

        let _server = IpcServer::start(&sock, MockHandler, cancel.clone())
            .await
            .unwrap();

        let mode = std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "socket should be owner-only, got {:#o}", mode);

        cancel.cancel();
    }

    #[test]
    fn harden_socket_permissions_errors_on_missing_path() {
        // Covers the error-return path: set_permissions fails (ENOENT) and the
        // helper returns ServerError::Permissions. The unlink-on-failure branch
        // is not directly observable here — a missing path has no file to
        // unlink; remove_socket_file's removal is covered separately by
        // remove_socket_file_deletes_existing_and_ignores_missing.
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist.sock");

        let result = harden_socket_permissions(&missing);

        assert!(
            matches!(result, Err(ServerError::Permissions { .. })),
            "expected ServerError::Permissions, got {result:?}"
        );
    }

    #[test]
    fn remove_socket_file_deletes_existing_and_ignores_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sock");
        std::fs::write(&path, b"x").unwrap();

        remove_socket_file(&path);
        assert!(!path.exists(), "file should be removed");

        // Second call on a now-missing path must not panic.
        remove_socket_file(&path);
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

    #[tokio::test]
    async fn shutdown_stops_server_and_removes_socket() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("test.sock");
        let cancel = CancellationToken::new();

        let server = IpcServer::start(&sock, MockHandler, cancel.clone())
            .await
            .unwrap();
        assert!(sock.exists());

        server.shutdown().await;

        assert!(
            !sock.exists(),
            "socket file should be removed after shutdown() returns"
        );
    }

    #[tokio::test]
    async fn drop_without_shutdown_eventually_stops_server() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("test.sock");
        let cancel = CancellationToken::new();

        {
            let _server = IpcServer::start(&sock, MockHandler, cancel.clone())
                .await
                .unwrap();
            assert!(sock.exists());
        } // handle dropped here — Drop cancels, cleanup is asynchronous

        // Drop only cancels; the accept task removes the socket asynchronously.
        // Poll for eventual cleanup rather than asserting immediately.
        let mut removed = false;
        for _ in 0..50 {
            if !sock.exists() {
                removed = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            removed,
            "socket file should be removed after the handle is dropped"
        );
    }
}
