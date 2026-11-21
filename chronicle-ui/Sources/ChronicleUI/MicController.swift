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
        await send(on)
    }

    private func runRecheck(triggeredAt: Int) async {
        let before = daemon.lastStatus?.data.audio?.micState
        let permissionBefore = permission.status
        await permission.refresh()
        let micState = (try? await daemon.requestStatus())?.data.audio?.micState
        if micState != before || permission.status != permissionBefore { mark = .none }
        // A user toggle after the trigger wins over this re-check.
        guard generation == triggeredAt else { return }
        // Only a permission event turns the mic off. An unavailable read is
        // not one (design Architectural Decision 11). A re-check never turns
        // the mic on (Architectural Decision 10).
        // The saved intent can be on while the mic is off, during a pause,
        // so a revoke clears it whatever the mic state is.
        let revoked: Set<MicPermission> = [.denied, .restricted, .notDetermined]
        if let status = permission.status, revoked.contains(status) {
            await send(false)
        }
    }

    private func send(_ on: Bool) async {
        do {
            let reply = try await daemon.setMicEnabled(on)
            if reply.pending == true {
                await poll(until: on ? .on : .off)
                return
            }
            if !reply.ok { mark = .rejected }
        } catch {
            Self.logger.error("set_mic_enabled failed: \(error.localizedDescription, privacy: .public)")
            mark = .rejected
        }
        _ = try? await daemon.requestStatus()
    }

    /// Poll until the mic state matches, for at most `pollLimit` intervals of
    /// wall time, counting the time spent waiting for each status reply.
    private func poll(until want: MicState) async {
        let clock = ContinuousClock()
        let deadline = clock.now.advanced(by: pollInterval * pollLimit)
        while clock.now < deadline {
            if let status = try? await daemon.requestStatus(),
               status.data.audio?.micState == want {
                return
            }
            try? await Task.sleep(for: pollInterval)
        }
        mark = .unconfirmed
    }
}
