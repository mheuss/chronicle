# OCR Pipeline Implementation Plan

**Date:** 2026-03-25
**Status:** Approved
**Original Design Doc:** docs/plans/2026-03-25-ocr-pipeline-design.md
**Issue:** HEU-238
**Branch:** mrheuss/heu-238-ocr-pipeline-on-device-text-extraction-via-vision-framework
**Base Branch:** main

---

> **For Claude:** REQUIRED SUB-SKILL: Use sop:subagent-driven-development or sop:executing-plans to implement this plan task-by-task.

**Goal:** Add on-device text extraction to the `chronicle-ocr` crate using Apple Vision framework.

**Architecture:** A single pure function `extract_text(path) -> Result<String>` in the existing `chronicle-ocr` crate. Uses `objc2-vision` bindings to call `VNRecognizeTextRequest` with accurate recognition via `VNImageRequestHandler::initWithURL_options()`. No state, no scheduling, no storage coupling.

**Tech Stack:** Rust, `objc2` 0.6, `objc2-vision` 0.3 (VNRecognizeTextRequest, VNImageRequestHandler), `objc2-foundation` 0.3 (NSURL, NSArray, NSString), `thiserror` 2

---

### Task 1: Update Dependencies

**Files:**
- Modify: `chronicle-daemon/crates/ocr/Cargo.toml:1-10`

**Step 1: Update Cargo.toml**

Replace the entire contents of `chronicle-daemon/crates/ocr/Cargo.toml` with:

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

Changes from the scaffolded version:
- Added feature flags on `objc2-vision` for the specific types we need
- Added `objc2-foundation` as a direct dependency (need to import NSURL, NSString, etc.)
- Added `thiserror` for error type derive

**Step 2: Verify dependencies resolve**

```bash
cd chronicle-daemon && cargo check -p chronicle-ocr
```

Expected: compiles with no errors.

**Step 3: Commit**

```bash
git add chronicle-daemon/crates/ocr/Cargo.toml
git commit -m "build(ocr): add Vision framework and foundation deps" \
  -m "Part of HEU-238"
```

---

### Task 2: Add OcrError Type and extract_text Stub

**Files:**
- Modify: `chronicle-daemon/crates/ocr/src/lib.rs:1-5`

**Step 1: Write the error type and function stub**

Replace the entire contents of `chronicle-daemon/crates/ocr/src/lib.rs` with:

```rust
//! OCR pipeline for Chronicle.
//!
//! Runs Apple Vision text recognition on captured screenshots and
//! feeds extracted text into the storage engine for FTS5 indexing.

use std::path::Path;

/// Errors from OCR text extraction.
#[derive(Debug, thiserror::Error)]
pub enum OcrError {
    /// The image file does not exist at the given path.
    #[error("image not found: {0}")]
    ImageNotFound(String),

    /// The Vision framework request failed.
    #[error("vision request failed: {0}")]
    VisionError(String),
}

/// Result alias for OCR operations.
pub type Result<T> = std::result::Result<T, OcrError>;

/// Extract text from an image file using Apple Vision framework.
///
/// Supports any ImageIO-compatible format (HEIF, PNG, JPEG, etc.).
/// Uses VNRecognizeTextRequest with accurate recognition level.
///
/// Returns the concatenated recognized text, or an empty string if
/// no text is found.
pub fn extract_text(image_path: &Path) -> Result<String> {
    if !image_path.exists() {
        return Err(OcrError::ImageNotFound(
            image_path.display().to_string(),
        ));
    }

    todo!("Vision framework integration — Task 5")
}
```

**Step 2: Verify it compiles**

```bash
cd chronicle-daemon && cargo check -p chronicle-ocr
```

Expected: compiles. Warning about `todo!()` is expected.

**Step 3: Commit**

```bash
git add chronicle-daemon/crates/ocr/src/lib.rs
git commit -m "feat(ocr): add OcrError type and extract_text stub" \
  -m "Part of HEU-238"
```

---

### Task 3: Create Test Fixtures

