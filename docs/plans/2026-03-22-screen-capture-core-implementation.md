# Core Frame Capture Implementation Plan

**Date:** 2026-03-22
**Status:** Complete
**Original Design Doc:** docs/plans/2026-03-22-screen-capture-core-design.md
**Issue:** HEU-249
**Branch:** mrheuss/heu-237-screen-capture-engine-multi-display-adaptive-capture-via
**Base Branch:** main

---

> **For Claude:** REQUIRED SUB-SKILL: Use sop:subagent-driven-development or sop:executing-plans to implement this plan task-by-task.

**Goal:** Implement the core screen capture loop — display enumeration, per-display SCStream setup, and frame delivery over an mpsc channel.

**Architecture:** One SCStream per display via the `screencapturekit` crate. A `FrameHandler` implementing `SCStreamOutputTrait` bridges SCK callbacks to a tokio mpsc channel. The `CaptureEngine` owns the streams and exposes start/stop/status.

**Tech Stack:** Rust, screencapturekit (v1.5), tokio (mpsc channel), thiserror, log

---

### Important: ScreenCaptureKit Constraints

ScreenCaptureKit requires a real macOS display and Screen Recording permission.
Unit tests CANNOT exercise SCK directly. The plan separates pure-Rust logic
(testable) from SCK-dependent code (integration-tested on real hardware).

The `screencapturekit` crate's exact API may differ from the design doc's
pseudocode. The implementer MUST check actual method signatures against the
crate's docs (`cargo doc -p screencapturekit --open`) and adapt as needed. The
behavior and structure are what matter, not exact method names.

---

### Task 1: Update Cargo.toml and verify dependencies

**Files:**
- Modify: `chronicle-daemon/crates/capture/Cargo.toml`

**Step 1: Update Cargo.toml**

Replace the current contents with:

```toml
[package]
name = "chronicle-capture"
version = "0.1.0"
edition = "2024"
description = "Screen capture engine — multi-display adaptive capture via ScreenCaptureKit"
license = "MIT"

[dependencies]
screencapturekit = { version = "1.5", features = ["macos_14_0"] }
tokio = { version = "1", features = ["sync"] }
thiserror = "2"
log = "0.4"

[dev-dependencies]
tokio = { version = "1", features = ["sync", "rt", "macros"] }
```

**Step 2: Verify dependencies resolve**

```bash
cd chronicle-daemon && cargo check -p chronicle-capture
```

Expected: compiles successfully. If `screencapturekit` 1.5 has version
resolution issues, check crates.io for the latest compatible version.

**Step 3: Commit**

```bash
git add chronicle-daemon/crates/capture/Cargo.toml chronicle-daemon/Cargo.lock
git commit -m "build(capture): replace objc2 with screencapturekit-rs" -m "Safe Rust bindings for ScreenCaptureKit. Adds tokio (mpsc), thiserror, log." -m "Part of HEU-249"
```

Note: Cargo.lock may not be tracked (check .gitignore). If not tracked, only
commit Cargo.toml.

---

### Task 2: Error type

**Files:**
- Create: `chronicle-daemon/crates/capture/src/error.rs`
- Modify: `chronicle-daemon/crates/capture/src/lib.rs`

**Step 1: Create error.rs with tests**

```rust
use thiserror::Error;

/// Errors that can occur during screen capture.
#[derive(Debug, Error)]
pub enum CaptureError {
    /// ScreenCaptureKit returned an error.
    #[error("screen capture kit error: {0}")]
    ScreenCaptureKit(String),

    /// No displays were found during enumeration.
    #[error("no displays found")]
    NoDisplays,

    /// The frame channel was closed unexpectedly.
    #[error("channel send failed")]
    ChannelClosed,
}

/// Convenience alias for capture operations.
pub type Result<T> = std::result::Result<T, CaptureError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_displays_message() {
        let err = CaptureError::ScreenCaptureKit("permission denied".into());
        assert!(err.to_string().contains("permission denied"));
    }

    #[test]
    fn no_displays_error_displays() {
        let err = CaptureError::NoDisplays;
        assert_eq!(err.to_string(), "no displays found");
    }
}
```

**Step 2: Update lib.rs**

Replace contents with:

