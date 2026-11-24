import Foundation
import Testing
@testable import ChronicleUI

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
        #expect(await waitUntil { spawns.count == 1 })
        // read() reaches its await without suspending first, so once this
        // flag is seen the second call is already waiting on the first read.
        let secondStarted = StartSignal()
        let second = Task {
            secondStarted.started = true
            return await reader.read()
        }
        #expect(await waitUntil { secondStarted.started })
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
