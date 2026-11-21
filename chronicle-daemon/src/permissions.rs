//! macOS TCC permission checks for the daemon.
//!
//! The boot read sets the initial Screen Recording gate. After boot, every
//! decision uses a fresh helper read (`--permission-status`, ADR-022).
//! A running process keeps its first answer. Neither read ever stops the
//! daemon.

use objc2_foundation::NSString;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Screen recording permission status.
///
/// `CGPreflightScreenCaptureAccess` returns `false` for both "denied" and
/// "not yet determined", so we collapse them into `Denied`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenRecordingStatus {
    Authorized,
    Denied,
}

/// Microphone authorization status from AVFoundation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicrophoneStatus {
    /// User has granted access.
    Authorized,
    /// User has explicitly denied access.
    Denied,
    /// Never prompted — can be requested later by the UI.
    NotDetermined,
    /// System-level restriction (parental controls, MDM).
    Restricted,
}

// ---------------------------------------------------------------------------
// FFI
// ---------------------------------------------------------------------------

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
}

#[link(name = "AVFoundation", kind = "framework")]
unsafe extern "C" {
    static AVMediaTypeAudio: &'static NSString;
}

// ---------------------------------------------------------------------------
// Checks
// ---------------------------------------------------------------------------

pub(crate) fn check_screen_recording() -> ScreenRecordingStatus {
    let authorized = unsafe { CGPreflightScreenCaptureAccess() };
    if authorized {
        ScreenRecordingStatus::Authorized
    } else {
        ScreenRecordingStatus::Denied
    }
}

pub(crate) fn check_microphone() -> MicrophoneStatus {
    let status: isize = unsafe {
        objc2::msg_send![
            objc2::class!(AVCaptureDevice),
            authorizationStatusForMediaType: AVMediaTypeAudio
        ]
    };
    match status {
        0 => MicrophoneStatus::NotDetermined, // AVAuthorizationStatusNotDetermined
        1 => MicrophoneStatus::Restricted,    // AVAuthorizationStatusRestricted
        2 => MicrophoneStatus::Denied,        // AVAuthorizationStatusDenied
        3 => MicrophoneStatus::Authorized,    // AVAuthorizationStatusAuthorized
        _ => {
            log::warn!("Unknown AVAuthorizationStatus value: {status}");
            MicrophoneStatus::Denied
        }
    }
}

/// One permission read taken through the helper process (design §1.3).
/// `Unavailable` means the read failed, and no caller treats it as a grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionRead {
    Authorized,
    NotDetermined,
    Denied,
    Restricted,
    Unavailable,
}

impl PermissionRead {
    /// Parse one helper output word. Anything else is `Unavailable`.
    pub(crate) fn from_word(word: &str) -> Self {
        match word.trim() {
            "authorized" => Self::Authorized,
            "denied" => Self::Denied,
            "not_determined" => Self::NotDetermined,
            "restricted" => Self::Restricted,
            _ => Self::Unavailable,
        }
    }

    /// The helper's output word, the inverse of `from_word`.
    pub(crate) fn as_word(self) -> &'static str {
        match self {
            Self::Authorized => "authorized",
            Self::Denied => "denied",
            Self::NotDetermined => "not_determined",
            Self::Restricted => "restricted",
            Self::Unavailable => "unavailable",
        }
    }
}

impl From<MicrophoneStatus> for PermissionRead {
    fn from(status: MicrophoneStatus) -> Self {
        match status {
            MicrophoneStatus::Authorized => Self::Authorized,
            MicrophoneStatus::Denied => Self::Denied,
            MicrophoneStatus::NotDetermined => Self::NotDetermined,
            MicrophoneStatus::Restricted => Self::Restricted,
        }
    }
}

impl From<ScreenRecordingStatus> for PermissionRead {
    fn from(status: ScreenRecordingStatus) -> Self {
        match status {
            ScreenRecordingStatus::Authorized => Self::Authorized,
            ScreenRecordingStatus::Denied => Self::Denied,
        }
    }
}

/// The helper's name for the Screen Recording permission.
pub(crate) const SCREEN_RECORDING: &str = "screen-recording";

/// The helper flag. A process spawned with it reads its permission fresh,
/// and TCC attributes the read to the process that spawned it.
pub(crate) const STATUS_FLAG: &str = "--permission-status";

