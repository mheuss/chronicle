# Domain: capture

**Status:** Decided 2026-09-16, revised 2026-09-18
**Owns:** Turning connected displays into encoded frames, and deciding when that should be happening.
**Code:** `chronicle-daemon/crates/capture/src/`, plus `src/capture_runtime.rs` and `src/capture_supervisor.rs`

## Boundary

- inside: the ScreenCaptureKit stream and its frame callback — `chronicle-daemon/crates/capture/src/engine.rs` — `CaptureEngine`
- inside: encoding a captured frame to HEIF — `chronicle-daemon/crates/capture/src/encoder.rs` — `encode_heif`
- inside: the foreground-app metadata attached to a frame — `chronicle-daemon/crates/capture/src/metadata.rs` — `get_frontmost_app`
- inside: binding an engine's lifetime to its consuming tasks — `chronicle-daemon/src/capture_runtime.rs` — `CaptureRuntime`
- inside: deciding whether capture should be running, and retrying a failed start — `chronicle-daemon/src/capture_supervisor.rs` — `CaptureSupervisor`
- outside: what happens to a frame after it leaves the channel — `chronicle-daemon/src/pipeline/sinks.rs` — `AppMetadata`
- outside: reporting engine state over the wire — `chronicle-daemon/src/ipc_handler.rs` — `EngineState`
- outside: the audio pipeline the supervisor reconciles alongside capture — `chronicle-daemon/crates/audio/src/lib.rs` — `AudioPipeline`

**Placement test:** Does the file decide whether a display is being captured, or turn a captured buffer into a frame? If it only reads a frame that already exists, it belongs to whoever consumes it.
**Document comparison:** differs — no existing document draws this boundary. `docs/use-cases/INDEX.md` assigns all of `chronicle-daemon/src/` to `pipeline`, which would place two of these files elsewhere.

## Owned Files

- chronicle-daemon/crates/capture/src/deps.rs
- chronicle-daemon/crates/capture/src/drops.rs
- chronicle-daemon/crates/capture/src/encoder.rs
- chronicle-daemon/crates/capture/src/engine.rs
- chronicle-daemon/crates/capture/src/error.rs
- chronicle-daemon/crates/capture/src/handler.rs
- chronicle-daemon/crates/capture/src/lib.rs
- chronicle-daemon/crates/capture/src/metadata.rs
- chronicle-daemon/crates/capture/src/pixel_buffer.rs
- chronicle-daemon/src/capture_runtime.rs
- chronicle-daemon/src/capture_supervisor.rs

## Depends On

- audio — `chronicle-daemon/crates/capture/src/lib.rs` — `AudioHandlerToken`
- audio — `chronicle-daemon/src/capture_supervisor.rs` — `AudioPipeline`
- pipeline — `chronicle-daemon/src/capture_runtime.rs` — `OcrSink`
- pipeline — `chronicle-daemon/src/capture_supervisor.rs` — `PipelineCounters`
- storage — `chronicle-daemon/src/capture_runtime.rs` — `Storage`
- permissions — `chronicle-daemon/src/capture_supervisor.rs` — `check_microphone`
- IPC — `chronicle-daemon/src/capture_supervisor.rs` — `map_outcome`

## Depended On By

- pipeline — `chronicle-daemon/src/pipeline/sinks.rs` — `AppMetadata`
- pipeline — `chronicle-daemon/src/pipeline.rs` — `encode_heif`
- IPC — `chronicle-daemon/src/ipc_handler.rs` — `EngineState`

## Seams

- SEAM-capture-audio-lifecycle — audio — the supervisor reconciles both pipelines against one desired state and owns microphone enable and disable — owner: capture — `chronicle-daemon/src/capture_supervisor.rs` — `CaptureSupervisor`
- SEAM-stage-enqueue — pipeline — this domain holds an `Arc<dyn OcrSink>` and enqueues through a bounded channel whose full case is a typed result rather than a block or a panic; pipeline decides the trait and the outcome vocabulary, this domain decides what it sends — owner: pipeline — `chronicle-daemon/src/capture_runtime.rs` — `OcrSink`

## How The Existing Documents Saw This

| Document | Said | Verdict |
|---|---|---|
| `docs/guides/screen-capture.md` | "The `chronicle-capture` crate captures screenshots from every connected display using Apple's ScreenCaptureKit framework. It delivers frames over a bounded Tokio channel" | accurate, but scoped to the crate only — silent on the two daemon modules |
| `docs/project-description.md` | Lists "Screen Capture Engine" as component 1, "Continuous screen capture using `ScreenCaptureKit`" | accurate |
| `docs/use-cases/INDEX.md` | Assigns `chronicle-daemon/src/` to `pipeline`, describing "capture-runtime drop order" as a pipeline concern | wrong — filed as CHR-151 |
| `docs/use-cases/pipeline.md` | Documents "CaptureRuntime Lifecycle — Drop Engine Before Joining Tasks" as a pipeline pattern | stale — the lifecycle rule is real, but the file it governs belongs here |
| `docs/decisions/008-raw-objc2-for-screencapturekit.md` | "Use raw `objc2-screen-capture-kit` bindings for all ScreenCaptureKit interaction across the project" | accurate |
| `docs/decisions/009-typed-lifecycle-for-capture-and-audio.md` | A borrow-based `AudioHandlerToken` ties the audio handler's lifetime to the pipeline, and `CaptureEngine` gets a state machine | accurate — this decision is why the seam below exists |

## Offshoots Filed

- CHR-151 — the use-case index gives `pipeline` a code location spanning two domains
