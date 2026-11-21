import AppKit
import CoreGraphics
import Observation

/// Owns the dialog for the whole app, so it shows even if the popover has
/// never opened (design §7.1).
@MainActor @Observable
final class ScreenRecordingPromptController {
    @ObservationIgnored private var latch = ScreenRecordingPromptLatch()
    @ObservationIgnored private let presenter: @MainActor () -> Void

    /// What the latch watches. Equatable, so a view can observe changes.
    struct Input: Equatable {
        let availability: CaptureAvailability
        /// The current Status's read, or nil without one.
        let screenRecording: PermissionState?
    }

    /// `presenter` shows the dialog. Tests pass a stub. The app uses the
    /// alert below.
    init(presenter: (@MainActor () -> Void)? = nil) {
        self.presenter = presenter ?? Self.showAlert
    }

    /// Called from `DaemonConnection.onStatus`, inside the heartbeat's
    /// request. The dialog is modal, so it shows from its own task and the
    /// heartbeat carries on.
    func observe(_ input: Input) {
        guard latch.observe(input.availability, screenRecording: input.screenRecording) else {
            return
        }
        Task { @MainActor [presenter] in presenter() }
    }

    /// Show the dialog now, from a button press.
    func present() {
        presenter()
    }

    private static func showAlert() {
        NSApp.activate(ignoringOtherApps: true)
        let alert = NSAlert()
        alert.messageText = CaptureAvailabilityCopy.dialogTitle
        alert.informativeText = CaptureAvailabilityCopy.dialogBody
        alert.addButton(withTitle: CaptureAvailabilityCopy.dialogOpenButton)
        alert.addButton(withTitle: CaptureAvailabilityCopy.dialogNotNowButton)
        if alert.runModal() == .alertFirstButtonReturn {
            ScreenRecordingPrompt.openSettings(
                request: { _ = CGRequestScreenCaptureAccess() },
                open: { NSWorkspace.shared.open($0) })
        }
    }
}
