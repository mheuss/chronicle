use std::ffi::OsStr;
use std::io::Read;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// Runs the daemon with `CHRONICLE_DATA_DIR` set to `data_dir`. When `data_dir`
/// is `None` the variable is removed, so a developer's exported override cannot
/// leak in. Returns the exit status and stderr. Kills the daemon and fails the
/// test if it is still running after 10 s.
pub fn run_daemon(home: &Path, data_dir: Option<&OsStr>, rust_log: &str) -> (ExitStatus, String) {
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
    (status, stderr)
}
