import Foundation
import Testing
@testable import ChronicleUI

@MainActor
final class FakeMicDaemon: MicDaemonClient {
    var log: [String] = []
    var lastStatus: StatusResponse?
    var replies: [SetMicEnabledResponse] = []
    var statuses: [StatusResponse] = []
    var setError: Error?
    var statusDelay: Duration = .zero
    var holdSet = false
    private var gate: CheckedContinuation<Void, Never>?

    func setMicEnabled(_ enabled: Bool) async throws -> SetMicEnabledResponse {
        log.append("set(\(enabled))")
        if holdSet && gate == nil { await withCheckedContinuation { gate = $0 } }
        if let setError { throw setError }
        return replies.isEmpty
            ? SetMicEnabledResponse(type: "set_mic_enabled", ok: true, state: enabled ? .on : .off, pending: false)
            : replies.removeFirst()
    }

    func requestStatus() async throws -> StatusResponse {
        log.append("status")
        if statusDelay > .zero { try? await Task.sleep(for: statusDelay) }
        if !statuses.isEmpty { lastStatus = statuses.count > 1 ? statuses.removeFirst() : statuses[0] }
        guard let lastStatus else { throw IPCError.notConnected }
        return lastStatus
    }

    func release() { gate?.resume(); gate = nil; holdSet = false }
}

actor CallCounter {
    private(set) var count = 0
    func bump() { count += 1 }
}

@MainActor
private func controller(_ daemon: FakeMicDaemon, reads: [String?] = ["authorized"],
                        granted: Bool = true, requests: CallCounter = CallCounter()) -> MicController {
    let script = HelperReads(reads)
    let mic = MicrophonePermission(
        readHelper: { await script.next() },
        requestAccess: { await requests.bump(); return granted })
    return MicController(daemon: daemon, permission: mic, pollInterval: .milliseconds(20), pollLimit: 5)
}

@MainActor
struct MicControllerTests {
    @Test func turningOnWhenAuthorizedSendsAndRefreshes() async {
        let daemon = FakeMicDaemon()
        daemon.statuses = [statusWithMic(.on)]
        let mic = controller(daemon)
        await mic.setEnabled(true)
        #expect(daemon.log == ["set(true)", "status"])
        #expect(mic.mark == .none)
    }

    @Test func turningOnWhenUndeterminedAsksFirstThenSends() async {
        let daemon = FakeMicDaemon()
        daemon.statuses = [statusWithMic(.on)]
        let requests = CallCounter()
        let mic = controller(daemon, reads: ["not_determined", "authorized"], requests: requests)
        await mic.setEnabled(true)
        #expect(await requests.count == 1)
        #expect(daemon.log == ["set(true)", "status"])
    }

    @Test func dontAllowSendsNothing() async {
        let daemon = FakeMicDaemon()
        let requests = CallCounter()
        let mic = controller(daemon, reads: ["not_determined", "denied"], granted: false, requests: requests)
        await mic.setEnabled(true)
        #expect(await requests.count == 1)
        #expect(daemon.log.isEmpty)
    }

    @Test func turningOnWhileDeniedSendsNothing() async {
        let daemon = FakeMicDaemon()
        let mic = controller(daemon, reads: ["denied"])
        await mic.setEnabled(true)
        #expect(daemon.log.isEmpty)
    }

    @Test func turningOnAfterAFailedReadSendsNothing() async {
        let daemon = FakeMicDaemon()
        let mic = controller(daemon, reads: ["authorized", nil])
        await mic.permission.refresh()
        #expect(mic.permission.status == .authorized)
        await mic.setEnabled(true)
        #expect(daemon.log.isEmpty)
    }

    @Test func turningOffSendsFalse() async {
        let daemon = FakeMicDaemon()
        daemon.statuses = [statusWithMic(.off)]
        let mic = controller(daemon, reads: ["denied"])
        await mic.setEnabled(false)
        #expect(daemon.log == ["set(false)", "status"])
    }

    @Test func rejectedReplyMarksRejected() async {
        let daemon = FakeMicDaemon()
        daemon.statuses = [statusWithMic(.off)]
        daemon.replies = [SetMicEnabledResponse(type: "set_mic_enabled", ok: false, state: .error, pending: false)]
        let mic = controller(daemon)
        await mic.setEnabled(true)
        #expect(mic.mark == .rejected)
    }

    @Test func transportErrorMarksRejected() async {
        let daemon = FakeMicDaemon()
        daemon.statuses = [statusWithMic(.off)]
        daemon.setError = IPCError.connectionClosed
        let mic = controller(daemon)
        await mic.setEnabled(true)
        #expect(mic.mark == .rejected)
        #expect(daemon.log == ["set(true)", "status"])
    }

    @Test func pendingReplyPollsUntilTheStateMatches() async {
        let daemon = FakeMicDaemon()
        daemon.replies = [SetMicEnabledResponse(type: "set_mic_enabled", ok: false, state: .off, pending: true)]
        daemon.statuses = [statusWithMic(.off), statusWithMic(.on)]
        let mic = controller(daemon)
        await mic.setEnabled(true)
        #expect(mic.mark == .none)
        #expect(daemon.log == ["set(true)", "status", "status"])
    }

