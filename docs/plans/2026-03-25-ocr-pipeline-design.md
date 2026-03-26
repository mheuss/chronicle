# OCR Pipeline - Design Document

**Date:** 2026-03-25
**Status:** Approved
**Issue:** HEU-238
**Branch:** mrheuss/heu-238-ocr-pipeline-on-device-text-extraction-via-vision-framework
**Base Branch:** main

---

## Overview

Add on-device text extraction to the `chronicle-ocr` crate using Apple's Vision
framework. The crate exposes a single pure function that takes an image file
path, runs `VNRecognizeTextRequest` with accurate recognition, and returns the
extracted text. No state, no scheduling, no storage coupling. The daemon
orchestrator calls this function asynchronously after storing each screenshot,
then feeds the result to `storage.update_ocr_text()`.

This follows ADR-005 (async processing behind capture) — OCR never blocks
the capture pipeline.

---

## 1. Public API

The crate exposes one function and one error type. No structs, no state.

### 1.1 Function Signature

**`extract_text`** (`chronicle-daemon/crates/ocr/src/lib.rs`):

```rust
/// Extract text from an image file using Apple Vision framework.
///
/// Supports any ImageIO-compatible format (HEIF, PNG, JPEG, etc.).
/// Uses VNRecognizeTextRequest with accurate recognition level.
///
/// Returns the concatenated recognized text, or an empty string if
/// no text is found.
pub fn extract_text(image_path: &Path) -> Result<String>
```

### 1.2 Error Type

**`OcrError`** (`chronicle-daemon/crates/ocr/src/lib.rs`):

```rust
#[derive(Debug, thiserror::Error)]
pub enum OcrError {
    /// The image file does not exist at the given path.
    #[error("image not found: {0}")]
    ImageNotFound(String),

    /// The Vision framework request failed.
    #[error("vision request failed: {0}")]
    VisionError(String),
}

pub type Result<T> = std::result::Result<T, OcrError>;
```

---

## 2. Implementation Flow

All Vision framework calls happen inside `extract_text()`, wrapped in an
`objc2::rc::autoreleasepool()` block. The function is synchronous and blocks
until OCR completes.

### 2.1 Steps

1. **Validate** the file exists at `image_path`. Return `OcrError::ImageNotFound`
   if not.
2. **Create `NSURL`** from the file path using
   `NSURL::fileURLWithPath(NSString::from_str(path))`.
3. **Create `VNImageRequestHandler`** via `initWithURL_options(url, None)`.
   Vision loads the image file internally — no manual format conversion needed.
   Supports HEIF, PNG, JPEG, and anything else ImageIO handles.
4. **Create `VNRecognizeTextRequest`** via `VNRecognizeTextRequest::new()`.
   Configure:
   - Recognition level: `VNRequestTextRecognitionLevelAccurate`
   - Language correction: enabled (default)
   - Languages: default (auto-detect, English primary)
5. **Call `performRequests_error()`** with the text request. Map any
   `NSError` to `OcrError::VisionError`.
6. **Extract results**: iterate the `VNRecognizedTextObservation` array from
   `request.results()`. For each observation, call `topCandidates(1)` and
   take the first candidate's `.string` value.
7. **Join** all extracted strings with `"\n"` and return. If no observations
   are found, return an empty `String`.

### 2.2 Text Ordering

Vision returns observations in no guaranteed order. For v1, we concatenate all
top candidates separated by newlines. FTS5 search doesn't care about ordering —
it just needs the words present.

Position-based sorting (top-to-bottom, left-to-right using observation bounding
boxes) can be added in a future iteration if the UI needs readable text display.

### 2.3 Threading

`extract_text()` runs on the calling thread. Since ADR-005 says OCR runs in a
background queue, the daemon orchestrator is responsible for calling this off the
capture thread. The OCR crate has no opinion about threading.

---

## 3. Dependencies

### 3.1 Cargo.toml

```toml
[package]
name = "chronicle-ocr"
version = "0.1.0"
edition = "2024"
description = "OCR pipeline — text extraction from screenshots via Apple Vision framework"
license = "MIT"

[dependencies]
objc2 = "0.6"
objc2-vision = { version = "0.3", features = [
    "VNRecognizeTextRequest",
    "VNRequest",
    "VNRequestHandler",
    "VNObservation",
] }
objc2-foundation = { version = "0.3", features = [
    "NSURL",
    "NSArray",
    "NSString",
    "NSDictionary",
    "NSError",
] }
thiserror = "2"
```

No dependency on `chronicle-capture` or `chronicle-storage`. No
`objc2-core-image` needed — `VNImageRequestHandler::initWithURL_options()`
handles image loading directly.

### 3.2 Version Compatibility

All `objc2-*` crates resolve to 0.3.2 in the workspace lockfile. `objc2` is at
0.6.4. No conflicts with `screencapturekit` (which has zero objc2 dependencies).

---

## 4. Testing Strategy

Vision framework requires a real macOS environment. No mocking.

### 4.1 Test Fixture

A small PNG file with known text, committed at
`chronicle-daemon/crates/ocr/tests/fixtures/sample-text.png`. This is a ~5KB
file with clear rendered text. PNG is used instead of HEIF because it's simpler
to create and Vision handles both identically via ImageIO.

### 4.2 Unit Tests

All tests run without special permissions (no Screen Recording needed).

| Test | What it verifies |
|------|-----------------|
| `extract_text_from_image_with_known_text` | Extracts text from fixture PNG, verifies expected words are present |
| `extract_text_returns_empty_for_blank_image` | Solid-color image with no text returns empty string, not an error |
| `extract_text_errors_on_missing_file` | Nonexistent path returns `OcrError::ImageNotFound` |
| `extract_text_never_panics` | Repeated calls with valid input don't panic from ObjC interop |

### 4.3 No Integration Test

The OCR crate accepts any ImageIO-supported format. There's no HEIF-specific
logic to test. HEIF decoding is ImageIO's responsibility, not ours. A plain PNG
fixture is a clean, portable test input.

---

## Architectural Decisions

| # | Decision | Rationale |
|---|---|---|
| 1 | Pure function, no state | Keeps the crate focused and testable. Scheduling is the daemon's job. |
| 2 | `VNImageRequestHandler::initWithURL_options()` | Vision loads the file directly — no manual image conversion, supports all ImageIO formats. |
| 3 | Accurate recognition level | OCR runs async behind capture (ADR-005), so accuracy is more valuable than speed. |
| 4 | PNG test fixture over programmatic generation | Deterministic, simple, no extra FFI just for tests. |
| 5 | No HEIF-specific logic | Vision/ImageIO handles format decoding transparently. The crate accepts any image path. |
| 6 | No developer guide | Single pure function — doc comments are sufficient. Pipeline integration docs belong in a future daemon orchestrator guide. |

## Out of Scope

- **Async scheduling / background queue** — daemon orchestrator handles when to
  call `extract_text()`. Addressed by HEU-252 (storage integration).
- **Storage integration** — no dependency on `chronicle-storage`. The daemon
  calls `extract_text()` then calls `update_ocr_text()` itself.
- **Position-based text ordering** — v1 concatenates with newlines. Reading-order
  sorting is a future enhancement.
- **Language configuration** — defaults to auto-detect. Configurable language
  lists can be added later.
- **Re-processing / backlog management** — the daemon decides which screenshots
  need OCR, not this crate.
- **Confidence thresholds** — top candidate is taken regardless of confidence.
  Low-confidence filtering is a future tuning exercise.
