import Foundation
import Testing
@testable import ChronicleUI

/// Helper reads in order. The last one repeats.
actor HelperReads {
    private var reads: [String?]
    init(_ reads: [String?]) { self.reads = reads }
    func next() -> String? { reads.count > 1 ? reads.removeFirst() : reads.first ?? nil }
}

@MainActor
struct MicrophonePermissionTests {
    @Test(arguments: [
        ("authorized", MicPermission.authorized),
        ("denied\n", .denied),
        ("not_determined", .notDetermined),
        (" restricted ", .restricted),
        ("granted", .unavailable),
        ("", .unavailable),
    ])
    func parsesHelperOutput(output: String, expected: MicPermission) {
        #expect(MicPermission(helperOutput: output) == expected)
    }

    @Test func nilOutputIsUnavailable() {
        #expect(MicPermission(helperOutput: nil) == .unavailable)
    }

    @Test func statusIsNilBeforeTheFirstRead() {
        let permission = MicrophonePermission(readHelper: { "authorized" }, requestAccess: { true })
        #expect(permission.status == nil)
    }

    @Test func refreshStoresTheHelperRead() async {
        let permission = MicrophonePermission(readHelper: { "denied" }, requestAccess: { true })
        await permission.refresh()
        #expect(permission.status == .denied)
    }

    @Test func refreshFailsClosed() async {
        let permission = MicrophonePermission(readHelper: { nil }, requestAccess: { true })
        await permission.refresh()
        #expect(permission.status == .unavailable)
    }

    @Test func failedReadReplacesAnEarlierAuthorized() async {
        let reads = HelperReads(["authorized", nil])
        let permission = MicrophonePermission(readHelper: { await reads.next() }, requestAccess: { true })
        await permission.refresh()
        #expect(permission.status == .authorized)
        await permission.refresh()
        #expect(permission.status == .unavailable)
    }

    @Test func requestReturnsTheInjectedAnswer() async {
        let permission = MicrophonePermission(readHelper: { nil }, requestAccess: { false })
        let granted = await permission.request()
        #expect(!granted)
    }

    @Test func runHelperReturnsStdout() async {
        let out = await MicrophonePermission.runHelper(
            at: URL(fileURLWithPath: "/bin/echo"), arguments: ["authorized"], timeout: 1)
        #expect(out == "authorized\n")
    }

    @Test func runHelperFailsOnNonzeroExitOrMissingBinary() async {
        let failing = await MicrophonePermission.runHelper(
            at: URL(fileURLWithPath: "/usr/bin/false"), arguments: [], timeout: 1)
        #expect(failing == nil)
        let missing = await MicrophonePermission.runHelper(
            at: URL(fileURLWithPath: "/no/such/helper"), arguments: [], timeout: 1)
        #expect(missing == nil)
    }

    @Test func runHelperGivesUpAtTheTimeout() async {
        let start = Date()
        let out = await MicrophonePermission.runHelper(
            at: URL(fileURLWithPath: "/bin/sleep"), arguments: ["5"], timeout: 0.1)
        #expect(out == nil)
        #expect(Date().timeIntervalSince(start) < 2)
    }
}
