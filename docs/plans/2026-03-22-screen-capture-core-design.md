# Core Frame Capture — Design Document

**Date:** 2026-03-22
**Status:** Complete
**Issue:** HEU-249
**Branch:** mrheuss/heu-237-screen-capture-engine-multi-display-adaptive-capture-via
**Base Branch:** main

---

## Overview

Implement the core screen capture loop for the `chronicle-capture` crate.
Enumerate all connected displays via ScreenCaptureKit, create one SCStream per
display, and deliver frames over an mpsc channel at a fixed 2-second interval.
Uses the `screencapturekit` crate (v1.5) for safe Rust bindings instead of
manual objc2 FFI.

This is the foundation that all other capture sub-issues (HEIF encoding,
metadata extraction, storage integration, perceptual hashing, adaptive rate,
display hotplug) build on.

---

## 1. Dependencies

Replace the current capture crate dependencies.

### Before

```toml
[dependencies]
objc2 = "0.6"
objc2-screen-capture-kit = "0.3"
core-graphics = "0.24"
core-foundation = "0.10"
```

### After

```toml
[dependencies]
screencapturekit = { version = "1.5", features = ["macos_14_0"] }
tokio = { version = "1", features = ["sync", "rt"] }
thiserror = "2"
log = "0.4"
```

- `screencapturekit` provides safe Rust bindings for Apple's ScreenCaptureKit.
  The `macos_14_0` feature enables newer APIs while the base crate supports
  macOS 12.3+.
- `tokio` for the `mpsc` channel that bridges SCK threads to async Rust.
- `thiserror` for error types (consistent with `chronicle-storage`).
- `log` for structured logging during capture.

---

## 2. Public API

### Types

```rust
/// Configuration for the capture engine.
pub struct CaptureConfig {
    /// Minimum time between frames in seconds. Default: 2.0
    pub frame_interval_secs: f64,
    /// Backpressure buffer size for the frame channel. Default: 32
    pub channel_buffer_size: usize,
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
pub struct CaptureStatus {
    /// Number of displays currently being captured.
    pub active_displays: usize,
    /// Total frames sent to the channel.
    pub total_frames_captured: u64,
    /// Total frames dropped due to channel backpressure.
    pub total_frames_dropped: u64,
}
```

### CaptureEngine

```rust
pub struct CaptureEngine { /* private */ }

impl CaptureEngine {
    /// Enumerate displays, create one SCStream per display, start capturing.
    /// Returns the engine handle and a receiver for captured frames.
    pub fn start(config: CaptureConfig) -> Result<(Self, mpsc::Receiver<CapturedFrame>)>;

    /// Stop all streams and clean up.
    pub fn stop(&mut self) -> Result<()>;

    /// Return current capture health metrics.
    pub fn status(&self) -> CaptureStatus;
}
```

### Notes

- **`CMSampleBuffer` not `CGImage`** — downstream consumers (HEIF encoding,
  perceptual hashing) choose how to extract pixel data. Conversion is cheap.
- **`start` is not async** — SCK operates on its own threads. The mpsc channel
  bridges into the async world.
- **`channel_buffer_size: 32`** — at 2-second intervals across 2-3 displays,
  this is ~20 seconds of buffer before frames drop.
- **Channel closure signals engine death.** When the daemon's frame loop
  receives `None`, the capture engine is gone. The daemon should log, notify
  the UI via IPC, and attempt recovery.

---

## 3. Internal Architecture

### Frame Handler

Bridges SCK callbacks to the mpsc channel:

```rust
struct FrameHandler {
    sender: mpsc::Sender<CapturedFrame>,
    display_id: u32,
    scale_factor: f64,
    frames_captured: Arc<AtomicU64>,
    frames_dropped: Arc<AtomicU64>,
}

impl SCStreamOutputTrait for FrameHandler {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, of_type: SCStreamOutputType) {
        if !matches!(of_type, SCStreamOutputType::Screen) { return; }

        let frame = CapturedFrame {
            width: /* from sample buffer */,
            height: /* from sample buffer */,
            timestamp: /* current time millis */,
            display_id: self.display_id,
            scale_factor: self.scale_factor,
            image_buffer: sample,
        };

        match self.sender.try_send(frame) {
            Ok(_) => { self.frames_captured.fetch_add(1, Ordering::Relaxed); }
            Err(_) => {
                self.frames_dropped.fetch_add(1, Ordering::Relaxed);
                log::warn!("Frame dropped for display {} — consumer falling behind", self.display_id);
            }
        }
    }
}
```

**`try_send` not `send`** — the handler runs on SCK's internal thread. Blocking
it would stall frame delivery for all streams. Dropped frames are logged and
counted in `CaptureStatus`.