```rust
//! Screen capture engine for Chronicle.
//!
//! Uses ScreenCaptureKit to capture all connected displays with adaptive
//! frame rates based on screen activity and input events.

/// Error types for capture operations.
pub mod error;

pub use error::{CaptureError, Result};
```

**Step 3: Run tests**

```bash
cd chronicle-daemon && cargo test -p chronicle-capture
```

Expected: 2 tests pass.

**Step 4: Commit**

```bash
git add chronicle-daemon/crates/capture/src/error.rs chronicle-daemon/crates/capture/src/lib.rs
git commit -m "feat(capture): add CaptureError type" -m "Part of HEU-249"
```

---

### Task 3: Public types

**Files:**
- Modify: `chronicle-daemon/crates/capture/src/lib.rs`

**Step 1: Add CaptureConfig, CapturedFrame, and CaptureStatus to lib.rs**

```rust
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use screencapturekit::prelude::CMSampleBuffer;

/// Configuration for the capture engine.
#[derive(Debug, Clone)]
pub struct CaptureConfig {
    /// Minimum time between frames in seconds. Default: 2.0
    pub frame_interval_secs: f64,
    /// Backpressure buffer size for the frame channel. Default: 32
    pub channel_buffer_size: usize,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            frame_interval_secs: 2.0,
            channel_buffer_size: 32,
        }
    }
}

/// A captured frame delivered over the channel.
pub struct CapturedFrame {
    /// Raw sample buffer from ScreenCaptureKit.
    pub image_buffer: CMSampleBuffer,
    /// macOS display identifier (CGDirectDisplayID).
    pub display_id: u32,
    /// Unix timestamp in milliseconds.
    pub timestamp: i64,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Retina scale factor (1.0 or 2.0).
    pub scale_factor: f64,
}

/// Health snapshot of the capture engine.
#[derive(Debug, Clone)]
pub struct CaptureStatus {
    /// Number of displays currently being captured.
    pub active_displays: usize,
    /// Total frames sent to the channel.
    pub total_frames_captured: u64,
    /// Total frames dropped due to channel backpressure.
    pub total_frames_dropped: u64,
}
```

Add tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_config_defaults() {
        let config = CaptureConfig::default();
        assert!((config.frame_interval_secs - 2.0).abs() < f64::EPSILON);
        assert_eq!(config.channel_buffer_size, 32);
    }
}
```

Note: `CapturedFrame` cannot derive `Debug` or be constructed in tests because
`CMSampleBuffer` comes from SCK and can't be created without a real capture
stream. This is an accepted constraint.

The import path for `CMSampleBuffer` may differ — check the actual
`screencapturekit` crate re-exports. It might be `screencapturekit::cm::CMSampleBuffer`
or `screencapturekit::prelude::CMSampleBuffer`. Adapt the import accordingly.

**Step 2: Run tests**

```bash
cd chronicle-daemon && cargo test -p chronicle-capture
```

Expected: 3 tests pass (2 error + 1 config).

**Step 3: Commit**

```bash
git add chronicle-daemon/crates/capture/src/lib.rs
git commit -m "feat(capture): add CaptureConfig, CapturedFrame, and CaptureStatus types" -m "Part of HEU-249"
```

---

### Task 4: Frame handler

**Files:**
- Create: `chronicle-daemon/crates/capture/src/handler.rs`
- Modify: `chronicle-daemon/crates/capture/src/lib.rs`

**Step 1: Create handler.rs**

```rust
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use screencapturekit::prelude::*;
use tokio::sync::mpsc;
use crate::CapturedFrame;

pub(crate) struct FrameHandler {
    pub(crate) sender: mpsc::Sender<CapturedFrame>,
    pub(crate) display_id: u32,
    pub(crate) scale_factor: f64,
    pub(crate) frames_captured: Arc<AtomicU64>,
    pub(crate) frames_dropped: Arc<AtomicU64>,
}

impl SCStreamOutputTrait for FrameHandler {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, of_type: SCStreamOutputType) {
        if !matches!(of_type, SCStreamOutputType::Screen) {
            return;
        }

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;

