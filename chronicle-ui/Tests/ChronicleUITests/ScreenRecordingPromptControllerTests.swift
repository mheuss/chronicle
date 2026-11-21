import Testing
@testable import ChronicleUI

@Suite("Screen Recording prompt controller")
@MainActor
struct ScreenRecordingPromptControllerTests {
    final class Calls { var count = 0 }

    @Test func observeSchedulesTheDialogInsteadOfShowingItInline() async {
        let calls = Calls()
        let prompt = ScreenRecordingPromptController(presenter: { calls.count += 1 })
        prompt.observe(.init(availability: .needsScreenRecording, screenRecording: .denied))
        // observe runs inside onStatus, which runs inside a requestStatus,
        // often the heartbeat's. A modal shown here would park that request.
        #expect(calls.count == 0, "observe returns before the dialog shows")
        await waitUntil { calls.count == 1 }
        #expect(calls.count == 1)
    }

    @Test func theLatchDecidesWhenToShow() async {
        let calls = Calls()
        let prompt = ScreenRecordingPromptController(presenter: { calls.count += 1 })
        prompt.observe(.init(availability: .needsScreenRecording, screenRecording: .denied))
        prompt.observe(.init(availability: .needsScreenRecording, screenRecording: .denied))
        prompt.observe(.init(availability: .capturing, screenRecording: nil))
        try? await Task.sleep(for: .milliseconds(100))
        #expect(calls.count == 1, "once per launch")
    }

    @Test func presentShowsAtOnce() {
        let calls = Calls()
        let prompt = ScreenRecordingPromptController(presenter: { calls.count += 1 })
        prompt.present()
        #expect(calls.count == 1, "a button press shows the dialog without waiting")
    }
}
