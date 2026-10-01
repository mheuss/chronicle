import Testing
@testable import ChronicleUI

@MainActor
@Suite("Heartbeat interval")
struct HeartbeatIntervalTests {
    @Test func productionConnectionPollsEveryFiveSeconds() {
        #expect(DaemonConnection().heartbeatInterval == .seconds(5))
    }

    @Test func injectedConnectionDefaultsToFiveSeconds() {
        let conn = DaemonConnection(connectionFactory: { throw CancellationError() })
        #expect(conn.heartbeatInterval == .seconds(5))
    }
}