**Files:**
- Create: `chronicle-daemon/crates/ocr/tests/fixtures/sample-text.png`
- Create: `chronicle-daemon/crates/ocr/tests/fixtures/blank.png`

**Step 1: Create fixtures directory**

```bash
mkdir -p chronicle-daemon/crates/ocr/tests/fixtures
```

**Step 2: Generate sample-text.png**

Run a Swift script to create a 400x100 white PNG with black text
"Hello Chronicle OCR" rendered in 36pt system font:

```bash
cat > /tmp/gen_sample_text.swift << 'SWIFT'
import AppKit

let size = NSSize(width: 400, height: 100)
let image = NSImage(size: size)
image.lockFocus()
NSColor.white.setFill()
NSRect(origin: .zero, size: size).fill()
let attrs: [NSAttributedString.Key: Any] = [
    .font: NSFont.systemFont(ofSize: 36),
    .foregroundColor: NSColor.black
]
("Hello Chronicle OCR" as NSString).draw(
    at: NSPoint(x: 20, y: 30),
    withAttributes: attrs
)
image.unlockFocus()

let tiff = image.tiffRepresentation!
let bitmap = NSBitmapImageRep(data: tiff)!
let png = bitmap.representation(using: .png, properties: [:])!
try! png.write(to: URL(fileURLWithPath: CommandLine.arguments[1]))
SWIFT
swift /tmp/gen_sample_text.swift chronicle-daemon/crates/ocr/tests/fixtures/sample-text.png
```

**Step 3: Generate blank.png**

Run a Swift script to create a 100x100 solid white PNG with no text:

```bash
cat > /tmp/gen_blank.swift << 'SWIFT'
import AppKit

let size = NSSize(width: 100, height: 100)
let image = NSImage(size: size)
image.lockFocus()
NSColor.white.setFill()
NSRect(origin: .zero, size: size).fill()
image.unlockFocus()

let tiff = image.tiffRepresentation!
let bitmap = NSBitmapImageRep(data: tiff)!
let png = bitmap.representation(using: .png, properties: [:])!
try! png.write(to: URL(fileURLWithPath: CommandLine.arguments[1]))
SWIFT
swift /tmp/gen_blank.swift chronicle-daemon/crates/ocr/tests/fixtures/blank.png
```

**Step 4: Verify fixtures exist**

```bash
ls -la chronicle-daemon/crates/ocr/tests/fixtures/
```

Expected: two PNG files, each a few KB.

**Step 5: Commit**

```bash
git add chronicle-daemon/crates/ocr/tests/fixtures/
git commit -m "test(ocr): add PNG test fixtures for OCR testing" \
  -m "sample-text.png: white background with 'Hello Chronicle OCR' in 36pt." \
  -m "blank.png: solid white 100x100, no text." \
  -m "Part of HEU-238"
```

---

### Task 4: Write Failing Test and Implement Missing File Error

**Files:**
- Modify: `chronicle-daemon/crates/ocr/src/lib.rs`

**Step 1: Write the failing test**

Add at the bottom of `lib.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn extract_text_errors_on_missing_file() {
        let path = PathBuf::from("/nonexistent/image.png");
        let result = extract_text(&path);
        assert!(result.is_err());
        match result.unwrap_err() {
            OcrError::ImageNotFound(p) => {
                assert!(p.contains("nonexistent"));
            }
            other => panic!("expected ImageNotFound, got: {other}"),
        }
    }
}
```

**Step 2: Run test to verify it passes**

This test should already pass because the `todo!()` is after the file existence
check. Verify:

```bash
cd chronicle-daemon && cargo test -p chronicle-ocr -- extract_text_errors_on_missing_file
```

Expected: PASS. The file check at the top of `extract_text` returns
`OcrError::ImageNotFound` before reaching `todo!()`.

**Step 3: Commit**

```bash
git add chronicle-daemon/crates/ocr/src/lib.rs
git commit -m "test(ocr): add missing file error test" \
  -m "Part of HEU-238"
```

---

### Task 5: Implement Vision Framework Text Extraction

**Files:**
- Modify: `chronicle-daemon/crates/ocr/src/lib.rs`

