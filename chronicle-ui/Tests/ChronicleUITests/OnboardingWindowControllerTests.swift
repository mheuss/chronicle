import Foundation
import Testing
@testable import ChronicleUI

@MainActor
final class FakePresenter: OnboardingPresenter {
    var presents = 0
    var closes = 0
    weak var controller: OnboardingWindowController?

    func present(_ controller: OnboardingWindowController) {
        presents += 1
        self.controller = controller
    }

    func close() {
        closes += 1
        controller?.didClose()
    }
}

// One defaults domain, cleared before and after each test. The tests run one
// at a time so they don't share it.
@Suite(.serialized, .watchdog)
@MainActor
final class OnboardingWindowControllerTests {
    let suite = "dev.heuss.chronicle.tests.Onboarding"

    init() {
        UserDefaults().removePersistentDomain(forName: suite)
    }

    deinit {
        UserDefaults().removePersistentDomain(forName: suite)
    }

    private func make(state: OnboardingState? = nil, read: String? = "denied",
                      spawns: ReadCount = ReadCount(), held: HeldRead? = nil,
                      daemon: FakeOnboardingDaemon = FakeOnboardingDaemon())
        -> (OnboardingWindowController, FakePresenter, UserDefaults) {
        let defaults = UserDefaults(suiteName: suite)!
        if let state { defaults.set(state.rawValue, forKey: OnboardingState.key) }
        let reader = ScreenRecordingPermission(readHelper: {
            spawns.bump()
            if let held { await held.wait() }
            return read
        })
        let model = OnboardingModel(
            mic: MicController(daemon: FakeMicDaemon(),
                               permission: MicrophonePermission(readHelper: { "authorized" }, requestAccess: { true })),
            reader: reader, daemon: daemon, backgroundEnabled: { true },
            interval: .milliseconds(10))
        let presenter = FakePresenter()
        let controller = OnboardingWindowController(model: model, reader: reader, defaults: defaults,
                                                    presenter: presenter)
        return (controller, presenter, defaults)
    }

    @Test func existingInstallIsMarkedDoneQuietly() async {
        let (controller, presenter, defaults) = make(read: "authorized")
        await controller.launchCheck()
        #expect(presenter.presents == 0)
        #expect(defaults.string(forKey: OnboardingState.key) == "done")
        #expect(controller.state == .done)
    }

    @Test func firstLaunchOpensAtScreenRecording() async {
        let (controller, presenter, defaults) = make(read: "denied")
        await controller.launchCheck()
        #expect(presenter.presents == 1)
        #expect(defaults.string(forKey: OnboardingState.key) == "started")
        #expect(controller.model.focus == .screenRecording)
        controller.close()
    }

    @Test func startedWithAGrantResumesAtTheMicrophone() async {
        let (controller, presenter, _) = make(state: .started, read: "authorized")
        await controller.launchCheck()
        #expect(presenter.presents == 1)
        #expect(controller.model.focus == .microphone)
        #expect(controller.state == .started)
        controller.close()
    }

    @Test func doneOpensNothing() async {
        let (controller, presenter, _) = make(state: .done, read: "denied")
        await controller.launchCheck()
        #expect(presenter.presents == 0)
    }

    @Test func launchCheckRunsOnce() async {
        let spawns = ReadCount()
        let (controller, _, _) = make(state: .done, spawns: spawns)
        await controller.launchCheck()
        await controller.launchCheck()
        #expect(spawns.count == 1)
    }

    /// Design §1.2: a Fix… show during the read saves `started` first.
    @Test func fixDuringTheReadIsTreatedAsStarted() async {
        let held = HeldRead()
        let spawns = ReadCount()
        let (controller, presenter, defaults) = make(read: "authorized", spawns: spawns, held: held)
        let check = Task { await controller.launchCheck() }
        #expect(await waitUntil { spawns.count == 1 })
        controller.show(.screenRecording)
        #expect(defaults.string(forKey: OnboardingState.key) == "started")
        await held.release()
        await check.value
        #expect(controller.state == .started, "the late authorized read does not mark done")
        #expect(presenter.presents == 2)
        #expect(controller.model.focus == .microphone)
        controller.close()
    }

