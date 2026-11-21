import Foundation

/// When the Screen Recording dialog shows (design §7.1, AD12). One per app
/// launch. Re-armed only by an observation that reports the grant.
struct ScreenRecordingPromptLatch {
    private(set) var armed = true

    /// Returns true when the dialog should show now.
    mutating func observe(_ availability: CaptureAvailability,
                          screenRecordingAuthorized: Bool) -> Bool {
        if screenRecordingAuthorized {
            armed = true
            return false
        }
        guard armed, availability == .needsScreenRecording else { return false }
        armed = false
        return true
    }
}

enum ScreenRecordingPrompt {
    static let paneURL = URL(
        string: "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")!

    static func showsPopoverRow(_ a: CaptureAvailability) -> Bool {
        a == .needsScreenRecording
    }

    /// Every press asks macOS first, then opens the pane (design §7.3, AD16).
    @MainActor static func openSettings(request: () -> Void, open: (URL) -> Void) {
        request()
        open(paneURL)
    }
}
