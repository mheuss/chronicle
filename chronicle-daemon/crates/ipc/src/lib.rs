//! IPC server for Chronicle.
//!
//! Listens on a Unix domain socket and handles JSON request/response
//! communication with the Chronicle UI.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Protocol messages
// ---------------------------------------------------------------------------

/// A request from the UI to the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Status,
}

/// A response from the daemon to the UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Status { ok: bool, data: StatusData },
    Error { ok: bool, message: String },
}

/// Payload for a successful status response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusData {
    pub uptime_secs: u64,
    pub version: String,
}

// ---------------------------------------------------------------------------
// Handler trait
// ---------------------------------------------------------------------------

/// Maps incoming requests to responses.
///
/// Implemented by the daemon to provide business logic. The IPC server
/// calls this for each parsed request.
pub trait RequestHandler: Send + Sync + 'static {
    fn handle(&self, req: Request) -> Response;
}

mod server;

pub use server::IpcServer;
pub use tokio_util::sync::CancellationToken;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_status_serializes_to_tagged_json() {
        let json = serde_json::to_string(&Request::Status).unwrap();
        assert_eq!(json, r#"{"type":"status"}"#);
    }

    #[test]
    fn request_status_deserializes_from_tagged_json() {
        let req: Request = serde_json::from_str(r#"{"type":"status"}"#).unwrap();
        assert!(matches!(req, Request::Status));
    }

    #[test]
    fn response_status_serializes_correctly() {
        let resp = Response::Status {
            ok: true,
            data: StatusData {
                uptime_secs: 3412,
                version: "0.1.0".to_string(),
            },
        };
        let json = serde_json::to_string(&resp).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["type"], "status");
        assert_eq!(value["ok"], true);
        assert_eq!(value["data"]["uptime_secs"], 3412);
        assert_eq!(value["data"]["version"], "0.1.0");
    }

    #[test]
    fn response_error_serializes_correctly() {
        let resp = Response::Error {
            ok: false,
            message: "unknown request type: foo".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["type"], "error");
        assert_eq!(value["ok"], false);
        assert_eq!(value["message"], "unknown request type: foo");
    }

    #[test]
    fn request_round_trips_through_json() {
        let original = Request::Status;
        let json = serde_json::to_string(&original).unwrap();
        let decoded: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(original, decoded);
    }
}