        // Extract dimensions from the sample buffer's image buffer.
        // The exact API depends on the screencapturekit crate — check
        // CMSampleBuffer or its pixel_buffer() / image_buffer() methods
        // for width/height. If not directly available, use the display
        // dimensions stored at stream creation time.
        let (width, height) = extract_dimensions(&sample);

        let frame = CapturedFrame {
            image_buffer: sample,
            display_id: self.display_id,
            timestamp,
            width,
            height,
            scale_factor: self.scale_factor,
        };

        match self.sender.try_send(frame) {
            Ok(_) => {
                self.frames_captured.fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => {
                self.frames_dropped.fetch_add(1, Ordering::Relaxed);
                log::warn!(
                    "Frame dropped for display {} — consumer falling behind",
                    self.display_id
                );
            }
        }
    }
}

/// Extract frame dimensions from a CMSampleBuffer.
///
/// The exact method depends on the screencapturekit crate API. Common
/// approaches:
/// - `sample.image_buffer().map(|buf| (buf.width(), buf.height()))`
/// - `sample.format_description().dimensions()`
///
/// The implementer should check the actual API and adapt. If dimensions
/// cannot be extracted from the sample buffer, fall back to the stream
/// configuration dimensions (add width/height fields to FrameHandler).
fn extract_dimensions(sample: &CMSampleBuffer) -> (u32, u32) {
    // Attempt to get dimensions from the pixel buffer
    if let Some(pixel_buffer) = sample.image_buffer() {
        return (pixel_buffer.width() as u32, pixel_buffer.height() as u32);
    }
    // Fallback — this shouldn't happen for screen capture frames
    (0, 0)
}
```

Note: The `extract_dimensions` function uses the `image_buffer()` method from
the context7 docs. The exact API may differ. If `CMSampleBuffer` doesn't expose
`image_buffer()` directly, check `screencapturekit::cv` or the `CVPixelBuffer`
type. The implementer MUST verify this against the actual crate API.

**Step 2: Add module to lib.rs**

Add `pub(crate) mod handler;` to lib.rs.

**Step 3: Verify it compiles**

```bash
cd chronicle-daemon && cargo check -p chronicle-capture
```

Expected: compiles. The handler can't be unit tested without constructing a
`CMSampleBuffer`, which requires a real SCK stream. The channel and counter
behavior will be verified in the integration test (Task 7).

**Step 4: Commit**

```bash
git add chronicle-daemon/crates/capture/src/handler.rs chronicle-daemon/crates/capture/src/lib.rs
git commit -m "feat(capture): add FrameHandler bridging SCK callbacks to mpsc channel" -m "Implements SCStreamOutputTrait. Uses try_send to avoid blocking SCK thread. Counts captured and dropped frames." -m "Part of HEU-249"
```

---

### Task 5: Capture engine

**Files:**
- Create: `chronicle-daemon/crates/capture/src/engine.rs`
- Modify: `chronicle-daemon/crates/capture/src/lib.rs`

**Step 1: Create engine.rs**

```rust
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use screencapturekit::prelude::*;
use tokio::sync::mpsc;
use crate::error::{CaptureError, Result};
use crate::handler::FrameHandler;
use crate::{CaptureConfig, CapturedFrame, CaptureStatus};

/// Owns per-display SCStreams and the sender side of the frame channel.
pub struct CaptureEngine {
    streams: Vec<SCStream>,
    _sender: mpsc::Sender<CapturedFrame>,
    frames_captured: Arc<AtomicU64>,
    frames_dropped: Arc<AtomicU64>,
}

