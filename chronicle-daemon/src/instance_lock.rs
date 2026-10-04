use std::fs::{DirBuilder, File, OpenOptions, TryLockError};
use std::future::Future;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

pub const LOCK_FILE: &str = "chronicle.lock";

#[derive(Debug, thiserror::Error)]
pub enum InstanceLockError {
    #[error("another chronicle-daemon is already running on {0}")]
    AlreadyRunning(PathBuf),
    #[error("could not lock {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

#[derive(Debug)]
pub struct InstanceLock {
    _file: File,
}

pub fn acquire(base_dir: &Path) -> Result<InstanceLock, InstanceLockError> {
    let path = base_dir.join(LOCK_FILE);
    let io = |source| InstanceLockError::Io {
        path: path.clone(),
        source,
    };
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(base_dir)
        .map_err(io)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .map_err(io)?;
    match file.try_lock() {
        Ok(()) => Ok(InstanceLock { _file: file }),
        Err(TryLockError::WouldBlock) => {
            Err(InstanceLockError::AlreadyRunning(base_dir.to_path_buf()))
        }
        Err(TryLockError::Error(e)) => Err(io(e)),
    }
}

/// Drops the runtime before the lock. Runtime teardown waits for running
/// `spawn_blocking` tasks, and those can still be writing to storage.
pub fn run_holding<T>(
    lock: InstanceLock,
    runtime: tokio::runtime::Runtime,
    body: impl Future<Output = T>,
) -> T {
    let out = runtime.block_on(body);
    drop(runtime);
    drop(lock);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Mutex};
    use tempfile::tempdir;

    #[test]
    fn second_acquire_fails_while_first_is_held() {
        let dir = tempdir().unwrap();
        let _first = acquire(dir.path()).unwrap();
        match acquire(dir.path()) {
            Err(InstanceLockError::AlreadyRunning(p)) => assert_eq!(p, dir.path()),
            other => panic!("expected AlreadyRunning, got {other:?}"),
        }
    }

    #[test]
    fn acquire_succeeds_after_guard_is_dropped() {
        let dir = tempdir().unwrap();
        let first = acquire(dir.path()).unwrap();
        drop(first);
        acquire(dir.path()).unwrap();
    }

    #[test]
    fn leftover_lock_file_does_not_block() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join(LOCK_FILE), b"").unwrap();
        acquire(dir.path()).unwrap();
    }

    #[test]
    fn lock_file_is_mode_0600() {
        let dir = tempdir().unwrap();
        let _lock = acquire(dir.path()).unwrap();
        let mode = std::fs::metadata(dir.path().join(LOCK_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn acquire_creates_a_missing_base_dir() {
        let dir = tempdir().unwrap();
        let base = dir.path().join("a/b/Chronicle");
        let _lock = acquire(&base).unwrap();
        assert!(base.join(LOCK_FILE).is_file());
    }

    #[test]
    fn run_holding_keeps_the_lock_until_blocking_work_finishes() {
        let dir = tempdir().unwrap();
        let base = dir.path().to_path_buf();
        let lock = acquire(&base).unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let seen = Arc::new(Mutex::new(None));
        let seen_in_task = Arc::clone(&seen);
        let base_in_task = base.clone();
        run_holding(lock, runtime, async move {
            let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel::<()>();
            tokio::spawn(async move {
                let _dropped_at_shutdown = shutdown_tx;
                std::future::pending::<()>().await;
            });
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            tokio::task::spawn_blocking(move || {
                started_tx.send(()).unwrap();
                let _ = shutdown_rx.recv();
                let held = matches!(
                    acquire(&base_in_task),
                    Err(InstanceLockError::AlreadyRunning(_))
                );
                *seen_in_task.lock().unwrap() = Some(held);
            });
            started_rx.await.unwrap();
        });
        assert_eq!(*seen.lock().unwrap(), Some(true));
        acquire(&base).unwrap();
    }
}