    @Test func finishSavesDoneAndCloses() async {
        let (controller, presenter, defaults) = make()
        controller.show(.screenRecording)
        controller.finish()
        #expect(defaults.string(forKey: OnboardingState.key) == "done")
        #expect(presenter.closes == 1)
        #expect(!controller.isOpen)
    }

    @Test func pollsStopOnClose() async {
        let spawns = ReadCount()
        let daemon = FakeOnboardingDaemon()
        let (controller, _, _) = make(spawns: spawns, daemon: daemon)
        controller.show(.screenRecording)
        #expect(await waitUntil { spawns.count >= 2 && daemon.requests >= 2 })
        controller.close()
        try? await Task.sleep(for: .milliseconds(30))
        let reads = spawns.count
        let requests = daemon.requests
        try? await Task.sleep(for: .milliseconds(100))
        #expect(spawns.count == reads, "no helper read after close")
        #expect(daemon.requests == requests, "no Status request after close")
    }

    @Test func closingEarlyKeepsStarted() async {
        let (controller, _, defaults) = make()
        controller.show(.screenRecording)
        controller.close()
        #expect(defaults.string(forKey: OnboardingState.key) == "started")
        #expect(!controller.isOpen)
    }

    @Test func closeWithTheKeyDoneKeepsDone() async {
        let (controller, presenter, defaults) = make(state: .done)
        controller.show(.screenRecording)
        controller.close()
        #expect(defaults.string(forKey: OnboardingState.key) == "done")
        #expect(presenter.closes == 1)
        #expect(!controller.isOpen)
    }

    @Test func showSavesStartedOnlyWhenTheKeyIsAbsent() async {
        let (controller, _, defaults) = make(state: .done)
        controller.show(.screenRecording)
        #expect(defaults.string(forKey: OnboardingState.key) == "done")
        controller.close()
    }

    @Test func aSecondShowPresentsAgainAndKeepsOneOpenWindow() async {
        let (controller, presenter, _) = make()
        controller.show(.screenRecording)
        #expect(await waitUntil { controller.model.screenRecordingRead != nil })
        controller.show(.microphone)
        #expect(presenter.presents == 2)
        #expect(controller.isOpen)
        #expect(controller.model.focus == .microphone)
        #expect(controller.model.screenRecordingRead != nil, "a show while open does not restart the model")
        controller.close()
    }

    @Test func aShowAfterCloseRestartsThePolls() async {
        let spawns = ReadCount()
        let (controller, _, _) = make(spawns: spawns)
        controller.show(.screenRecording)
        #expect(await waitUntil { spawns.count >= 1 })
        controller.close()
        let before = spawns.count
        controller.show(.screenRecording)
        #expect(await waitUntil { spawns.count > before })
        #expect(controller.isOpen)
        controller.close()
    }

    @Test func anUnknownSavedValueIsTreatedAsAbsent() async {
        let (controller, presenter, defaults) = make(read: "denied")
        defaults.set("bogus", forKey: OnboardingState.key)
        await controller.launchCheck()
        #expect(presenter.presents == 1)
        #expect(defaults.string(forKey: OnboardingState.key) == "started")
        controller.close()
    }

    // The mic consent prompt hands focus back to the app that was in front
    // (V4), so a finished Turn on microphone brings the window forward again.
    @Test func turnOnMicBringsTheOpenWindowForward() async {
        let (controller, presenter, _) = make(read: "authorized")
        controller.show(.microphone)
        await controller.turnOnMic()
        #expect(presenter.presents == 2)
        controller.close()
    }

    @Test func turnOnMicAfterCloseLeavesTheWindowClosed() async {
        let (controller, presenter, _) = make(read: "authorized")
        controller.show(.microphone)
        controller.close()
        await controller.turnOnMic()
        #expect(presenter.presents == 1)
        #expect(!controller.isOpen)
    }

    @Test(arguments: [("denied", OnboardingStep.screenRecording), ("authorized", .microphone)])
    func resumeOpensAtTheStepAFreshReadPicks(read: String, expected: OnboardingStep) async {
        let spawns = ReadCount()
        let (controller, presenter, _) = make(state: .started, read: read, spawns: spawns)
        await controller.resume()
        // The authorized row proves resume reads for itself: focus comes from
        // that read, and without it focus stays at Screen Recording.
        #expect(spawns.count >= 1)
        #expect(presenter.presents == 1)
        #expect(controller.model.focus == expected)
        controller.close()
    }
}
