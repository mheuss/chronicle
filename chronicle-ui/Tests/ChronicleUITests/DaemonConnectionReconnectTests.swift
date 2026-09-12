import Testing
import Foundation
import Darwin
@testable import ChronicleUI

/// Lets a test hold a connection attempt open.
actor AsyncGate {
    private var opened = false
    private var waiters: [CheckedContinuation<Void, Never>] = []

    func wait() async {
        if opened { return }
        await withCheckedContinuation { waiters.append($0) }
    }

    func open() {
        opened = true
        let pending = waiters
        waiters.removeAll()
        for cont in pending { cont.resume() }
    }
}

/// Records the socketpairs a test's factory hands out, on the MainActor.
@MainActor
final class FactoryLog {
    private(set) var pairs: [(clientFD: Int32, serverFD: Int32)] = []
    var count: Int { pairs.count }

    func make() throws -> Int32 {
        let pair = try SocketPairHelper.make()
        pairs.append(pair)
        return pair.clientFD
    }

    /// The client side belongs to the session that was handed it. `index`
    /// skips the pairs whose server a test already closed itself.
    func closeServers(from index: Int = 0) {
        for pair in pairs.dropFirst(index) { Darwin.close(pair.serverFD) }
    }
}

@Suite("DaemonConnection reconnect", .timeLimit(.minutes(1)))
@MainActor
struct DaemonConnectionReconnectTests {

    @Test("connect uses the injected factory")
    func connectUsesInjectedFactory() async throws {
        let log = FactoryLog()
        let conn = DaemonConnection(connectionFactory: { try log.make() })
        conn.connect()
        await waitUntil { log.count >= 1 }
        #expect(log.count >= 1)
        conn.disconnect()
        log.closeServers()
    }

    @Test("the live session never holds more than one waiter")
    func liveSessionHoldsAtMostOneWaiter() async throws {
        // AD-8's invariant is a property of the PRODUCTION path, not of
        // ConnectionSession alone: `send` admits concurrent callers, and only
        // `sendRequest`'s requestQueue chaining keeps one outstanding at a
        // time. So it is asserted here rather than in the session's own suite.
        let pair = try SocketPairHelper.make()
        let conn = DaemonConnection(testingSocketFD: pair.clientFD)
        let serverFD = pair.serverFD
        defer { Darwin.close(serverFD) }

        // The gate holds the first response so the checkpoint below lands
        // while a request is definitely outstanding. Without it, both requests
        // can finish on a local socketpair before the check runs, and the
        // assertion passes without ever having observed the window. A
        // semaphore rather than an actor: `blockingServer` runs its body on the
        // dispatch global queue, off any task, so it cannot await.
        let gate = DispatchSemaphore(value: 0)
        // Runs before the close above, so a throw releases the server thread
        // rather than parking it on an untimed wait for the rest of the run.
        defer { gate.signal() }
        let reply = #"{"type":"status","ok":true,"data":{"uptime_secs":1,"version":"0.0.1"}}"# + "\n"
        let server = blockingServer {
            guard readLineSync(from: serverFD) != nil else { return }
            gate.wait()
            writeAll(reply, to: serverFD)
            guard readLineSync(from: serverFD) != nil else { return }
            writeAll(reply, to: serverFD)
        }

        async let first = conn.requestStatus()
        async let second = conn.requestStatus()

        // Two waits, because the assertion has two halves. `waitUntil` covers
        // the positive one — the first waiter has to exist before a count of 1
        // means anything. The settle covers the negative one: a broken
        // requestQueue registers the second waiter too, and only elapsed time
        // gives it the chance to.
        await waitUntil { conn.sessionForTesting?.waiterCountForTesting == 1 }
        try await Task.sleep(for: .milliseconds(200))
        #expect(conn.sessionForTesting?.waiterCountForTesting == 1,
                "requestQueue must keep exactly one request outstanding")

        gate.signal()
        _ = try await (first, second)
        _ = await server.value

        conn.disconnect()
    }

    @Test("disconnect during establishment leaves nothing open")
    func disconnectDuringEstablishmentClosesTheDescriptor() async throws {
        let pair = try SocketPairHelper.make()
        let serverFD = pair.serverFD
        defer { Darwin.close(serverFD) }
        let gate = AsyncGate()
        // Runs before the close above. A throw before `open()` below would
        // otherwise leave the connect task parked on the gate, holding the
        // client descriptor for the rest of the run.
        defer { Task { await gate.open() } }
        let conn = DaemonConnection(connectionFactory: {
            await gate.wait()                 // hold establishment open
            return pair.clientFD
        })
        conn.connect()
        try await Task.sleep(for: .milliseconds(50))

        conn.disconnect()                     // lands mid-establishment
        await gate.open()

        #expect(conn.sessionForTesting == nil)

        // Read from the peer rather than probing the client fd directly. A
        // closed fd number is handed straight back out to the next socketpair
        // in this parallel suite, so a probe on that number can succeed against
        // some other test's socket. The peer is exact instead: `close()` on a
        // session whose reader never started shuts the descriptor down and
        // closes it in the same call, so EOF arrives; a `connect()` that
        // dropped the session on the floor would leave this read waiting out
        // SO_RCVTIMEO and return -1.
        let peer = blockingServer { () -> Int in
            var byte: UInt8 = 0
            return Darwin.read(serverFD, &byte, 1)
        }
        #expect(await peer.value == 0, "the descriptor was left open after disconnect")

        // BR-14's other half — that no reader ran — is not asserted. The
        // session `connect()` builds here is local to it, and a session whose
        // reader HAD started reaches the same EOF anyway, because `close()`
        // shuts the descriptor down and the woken reader closes it. The two
        // are indistinguishable from the peer.
    }
}
