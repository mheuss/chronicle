import Testing
import Foundation
import Darwin
@testable import ChronicleUI

@Suite("DaemonConnection IPC gate")
@MainActor
struct DaemonConnectionGateTests {

    @Test("testing init produces a connected DaemonConnection")
    func testingInitProducesConnected() async throws {
        let pair = try SocketPairHelper.make()
        defer {
            // serverFD owned by the test, clientFD owned by FileHandle inside conn
            Darwin.close(pair.serverFD)
        }
        let conn = DaemonConnection(testingSocketFD: pair.clientFD)
        #expect(conn.state == .connected)
    }
}
