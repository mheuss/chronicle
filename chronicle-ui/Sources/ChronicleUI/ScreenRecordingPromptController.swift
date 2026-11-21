import AppKit
import CoreGraphics
import Observation

/// Owns the dialog for the whole app, so it shows even if the popover has
/// never opened (design §7.1).
@MainActor @Observable
final class ScreenRecordingPromptController {
    typealias Scheduler = @MainActor (@escaping @MainActor () -> Void) -> Void

    @ObservationIgnored private var latch = ScreenRecordingPromptLatch()
    @ObservationIgnored private let presenter: @MainActor () -> Void
    @ObservationIgnored private let schedule: Scheduler
    @ObservationIgnored private var isPresenting = false

    struct Input {
        let availability: CaptureAvailability
        /// The current Status's read, or nil without one.
        let screenRecording: PermissionState?
    }

    /// `presenter` shows the dialog and `schedule` decides when it runs.
    /// Tests pass stubs. The app uses the alert and the run loop below.
    init(presenter: (@MainActor () -> Void)? = nil, schedule: Scheduler? = nil) {
        self.presenter = presenter ?? Self.showAlert
        self.schedule = schedule ?? Self.onRunLoop
    }

    /// Called from `DaemonConnection.onStatus`, inside whichever
    /// `requestStatus` delivered the Status.
    func observe(_ input: Input) {
        guard latch.observe(input.availability, screenRecording: input.screenRecording) else {
            return
        }
        schedule { [weak self] in self?.showOnce() }
    }

    func present() {
        schedule { [weak self] in self?.showOnce() }
    }

    /// The modal loop runs run-loop blocks, so a second trigger would
    /// open a second dialog on top of the first.
    private func showOnce() {
        guard !isPresenting else { return }
        isPresenting = true
        defer { isPresenting = false }
        presenter()
    }

    /// The alert is modal. From a Task it would hold the main queue and stall
    /// the heartbeat and reply watchdog. A run-loop block keeps the queue
    /// draining. The common modes include the modal panel mode.
    private static func onRunLoop(_ block: @escaping @MainActor () -> Void) {
        RunLoop.main.perform(inModes: [.common]) {
            MainActor.assumeIsolated { block() }
        }
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
