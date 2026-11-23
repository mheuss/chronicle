import Testing
import Foundation
@testable import ChronicleUI

@Suite("Screen Recording prompt")
struct ScreenRecordingPromptTests {
    /// In this sequence, disconnected is `notRunning`. Connected with no
    /// Status yet is `starting`.
    @Test func latchShowsOncePerLaunch() {
        var latch = ScreenRecordingPromptLatch()
        let first = latch.observe(.needsScreenRecording, screenRecording: .denied)
        #expect(first)
        let disconnected = latch.observe(.notRunning, screenRecording: nil)
        #expect(!disconnected)
        let reconnecting = latch.observe(.starting, screenRecording: nil)
        #expect(!reconnecting)
        let afterReconnect = latch.observe(.needsScreenRecording, screenRecording: .denied)
        #expect(!afterReconnect, "a reconnect does not re-arm it")
    }

    @Test func latchReArmsOnAGrant() {
        var latch = ScreenRecordingPromptLatch()
        let first = latch.observe(.needsScreenRecording, screenRecording: .denied)
        #expect(first)
        let granted = latch.observe(.capturing, screenRecording: .authorized)
        #expect(!granted)
        let revoked = latch.observe(.needsScreenRecording, screenRecording: .denied)
        #expect(revoked, "a later revoke shows it again")
    }

    @Test func aGrantStillShowingDeniedDoesNotShow() {
        // During a grant, the read says authorized a moment before the
        // state leaves permission_denied.
        var latch = ScreenRecordingPromptLatch()
        let duringGrant = latch.observe(.needsScreenRecording, screenRecording: .authorized)
        #expect(!duringGrant)
        let later = latch.observe(.needsScreenRecording, screenRecording: .denied)
        #expect(later, "the latch stayed armed")
    }

    @Test(arguments: CaptureAvailability.allCases.filter { $0 != .needsScreenRecording })
    func onlyNeedsScreenRecordingShowsItOrDisarmsIt(a: CaptureAvailability) {
        var latch = ScreenRecordingPromptLatch()
        let shown = latch.observe(a, screenRecording: .denied)
        #expect(!shown)
        #expect(latch.armed, "\(a) leaves the latch armed")
    }

    @Test(arguments: [PermissionState?.none, .unknown, .unavailable, .notDetermined, .restricted, .denied])
    func onlyAnAuthorizedReadReArms(read: PermissionState?) {
        var latch = ScreenRecordingPromptLatch()
        let first = latch.observe(.needsScreenRecording, screenRecording: .denied)
        #expect(first)
        let notAGrant = latch.observe(.capturing, screenRecording: read)
        #expect(!notAGrant)
        #expect(!latch.armed, "\(String(describing: read)) is not a grant")
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
