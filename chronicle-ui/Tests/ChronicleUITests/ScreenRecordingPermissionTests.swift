import Foundation
import Synchronization
import Testing
@testable import ChronicleUI

/// Counts calls from any isolation.
final class ReadCount: Sendable {
    private let value = Mutex(0)
    func bump() { value.withLock { $0 += 1 } }
    var count: Int { value.withLock { $0 } }
}

/// Holds helper reads until `release` is called. Every waiter is released,
/// so a test that spawns two reads by mistake fails instead of hanging.
actor HeldRead {
    private var waiters: [CheckedContinuation<Void, Never>] = []
    private var released = false
    func wait() async {
        if released { return }
        await withCheckedContinuation { waiters.append($0) }
    }
    func release() {
        released = true
        waiters.forEach { $0.resume() }
        waiters = []
    }
}

@MainActor final class Flag { var on = false }

/// Polls `condition` on the main actor until it holds or `timeout` passes.
@MainActor
func pollUntil(_ timeout: Duration = .seconds(2), _ condition: () -> Bool) async -> Bool {
    let deadline = ContinuousClock.now.advanced(by: timeout)
    while ContinuousClock.now < deadline {
        if condition() { return true }
        try? await Task.sleep(for: .milliseconds(5))
    }
    return condition()
}

@Suite(.watchdog)
@MainActor
struct ScreenRecordingPermissionTests {
    @Test(arguments: [
        ("authorized\n", HelperPermission.authorized),
        ("denied", .denied),
        ("granted", .unavailable),
    ])
    func readParsesTheHelperOutput(output: String, expected: HelperPermission) async {
        let reader = ScreenRecordingPermission(readHelper: { output })
        #expect(await reader.read() == expected)
        #expect(reader.last == expected)
    }

    @Test func helperCommandAndDeadline() {
        #expect(ScreenRecordingPermission.helperArguments == ["--permission-status", "screen-recording"])
        #expect(ScreenRecordingPermission.helperTimeout == 1)
    }

    @Test func noOutputIsUnavailable() async {
        let reader = ScreenRecordingPermission(readHelper: { nil })
        #expect(await reader.read() == .unavailable)
    }

    @Test func concurrentReadsSpawnOneHelper() async {
        let spawns = ReadCount()
        let held = HeldRead()
        let reader = ScreenRecordingPermission(readHelper: {
            spawns.bump()
            await held.wait()
            return "authorized"
        })
        let first = Task { await reader.read() }
        #expect(await pollUntil { spawns.count == 1 })
        // read() reaches its await without suspending first, so once this
        // flag is seen the second call is already waiting on the first read.
        let secondStarted = Flag()
        let second = Task {
            secondStarted.on = true
            return await reader.read()
        }
        #expect(await pollUntil { secondStarted.on })
        await held.release()
        #expect(await [first.value, second.value] == [.authorized, .authorized])
        #expect(spawns.count == 1)
    }

    @Test func warnsOnlyWhenTheReadChangesToUnavailable() async {
        let reads = HelperReads([nil, nil, "denied", nil])
        let reader = ScreenRecordingPermission(readHelper: { await reads.next() })
        _ = await reader.read()
        _ = await reader.read()
        #expect(reader.unavailableWarnings == 1)
        _ = await reader.read()
        _ = await reader.read()
        #expect(reader.unavailableWarnings == 2)
    }
}
