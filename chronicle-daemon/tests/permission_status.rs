//! The `--permission-status` helper contract, checked against the built binary
//! (design §1.3).

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Runs the binary with `args` and an empty `HOME`. Returns the exit code,
/// stdout and stderr. Fails the test if it is still running after 10 s, which
/// means it started the daemon instead of answering as the helper.
fn helper(home: &Path, args: &[&str]) -> (Option<i32>, String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_chronicle-daemon"))
        .args(args)
        .current_dir(home)
        .env("HOME", home)
        .env("RUST_LOG", "trace")
        .env_remove("CHRONICLE_LOG_BACKEND")
        .env_remove("CHRONICLE_DATA_DIR")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("{args:?} did not exit within 10 s");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut stdout)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    (status.code(), stdout, stderr)
}

fn assert_no_daemon_start(home: &Path, stderr: &str) {
    assert!(stderr.is_empty(), "the helper logged: {stderr}");
    let root = chronicle_storage::data_root(home, None).unwrap();
    assert!(!root.exists(), "the helper created {}", root.display());
}

#[test]
fn microphone_prints_one_word_and_exits_0() {
    let home = tempfile::tempdir().unwrap();
    let (code, stdout, stderr) = helper(home.path(), &["--permission-status", "microphone"]);
    assert_eq!(code, Some(0), "stderr: {stderr}");
    // The word depends on this host's TCC state, so only its shape is checked.
    assert!(
        [
            "authorized\n",
            "denied\n",
            "not_determined\n",
            "restricted\n"
        ]
        .contains(&stdout.as_str()),
        "stdout: {stdout:?}"
    );
    assert_no_daemon_start(home.path(), &stderr);
}

#[test]
fn unknown_name_exits_2_with_no_output() {
    let home = tempfile::tempdir().unwrap();
    let (code, stdout, stderr) = helper(home.path(), &["--permission-status", "camera"]);
    assert_eq!(code, Some(2));
    assert_eq!(stdout, "");
    assert_no_daemon_start(home.path(), &stderr);
}

#[test]
fn trailing_argument_exits_2_with_no_output() {
    let home = tempfile::tempdir().unwrap();
    let args = ["--permission-status", "microphone", "extra"];
    let (code, stdout, stderr) = helper(home.path(), &args);
    assert_eq!(code, Some(2));
    assert_eq!(stdout, "");
    assert_no_daemon_start(home.path(), &stderr);
}
