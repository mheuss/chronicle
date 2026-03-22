//! Screen capture engine for Chronicle.
//!
//! Uses ScreenCaptureKit to capture all connected displays with adaptive
//! frame rates based on screen activity and input events.

/// Error types for capture operations.
pub mod error;

pub use error::{CaptureError, Result};
