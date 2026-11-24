import Testing
import Foundation
@testable import ChronicleUI

@Suite("Screen Recording prompt", .watchdog)
struct ScreenRecordingPromptTests {
    @Test(arguments: CaptureAvailability.allCases)
    func popoverRowVisibility(a: CaptureAvailability) {
        if a == .needsScreenRecording { #expect(ScreenRecordingPrompt.showsPopoverRow(a)) } else { #expect(!ScreenRecordingPrompt.showsPopoverRow(a)) }
    }

    // One row per cause: the Fix… row wins while Screen Recording is off.
    @Test(arguments: [OnboardingState?.none, .started, .done], CaptureAvailability.allCases)
    func finishSetupRowVisibility(state: OnboardingState?, a: CaptureAvailability) {
        let expected = state == .started && a != .needsScreenRecording
        let shows = ScreenRecordingPrompt.showsFinishSetupRow(state: state, availability: a)
        if expected { #expect(shows) } else { #expect(!shows) }
    }

    @Test @MainActor func openSettingsRequestsThenOpens() {
        var calls: [String] = []
        ScreenRecordingPrompt.openSettings(
            request: { calls.append("request") },
            open: { calls.append("open \($0.absoluteString)") })
        #expect(calls == [
            "request",
            "open x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture",
        ])
    }
}
