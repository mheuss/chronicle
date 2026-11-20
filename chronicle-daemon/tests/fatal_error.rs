mod common;

use std::ffi::OsStr;
use std::path::PathBuf;

/// Runs the daemon with a regular file where its data directory should be, so
/// taking the instance lock fails. Returns the blocked path, exit code and stderr.
fn run_with_blocked_data_dir(rust_log: &str) -> (PathBuf, Option<i32>, String) {
    let home = tempfile::tempdir().unwrap();
    let base = chronicle_storage::data_root(home.path(), None).unwrap();
    std::fs::create_dir_all(base.parent().unwrap()).unwrap();
    std::fs::write(&base, b"").unwrap();
    let (status, stderr) = common::run_daemon(home.path(), None, rust_log);
    let code = status.code();
    (base, code, stderr)
}

#[test]
fn fatal_error_is_logged_once() {
    let (base, code, stderr) = run_with_blocked_data_dir("warn,chronicle=info");
    assert_eq!(code, Some(1), "stderr: {stderr}");
    assert_eq!(
        stderr.matches("chronicle-daemon failed").count(),
        1,
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("could not take the instance lock"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains(&base.display().to_string()),
        "stderr does not name the group container: {stderr}"
    );
}

#[test]
fn fatal_error_reaches_stderr_when_the_filter_drops_it() {
    for rust_log in ["off", "chronicle_audio=debug", "error/zzz"] {
        let (_, code, stderr) = run_with_blocked_data_dir(rust_log);
        assert_eq!(code, Some(1), "RUST_LOG={rust_log}: {stderr}");
        assert!(
            stderr.contains("chronicle-daemon failed"),
            "RUST_LOG={rust_log}: {stderr}"
        );
    }
}

#[test]
fn relative_override_fails_before_the_lock() {
    let home = tempfile::tempdir().unwrap();
    let (status, stderr) = common::run_daemon(
        home.path(),
        Some(OsStr::new("relative-data")),
        "warn,chronicle=info",
    );
    let code = status.code();
    assert_eq!(code, Some(1), "stderr: {stderr}");
    assert!(
        stderr.contains("CHRONICLE_DATA_DIR must be an absolute path, got \"relative-data\""),
        "stderr: {stderr}"
    );
    assert!(!stderr.contains("instance lock"), "stderr: {stderr}");
    assert!(
        !home.path().join("relative-data").exists(),
        "daemon created the relative directory: {stderr}"
    );
    assert!(
        !home.path().join("Library").exists(),
        "daemon fell back to the default root: {stderr}"
    );
}

#[test]
fn oversized_socket_path_fails_before_preflight() {
    let home = tempfile::tempdir().unwrap();
    let data = home.path().join("a".repeat(120));
    let (status, stderr) =
        common::run_daemon(home.path(), Some(data.as_os_str()), "warn,chronicle=info");
    let code = status.code();
    assert_eq!(code, Some(1), "stderr: {stderr}");
    assert!(stderr.contains("103-byte limit"), "stderr: {stderr}");
    assert!(
        !stderr.contains("Microphone permission"),
        "preflight ran before the socket check: {stderr}"
    );
    assert!(
        !data.join("chronicle.db").exists(),
        "storage opened before the socket check: {stderr}"
    );
}
