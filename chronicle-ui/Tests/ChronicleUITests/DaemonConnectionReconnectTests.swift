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
        let conn = DaemonConnection(connectionFactory: {
            let fd = try log.make()
            #expect(setNoSigPipe(fd))
            return fd
        })
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

        // The gate holds the first response so the checkpoint below lands
        // while a request is definitely outstanding. Without it, both requests
        // can finish on a local socketpair before the check runs, and the
        // assertion passes without ever having observed the window. A
        // semaphore rather than an actor: `blockingServer` runs its body on the
        // dispatch global queue, off any task, so it cannot await.
        let gate = DispatchSemaphore(value: 0)
        // A throw anywhere below releases the server thread rather than leaving
        // it parked on an untimed wait for the rest of the run.
        defer { gate.signal() }
        let reply = #"{"type":"status","ok":true,"data":{"uptime_secs":1,"version":"0.0.1"}}"# + "\n"
        let server = blockingServer {
            // The server owns serverFD. Closing it from the test instead would
            // free the number back to the next socketpair in this parallel
            // suite while this thread was still inside a read.
            defer { Darwin.close(serverFD) }
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
        // Neither `SocketPairHelper` nor `init(connectionFactory:)` sets this,
        // where `init(testingSocketFD:)` would. Without it, a run cancelled
        // before `disconnect()` below releases the factory into a live
        // connection whose peer the next defer has closed, and the heartbeat
        // write takes the whole test process down with SIGPIPE.
        #expect(setNoSigPipe(pair.clientFD))
        defer { Darwin.close(serverFD) }
        let gate = AsyncGate()
        // Releases the connect task, which a throw before `open()` below would
        // otherwise leave parked on the gate holding the client descriptor.
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

    @Test("an idle end of file reconnects without waiting for the heartbeat")
    func idleEOFReconnectsPromptly() async throws {
        let log = FactoryLog()
        let conn = DaemonConnection(connectionFactory: {
            let fd = try log.make()
            // Nothing on this path sets SO_NOSIGPIPE, and a write to a peer the
            // teardown has closed would kill the test process rather than fail
            // a test. See disconnectDuringEstablishmentClosesTheDescriptor.
            #expect(setNoSigPipe(fd))
            // Answer the first heartbeat so monitorConnection enters its 30s
            // sleep. Closing the peer before this would test a blocked
            // request instead, which is a different path.
            let serverFD = log.pairs.last!.serverFD
            _ = blockingServer {
                guard readLineSync(from: serverFD) != nil else { return }
                writeAll(#"{"type":"status","ok":true,"data":{"uptime_secs":1,"version":"0.0.1"}}"# + "\n", to: serverFD)
            }
            return fd
        })
        conn.connect()
        // `lastStatus` rather than `log.count`, and this is the load-bearing
        // wait: it proves the heartbeat round trip completed, which puts
        // monitorConnection into its 30s sleep and means pair 0's server is
        // past its read, so the close below cannot free a descriptor number
        // out from under a parked one. An implementation that never runs the
        // heartbeat at all never sets this and burns the whole budget here.
        await waitUntil { conn.lastStatus != nil }
        #expect(conn.lastStatus != nil, "the heartbeat never completed; the wait below proves nothing")

        // A reconnect costs race-return time plus the 1 second backoff floor,
        // so this rejects any implementation that cycles a healthy connection
        // in under 2.5 seconds — a 1 second timer included. It does not reject
        // an arbitrarily slow one; nothing here can. The 30 second stall this
        // task removes is caught by the budget after the close, not by this.
        try await Task.sleep(for: .milliseconds(2500))
        #expect(log.count == 1, "a healthy connection must not reconnect")

        // The monitor is now asleep. Close the peer and require a prompt
        // reconnect rather than a 30 second one.
        Darwin.close(log.pairs[0].serverFD)
        await waitUntil(ticks: 300) { log.count >= 2 }
        #expect(log.count >= 2, "idle EOF did not reconnect: \(log.count) attempts")

        conn.disconnect()
        log.closeServers(from: 1)
    }

    @Test("a heartbeat failure on a live socket reconnects")
    func heartbeatFailureReconnects() async throws {
        let log = FactoryLog()
        let conn = DaemonConnection(connectionFactory: {
            let fd = try log.make()
            #expect(setNoSigPipe(fd))
            let serverFD = log.pairs.last!.serverFD
            // Valid JSON of the wrong shape. It carries no `type`, so it
            // reaches the waiter; the decode then fails and monitorConnection
            // throws, and that throw is what returns from `group.next()`.
            // This test does not pin `cancelAll()` — measured, it passes
            // without it, because the server below closes its own descriptor
            // and that EOF finishes the session's child. What it pins is
            // BR-13: a heartbeat failure on a live socket reconnects.
            // `idleEOFReconnectsPromptly` is what pins `cancelAll()`.
            _ = blockingServer {
                // The server owns serverFD, so the test cannot free the number
                // back to the next socketpair while this thread is in a read.
                defer { Darwin.close(serverFD) }
                guard readLineSync(from: serverFD) != nil else { return }
                writeAll(#"{"unexpected":"shape"}"# + "\n", to: serverFD)
            }
            return fd
        })
        conn.connect()
        await waitUntil(ticks: 300) { log.count >= 2 }
        #expect(log.count >= 2, "heartbeat failure did not reconnect: \(log.count) attempts")

        conn.disconnect()
    }

    @Test("a reconnect leaves the previous session finished")
    func reconnectFinishesPreviousSession() async throws {
        let log = FactoryLog()
        let conn = DaemonConnection(connectionFactory: {
            let fd = try log.make()
            #expect(setNoSigPipe(fd))
            return fd
        })
        conn.connect()
        await waitUntil { conn.sessionForTesting != nil }
        let first = conn.sessionForTesting
        #expect(first != nil)

        Darwin.close(log.pairs[0].serverFD)
        await waitUntil(ticks: 300) { first?.state == .finished && conn.sessionForTesting !== first }
        #expect(first?.state == .finished)
        #expect(conn.sessionForTesting !== first)

        conn.disconnect()
        log.closeServers(from: 1)
    }
}