This is the core task. Replace the `todo!()` in `extract_text` with the full
Vision framework implementation.

**Obligations:**
- [Security] Sanitize input reaching file paths — validate the path exists before passing to Vision framework. The file existence check at the top of `extract_text` provides this. Since the caller is the daemon (not direct user input), this is low risk.
- [Security] Never expose stack traces or internal paths to end users — `OcrError::ImageNotFound` contains the file path. Acceptable for a library crate; the daemon should sanitize paths before exposing errors to the UI.

**Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block:

```rust
    /// Path to the test fixtures directory.
    fn fixtures_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
    }

    #[test]
    fn extract_text_from_image_with_known_text() {
        let path = fixtures_dir().join("sample-text.png");
        assert!(path.exists(), "fixture missing: {}", path.display());

        let result = extract_text(&path);
        assert!(result.is_ok(), "extract_text failed: {:?}", result.err());

        let text = result.unwrap();
        // The fixture contains "Hello Chronicle OCR".
        // Vision may split across observations or vary capitalization.
        let lower = text.to_lowercase();
        assert!(
            lower.contains("hello") || lower.contains("chronicle") || lower.contains("ocr"),
            "expected recognized text to contain at least one of 'hello', 'chronicle', or 'ocr', got: '{text}'"
        );
    }
```

**Step 2: Run test to verify it fails**

```bash
cd chronicle-daemon && cargo test -p chronicle-ocr -- extract_text_from_image_with_known_text
```

Expected: FAIL — hits `todo!()` panic.

**Step 3: Implement extract_text**

Replace the `extract_text` function body (everything after the signature line)
with the full implementation. The complete function:

```rust
pub fn extract_text(image_path: &Path) -> Result<String> {
    if !image_path.exists() {
        return Err(OcrError::ImageNotFound(
            image_path.display().to_string(),
        ));
    }

    let path_str = image_path
        .to_str()
        .ok_or_else(|| OcrError::VisionError("path is not valid UTF-8".into()))?;

    objc2::rc::autoreleasepool(|_pool| {
        unsafe { extract_text_inner(path_str) }
    })
}

/// Inner implementation wrapped in autoreleasepool.
///
/// # Safety
/// Must be called within an autoreleasepool. All objc2 calls are
/// encapsulated here.
unsafe fn extract_text_inner(path_str: &str) -> Result<String> {
    use objc2_foundation::{NSArray, NSDictionary, NSString, NSURL};
    use objc2_vision::{
        VNImageRequestHandler, VNRecognizeTextRequest,
        VNRequestTextRecognitionLevel,
    };

    // 1. Create NSURL from file path.
    let ns_path = NSString::from_str(path_str);
    let url = NSURL::fileURLWithPath(&ns_path);

    // 2. Create image request handler.
    let handler = VNImageRequestHandler::initWithURL_options(
        VNImageRequestHandler::alloc(),
        &url,
        &NSDictionary::new(),
    );

    // 3. Create and configure text recognition request.
    let request = VNRecognizeTextRequest::new();
    request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);

    // 4. Perform the request.
    //    performRequests_error takes &NSArray<VNRequest>.
    //    VNRecognizeTextRequest is a subclass of VNRequest.
    let requests = NSArray::from_retained_slice(&[
        objc2::rc::Retained::into_super(
            objc2::rc::Retained::into_super(request.clone()),
        ),
    ]);
    handler
        .performRequests_error(&requests)
        .map_err(|e| OcrError::VisionError(e.localizedDescription().to_string()))?;

    // 5. Extract recognized text from observations.
    let mut texts: Vec<String> = Vec::new();

    if let Some(observations) = request.results() {
        for i in 0..observations.count() {
            let observation = &observations[i];
            let candidates = observation.topCandidates(1);
            if candidates.count() > 0 {
                let recognized = &candidates[0];
                let s = recognized.string().to_string();
                if !s.is_empty() {
                    texts.push(s);
                }
            }
        }
    }

    Ok(texts.join("\n"))
}
```