/// Handle `--permission-status <name>`. Returns `None` when the flag is
/// absent, so the daemon starts as usual. Otherwise returns the exit code:
/// 0 after printing one word, 1 when the word could not be written, and 2 for
/// a missing or unknown name or any extra argument.
pub(crate) fn run_status_command<I, W>(
    args: I,
    out: &mut W,
    read_microphone: impl FnOnce() -> MicrophoneStatus,
    read_screen_recording: impl FnOnce() -> ScreenRecordingStatus,
) -> Option<u8>
where
    I: IntoIterator<Item = String>,
    W: std::io::Write,
{
    let mut args = args.into_iter();
    if args.next().as_deref() != Some(STATUS_FLAG) {
        return None;
    }
    let (Some(name), None) = (args.next(), args.next()) else {
        return Some(2);
    };
    let read = match name.as_str() {
        "microphone" => PermissionRead::from(read_microphone()),
        SCREEN_RECORDING => PermissionRead::from(read_screen_recording()),
        _ => return Some(2),
    };
    match writeln!(out, "{}", read.as_word()).and_then(|()| out.flush()) {
        Ok(()) => Some(0),
        Err(_) => Some(1),
    }
}

/// Longest wait for one helper read (design N9).
pub(crate) const HELPER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

/// The microphone permission as a fresh helper process reads it. A running
/// process's own read goes stale (design §1.3), so every gate input comes
/// from here.
pub(crate) fn fresh_microphone_status() -> PermissionRead {
    match std::env::current_exe() {
        Ok(exe) => fresh_status_with(&exe, "microphone", &run_helper),
        Err(e) => {
            log::warn!("permission helper: current_exe failed: {e}");
            PermissionRead::Unavailable
        }
    }
}

/// The Screen Recording permission as a fresh helper process reads it (design §2).
/// `CGPreflightScreenCaptureAccess` keeps its first answer in a running process.
/// The daemon never decides on its own read after boot.
pub(crate) fn fresh_screen_recording_status() -> PermissionRead {
    match std::env::current_exe() {
        Ok(exe) => fresh_status_with(&exe, SCREEN_RECORDING, &run_helper),
        Err(e) => {
            log::warn!("permission helper: current_exe failed: {e}");
            PermissionRead::Unavailable
        }
    }
}

/// Spawns the helper and returns its stdout, or `None` on any failure.
/// `run_helper` is the production runner; tests pass a stub.
pub(crate) type HelperRunner<'a> =
    dyn Fn(&std::path::Path, &[&str], std::time::Duration) -> Option<String> + 'a;

/// Run the helper through `run` and parse its answer. Any failure is
/// `Unavailable`.
pub(crate) fn fresh_status_with(
    exe: &std::path::Path,
    name: &str,
    run: &HelperRunner<'_>,
) -> PermissionRead {
    let Some(out) = run(exe, &[STATUS_FLAG, name], HELPER_TIMEOUT) else {
        log::warn!("permission helper: no answer for {name}");
        return PermissionRead::Unavailable;
    };
    let read = PermissionRead::from_word(&out);
    if read == PermissionRead::Unavailable {
        log::warn!("permission helper: unexpected output for {name}");
    }
    read
}

/// Spawn `exe` with `args` and return its stdout after a zero exit, or `None`
/// on any failure. A child still running at `timeout` is killed and reaped.
/// `timeout` bounds the child's exit only, so `exe` must start no children of
/// its own and write little output, as `--permission-status` does.
pub(crate) fn run_helper(
    exe: &std::path::Path,
    args: &[&str],
    timeout: std::time::Duration,
) -> Option<String> {
    use std::io::Read;
    use std::process::{Command, Stdio};

    let mut child = Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                let mut out = String::new();
                child.stdout.take()?.read_to_string(&mut out).ok()?;
                return Some(out);
            }
            Ok(Some(_)) => return None,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Preflight
// ---------------------------------------------------------------------------

/// What the daemon read at boot (ADR-022). Only `screen` sets a gate.
/// `mic` is logged and seeds the first `Status`.
pub struct BootPermissions {
    /// In-process read. Fresh, because the process has just started.
    pub screen: ScreenRecordingStatus,
    /// Helper read, the same source as the mic gate, so the first `Status`
    /// agrees with it.
    pub mic: PermissionRead,
}

