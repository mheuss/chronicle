use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

fn hold_lock(base: &Path) -> File {
    std::fs::create_dir_all(base).unwrap();
    let held = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(base.join("chronicle.lock"))
        .unwrap();
    held.try_lock().unwrap();
    held
}

/// Runs the daemon with `CHRONICLE_DATA_DIR` set to `data_dir`, or removed so
/// a developer's exported override cannot leak in.
fn run_daemon(home: &Path, data_dir: Option<&OsStr>) -> (ExitStatus, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_chronicle-daemon"));
    command
        .current_dir(home)
        .env("HOME", home)
        .env("RUST_LOG", "warn,chronicle=info")
        .env_remove("CHRONICLE_LOG_BACKEND")
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
            panic!("daemon did not exit within 10 s while the lock was held; stderr: {stderr}");
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
    (status, stderr)
}

#[test]
fn second_daemon_exits_before_touching_the_database() {
    let home = tempfile::tempdir().unwrap();
    let base = chronicle_storage::data_root(home.path(), None).unwrap();
    let _held = hold_lock(&base);

    let (status, stderr) = run_daemon(home.path(), None);

    assert!(status.success(), "daemon exited non-zero; stderr: {stderr}");
    assert!(stderr.contains("already running"), "stderr: {stderr}");
    assert!(
        stderr.contains("chronicle-daemon starting"),
        "logging never reached stderr, so the preflight check below proves nothing: {stderr}"
    );
    assert!(
        stderr.contains(&base.display().to_string()),
        "stderr does not name the group container: {stderr}"
    );
    assert!(
        !stderr.contains("Microphone permission"),
        "preflight ran before the lock: {stderr}"
    );
    let mut entries: Vec<_> = std::fs::read_dir(&base)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    entries.sort();
    assert_eq!(
        entries,
        ["chronicle.lock"],
        "daemon touched the data directory"
    );
}

#[test]
fn override_moves_the_lock_out_of_home() {
    let home = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let _held = hold_lock(data.path());

    let (status, stderr) = run_daemon(home.path(), Some(data.path().as_os_str()));

    assert!(status.success(), "daemon exited non-zero; stderr: {stderr}");
    assert!(
        stderr.contains(&format!("already running on {}", data.path().display())),
        "the lock the daemon found is not the one under the override: {stderr}"
    );
    let entries: Vec<_> = std::fs::read_dir(data.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(entries, ["chronicle.lock"], "stderr: {stderr}");
    assert!(
        !home.path().join("Library").exists(),
        "daemon created a data root under HOME: {stderr}"
    );
}
