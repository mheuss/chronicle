import Testing
import Darwin
@testable import ChronicleUI

@MainActor
@Suite("Heartbeat interval", .timeLimit(.minutes(1)))
struct HeartbeatIntervalTests {
    @Test func productionConnectionPollsEveryFiveSeconds() {
        #expect(DaemonConnection().heartbeatInterval == .seconds(5))
    }

    @Test func injectedConnectionDefaultsToFiveSeconds() {
        let conn = DaemonConnection(connectionFactory: { throw CancellationError() })
        #expect(conn.heartbeatInterval == .seconds(5))
    }

    @Test func monitorSleepsForTheInterval() async throws {
        let log = FactoryLog()
        let conn = DaemonConnection(connectionFactory: {
            let fd = try log.make()
            #expect(setNoSigPipe(fd))
            let serverFD = log.pairs.last!.serverFD
            // Each reply carries its request count as the uptime, so the test
            // can read how many heartbeats arrived from `lastStatus`.
            _ = blockingServer {
                defer { Darwin.close(serverFD) }
                var requests = 0
                while readLineSync(from: serverFD) != nil {
                    requests += 1
                    let reply = #"{"type":"status","ok":true,"data":{"uptime_secs":\#(requests),"version":"0.0.1"}}"#
                    guard writeAll(reply + "\n", to: serverFD) else { return }
                }
            }
            return fd
        }, heartbeatInterval: .milliseconds(100))
        conn.connect()
        await waitUntil { (conn.lastStatus?.data.uptimeSecs ?? 0) >= 3 }
        #expect((conn.lastStatus?.data.uptimeSecs ?? 0) >= 3,
                "expected 3 heartbeats within 2 s at a 100 ms interval")
        #expect(log.count == 1, "the heartbeats should share one connection")
        conn.disconnect()
    }
}
