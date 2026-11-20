mod common;

use std::fs::{File, OpenOptions};
use std::path::Path;

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

#[test]
fn second_daemon_exits_before_touching_the_database() {
    let home = tempfile::tempdir().unwrap();
    let base = chronicle_storage::data_root(home.path(), None).unwrap();
    let _held = hold_lock(&base);

    let (status, stderr) = common::run_daemon(home.path(), None, "warn,chronicle=info");

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

    let (status, stderr) = common::run_daemon(
        home.path(),
        Some(data.path().as_os_str()),
        "warn,chronicle=info",
    );

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