/// Read both permissions at boot and log them. Never fails: a missing
/// Screen Recording grant gates capture instead of stopping the daemon
/// (design AD1).
pub fn preflight() -> BootPermissions {
    preflight_with(check_screen_recording, fresh_microphone_status)
}

fn preflight_with(
    read_screen: impl FnOnce() -> ScreenRecordingStatus,
    read_mic: impl FnOnce() -> PermissionRead,
) -> BootPermissions {
    let screen = read_screen();
    let mic = read_mic();
    log::info!("Microphone permission: {mic:?}");
    match screen {
        ScreenRecordingStatus::Authorized => {
            log::info!("Screen recording permission: Authorized");
        }
        ScreenRecordingStatus::Denied => {
            log::warn!(
                "Screen recording permission: Denied — capture stays off until it is granted"
            );
        }
    }
    BootPermissions { screen, mic }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::{Duration, Instant};

    #[test]
    fn preflight_takes_the_mic_from_the_fresh_reader() {
        let boot = preflight_with(
            || ScreenRecordingStatus::Denied,
            || PermissionRead::NotDetermined,
        );
        assert_eq!(boot.screen, ScreenRecordingStatus::Denied);
        assert_eq!(boot.mic, PermissionRead::NotDetermined);
    }

    fn answer(out: Option<&'static str>) -> impl Fn(&Path, &[&str], Duration) -> Option<String> {
        move |_, _, _| out.map(str::to_string)
    }

    #[test]
    fn fresh_status_parses_each_word() {
        let exe = Path::new("/unused");
        assert_eq!(
            fresh_status_with(exe, "microphone", &answer(Some("authorized\n"))),
            PermissionRead::Authorized
        );
        assert_eq!(
            fresh_status_with(exe, "microphone", &answer(Some("denied\n"))),
            PermissionRead::Denied
        );
        assert_eq!(
            fresh_status_with(exe, "microphone", &answer(Some("not_determined\n"))),
            PermissionRead::NotDetermined
        );
        assert_eq!(
            fresh_status_with(exe, "microphone", &answer(Some("restricted\n"))),
            PermissionRead::Restricted
        );
    }

    #[test]
    fn fresh_status_fails_closed() {
        let exe = Path::new("/unused");
        // No answer (spawn error, timeout, non-zero exit) and unknown output
        // are both Unavailable. Neither falls back to the in-process read.
        assert_eq!(
            fresh_status_with(exe, "microphone", &answer(None)),
            PermissionRead::Unavailable
        );
        assert_eq!(
            fresh_status_with(exe, "microphone", &answer(Some("yes\n"))),
            PermissionRead::Unavailable
        );
    }

    #[test]
    fn fresh_status_passes_the_flag_and_name() {
        let seen = std::cell::RefCell::new(Vec::new());
        let run = |_: &Path, args: &[&str], timeout: Duration| {
            seen.borrow_mut().extend(args.iter().map(|s| s.to_string()));
            assert_eq!(timeout, HELPER_TIMEOUT);
            Some("authorized".to_string())
        };
        fresh_status_with(Path::new("/unused"), "microphone", &run);
        assert_eq!(*seen.borrow(), vec!["--permission-status", "microphone"]);
    }

    #[test]
    fn run_helper_returns_stdout_on_success() {
        let out = run_helper(
            Path::new("/bin/echo"),
            &["authorized"],
            Duration::from_secs(1),
        );
        assert_eq!(out.as_deref(), Some("authorized\n"));
    }

    #[test]
    fn run_helper_returns_none_on_nonzero_exit_or_missing_binary() {
        assert_eq!(
            run_helper(Path::new("/usr/bin/false"), &[], Duration::from_secs(1)),
            None
        );
        assert_eq!(
            run_helper(Path::new("/no/such/helper"), &[], Duration::from_secs(1)),
            None
        );
    }

    #[test]
    fn run_helper_kills_a_slow_helper_at_the_timeout() {
        let start = Instant::now();
        let out = run_helper(Path::new("/bin/sleep"), &["5"], Duration::from_millis(100));
        assert_eq!(out, None);
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    fn run(args: &[&str]) -> (Option<u8>, String) {
        run_with(args, MicrophoneStatus::Authorized)
    }

    fn run_with(args: &[&str], status: MicrophoneStatus) -> (Option<u8>, String) {
        let mut out = Vec::new();
        let code = run_status_command(
            args.iter().map(|s| s.to_string()),
            &mut out,
            || status,
            || ScreenRecordingStatus::Denied,
        );
        (code, String::from_utf8(out).unwrap())
    }

    #[test]
    fn status_command_screen_recording_prints_the_reader_word() {
        assert_eq!(
            run(&["--permission-status", "screen-recording"]),
            (Some(0), "denied\n".to_string())
        );
    }

    #[test]
    fn screen_recording_status_maps_to_its_read() {
        assert_eq!(
            PermissionRead::from(ScreenRecordingStatus::Authorized),
            PermissionRead::Authorized
        );
        assert_eq!(
            PermissionRead::from(ScreenRecordingStatus::Denied),
            PermissionRead::Denied
        );
    }

    #[test]
    fn fresh_screen_status_uses_its_own_name() {
        let seen = std::cell::RefCell::new(Vec::new());
        let run = |_: &Path, args: &[&str], _: Duration| {
            seen.borrow_mut().extend(args.iter().map(|s| s.to_string()));
            Some("denied\n".to_string())
        };
        let read = fresh_status_with(Path::new("/unused"), SCREEN_RECORDING, &run);
        assert_eq!(read, PermissionRead::Denied);
        assert_eq!(
            *seen.borrow(),
            vec!["--permission-status", "screen-recording"]
        );
    }

    #[test]
    fn status_command_absent_flag_returns_none() {
        assert_eq!(run(&[]).0, None);
        assert_eq!(run(&["--other"]).0, None);
    }

    #[test]
    fn status_command_unknown_name_exits_2_with_no_output() {
        assert_eq!(
            run(&["--permission-status", "camera"]),
            (Some(2), String::new())
        );
        assert_eq!(run(&["--permission-status"]), (Some(2), String::new()));
    }

    #[test]
    fn status_command_prints_each_status_word() {
        for (status, line) in [
            (MicrophoneStatus::Authorized, "authorized\n"),
            (MicrophoneStatus::Denied, "denied\n"),
            (MicrophoneStatus::NotDetermined, "not_determined\n"),
            (MicrophoneStatus::Restricted, "restricted\n"),
        ] {
            let got = run_with(&["--permission-status", "microphone"], status);
            assert_eq!(got, (Some(0), line.to_string()), "{status:?}");
        }
    }

    #[test]
    fn status_command_reads_only_for_a_known_name() {
        let mut out = Vec::new();
        let args = ["--permission-status", "camera"].map(String::from);
        let code = run_status_command(
            args,
            &mut out,
            || panic!("read for an unknown name"),
            || panic!("read for an unknown name"),
        );
        assert_eq!(code, Some(2));
    }

    #[test]
    fn words_round_trip_for_every_read() {
        for read in [
            PermissionRead::Authorized,
            PermissionRead::Denied,
            PermissionRead::NotDetermined,
            PermissionRead::Restricted,
            PermissionRead::Unavailable,
        ] {
            assert_eq!(PermissionRead::from_word(read.as_word()), read);
        }
    }

    #[test]
    fn status_command_rejects_a_trailing_argument() {
        let got = run(&["--permission-status", "microphone", "extra"]);
        assert_eq!(got, (Some(2), String::new()));
    }

    #[test]
    fn status_command_exits_1_when_the_word_cannot_be_written() {
        struct Closed;
        impl std::io::Write for Closed {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let args = ["--permission-status", "microphone"].map(String::from);
        let code = run_status_command(
            args,
            &mut Closed,
            || MicrophoneStatus::Authorized,
            || ScreenRecordingStatus::Denied,
        );
        assert_eq!(code, Some(1));
    }

    #[test]
    fn unknown_word_reads_as_unavailable() {
        assert_eq!(PermissionRead::from_word(""), PermissionRead::Unavailable);
        assert_eq!(
            PermissionRead::from_word("granted"),
            PermissionRead::Unavailable
        );
    }

    #[test]
    fn from_word_ignores_surrounding_whitespace() {
        assert_eq!(
            PermissionRead::from_word("denied\n"),
            PermissionRead::Denied
        );
    }
}
