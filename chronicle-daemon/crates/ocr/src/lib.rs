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
