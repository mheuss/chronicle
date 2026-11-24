import Testing
import Foundation
@testable import ChronicleUI

@Suite("Screen Recording prompt", .watchdog)
struct ScreenRecordingPromptTests {
    @Test(arguments: CaptureAvailability.allCases)
    func popoverRowVisibility(a: CaptureAvailability) {
        #expect(ScreenRecordingPrompt.showsPopoverRow(a) == (a == .needsScreenRecording))
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
