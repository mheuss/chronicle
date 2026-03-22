# Screen Capture Engine

The `chronicle-capture` crate captures screenshots from every connected display
using Apple's ScreenCaptureKit framework. It delivers frames over a bounded
Tokio channel so the rest of the daemon can process them without worrying about
SCK threading details.

## How frames flow

```
SCK callback thread          FrameHandler           mpsc channel           daemon
     |                            |                      |                   |
     |-- did_output_sample_buffer |                      |                   |
     |                            |-- try_send(frame) -->|                   |
     |                            |                      |-- recv() -------->|
```

1. ScreenCaptureKit fires `did_output_sample_buffer` on its own thread,
   once per frame interval per display.
2. `FrameHandler` wraps the raw `CMSampleBuffer` into a `CapturedFrame`
   with metadata (display ID, dimensions, scale factor, timestamp).
3. The handler calls `try_send` on a bounded `mpsc::Sender`. This never
   blocks the SCK thread. If the channel is full, the frame is dropped and
   the drop counter increments.
4. The daemon reads frames from the `mpsc::Receiver` at its own pace.

## Module structure

```
crates/capture/src/
  lib.rs        Public types: CaptureConfig, CapturedFrame, CaptureStatus.
                Module declarations and re-exports.
  engine.rs     CaptureEngine: display enumeration, SCStream setup, start/stop/status.
  handler.rs    FrameHandler: SCStreamOutputTrait impl, try_send, atomic counters.
  error.rs      CaptureError enum (ScreenCaptureKit, NoDisplays, ChannelClosed).
```

- **`lib.rs`** defines the three public data types and re-exports
  `CaptureEngine`, `CaptureError`, and `Result`. The `handler` module is
  `pub(crate)` so it stays internal.
- **`engine.rs`** does the heavy lifting. `CaptureEngine::start` calls
  `SCShareableContent::get()` to enumerate displays, builds one `SCStream`
  per display with a `FrameHandler`, and starts capture. `stop()` tears them
  all down. `status()` reads the shared atomic counters.
- **`handler.rs`** implements the `SCStreamOutputTrait` callback. It extracts
  actual pixel dimensions from the sample buffer when available and falls
  back to the configured dimensions otherwise.
- **`error.rs`** uses `thiserror` to define three error variants. The `Result`
  type alias keeps call sites clean.

## Running integration tests

The integration tests are `#[ignore]`'d by default because they need a real
display and Screen Recording permission.

```bash
# Grant Screen Recording permission to your terminal first.
# System Settings > Privacy & Security > Screen Recording

cd chronicle-daemon
DYLD_LIBRARY_PATH="/Library/Developer/CommandLineTools/usr/lib/swift-5.5/macosx" \
  cargo test -p chronicle-capture --test integration -- --ignored
```

The `DYLD_LIBRARY_PATH` is needed because the `screencapturekit` crate links
against Swift concurrency libraries that aren't on the default search path.

Regular unit tests (no permission needed) run without the `--ignored` flag:

```bash
cd chronicle-daemon
DYLD_LIBRARY_PATH="/Library/Developer/CommandLineTools/usr/lib/swift-5.5/macosx" \
  cargo test -p chronicle-capture
```

## Channel backpressure

The frame channel is bounded. The default buffer size is 32 frames (set in
`CaptureConfig::default()`). When the consumer can't keep up:

1. `FrameHandler::try_send` returns `TrySendError::Full`.
2. The frame is silently discarded. No retry, no blocking.
3. The `frames_dropped` atomic counter increments.
4. A `log::warn!` message fires.

You can check drop rates at any time with `engine.status()`, which returns a
`CaptureStatus` snapshot containing `total_frames_captured` and
`total_frames_dropped`.

This design keeps the SCK callback thread non-blocking. If you see high drop
rates, increase `channel_buffer_size` or make the consumer faster.

## Changing the capture interval

Set `CaptureConfig::frame_interval_secs` before calling `CaptureEngine::start`.
The value is converted to a `CMTime` with millisecond precision (timescale =
1000) via the `seconds_to_cmtime` helper in `engine.rs`.

```rust
let config = CaptureConfig {
    frame_interval_secs: 0.5,  // 2 fps
    channel_buffer_size: 16,
};
let (engine, receiver) = CaptureEngine::start(config)?;
```

The default is 2.0 seconds. Lower values mean more frames and higher CPU usage.

## Adding new frame metadata

To add a new field to captured frames:

1. Add the field to the `CapturedFrame` struct in `lib.rs`. Give it a doc
   comment.
2. Populate it in `FrameHandler::did_output_sample_buffer` in `handler.rs`.
   You can extract data from the `CMSampleBuffer`, or compute it from existing
   fields.
3. If the value needs to come from display enumeration (like `scale_factor`),
   pass it through `FrameHandler::new` in `engine.rs`.
4. Run `cargo doc` with `-D missing_docs` to make sure you documented the field.
