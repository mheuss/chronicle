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

fn check_microphone() -> MicrophoneStatus {
    let status: isize = unsafe {
        objc2::msg_send![
            objc2::class!(AVCaptureDevice),
            authorizationStatusForMediaType: AVMediaTypeAudio
        ]
    };
    match status {
        0 => MicrophoneStatus::NotDetermined,
        1 => MicrophoneStatus::Restricted,
        2 => MicrophoneStatus::Denied,
        3 => MicrophoneStatus::Authorized,
        _ => MicrophoneStatus::Denied,
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

    log::info!("Screen recording permission: {screen:?}");
    log::info!("Microphone permission: {mic:?}");

    if screen == ScreenRecordingStatus::Denied {
        return Err(PermissionError::ScreenRecordingDenied);
    }

    Ok(mic)
}
