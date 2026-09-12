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

        // Holds the first response so the checkpoint below lands while a
        // request is definitely outstanding; without it both can finish first
        // and the assertion never observes the window. A semaphore because
        // `blockingServer`'s body runs off any task and cannot await.
        let gate = DispatchSemaphore(value: 0)
        // A throw below must not leave the server parked on an untimed wait.
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

        // `waitUntil` for the positive half — a count of 1 means nothing until
        // the first waiter exists. The settle for the negative half: only
        // elapsed time gives a broken requestQueue the chance to register.
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
        // Nothing on this path sets SO_NOSIGPIPE. Without it, a cancelled run
        // releases the factory into a connection whose peer the next defer has
        // closed, and the heartbeat write kills the test process.
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

        // Read the peer, never the client fd: a closed fd number goes straight
        // back to the next socketpair in this parallel suite, so a probe on it
        // can succeed against another test's socket. EOF here is exact —
        // dropping the session instead leaves this read to expire on
        // SO_RCVTIMEO and return -1.
        let peer = blockingServer { () -> Int in
            var byte: UInt8 = 0
            return Darwin.read(serverFD, &byte, 1)
        }
        #expect(await peer.value == 0, "the descriptor was left open after disconnect")

        // BR-14's other half — that no reader ran — is not asserted: a started
        // reader reaches the same EOF, so the two are indistinguishable here.
    }

    @Test("an idle end of file reconnects without waiting for the heartbeat")
    func idleEOFReconnectsPromptly() async throws {
        let log = FactoryLog()
        let conn = DaemonConnection(connectionFactory: {
            let fd = try log.make()
            // Nothing on this path sets SO_NOSIGPIPE; a write to a closed peer
            // would kill the test process rather than fail a test.
            #expect(setNoSigPipe(fd))
            // Answer the first heartbeat so monitorConnection enters its 30s
            // sleep. Closing the peer before this would test a blocked
            // request instead, which is a different path.
            let serverFD = log.pairs.last!.serverFD
            // Decided here on the MainActor: the closure below is Sendable and
            // cannot read `log`.
            let serverOwnsFD = log.count > 1
            _ = blockingServer {
                // Pair 0's descriptor stays with the test, which closes it
                // below to force the EOF. Every later pair is the server's, so
                // the test never frees a number a parked read still holds.
                defer { if serverOwnsFD { Darwin.close(serverFD) } }
                guard readLineSync(from: serverFD) != nil else { return }
                writeAll(#"{"type":"status","ok":true,"data":{"uptime_secs":1,"version":"0.0.1"}}"# + "\n", to: serverFD)
                _ = readLineSync(from: serverFD)
            }
            return fd
        })
        conn.connect()
        // `lastStatus`, not `log.count`: it proves the round trip completed, so
        // monitorConnection is in its 30s sleep and pair 0's server is past its
        // read — the close below cannot free a number out from under one.
        await waitUntil { conn.lastStatus != nil }
        #expect(conn.lastStatus != nil, "the heartbeat never completed; the wait below proves nothing")

        // Headroom, not a derived bound: a cycle costs about a second (the
        // backoff floor), so anything cycling a healthy connection shows up
        // here. A slower cycler still slips through. The 30 second stall this
        // task removes is a different failure, caught by the budget below.
        try await Task.sleep(for: .milliseconds(2500))
        #expect(log.count == 1, "a healthy connection must not reconnect")

        // The monitor is now asleep. Close the peer and require a prompt
        // reconnect rather than a 30 second one.
        Darwin.close(log.pairs[0].serverFD)
        await waitUntil(ticks: 300) { log.count >= 2 }
        #expect(log.count >= 2, "idle EOF did not reconnect: \(log.count) attempts")

        conn.disconnect()
    }

    @Test("a heartbeat failure on a live socket reconnects")
    func heartbeatFailureReconnects() async throws {
        let log = FactoryLog()
        let conn = DaemonConnection(connectionFactory: {
            let fd = try log.make()
            #expect(setNoSigPipe(fd))
            let serverFD = log.pairs.last!.serverFD
            // Valid JSON of the wrong shape: no `type`, so it reaches the
            // waiter, the decode fails and monitorConnection throws. The
            // second read holds the peer OPEN afterwards, which is what makes
            // this pin BR-13: closing here instead would EOF the reader, and
            // the reconnect would happen whether or not the decode failure was
            // handled at all. Holding it open also makes this the one test
            // where the monitor exits first on a live session, so `cancelAll`
            // is what frees the other child and the backoff tail is what
            // closes the session.
            _ = blockingServer {
                // The server owns serverFD, so the test cannot free the number
                // back to the next socketpair while this thread is in a read.
                defer { Darwin.close(serverFD) }
                guard readLineSync(from: serverFD) != nil else { return }
                writeAll(#"{"unexpected":"shape"}"# + "\n", to: serverFD)
                _ = readLineSync(from: serverFD)   // park, keeping the peer open
            }
            return fd
        })
        conn.connect()
        await waitUntil { conn.sessionForTesting != nil }
        let first = conn.sessionForTesting

        await waitUntil(ticks: 300) { log.count >= 2 }
        #expect(log.count >= 2, "heartbeat failure did not reconnect: \(log.count) attempts")

        // This session never EOFs on its own — its peer stays open — so
        // connect()'s backoff tail is the only thing that can close it.
        // `.finished` is reached only after Darwin.close(fd) runs, and unlike a
        // probe on the descriptor number it cannot be fooled by the kernel
        // handing that number to the next socketpair in this parallel suite.
        await waitUntil { first?.state == .finished }
        #expect(first?.state == .finished, "the previous session was never closed")

        conn.disconnect()
    }

    @Test("a cancelled reconnect does not tear down the next connection")
    func cancelledReconnectLeavesTheNextSessionAlone() async throws {
        let log = FactoryLog()
        let conn = DaemonConnection(connectionFactory: {
            let fd = try log.make()
            #expect(setNoSigPipe(fd))
            let serverFD = log.pairs.last!.serverFD
            _ = blockingServer {
                defer { Darwin.close(serverFD) }
                guard readLineSync(from: serverFD) != nil else { return }
                writeAll(#"{"type":"status","ok":true,"data":{"uptime_secs":1,"version":"0.0.1"}}"# + "\n", to: serverFD)
                _ = readLineSync(from: serverFD)   // park, keeping the peer open
            }
            return fd
        })

        conn.connect()
        await waitUntil { conn.lastStatus != nil }

        // The cancelled task's error unwinds while the second connect() is
        // establishing. Without the guard in connect()'s backoff tail it runs
        // closeSocket() on the session the SECOND attempt just published,
        // tearing down a live connection and driving a third attempt.
        conn.disconnect()
        conn.connect()
        await waitUntil { conn.sessionForTesting != nil }
        try await Task.sleep(for: .milliseconds(500))

        #expect(log.count == 2, "a cancelled task drove an extra attempt: \(log.count)")
        #expect(conn.state == .connected)

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
