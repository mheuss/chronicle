//! Capture-engine test seam.
//!
//! `EngineDeps` is a zero-cost (fn-pointer) abstraction over the three SCK
//! helpers the engine calls. Production builds use `EngineDeps::default()`,
//! which binds the real helpers. Unit tests in `engine.rs` construct a custom
//! `EngineDeps` to simulate failures without a real SCK runtime.
//!
//! Note: `start_stream` and `stop_stream` return `Result<(), String>` rather
//! than the design doc's `Result<()>` (i.e., `Result<(), CaptureError>`). This
//! deliberate simplification matches the existing production helper
//! signatures in `engine.rs` (which already use `String`); the engine's
//! caller-side code wraps String errors into `CaptureError::ScreenCaptureKit`
//! at use sites. Keeping the seam's error type narrow avoids pulling the
//! full `CaptureError` type into the test boundary.

use objc2::rc::Retained;
use objc2_screen_capture_kit::{SCDisplay, SCStream};

use crate::error::Result;

/// Function-pointer seams for SCK operations the engine performs.
///
/// All three are `fn` (not `Fn` closures) to keep the struct `Copy`-able and
/// avoid heap allocation. Tests substitute alternate implementations; the
/// default implementation binds to the real SCK helpers.
#[derive(Clone, Copy)]
pub(crate) struct EngineDeps {
    pub enumerate_displays: fn() -> Result<Vec<Retained<SCDisplay>>>,
    pub start_stream: fn(&SCStream) -> std::result::Result<(), String>,
    pub stop_stream: fn(&SCStream) -> std::result::Result<(), String>,
}

impl Default for EngineDeps {
    fn default() -> Self {
        Self {
            enumerate_displays: crate::engine::enumerate_displays,
            start_stream: crate::engine::start_stream,
            stop_stream: crate::engine::stop_stream,
        }
    }
}
