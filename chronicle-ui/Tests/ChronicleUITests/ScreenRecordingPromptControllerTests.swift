import Testing
@testable import ChronicleUI

@Suite("Screen Recording prompt controller")
@MainActor
struct ScreenRecordingPromptControllerTests {
    final class Calls { var count = 0 }

    @MainActor final class HeldBlocks {
        var blocks: [@MainActor () -> Void] = []
        func runAll() { blocks.forEach { $0() } }
    }

    @Test func observeSchedulesTheDialogInsteadOfShowingItInline() async {
        let calls = Calls()
        let prompt = ScreenRecordingPromptController(presenter: { calls.count += 1 })
        prompt.observe(.init(availability: .needsScreenRecording, screenRecording: .denied))
        #expect(calls.count == 0, "observe returns before the dialog shows")
        await waitUntil { calls.count == 1 }
        #expect(calls.count == 1)
    }

    @Test func observeShowsOnlyThroughTheScheduler() async {
        let calls = Calls()
        let held = HeldBlocks()
        let prompt = ScreenRecordingPromptController(
            presenter: { calls.count += 1 },
            schedule: { held.blocks.append($0) })
        prompt.observe(.init(availability: .needsScreenRecording, screenRecording: .denied))
        try? await Task.sleep(for: .milliseconds(100))
        #expect(calls.count == 0, "nothing but the scheduler may run the presenter")
        #expect(held.blocks.count == 1)
        held.runAll()
        #expect(calls.count == 1)
    }

    @Test func presentShowsOnlyThroughTheScheduler() async {
        let calls = Calls()
        let held = HeldBlocks()
        let prompt = ScreenRecordingPromptController(
            presenter: { calls.count += 1 },
            schedule: { held.blocks.append($0) })
        prompt.present()
        try? await Task.sleep(for: .milliseconds(100))
        #expect(calls.count == 0, "a button press goes through the scheduler too")
        held.runAll()
        #expect(calls.count == 1)
    }

    @Test func aSecondTriggerWhileTheDialogIsOpenShowsNothing() {
        let calls = Calls()
        var prompt: ScreenRecordingPromptController!
        prompt = ScreenRecordingPromptController(
            presenter: {
                calls.count += 1
                guard calls.count == 1 else { return }
                prompt.present()
                prompt.observe(.init(availability: .capturing, screenRecording: .authorized))
                prompt.observe(.init(availability: .needsScreenRecording, screenRecording: .denied))
            },
            schedule: { $0() })
        prompt.present()
        #expect(calls.count == 1, "triggers inside an open dialog are dropped")
        prompt.present()
        #expect(calls.count == 2, "the next press after it closes shows again")
    }

    @Test func theLatchDecidesWhenToShow() {
        let calls = Calls()
        let held = HeldBlocks()
        let prompt = ScreenRecordingPromptController(
            presenter: { calls.count += 1 },
            schedule: { held.blocks.append($0) })
        prompt.observe(.init(availability: .needsScreenRecording, screenRecording: .denied))
        prompt.observe(.init(availability: .needsScreenRecording, screenRecording: .denied))
        prompt.observe(.init(availability: .capturing, screenRecording: nil))
        held.runAll()
        #expect(calls.count == 1, "once per launch")
    }
}
