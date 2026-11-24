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
        let reader = ScreenRecordingPermission(readHelper: {
            spawns.bump()
            try? await Task.sleep(for: .milliseconds(50))
            return "authorized"
        })
        async let first = reader.read()
        async let second = reader.read()
        let reads = await [first, second]
        #expect(reads == [.authorized, .authorized])
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