impl CaptureEngine {
    /// Enumerate displays, create one SCStream per display, start capturing.
    ///
    /// Returns the engine handle and a receiver for captured frames.
    /// The receiver yields `None` when the engine is dropped or all streams
    /// have stopped — the daemon should treat this as "capture engine gone."
    pub fn start(config: CaptureConfig) -> Result<(Self, mpsc::Receiver<CapturedFrame>)> {
        let (sender, receiver) = mpsc::channel(config.channel_buffer_size);

        let content = SCShareableContent::get().map_err(|e| {
            CaptureError::ScreenCaptureKit(format!("failed to get shareable content: {e}"))
        })?;
        let displays = content.displays();

        if displays.is_empty() {
            return Err(CaptureError::NoDisplays);
        }

        let frames_captured = Arc::new(AtomicU64::new(0));
        let frames_dropped = Arc::new(AtomicU64::new(0));

        let mut streams = Vec::new();
        for display in &displays {
            let filter = SCContentFilter::create()
                .with_display(display)
                .with_excluding_windows(&[])
                .build();

            // Configure the stream for this display.
            // The exact builder methods depend on the screencapturekit crate.
            // Check the crate docs for:
            //   - Setting frame interval (minimumFrameInterval or equivalent)
            //   - Setting pixel format (BGRA)
            //   - Setting dimensions (display width/height)
            let stream_config = SCStreamConfiguration::new()
                .with_width(display.width() as u32)
                .with_height(display.height() as u32)
                .with_pixel_format(PixelFormat::BGRA);
            // TODO: set minimum frame interval from config.frame_interval_secs
            // Check if .with_minimum_frame_interval() exists, or if the
            // interval needs to be set via a different API.

            let handler = FrameHandler {
                sender: sender.clone(),
                display_id: display.display_id(),
                scale_factor: display.scale_factor(),
                frames_captured: frames_captured.clone(),
                frames_dropped: frames_dropped.clone(),
            };

            let mut stream = SCStream::new(&filter, &stream_config);
            stream.add_output_handler(handler, SCStreamOutputType::Screen);
            stream.start_capture().map_err(|e| {
                CaptureError::ScreenCaptureKit(format!(
                    "failed to start capture on display {}: {e}",
                    display.display_id()
                ))
            })?;

            streams.push(stream);
        }

        log::info!("Capture started on {} display(s)", streams.len());

        Ok((
            Self {
                streams,
                _sender: sender,
                frames_captured,
                frames_dropped,
            },
            receiver,
        ))
    }

    /// Stop all streams and clean up.
    pub fn stop(&mut self) -> Result<()> {
        for stream in &mut self.streams {
            stream.stop_capture().map_err(|e| {
                CaptureError::ScreenCaptureKit(format!("failed to stop capture: {e}"))
            })?;
        }
        self.streams.clear();
        log::info!("Capture stopped");
        Ok(())
    }

    /// Return current capture health metrics.
    pub fn status(&self) -> CaptureStatus {
        CaptureStatus {
            active_displays: self.streams.len(),
            total_frames_captured: self.frames_captured.load(Ordering::Relaxed),
            total_frames_dropped: self.frames_dropped.load(Ordering::Relaxed),
        }
    }
}
```

Note: Several `screencapturekit` API details need verification during
implementation:
- `display.display_id()` — might be `.id()` or `.display_id`
- `display.scale_factor()` — might need to come from a different source
- `display.width()` / `display.height()` — check exact method names
- Frame interval setting — may require a CMTime value or a different API
- Error types from SCK — adapt the `map_err` calls to the actual error type

The implementer should run `cargo doc -p screencapturekit --open` to browse
the actual API and adapt the code accordingly.

**Step 2: Add module to lib.rs and re-export CaptureEngine**

Add `pub mod engine;` to lib.rs and `pub use engine::CaptureEngine;`.

**Step 3: Verify it compiles**

```bash
cd chronicle-daemon && cargo check -p chronicle-capture
```

Expected: compiles. If SCK API methods differ from the design, adapt the code.

**Step 4: Commit**

```bash
git add chronicle-daemon/crates/capture/src/engine.rs chronicle-daemon/crates/capture/src/lib.rs
git commit -m "feat(capture): add CaptureEngine with per-display SCStream setup" -m "Enumerates displays, creates one SCStream per display, delivers frames over mpsc channel. Includes start/stop/status." -m "Part of HEU-249"
```

---

### Task 6: Integration test

**Files:**
- Create: `chronicle-daemon/crates/capture/tests/integration.rs`

This test requires Screen Recording permission and real displays. It is marked
`#[ignore]` so it doesn't run in CI or automated test suites.

**Step 1: Create integration test**

