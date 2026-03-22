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
