use std::fs::OpenOptions;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn second_daemon_exits_before_touching_the_database() {
    let home = tempfile::tempdir().unwrap();
    let base = home.path().join("Library/Application Support/Chronicle");
    std::fs::create_dir_all(&base).unwrap();
    let held = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(base.join("chronicle.lock"))
        .unwrap();
    held.try_lock().unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_chronicle-daemon"))
        .env("HOME", home.path())
        .env("RUST_LOG", "warn,chronicle=info")
        .stdout(Stdio::null())
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
            panic!("daemon did not exit within 10 s while the lock was held");
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
    assert!(!status.success(), "daemon exited 0; stderr: {stderr}");
    assert!(stderr.contains("already running"), "stderr: {stderr}");
    assert!(
        stderr.contains(&base.display().to_string()),
        "stderr does not name the data directory: {stderr}"
    );
    assert!(
        !stderr.contains("Microphone permission"),
        "preflight ran before the lock: {stderr}"
    );
    assert!(
        !base.join("chronicle.db").exists(),
        "daemon created chronicle.db"
    );
}
