use thiserror::Error;

/// Errors that can occur during storage operations.
#[derive(Debug, Error)]
pub enum StorageError {
    /// SQLite query or schema error.
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),

    /// Connection pool exhausted or initialization failure.
    #[error("connection pool error: {0}")]
    Pool(#[from] r2d2::Error),

    /// Filesystem read/write error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// A spawned blocking task panicked or was cancelled.
    #[error("task join error: {0}")]
    Join(#[from] tokio::task::JoinError),

    /// Catch-all for errors that don't fit another variant.
    #[error("{0}")]
    Other(String),
}

/// Convenience alias for results that carry a [`StorageError`].
pub type Result<T> = std::result::Result<T, StorageError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_error_converts_to_storage_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
        let storage_err: StorageError = io_err.into();
        assert!(matches!(storage_err, StorageError::Io(_)));
        assert!(storage_err.to_string().contains("file missing"));
    }

    #[test]
    fn other_error_displays_message() {
        let err = StorageError::Other("something went wrong".into());
        assert_eq!(err.to_string(), "something went wrong");
    }
}
