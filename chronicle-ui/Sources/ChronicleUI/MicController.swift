import Foundation
import os

/// The daemon calls the microphone controller makes, so tests can use a fake.
@MainActor
protocol MicDaemonClient: AnyObject {
    var lastStatus: StatusResponse? { get }
    func setMicEnabled(_ enabled: Bool) async throws -> SetMicEnabledResponse
    func requestStatus() async throws -> StatusResponse
}

extension DaemonConnection: MicDaemonClient {}

/// Runs every microphone operation one at a time: user toggles, consent
/// requests and re-checks (design §2.3, §2.4).
@MainActor @Observable
final class MicController {
    private static let logger = Logger(subsystem: "dev.heuss.chronicle", category: "ui")

    let permission: MicrophonePermission
    private(set) var mark: MicOperationMark = .none
    private(set) var isBusy = false

    @ObservationIgnored private let daemon: MicDaemonClient
    @ObservationIgnored private let pollInterval: Duration
    @ObservationIgnored private let pollLimit: Int
    @ObservationIgnored private var generation = 0
    @ObservationIgnored private var tail: Task<Void, Never>?
    @ObservationIgnored private var recheckQueued = false
    @ObservationIgnored private var markedMicState: MicState?
    @ObservationIgnored private var markedPermission: MicPermission?

    init(daemon: MicDaemonClient,
         permission: MicrophonePermission = MicrophonePermission(),
         pollInterval: Duration = .seconds(1),
         pollLimit: Int = 5) {
        self.daemon = daemon
        self.permission = permission
        self.pollInterval = pollInterval
        self.pollLimit = pollLimit
    }

    /// The user flipped the toggle.
    func setEnabled(_ on: Bool) async {
        generation += 1
        await enqueue { await self.runToggle(on) }
    }

    /// Settings opened or the app became active. Triggers that arrive while
    /// one re-check is still waiting merge into it.
    func recheck() async {
        guard !recheckQueued else { return }
        recheckQueued = true
        let triggeredAt = generation
        await enqueue {
            self.recheckQueued = false
            await self.runRecheck(triggeredAt: triggeredAt)
        }
    }

    /// Operations must not call this, or they wait on themselves.
    private func enqueue(_ operation: @escaping @MainActor () async -> Void) async {
        let previous = tail
        let task = Task { @MainActor in
            await previous?.value
            self.isBusy = true
            await operation()
            self.isBusy = false
        }
        tail = task
        await task.value
    }

    private func runToggle(_ on: Bool) async {
        mark = .none
        if on {
            await permission.refresh()
            if permission.status == .notDetermined {
                let granted = await permission.request()
                await permission.refresh()
                // After Don't Allow the intent stays off (design §2.3 step 1).
                guard granted else { return }
            }
            guard permission.status == .authorized else { return }
        }
        let (outcome, seen) = await send(on)
        if let outcome { setMark(outcome, micState: seen) }
    }

    private func runRecheck(triggeredAt: Int) async {
        await permission.refresh()
        let micState = (try? await daemon.requestStatus())?.data.audio?.micState
        clearStaleMark(micState: micState)
        // A user toggle after the trigger wins over this re-check.
        guard generation == triggeredAt else { return }
        // Only a permission event turns the mic off. An unavailable read is
        // not one (design Architectural Decision 11). A re-check never turns
        // the mic on (Architectural Decision 10).
        // The saved intent can be on while the mic is off, during a pause,
        // so a revoke clears it whatever the mic state is.
        let revoked: Set<MicPermission> = [.denied, .restricted, .notDetermined]
        // Its send sets no mark: the user did not ask for this change.
        if let status = permission.status, revoked.contains(status) {
            _ = await send(false)
        }
    }

    /// `micState` is the state this operation itself last read. The shared
    /// `lastStatus` is not used: the heartbeat can update it after that read.
    private func setMark(_ outcome: MicOperationMark, micState: MicState?) {
        mark = outcome
        markedMicState = micState
        markedPermission = permission.status
    }

    /// The mark describes the last toggle. It goes stale once the mic state
    /// or the permission read differs from when it was set. A failed read of
    /// either kind is no news, so it keeps the mark (design §2.3 step 7).
    private func clearStaleMark(micState: MicState?) {
        guard mark != .none else { return }
        let stateMoved = micState != nil && micState != markedMicState
        let permissionMoved = markedPermission != nil && permission.status != .unavailable
            && permission.status != markedPermission
        if stateMoved || permissionMoved { mark = .none }
    }

    /// Returns the mark the outcome earns, or `nil` when it earns none, and
    /// the mic state this send last read.
    private func send(_ on: Bool) async -> (MicOperationMark?, MicState?) {
        var outcome: MicOperationMark?
        do {
            let reply = try await daemon.setMicEnabled(on)
            if reply.pending == true {
                let (matched, seen) = await poll(until: on ? .on : .off)
                return (matched ? nil : .unconfirmed, seen)
            }
            if !reply.ok { outcome = .rejected }
        } catch {
            Self.logger.error("set_mic_enabled failed: \(error.localizedDescription, privacy: .public)")
            outcome = .rejected
        }
        let seen = (try? await daemon.requestStatus())?.data.audio?.micState
        return (outcome, seen)
    }

    /// Poll until the mic state matches, for at most `pollLimit` intervals of
    /// wall time, counting the time spent waiting for each status reply. The
    /// last read lands at the deadline. Returns whether it matched, and the
    /// last state read.
    private func poll(until want: MicState) async -> (Bool, MicState?) {
        let clock = ContinuousClock()
        let deadline = clock.now.advanced(by: pollInterval * pollLimit)
        var seen: MicState?
        while true {
            if let state = (try? await daemon.requestStatus())?.data.audio?.micState {
                seen = state
                if state == want { return (true, seen) }
            }
            let left = clock.now.duration(to: deadline)
            guard left > .zero else { return (false, seen) }
            try? await Task.sleep(for: min(pollInterval, left))
        }
    }
}
