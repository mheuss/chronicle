import Foundation
import Testing
@testable import ChronicleUI

@MainActor
final class FakeOnboardingDaemon: OnboardingDaemon {
    var isConnected = true
    var requests = 0
    var hold = false
    var reply: StatusResponse? = statusWithCapture(state: "running", paused: false)
    private var gate: CheckedContinuation<Void, Never>?

    func requestStatus() async throws -> StatusResponse {
        requests += 1
        if hold { await withCheckedContinuation { gate = $0 } }
        guard let reply else { throw IPCError.notConnected }
        return reply
    }

    func release() { hold = false; gate?.resume(); gate = nil }
}

@MainActor
func onboardingModel(
    daemon: FakeOnboardingDaemon = FakeOnboardingDaemon(),
    micDaemon: FakeMicDaemon = FakeMicDaemon(),
    srReads: [String?] = ["authorized"],
    micReads: [String?] = ["authorized"],
    spawns: ReadCount = ReadCount(),
    held: HeldRead? = nil,
    consents: CallCounter = CallCounter(),
    micSpawns: ReadCount = ReadCount(),
    backgroundEnabled: Bool = true
) -> OnboardingModel {
    let sr = HelperReads(srReads)
    let reader = ScreenRecordingPermission(readHelper: {
        spawns.bump()
        if let held { await held.wait() }
        return await sr.next()
    })
    let micScript = HelperReads(micReads)
    let permission = MicrophonePermission(readHelper: { micSpawns.bump(); return await micScript.next() },
                                          requestAccess: { await consents.bump(); return true })
    let mic = MicController(daemon: micDaemon, permission: permission,
                            pollInterval: .milliseconds(20), pollLimit: 50)
    return OnboardingModel(mic: mic, reader: reader, daemon: daemon,
                           backgroundEnabled: { backgroundEnabled }, interval: .milliseconds(10))
}

@Suite(.watchdog)
@MainActor
struct OnboardingModelTests {
    @Test func aStalledStatusBlocksNeitherTheHelperLoopNorAReopen() async {
        let daemon = FakeOnboardingDaemon()
        daemon.hold = true
        let spawns = ReadCount()
        let model = onboardingModel(daemon: daemon, spawns: spawns)
        model.start()
        #expect(await waitUntil { spawns.count >= 3 })
        #expect(daemon.requests == 1)
        model.stop()
        model.start()
        try? await Task.sleep(for: .milliseconds(100))
        #expect(daemon.requests == 1, "a reopen queues no second request behind the stalled one")
        model.stop()
        daemon.release()
        try? await Task.sleep(for: .milliseconds(50))
        #expect(model.status == nil, "a reply after stop() is dropped")
    }

    @Test func startRechecksTheMicrophoneOnce() async {
        let micSpawns = ReadCount()
        let model = onboardingModel(micSpawns: micSpawns)
        model.start()
        #expect(await waitUntil { micSpawns.count == 1 })
        try? await Task.sleep(for: .milliseconds(50))
        #expect(micSpawns.count == 1, "the microphone is read once per open, not polled")
        model.stop()
    }

    @Test func stopClearsTheGrantSoAReopenCannotFinishOnIt() async {
        let model = onboardingModel()
        await model.refreshScreenRecording()
        model.skipMic()
        #expect(model.canFinish)
        model.stop()
        #expect(model.screenRecordingRead == nil)
        #expect(!model.canFinish)
    }

    @Test func anOverdueConnectionHidesTheLastStatusWithoutANewRequest() async {
        let daemon = FakeOnboardingDaemon()
        let model = onboardingModel(daemon: daemon)
        await model.refreshScreenRecording()
        await model.refreshStatus()
        #expect(model.screenRecordingStep == .done(captureStarted: true))
        daemon.isConnected = false
        #expect(model.screenRecordingStep == .done(captureStarted: false))
    }

    @Test func aBusyMicBlocksDoneEvenWhenSettled() async {
        let micDaemon = FakeMicDaemon()
        micDaemon.holdSet = true
        let model = onboardingModel(micDaemon: micDaemon)
        await model.refreshScreenRecording()
        model.skipMic()
        #expect(model.canFinish)
        let toggle = Task { await model.mic.setEnabled(true) }
        #expect(await waitUntil { model.mic.isBusy })
        #expect(!model.canFinish)
        micDaemon.release()
        await toggle.value
    }

    @Test func noStatusRequestWhileDisconnected() async {
        let daemon = FakeOnboardingDaemon()
        daemon.isConnected = false
        let model = onboardingModel(daemon: daemon)
        await model.refreshStatus()
        #expect(daemon.requests == 0)
    }

    @Test func aDisconnectOrAFailedRequestClearsCaptureStarted() async {
        let daemon = FakeOnboardingDaemon()
        let model = onboardingModel(daemon: daemon)
        await model.refreshScreenRecording()
        await model.refreshStatus()
        #expect(model.screenRecordingStep == .done(captureStarted: true))
        daemon.isConnected = false
        await model.refreshStatus()
        #expect(model.screenRecordingStep == .done(captureStarted: false))
        daemon.isConnected = true
        await model.refreshStatus()
        #expect(model.screenRecordingStep == .done(captureStarted: true))
        daemon.reply = nil
        await model.refreshStatus()
        #expect(model.screenRecordingStep == .done(captureStarted: false))
    }

    @Test func statusFeedsCaptureStarted() async {
        let model = onboardingModel()
        await model.refreshScreenRecording()
        await model.refreshStatus()
        #expect(model.screenRecordingStep == .done(captureStarted: true))
    }

    @Test func aReadAfterStopIsDropped() async {
        let held = HeldRead()
        let spawns = ReadCount()
        let model = onboardingModel(spawns: spawns, held: held)
        model.start()
        #expect(await waitUntil { spawns.count == 1 })
        model.stop()
        await held.release()
        try? await Task.sleep(for: .milliseconds(50))
        #expect(model.screenRecordingRead == nil)
    }

    @Test func turnOnSettlesAsSavedOn() async {
        let micDaemon = FakeMicDaemon()
        let consents = CallCounter()
        let model = onboardingModel(micDaemon: micDaemon, consents: consents)
        await model.refreshScreenRecording()
        #expect(model.micStep == .ready)
        await model.turnOnMic()
        #expect(model.micStep == .savedOn)
        #expect(model.canFinish)
        // setEnabled(true)'s own path: one set, then its state read (BR5).
        #expect(micDaemon.log == ["set(true)", "status"])
        #expect(await consents.count == 0, "already authorized, so no consent prompt")
    }

    @Test func turnOnTwiceSendsOnce() async {
        let micDaemon = FakeMicDaemon()
        micDaemon.holdSet = true
        let model = onboardingModel(micDaemon: micDaemon)
        await model.refreshScreenRecording()
        async let first: Void = model.turnOnMic()
        async let second: Void = model.turnOnMic()
        #expect(await waitUntil { micDaemon.log.contains("set(true)") })
        #expect(model.micStep == .working)
        micDaemon.release()
        _ = await (first, second)
        #expect(micDaemon.log.filter { $0 == "set(true)" }.count == 1)
    }

    @Test func skipSettles() async {
        let model = onboardingModel()
        await model.refreshScreenRecording()
        model.skipMic()
        #expect(model.micStep == .skipped)
        #expect(model.canFinish)
    }

    @Test func skipIsIgnoredWhileLocked() async {
        let model = onboardingModel(srReads: ["denied"])
        await model.refreshScreenRecording()
        model.skipMic()
        #expect(model.micStep == .locked)
    }
}
