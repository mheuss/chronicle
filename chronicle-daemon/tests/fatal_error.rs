use std::process::{Command, Output, Stdio};

/// Runs the daemon with a regular file where its data directory should be, so
/// taking the instance lock fails.
fn run_with_blocked_data_dir(rust_log: &str) -> Output {
    let home = tempfile::tempdir().unwrap();
    let support = home.path().join("Library/Application Support");
    std::fs::create_dir_all(&support).unwrap();
    std::fs::write(support.join("Chronicle"), b"").unwrap();

    Command::new(env!("CARGO_BIN_EXE_chronicle-daemon"))
        .env("HOME", home.path())
        .env("RUST_LOG", rust_log)
        .env_remove("CHRONICLE_LOG_BACKEND")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

#[test]
fn fatal_error_is_logged_once() {
    let out = run_with_blocked_data_dir("warn,chronicle=info");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    assert_eq!(
        stderr.matches("chronicle-daemon failed").count(),
        1,
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("could not take the instance lock"),
        "stderr: {stderr}"
    );
}

#[test]
fn fatal_error_reaches_stderr_when_the_filter_drops_it() {
    for rust_log in ["off", "chronicle_audio=debug", "error/zzz"] {
        let out = run_with_blocked_data_dir(rust_log);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(1), "RUST_LOG={rust_log}: {stderr}");
        assert!(
            stderr.contains("chronicle-daemon failed"),
            "RUST_LOG={rust_log}: {stderr}"
        );
    }
}
