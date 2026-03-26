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
    if !image_path.is_file() {
        return Err(OcrError::ImageNotFound(
            image_path.display().to_string(),
        ));
    }

    let path_str = image_path
        .to_str()
        .ok_or_else(|| OcrError::VisionError("path is not valid UTF-8".into()))?;

    objc2::rc::autoreleasepool(|_pool| unsafe { extract_text_inner(path_str) })
}

/// Inner implementation wrapped in autoreleasepool.
///
/// # Safety
/// Must be called within an autoreleasepool. All objc2 calls are
/// encapsulated here.
unsafe fn extract_text_inner(path_str: &str) -> Result<String> {
    use objc2::runtime::AnyObject;
    use objc2::AnyThread; // Required for VNImageRequestHandler::alloc() trait resolution
    use objc2_foundation::{NSArray, NSDictionary, NSString, NSURL};
    use objc2_vision::{VNImageRequestHandler, VNRecognizeTextRequest, VNRequestTextRecognitionLevel};

    // 1. Create NSURL from file path.
    let ns_path = NSString::from_str(path_str);
    let url = NSURL::fileURLWithPath(&ns_path);

    // 2. Create image request handler.
    let options: objc2::rc::Retained<NSDictionary<NSString, AnyObject>> = NSDictionary::new();
    let handler = unsafe {
        VNImageRequestHandler::initWithURL_options(
            VNImageRequestHandler::alloc(),
            &url,
            &options,
        )
    };

    // 3. Create and configure text recognition request.
    let request = VNRecognizeTextRequest::new();
    request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);

    // 4. Perform the request.
    //    Upcast VNRecognizeTextRequest -> VNImageBasedRequest -> VNRequest.
    // Clone needed: into_super consumes the Retained, but we need request again for .results().
    let request_as_vn = objc2::rc::Retained::into_super(
        objc2::rc::Retained::into_super(request.clone()),
    );
    let requests = NSArray::from_retained_slice(&[request_as_vn]);
    handler
        .performRequests_error(&requests)
        .map_err(|e| OcrError::VisionError(e.localizedDescription().to_string()))?;

    // 5. Extract recognized text from observations.
    let mut texts: Vec<String> = Vec::new();

    if let Some(observations) = request.results() {
        for i in 0..observations.len() {
            let observation = observations.objectAtIndex(i);
            let candidates = observation.topCandidates(1);
            if !candidates.is_empty() {
                let recognized = candidates.objectAtIndex(0);
                let s = recognized.string().to_string();
                if !s.is_empty() {
                    texts.push(s);
                }
            }
        }
    }

    Ok(texts.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixtures_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
    }

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

    #[test]
    fn extract_text_from_image_with_known_text() {
        let path = fixtures_dir().join("sample-text.png");
        assert!(path.exists(), "fixture missing: {}", path.display());

        let result = extract_text(&path);
        assert!(result.is_ok(), "extract_text failed: {:?}", result.err());

        let text = result.unwrap();
        let lower = text.to_lowercase();
        assert!(
            lower.contains("hello") || lower.contains("chronicle") || lower.contains("ocr"),
            "expected recognized text to contain at least one of 'hello', 'chronicle', or 'ocr', got: '{text}'"
        );
    }

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

    #[test]
    fn extract_text_never_panics() {
        let path = fixtures_dir().join("sample-text.png");
        assert!(path.exists(), "fixture missing: {}", path.display());
        // Call 50 times rapidly — must never panic from ObjC interop
        // or autorelease pool issues.
        for _ in 0..50 {
            let _ = extract_text(&path);
        }
    }
}
