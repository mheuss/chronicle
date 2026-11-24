import Foundation

enum ScreenRecordingPrompt {
    static let paneURL = URL(
        string: "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")!

    static func showsPopoverRow(_ a: CaptureAvailability) -> Bool {
        a == .needsScreenRecording
    }

    /// One row per cause: while the Fix… row shows, it carries the setup too.
    static func showsFinishSetupRow(state: OnboardingState?, availability a: CaptureAvailability) -> Bool {
        state == .started && !showsPopoverRow(a)
    }

    /// Every press asks macOS first, then opens the pane (CHR-96/97 design §7.3, AD16).
    @MainActor static func openSettings(request: () -> Void, open: (URL) -> Void) {
        request()
        open(paneURL)
    }
}