### Display Enumeration and Stream Setup

```rust
impl CaptureEngine {
    pub fn start(config: CaptureConfig) -> Result<(Self, mpsc::Receiver<CapturedFrame>)> {
        let (sender, receiver) = mpsc::channel(config.channel_buffer_size);

        let content = SCShareableContent::get()?;
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

            let stream_config = SCStreamConfiguration::new()
                .with_width(display.width() as u32)
                .with_height(display.height() as u32)
                .with_minimum_frame_interval(/* CMTime for config.frame_interval_secs */)
                .with_pixel_format(PixelFormat::BGRA);

            let handler = FrameHandler {
                sender: sender.clone(),
                display_id: display.display_id(),
                scale_factor: display.scale_factor(),
                frames_captured: frames_captured.clone(),
                frames_dropped: frames_dropped.clone(),
            };

            let mut stream = SCStream::new(&filter, &stream_config);
            stream.add_output_handler(handler, SCStreamOutputType::Screen);
            stream.start_capture()?;

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
}
```

### Stop and Status

```rust
impl CaptureEngine {
    pub fn stop(&mut self) -> Result<()> {
        for stream in &mut self.streams {
            stream.stop_capture()?;
        }
        self.streams.clear();
        log::info!("Capture stopped");
        Ok(())
    }

    pub fn status(&self) -> CaptureStatus {
        CaptureStatus {
            active_displays: self.streams.len(),
            total_frames_captured: self.frames_captured.load(Ordering::Relaxed),
            total_frames_dropped: self.frames_dropped.load(Ordering::Relaxed),
        }
    }
}
```

### Why the engine holds `_sender`

The `CaptureEngine` keeps a clone of the sender so the channel stays open even
if individual `FrameHandler` instances are dropped during stream
reconfiguration. When the engine is dropped, all senders drop, and the receiver
returns `None` — clean shutdown signal to the daemon.

---

## 4. Module Structure

```text
chronicle-daemon/crates/capture/src/
├── lib.rs          -- pub types, CaptureEngine, re-exports
├── engine.rs       -- CaptureEngine::start, stop, status
├── handler.rs      -- FrameHandler impl of SCStreamOutputTrait
└── error.rs        -- CaptureError enum
```

### Error Type

```rust
#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("screen capture kit error: {0}")]
    ScreenCaptureKit(String),

    #[error("no displays found")]
    NoDisplays,

    #[error("channel send failed")]
    ChannelClosed,
}
```

---

## 5. Testing Strategy

ScreenCaptureKit requires a real macOS display and Screen Recording permission.
Unit tests can't exercise SCK directly.

### Testable without SCK

- `CaptureConfig` defaults — pure Rust
- `CaptureStatus` accounting — atomic counters
- `FrameHandler` channel behavior — if a dummy `CMSampleBuffer` can be
  constructed, test that `try_send` works and dropped frames increment the
  counter
- `CaptureError` conversions — pure Rust

### Integration tests (require real hardware)

- Start engine, wait for frames, verify they arrive with correct display_id
  and reasonable dimensions
- Stop engine, verify channel closes
- Verify `status()` reflects captured/dropped counts

Integration tests use `#[ignore]` and are run manually on developer machines.

```text
src/lib.rs              -- unit tests for config, status, error
src/handler.rs          -- unit tests for channel behavior
tests/integration.rs    -- #[ignore] tests requiring Screen Recording permission
```

---

## Architectural Decisions

| # | Decision | Rationale |
|---|---|---|
| 1 | screencapturekit crate instead of raw objc2 | Safe Rust API, trait-based handler, eliminates hundreds of lines of unsafe FFI. Well-maintained (v1.5.4, benchmark 90.25). |
| 2 | One SCStream per display | Per ADR-002. Independent frame delivery, per-display timing, clean separation. |
| 3 | mpsc channel for frame delivery | Decouples SCK threads from async processing. Natural backpressure via try_send. |
| 4 | try_send with frame dropping | Never block SCK's thread. Dropped frames are logged and counted, not fatal. |
| 5 | CMSampleBuffer in CapturedFrame | Let consumers choose extraction method (CGImage, pixel buffer, IOSurface). Avoids premature conversion. |
| 6 | Channel closure as death signal | No separate error channel. Receiver returning None = engine gone. Daemon handles recovery. |

## Out of Scope

- **HEIF encoding** — HEU-250
- **App metadata extraction** — HEU-251
- **Storage integration** — HEU-252
- **Perceptual hashing** — HEU-253
- **Adaptive capture rate** — HEU-254
- **Display hotplug** — HEU-255