    @Test func pendingReplyThatNeverMatchesIsUnconfirmed() async {
        let daemon = FakeMicDaemon()
        daemon.replies = [SetMicEnabledResponse(type: "set_mic_enabled", ok: false, state: .off, pending: true)]
        daemon.statuses = [statusWithMic(.off)]
        let mic = controller(daemon)
        await mic.setEnabled(true)
        #expect(mic.mark == .unconfirmed)
        #expect(daemon.log.first == "set(true)")
        #expect(daemon.log.filter { $0 == "status" }.count >= 2)
    }

    @Test func pendingPollStopsAtTheDeadlineEvenWhenStatusIsSlow() async {
        let daemon = FakeMicDaemon()
        daemon.replies = [SetMicEnabledResponse(type: "set_mic_enabled", ok: false, state: .off, pending: true)]
        daemon.statuses = [statusWithMic(.off)]
        // One reply takes longer than the whole 100 ms deadline.
        daemon.statusDelay = .milliseconds(150)
        let mic = controller(daemon)
        await mic.setEnabled(true)
        #expect(mic.mark == .unconfirmed)
        #expect(daemon.log == ["set(true)", "status"])
    }

    @Test(arguments: ["denied", "restricted", "not_determined"])
    func recheckTurnsOffARevokedMicExactlyOnce(read: String) async {
        let daemon = FakeMicDaemon()
        daemon.statuses = [statusWithMic(.on), statusWithMic(.off)]
        let mic = controller(daemon, reads: [read])
        await mic.recheck()
        #expect(daemon.log == ["status", "set(false)", "status"])
    }

    @Test func recheckClearsIntentWhilePaused() async {
        // Pause stops the mic but keeps the saved intent. A revoke seen during
        // the pause must clear it, or a re-grant before Resume restarts the mic.
        let daemon = FakeMicDaemon()
        daemon.statuses = [statusWithMic(.off, paused: true)]
        let mic = controller(daemon, reads: ["denied"])
        await mic.recheck()
        #expect(daemon.log == ["status", "set(false)", "status"])
    }

    @Test(arguments: ["authorized", nil] as [String?])
    func recheckLeavesTheMicAlone(read: String?) async {
        let daemon = FakeMicDaemon()
        daemon.statuses = [statusWithMic(.on)]
        let mic = controller(daemon, reads: [read])
        await mic.recheck()
        #expect(daemon.log == ["status"])
    }

    @Test func recheckNeverTurnsTheMicOn() async {
        let daemon = FakeMicDaemon()
        daemon.statuses = [statusWithMic(.permissionDenied)]
        let mic = controller(daemon)
        await mic.recheck()
        #expect(daemon.log == ["status"])
    }

    @Test func recheckClearsTheMarkWhenTheStateChanges() async {
        let daemon = FakeMicDaemon()
        daemon.statuses = [statusWithMic(.off)]
        daemon.replies = [SetMicEnabledResponse(type: "set_mic_enabled", ok: false, state: .error, pending: false)]
        let mic = controller(daemon)
        await mic.setEnabled(true)
        #expect(mic.mark == .rejected)
        daemon.statuses = [statusWithMic(.on)]
        await mic.recheck()
        #expect(mic.mark == .none)
    }

    @Test func recheckClearsTheMarkWhenPermissionChanges() async {
        // A rejected toggle, then a revoke, all with the mic off. The old
        // "Couldn't change" caption must not survive onto the next grant.
        let daemon = FakeMicDaemon()
        daemon.statuses = [statusWithMic(.off)]
        daemon.replies = [SetMicEnabledResponse(type: "set_mic_enabled", ok: false, state: .error, pending: false)]
        let mic = controller(daemon, reads: ["authorized", "denied"])
        await mic.setEnabled(true)
        #expect(mic.mark == .rejected)
        await mic.recheck()
        #expect(mic.mark == .none)
    }

    @Test func recheckYieldsToAUserToggleThatCameAfterIt() async {
        let daemon = FakeMicDaemon()
        daemon.statuses = [statusWithMic(.on)]
        daemon.holdSet = true
        let mic = controller(daemon, reads: ["denied"])
        // A held off-toggle occupies the queue. A re-check is triggered, then
        // the user toggles again before the re-check runs.
        let first = Task { await mic.setEnabled(false) }
        while daemon.log.isEmpty { await Task.yield() }
        let check = Task { await mic.recheck() }
        for _ in 0..<5 { await Task.yield() }
        let second = Task { await mic.setEnabled(false) }
        for _ in 0..<5 { await Task.yield() }
        daemon.release()
        await first.value; await check.value; await second.value
        // The re-check saw a newer user action, so it sent nothing of its own.
        #expect(daemon.log.filter { $0.hasPrefix("set") } == ["set(false)", "set(false)"])
    }

    @Test func overlappingRechecksCoalesce() async {
        let daemon = FakeMicDaemon()
        daemon.statuses = [statusWithMic(.off)]
        daemon.holdSet = true
        let mic = controller(daemon)
        let toggle = Task { await mic.setEnabled(false) }
        while daemon.log.isEmpty { await Task.yield() }
        let a = Task { await mic.recheck() }
        let b = Task { await mic.recheck() }
        for _ in 0..<5 { await Task.yield() }
        daemon.release()
        await toggle.value; await a.value; await b.value
        // One status from the toggle, one from the single coalesced re-check.
        #expect(daemon.log.filter { $0 == "status" }.count == 2)
    }
}