Add the required imports at the top of `lib.rs` (after the module doc comment):

```rust
use std::path::Path;
```

**Important notes for the implementer:**

- The `Retained::into_super()` calls upcast `VNRecognizeTextRequest` through
  `VNImageBasedRequest` to `VNRequest`. The class hierarchy is:
  `VNRecognizeTextRequest -> VNImageBasedRequest -> VNRequest`. You may need
  to adjust the number of `into_super()` calls based on the actual hierarchy
  in `objc2-vision` 0.3.2.
- If `NSArray::from_retained_slice` is not available, try
  `NSArray::from_vec` or construct via `NSMutableArray`.
- If `NSString::from_str` is not available, use
  `NSString::stringWithUTF8String` with a CString conversion.
- `observations[i]` uses `NSArray`'s index operator. If not available, use
  `observations.objectAtIndex(i)`.
- `request.clone()` is needed because we use `request` again for `.results()`.
  If `Clone` is not implemented, restructure to extract results from the
  request after `performRequests_error`.

**Step 4: Run test to verify it passes**

```bash
cd chronicle-daemon && cargo test -p chronicle-ocr -- extract_text_from_image_with_known_text
```

Expected: PASS. Vision framework recognizes text from the fixture PNG.

**Step 5: Commit**

```bash
git add chronicle-daemon/crates/ocr/src/lib.rs
git commit -m "feat(ocr): implement extract_text via Vision framework" \
  -m "Uses VNRecognizeTextRequest with accurate recognition to extract text from image files. Loads images via VNImageRequestHandler URL initializer." \
  -m "Part of HEU-238"
```

---

### Task 6: Add Remaining Tests

**Files:**
- Modify: `chronicle-daemon/crates/ocr/src/lib.rs` (tests module)

**Step 1: Write blank image test**

Add to the `#[cfg(test)] mod tests` block:

```rust
    #[test]
    fn extract_text_returns_empty_for_blank_image() {
        let path = fixtures_dir().join("blank.png");
        assert!(path.exists(), "fixture missing: {}", path.display());

        let result = extract_text(&path);
        assert!(result.is_ok(), "extract_text failed: {:?}", result.err());

        let text = result.unwrap();
        assert!(
            text.trim().is_empty(),
            "expected empty text for blank image, got: '{text}'"
        );
    }
```

**Step 2: Write panic safety test**

Add to the `#[cfg(test)] mod tests` block:

```rust
    #[test]
    fn extract_text_never_panics() {
        let path = fixtures_dir().join("sample-text.png");
        // Call 50 times rapidly — must never panic from ObjC interop
        // or autorelease pool issues.
        for _ in 0..50 {
            let _ = extract_text(&path);
        }
    }
```

**Step 3: Run all tests**

```bash
cd chronicle-daemon && cargo test -p chronicle-ocr
```

Expected: all 4 tests PASS.

**Step 4: Commit**

```bash
git add chronicle-daemon/crates/ocr/src/lib.rs
git commit -m "test(ocr): add blank image and panic safety tests" \
  -m "Part of HEU-238"
```

---

### Task 7: Final Verification

**Step 1: Run all OCR crate tests**

```bash
cd chronicle-daemon && cargo test -p chronicle-ocr
```

Expected: all 4 tests PASS.

**Step 2: Run cargo clippy**

```bash
cd chronicle-daemon && cargo clippy -p chronicle-ocr -- -D warnings
```

Expected: no warnings. If `unsafe` blocks trigger clippy, add targeted
`#[allow(...)]` attributes with a comment explaining why.

**Step 3: Verify public API**

Confirm these are exported from `chronicle_ocr`:
- `extract_text` function
- `OcrError` enum
- `Result` type alias

```bash
cd chronicle-daemon && cargo doc -p chronicle-ocr --no-deps 2>&1 | head -5
```

Expected: docs generate without errors.

**Step 4: Run full workspace tests**

```bash
cd chronicle-daemon && cargo test --workspace
```

Expected: all workspace tests PASS. The new OCR crate tests should not
interfere with existing capture or storage tests.
