use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Runs the daemon with `CHRONICLE_DATA_DIR` set to `data_dir`. When `data_dir`
/// is `None` the variable is removed, so a developer's exported override cannot
/// leak in. Returns the exit code and stderr. Kills the daemon and fails the
/// test if it is still running after 10 s.
fn run_daemon(home: &Path, data_dir: Option<&OsStr>, rust_log: &str) -> (Option<i32>, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_chronicle-daemon"));
    command
        .current_dir(home)
        .env("HOME", home)
        .env("RUST_LOG", rust_log)
        .env_remove("CHRONICLE_LOG_BACKEND")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    match data_dir {
        Some(dir) => command.env("CHRONICLE_DATA_DIR", dir),
        None => command.env_remove("CHRONICLE_DATA_DIR"),
    };
    let mut child = command.spawn().unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            let mut stderr = String::new();
            let _ = child.stderr.take().unwrap().read_to_string(&mut stderr);
            panic!("daemon did not exit within 10 s; stderr: {stderr}");
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    (status.code(), stderr)
}

/// Runs the daemon with a regular file where its data directory should be, so
/// taking the instance lock fails. Returns the blocked path, exit code and stderr.
fn run_with_blocked_data_dir(rust_log: &str) -> (PathBuf, Option<i32>, String) {
    let home = tempfile::tempdir().unwrap();
    let base = chronicle_storage::data_root(home.path(), None).unwrap();
    std::fs::create_dir_all(base.parent().unwrap()).unwrap();
    std::fs::write(&base, b"").unwrap();
    let (code, stderr) = run_daemon(home.path(), None, rust_log);
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
    let (code, stderr) = run_daemon(
        home.path(),
        Some(OsStr::new("relative-data")),
        "warn,chronicle=info",
    );
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
    let (code, stderr) = run_daemon(home.path(), Some(data.as_os_str()), "warn,chronicle=info");
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
