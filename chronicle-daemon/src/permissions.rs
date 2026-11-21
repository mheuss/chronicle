//! macOS TCC permission checks for the daemon.
//!
//! Checks Screen Recording and Microphone authorization status before
//! starting capture engines. Screen Recording is a hard gate — the
//! daemon exits if denied. Microphone is informational only.

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

/// Errors from the permission preflight check.
#[derive(Debug, thiserror::Error)]
pub enum PermissionError {
    #[error(
        "screen recording permission not granted — grant access in \
         System Settings > Privacy & Security > Screen Recording, \
         then restart chronicle-daemon"
    )]
    ScreenRecordingDenied,
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

fn check_screen_recording() -> ScreenRecordingStatus {
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

impl MicrophoneStatus {
    /// The helper's output word for this status.
    pub(crate) fn as_word(self) -> &'static str {
        match self {
            Self::Authorized => "authorized",
            Self::Denied => "denied",
            Self::NotDetermined => "not_determined",
            Self::Restricted => "restricted",
        }
    }
}

/// The helper flag. A process spawned with it reads its permission fresh,
/// and TCC attributes the read to the process that spawned it.
pub(crate) const STATUS_FLAG: &str = "--permission-status";

/// Handle `--permission-status <name>`. Returns `None` when the flag is
/// absent, so the daemon starts as usual. Otherwise returns the exit code:
/// 0 after printing one word, 2 for a missing or unknown name.
pub(crate) fn run_status_command<I, W>(
    args: I,
    out: &mut W,
    read_microphone: impl FnOnce() -> MicrophoneStatus,
) -> Option<u8>
where
    I: IntoIterator<Item = String>,
    W: std::io::Write,
{
    let mut args = args.into_iter();
    if args.next().as_deref() != Some(STATUS_FLAG) {
        return None;
    }
    match args.next().as_deref() {
        Some("microphone") => {
            let _ = writeln!(out, "{}", read_microphone().as_word());
            Some(0)
        }
        _ => Some(2),
    }
}

// ---------------------------------------------------------------------------
// Preflight
// ---------------------------------------------------------------------------

/// Check all required permissions before starting engines.
///
/// Returns microphone status for informational use. Returns
/// `Err(PermissionError::ScreenRecordingDenied)` if screen recording
/// is not authorized.
pub fn preflight() -> Result<MicrophoneStatus, PermissionError> {
    let screen = check_screen_recording();
    let mic = check_microphone();

    log::info!("Microphone permission: {mic:?}");

    if screen == ScreenRecordingStatus::Denied {
        log::error!("Screen recording permission: {screen:?}");
        return Err(PermissionError::ScreenRecordingDenied);
    }

    log::info!("Screen recording permission: {screen:?}");

    Ok(mic)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> (Option<u8>, String) {
        run_with(args, MicrophoneStatus::Authorized)
    }

    fn run_with(args: &[&str], status: MicrophoneStatus) -> (Option<u8>, String) {
        let mut out = Vec::new();
        let code = run_status_command(args.iter().map(|s| s.to_string()), &mut out, || status);
        (code, String::from_utf8(out).unwrap())
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
        let code = run_status_command(args, &mut out, || panic!("read for an unknown name"));
        assert_eq!(code, Some(2));
    }

    #[test]
    fn words_round_trip_for_every_status() {
        for status in [
            MicrophoneStatus::Authorized,
            MicrophoneStatus::Denied,
            MicrophoneStatus::NotDetermined,
            MicrophoneStatus::Restricted,
        ] {
            let read = PermissionRead::from_word(status.as_word());
            assert_eq!(read, PermissionRead::from(status));
        }
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
