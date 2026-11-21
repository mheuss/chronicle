import Testing
import Foundation
@testable import ChronicleUI

@Suite("Screen Recording prompt")
struct ScreenRecordingPromptTests {
    /// In this sequence, disconnected is `notRunning`. Connected with no
    /// Status yet is `starting`.
    @Test func latchShowsOncePerLaunch() {
        var latch = ScreenRecordingPromptLatch()
        let shown1 = latch.observe(.needsScreenRecording, screenRecordingAuthorized: false)
        #expect(shown1)
        let shown2 = latch.observe(.notRunning, screenRecordingAuthorized: false)
        #expect(!shown2)
        let shown3 = latch.observe(.starting, screenRecordingAuthorized: false)
        #expect(!shown3)
        let shown4 = latch.observe(.needsScreenRecording, screenRecordingAuthorized: false)
        #expect(!shown4,
                "a reconnect does not re-arm it")
    }

    @Test func latchReArmsOnAGrant() {
        var latch = ScreenRecordingPromptLatch()
        let shown5 = latch.observe(.needsScreenRecording, screenRecordingAuthorized: false)
        #expect(shown5)
        let shown6 = latch.observe(.capturing, screenRecordingAuthorized: true)
        #expect(!shown6)
        let shown7 = latch.observe(.needsScreenRecording, screenRecordingAuthorized: false)
        #expect(shown7,
                "a later revoke shows it again")
    }

    @Test func aGrantStillShowingDeniedDoesNotShow() {
        // Between a grant and capture restarting, the state can still be
        // permission_denied while the read already says authorized.
        var latch = ScreenRecordingPromptLatch()
        let shown8 = latch.observe(.needsScreenRecording, screenRecordingAuthorized: true)
        #expect(!shown8)
        let shown9 = latch.observe(.needsScreenRecording, screenRecordingAuthorized: false)
        #expect(shown9,
                "the latch stayed armed")
    }

    @Test func unverifiedNeverShows() {
        var latch = ScreenRecordingPromptLatch()
        let shown10 = latch.observe(.permissionUnverified, screenRecordingAuthorized: false)
        #expect(!shown10)
    }

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