```rust
//! Integration tests for chronicle-capture.
//!
//! These tests require:
//! - A real macOS display (no headless/CI)
//! - Screen Recording permission granted to the test runner
//! - macOS 12.3+
//!
//! Run manually: cargo test -p chronicle-capture --test integration -- --ignored

use chronicle_capture::{CaptureConfig, CaptureEngine};

#[ignore]
#[tokio::test]
async fn capture_engine_delivers_frames() {
    // Start with a short interval so we get frames quickly
    let config = CaptureConfig {
        frame_interval_secs: 0.5,
        channel_buffer_size: 16,
    };

    let (mut engine, mut receiver) = CaptureEngine::start(config)
        .expect("Failed to start capture — is Screen Recording permission granted?");

    // Wait for at least one frame
    let frame = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        receiver.recv(),
    )
    .await
    .expect("Timed out waiting for frame")
    .expect("Channel closed before receiving a frame");

    // Verify frame has reasonable values
    assert!(frame.display_id > 0, "display_id should be non-zero");
    assert!(frame.width > 0, "width should be non-zero");
    assert!(frame.height > 0, "height should be non-zero");
    assert!(frame.timestamp > 0, "timestamp should be non-zero");
    assert!(frame.scale_factor >= 1.0, "scale_factor should be >= 1.0");

    // Check status
    let status = engine.status();
    assert!(status.active_displays > 0);
    assert!(status.total_frames_captured >= 1);

    // Stop and verify channel closes
    engine.stop().expect("Failed to stop capture");

    // Drain any remaining frames
    while receiver.try_recv().is_ok() {}

    // After stop + drain, engine dropped => channel should close eventually
    // (though there may be a brief delay)
}

#[ignore]
#[tokio::test]
async fn capture_engine_finds_displays() {
    let config = CaptureConfig::default();
    let (mut engine, _receiver) = CaptureEngine::start(config)
        .expect("Failed to start — no displays or no permission");

    let status = engine.status();
    assert!(
        status.active_displays >= 1,
        "Should find at least one display"
    );

    engine.stop().expect("Failed to stop");
}
```

**Step 2: Run the integration test (manual, on your machine)**

```bash
cd chronicle-daemon && cargo test -p chronicle-capture --test integration -- --ignored
```

Expected: tests pass if Screen Recording permission is granted. If not granted,
macOS will prompt for permission. Grant it and re-run.

**Step 3: Commit**

```bash
git add chronicle-daemon/crates/capture/tests/integration.rs
git commit -m "test(capture): add integration tests for CaptureEngine" -m "Requires Screen Recording permission. Run with --ignored flag." -m "Part of HEU-249"
```

---

### Task 7: Verify clean build

**Step 1: Run full test suite (unit tests only)**

```bash
cd chronicle-daemon && cargo test -p chronicle-capture
```

Expected: all unit tests pass (error + config tests). Integration tests are
skipped (they require `--ignored`).

**Step 2: Run clippy**

```bash
cd chronicle-daemon && cargo clippy -p chronicle-capture -- -D warnings
```

Expected: no warnings. Fix any that appear.

**Step 3: Run doc check**

```bash
cd chronicle-daemon && RUSTDOCFLAGS="-D missing_docs" cargo doc -p chronicle-capture --no-deps
```

Expected: no missing docs warnings. All public items should have doc comments
from the implementation.

**Step 4: Commit fixes if any**

```bash
git add chronicle-daemon/crates/capture/
git commit -m "chore(capture): fix clippy and doc warnings" -m "Part of HEU-249"
```

Only commit if there were actual fixes.

---

### Task 8: Write developer guide

**Files:**
- Modify: `docs/guides/index.md`
- Create: `docs/guides/screen-capture.md`

**Step 1: Update index.md**

Add a row to the Components table:

```markdown
| [Screen Capture](screen-capture.md) | `chronicle-capture` | Display enumeration, SCStream setup, frame delivery |
```

**Step 2: Create screen-capture.md**

Cover:
- What the capture engine does (high level)
- How frames flow: SCK thread → FrameHandler → mpsc channel → daemon
- The module structure (lib.rs, engine.rs, handler.rs, error.rs)
- How to run integration tests (Screen Recording permission, --ignored flag)
- How to add new frame metadata or change the capture interval
- The channel backpressure model (try_send, frame dropping)

Target: a developer seeing the capture crate for the first time.

**Step 3: Commit**

```bash
git add docs/guides/
git commit -m "docs: add developer guide for screen capture engine" -m "Part of HEU-249"
```
