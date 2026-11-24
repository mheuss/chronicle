import Foundation
import Observation

/// The daemon calls onboarding makes, so tests can use a fake.
@MainActor
protocol OnboardingDaemon: AnyObject {
    var isConnected: Bool { get }
    func requestStatus() async throws -> StatusResponse
}

extension DaemonConnection: OnboardingDaemon {
    /// A connection whose replies are overdue counts as down, as it does for
    /// `currentStatus`.
    var isConnected: Bool { state == .connected && !replyOverdue }
}

/// The onboarding window's state (CHR-94 design §3). stop() keeps Skip, a
/// Turn on outcome and the focused step, so they survive a close and reopen.
@MainActor @Observable
final class OnboardingModel {
    private(set) var screenRecordingRead: HelperPermission?
    private var lastStatus: StatusResponse?
    /// Hidden while the connection is down or overdue, even when a request
    /// is still stalled.
    var status: StatusResponse? { daemon.isConnected ? lastStatus : nil }
    private(set) var micSkipped = false
    /// The mic controller's send count when Turn on microphone succeeded.
    /// A later send from anywhere makes the success out of date.
    private var micTurnOnSendCount: Int?
    private(set) var turningOn = false
    var focus: OnboardingStep = .screenRecording

    let mic: MicController
    @ObservationIgnored private let reader: ScreenRecordingPermission
    @ObservationIgnored private let daemon: OnboardingDaemon
    @ObservationIgnored private let backgroundEnabled: @MainActor () -> Bool
    @ObservationIgnored private let interval: Duration
    @ObservationIgnored private var helperLoop: Task<Void, Never>?
    @ObservationIgnored private var statusLoop: Task<Void, Never>?
    /// The last helper loop stop() cancelled. Its read may still be running.
    @ObservationIgnored private var retiredHelperLoop: Task<Void, Never>?
    /// Kept across opens, so a reopen never queues behind a stalled request.
    @ObservationIgnored private var statusInFlight = false
    /// Bumped by stop(). A result from an older generation is dropped.
    @ObservationIgnored private var generation = 0

    init(mic: MicController, reader: ScreenRecordingPermission, daemon: OnboardingDaemon,
         backgroundEnabled: @escaping @MainActor () -> Bool, interval: Duration = .seconds(1)) {
        self.mic = mic
        self.reader = reader
        self.daemon = daemon
        self.backgroundEnabled = backgroundEnabled
        self.interval = interval
    }

    var screenRecordingStep: ScreenRecordingStepState {
        .from(read: screenRecordingRead, captureRunning: status?.data.capture?.state == "running")
    }

    var micStep: MicStepState {
        .from(.init(
            screenRecordingDone: screenRecordingStep.isDone,
            permission: mic.permission.status,
            micState: status?.data.audio?.micState,
            skipped: micSkipped,
            turnOnSucceeded: micTurnOnSendCount == mic.sendCount,
            busy: mic.isBusy,
            turningOn: turningOn,
            connected: daemon.isConnected,
            backgroundEnabled: backgroundEnabled()))
    }

    /// A running microphone operation blocks Done in every row (design §3.3).
    var canFinish: Bool {
        !(mic.isBusy || turningOn)
            && OnboardingProgress.canFinish(screenRecording: screenRecordingRead, mic: micStep)
    }

    /// `seed` is a read the caller just took, so the window opens on it.
    func start(seed: HelperPermission? = nil) {
        stop()
        screenRecordingRead = seed
        let previous = retiredHelperLoop
        helperLoop = Task { [weak self] in
            // The old loop drops the read it started before stop(). Waiting
            // for it keeps this loop from joining that read and storing it.
            await previous?.value
            while !Task.isCancelled {
                guard let self else { return }
                await self.refreshScreenRecording()
                try? await Task.sleep(for: self.interval)
            }
        }
        statusLoop = Task { [weak self] in
            while !Task.isCancelled {
                guard let self else { return }
                await self.refreshStatus()
                try? await Task.sleep(for: self.interval)
            }
        }
        Task { await mic.recheck() }
    }

    /// Clears both reads, so a reopen shows nothing old. A helper read still
    /// running finishes within its 1 s timeout, and its result is dropped.
    func stop() {
        generation += 1
        screenRecordingRead = nil
        lastStatus = nil
        helperLoop?.cancel()
        statusLoop?.cancel()
        retiredHelperLoop = helperLoop ?? retiredHelperLoop
        helperLoop = nil
        statusLoop = nil
    }

    func refreshScreenRecording() async {
        let started = generation
        let read = await reader.read()
        if started == generation { screenRecordingRead = read }
    }

    /// A disconnect or a failed request clears the last Status, so "Capture
    /// started" goes away during a daemon restart (NFR5).
    func refreshStatus() async {
        guard daemon.isConnected else {
            lastStatus = nil
            return
        }
        guard !statusInFlight else { return }
        statusInFlight = true
        let started = generation
        let reply = try? await daemon.requestStatus()
        statusInFlight = false
        if started == generation { lastStatus = reply }
    }

    func turnOnMic() async {
        guard micStep == .ready else { return }
        turningOn = true
        await mic.setEnabled(true)
        let succeeded = mic.mark == .none && mic.permission.status == .authorized
        micTurnOnSendCount = succeeded ? mic.sendCount : nil
        turningOn = false
    }

    func skipMic() {
        switch micStep {
        case .ready, .noDaemon: micSkipped = true
        default: break
        }
    }
}
