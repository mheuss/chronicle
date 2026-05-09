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

    /// HEIF encoding failed.
    #[error("heif encoding failed: {0}")]
    Encoding(String),

    /// One or more capture streams could not be torn down cleanly.
    ///
    /// Returned from both startup rollback (when a `start_stream` failure
    /// triggers stop of already-started peers) and `CaptureEngine::stop()`
    /// (when one or more `stop_stream` calls fail). Survivors are identified
    /// by display ID. The `original` cause is the error that triggered the
    /// teardown.
    #[error("capture teardown failed; {} stream(s) may still be running: {survivors:?}", survivors.len())]
    PartialTeardown {
        survivors: Vec<u32>,
        stop_errors: Vec<(u32, String)>,
        #[source]
        original: Box<CaptureError>,
    },

    /// Engine entered a poisoned state after `stop()` failed.
    #[error("engine is poisoned after stop failure")]
    Poisoned,
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

    #[test]
    fn encoding_error_displays() {
        let err = CaptureError::Encoding("pixel buffer locked".into());
        assert!(err.to_string().contains("pixel buffer locked"));
        assert!(err.to_string().contains("heif encoding failed"));
    }

    #[test]
    fn partial_teardown_error_includes_survivor_count() {
        let err = CaptureError::PartialTeardown {
            survivors: vec![1, 2],
            stop_errors: vec![(1, "timeout".into()), (2, "hung".into())],
            original: Box::new(CaptureError::ScreenCaptureKit("boom".into())),
        };
        let s = err.to_string();
        assert!(
            s.contains("2"),
            "message should include survivor count: {s}"
        );
        assert!(
            s.contains("[1, 2]"),
            "message should include survivor ids: {s}"
        );
    }

    #[test]
    fn poisoned_error_displays() {
        let err = CaptureError::Poisoned;
        assert_eq!(err.to_string(), "engine is poisoned after stop failure");
    }
}
