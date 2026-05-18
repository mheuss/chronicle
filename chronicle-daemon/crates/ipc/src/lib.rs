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
    /// Turn microphone capture on or off at runtime.
    SetMicEnabled {
        enabled: bool,
    },
}

/// Stable, machine-readable error code on an error response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Request line exceeded the maximum length.
    RequestTooLarge,
    /// Request bytes were not valid UTF-8.
    InvalidUtf8,
    /// Request was valid UTF-8 but not a valid `Request` JSON value —
    /// malformed JSON, unknown `type`, or schema mismatch.
    InvalidRequest,
}

/// Current microphone capture state, reported to the UI.
///
/// `#[repr(u8)]` so the daemon can publish it through an `AtomicU8` for the
/// `Status` read path (mirrors `chronicle_capture::EngineState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum MicState {
    /// Microphone capture is off.
    #[default]
    Off = 0,
    /// Microphone capture is on.
    On = 1,
    /// A toggle failed because microphone permission is not granted.
    PermissionDenied = 2,
    /// A toggle failed for another reason (see daemon logs).
    Error = 3,
}

impl MicState {
    /// Reconstruct from the `u8` published in an `AtomicU8`. Any unknown
    /// value maps to `Off` — the safe default.
    pub fn from_u8(value: u8) -> Self {
        match value {
            1 => MicState::On,
            2 => MicState::PermissionDenied,
            3 => MicState::Error,
            _ => MicState::Off,
        }
    }
}

/// A response from the daemon to the UI.
///
/// Only ever decoded by a same-version client over the live IPC socket, so
/// the `Error.code` field is required (no `#[serde(default)]`). Cross-version
/// decoding is out of scope until protocol version negotiation (HEU-456).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Status {
        ok: bool,
        data: StatusData,
    },
    /// Result of a `SetMicEnabled` request: the resulting microphone state.
    SetMicEnabled {
        ok: bool,
        state: MicState,
    },
    Error {
        ok: bool,
        code: ErrorCode,
        message: String,
    },
}

/// Payload for a successful status response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusData {
    pub uptime_secs: u64,
    pub version: String,
    pub capture: CaptureStats,
    pub ocr: OcrStats,
    pub audio: AudioStats,
    pub storage: StorageStats,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureStats {
    /// Engine state: "running" | "stopping" | "idle" | "poisoned" | "unknown".
    /// "unknown" is reported before the daemon's first 1 Hz status snapshot
    /// has populated the engine's live state.
    pub state: String,
    pub active_displays: usize,
    /// Total frames delivered by SCK (from CaptureEngine::status()).
    pub frames_captured: u64,
    /// Frames dropped at the capture boundary (from CaptureEngine::status()).
    pub frames_dropped: u64,
    /// Frames fully processed by the pipeline (PipelineCounters).
    pub frames_processed: u64,
    /// Frames that failed post-capture processing (PipelineCounters).
    pub frames_failed: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OcrStats {
    pub enqueued: u64,
    /// Aggregate: channel full + channel closed.
    pub dropped: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioStats {
    pub segments_persisted: u64,
    /// Current microphone capture state.
    pub mic_state: MicState,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageStats {
    pub db_size_bytes: u64,
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

pub use server::{IpcServer, ServerError};
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
                capture: CaptureStats::default(),
                ocr: OcrStats::default(),
                audio: AudioStats::default(),
                storage: StorageStats::default(),
            },
        };
        let json = serde_json::to_string(&resp).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["type"], "status");
        assert_eq!(value["ok"], true);
        assert_eq!(value["data"]["uptime_secs"], 3412);
        assert_eq!(value["data"]["version"], "0.1.0");
        assert!(value["data"]["capture"].is_object());
        assert!(value["data"]["ocr"].is_object());
        assert!(value["data"]["audio"].is_object());
        assert!(value["data"]["storage"].is_object());
    }

    #[test]
    fn capture_stats_round_trip() {
        let stats = CaptureStats {
            state: "running".into(),
            active_displays: 2,
            frames_captured: 100,
            frames_dropped: 3,
            frames_processed: 97,
            frames_failed: 0,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let parsed: CaptureStats = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, stats);
    }

    #[test]
    fn response_error_serializes_correctly() {
        let resp = Response::Error {
            ok: false,
            code: ErrorCode::InvalidRequest,
            message: "unknown request type: foo".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["type"], "error");
        assert_eq!(value["ok"], false);
        assert_eq!(value["code"], "invalid_request");
        assert_eq!(value["message"], "unknown request type: foo");
    }

    #[test]
    fn request_round_trips_through_json() {
        let original = Request::Status;
        let json = serde_json::to_string(&original).unwrap();
        let decoded: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn mic_state_permission_denied_serializes_correctly() {
        let json = serde_json::to_string(&MicState::PermissionDenied).unwrap();
        assert_eq!(json, r#""permission_denied""#);
    }

    #[test]
    fn mic_state_on_serializes_correctly() {
        let json = serde_json::to_string(&MicState::On).unwrap();
        assert_eq!(json, r#""on""#);
    }

    #[test]
    fn mic_state_all_variants_round_trip() {
        let variants = [
            MicState::Off,
            MicState::On,
            MicState::PermissionDenied,
            MicState::Error,
        ];
        for variant in variants {
            let json = serde_json::to_string(&variant).unwrap();
            let decoded: MicState = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, variant);
        }
    }

    #[test]
    fn mic_state_default_is_off() {
        assert_eq!(MicState::default(), MicState::Off);
    }

    #[test]
    fn mic_state_from_u8_on() {
        assert_eq!(MicState::from_u8(1), MicState::On);
    }

    #[test]
    fn mic_state_from_u8_unknown_maps_to_off() {
        assert_eq!(MicState::from_u8(99), MicState::Off);
    }

    #[test]
    fn mic_state_atom_round_trip_holds_for_every_variant() {
        // The daemon publishes MicState through an AtomicU8 (store `as u8`,
        // read via from_u8). Lock the discriminant↔variant contract so a
        // reordered enum cannot silently corrupt the Status read path.
        for variant in [
            MicState::Off,
            MicState::On,
            MicState::PermissionDenied,
            MicState::Error,
        ] {
            assert_eq!(MicState::from_u8(variant as u8), variant);
        }
    }

    #[test]
    fn request_set_mic_enabled_serializes_to_tagged_json() {
        let json = serde_json::to_string(&Request::SetMicEnabled { enabled: true }).unwrap();
        assert_eq!(json, r#"{"type":"set_mic_enabled","enabled":true}"#);
    }

    #[test]
    fn request_set_mic_enabled_round_trips_through_json() {
        let original = Request::SetMicEnabled { enabled: true };
        let json = serde_json::to_string(&original).unwrap();
        let decoded: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn response_set_mic_enabled_serializes_correctly() {
        let resp = Response::SetMicEnabled {
            ok: true,
            state: MicState::On,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["type"], "set_mic_enabled");
        assert_eq!(value["ok"], true);
        assert_eq!(value["state"], "on");
    }

    #[test]
    fn status_response_serializes_audio_with_mic_state() {
        let resp = Response::Status {
            ok: true,
            data: StatusData {
                uptime_secs: 1,
                version: "0.1.0".to_string(),
                capture: CaptureStats::default(),
                ocr: OcrStats::default(),
                audio: AudioStats {
                    segments_persisted: 7,
                    mic_state: MicState::Off,
                },
                storage: StorageStats::default(),
            },
        };
        let json = serde_json::to_string(&resp).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["data"]["audio"]["segments_persisted"], 7);
        assert_eq!(value["data"]["audio"]["mic_state"], "off");
    }
}
