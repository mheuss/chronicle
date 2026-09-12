import Testing
import Darwin
@testable import ChronicleUI

@Suite("ConnectionSession lifecycle", .timeLimit(.minutes(1)))
@MainActor
struct ConnectionSessionLifecycleTests {

    /// Builds a session over a socketpair and guarantees both descriptors are
    /// closed, including when the body throws.
    private func withSession(
        _ body: (ConnectionSession, Int32) async throws -> Void
    ) async throws {
        let pair = try SocketPairHelper.make()
        let session = ConnectionSession(fd: pair.clientFD, maxResponseSize: 64 * 1024)
        defer {
            // Correct only while no test here starts a real reader: marking
            // the reader exited is what lets close() release clientFD, and a
            // test with a thread actually parked on it would be closed out from
            // under that thread. Without the line, a test that calls
            // markReaderStarted() leaks clientFD for the run instead.
            session.markReaderExitedForTesting()
            session.close()
            session.closeIfDoneForTesting()
            Darwin.close(pair.serverFD)
        }
        try await body(session, pair.serverFD)
    }

    @Test("a new session is live")
    func newSessionIsLive() async throws {
        try await withSession { session, _ in
            #expect(session.state == .live)
        }
    }

    @Test("close with no reader started reaches finished")
    func closeWithoutReaderFinishes() async throws {
        try await withSession { session, _ in
            session.close()
            #expect(session.state == .finished)
        }
    }

    @Test("close is idempotent")
    func closeIsIdempotent() async throws {
        try await withSession { session, _ in
            session.close()
            session.close()
            session.close()
            #expect(session.state == .finished)
            #expect(session.shutdownCountForTesting == 1)
            #expect(session.descriptorCloseCountForTesting == 1)
        }
    }

    @Test("the terminal cause is the first one recorded")
    func firstTerminalCauseWins() async throws {
        try await withSession { session, _ in
            session.fail(IPCError.invalidUTF8)
            session.fail(IPCError.responseTooLarge)
            session.close()
            #expect(session.terminalError as? IPCError == IPCError.invalidUTF8)
        }
    }

    @Test("waitUntilFinished returns immediately when already finished")
    func latchedWhenAlreadyFinished() async throws {
        try await withSession { session, _ in
            session.close()
            #expect(session.state == .finished)
            await session.waitUntilFinished()
        }
    }

    @Test("waitUntilFinished returns when the session finishes later")
    func resumesOnFinish() async throws {
        try await withSession { session, _ in
            let waiter = Task { await session.waitUntilFinished() }
            try await Task.sleep(for: .milliseconds(20))
            #expect(session.finishedWaiterCountForTesting == 1,
                    "without a registered waiter this passes on the latch instead")
            session.close()
            await waiter.value
        }
    }

    @Test("waitUntilFinished returns when its task is cancelled")
    func resumesOnCancellation() async throws {
        try await withSession { session, _ in
            let waiter = Task { await session.waitUntilFinished() }
            try await Task.sleep(for: .milliseconds(20))
            waiter.cancel()
            await waiter.value
        }
    }

    @Test("a cancelled waiter is not resumed again when the session finishes")
    func cancelledWaiterIsNotDoubleResumed() async throws {
        try await withSession { session, _ in
            let waiter = Task { await session.waitUntilFinished() }
            try await Task.sleep(for: .milliseconds(20))
            waiter.cancel()
            await waiter.value
            session.close()          // a double resume traps here
            #expect(session.state == .finished)
        }
    }

    @Test("a waiter entering already cancelled still returns")
    func alreadyCancelledWaiterReturns() async throws {
        try await withSession { session, _ in
            let waiter = Task {
                // Cancellation ends the sleep, so waitUntilFinished is entered
                // on a task that is already cancelled. That ordering hangs if
                // anything ever suspends before the continuation registers.
                try? await Task.sleep(for: .seconds(60))
                await session.waitUntilFinished()
            }
            try await Task.sleep(for: .milliseconds(20))
            waiter.cancel()
            await waiter.value
        }
    }

    @Test("close while the reader is live leaves the descriptor open")
    func closeWithLiveReaderHoldsDescriptor() async throws {
        try await withSession { session, _ in
            session.markReaderStartedForTesting()
            session.close()
            #expect(session.state == .closing)
            #expect(session.descriptorCloseCountForTesting == 0,
                    "closing the fd with a thread parked on it is the AD-3 hazard")

            session.markReaderExitedForTesting()
            session.closeIfDoneForTesting()
            #expect(session.state == .finished)
            #expect(session.descriptorCloseCountForTesting == 1)
        }
    }
}

@Suite("ConnectionSession routing", .timeLimit(.minutes(1)))
@MainActor
struct ConnectionSessionRoutingTests {

    private func withStartedSession(
        _ body: (ConnectionSession, Int32) async throws -> Void
    ) async throws {
        let pair = try SocketPairHelper.make()
        let session = ConnectionSession(fd: pair.clientFD, maxResponseSize: 64 * 1024)
        session.start()
        defer {
            session.close()
            Darwin.close(pair.serverFD)
        }
        try await body(session, pair.serverFD)
    }

    /// Answer exactly one request on `fd`. Reads the request first, so the
    /// response lands after the caller has registered its waiter.
    private func respond(on fd: Int32, with response: String) -> Task<Void, Never> {
        blockingServer {
            // Only if a request arrived. See respondToOneRequest.
            guard readLineSync(from: fd) != nil else { return }
            _ = writeAll(response + "\n", to: fd)
        }
    }

    @Test("a line with type event goes to the event stream")
    func eventLineGoesToStream() async throws {
        try await withStartedSession { session, serverFD in
            _ = writeAll(#"{"type":"event","event":"transcription_changed"}"# + "\n", to: serverFD)
            var iterator = session.eventLines.makeAsyncIterator()
            let line = await iterator.next()
            #expect(line?.contains("transcription_changed") == true)
        }
    }

    @Test("an event arriving while idle does not disturb the next request")
    func idleEventDoesNotDisturbNextRequest() async throws {
        try await withStartedSession { session, serverFD in
            _ = writeAll(#"{"type":"event","event":"capture_changed"}"# + "\n", to: serverFD)
            await waitUntil { session.eventsYieldedForTesting == 1 }
            // Without this the test passes even with routing deleted: the event
            // would hit the empty-waiter drop and the send below still works.
            #expect(session.eventsYieldedForTesting == 1)

            let server = respond(on: serverFD, with: #"{"type":"status","ok":true}"#)
            let received = try await session.send("{\"type\":\"status\"}\n")
            #expect(received.contains("\"type\":\"status\""))
            _ = await server.value
        }
    }

    @Test("an event arriving mid-request does not become the response")
    func eventMidRequestDoesNotBecomeTheResponse() async throws {
        try await withStartedSession { session, serverFD in
            // Read the request, slip an event in front of the response, then answer.
            let server = blockingServer {
                guard readLineSync(from: serverFD) != nil else { return }
                _ = writeAll(#"{"type":"event","event":"capture_changed"}"# + "\n", to: serverFD)
                _ = writeAll(#"{"type":"status","ok":true}"# + "\n", to: serverFD)
            }
            let received = try await session.send("{\"type\":\"status\"}\n")
            #expect(received.contains("\"type\":\"status\""))
            #expect(!received.contains("capture_changed"))
            _ = await server.value
        }
    }

    @Test("a response with no waiting caller is dropped, not published")
    func unsolicitedResponseIsDropped() async throws {
        try await withStartedSession { session, serverFD in
            _ = writeAll(#"{"type":"status","ok":true}"# + "\n", to: serverFD)
            // The send below must not start until the reader has dropped this
            // line. Otherwise it registers a waiter first and is handed the
            // stale line as its own response.
            await waitUntil { session.droppedResponsesForTesting == 1 }
            #expect(session.droppedResponsesForTesting == 1)

            // The counter alone cannot catch this: it is incremented only in
            // the event branch, so a drop branch that published would not move
            // it. Draining after close() is what covers that — the stream is
            // finished by then, so the loop ends rather than hanging.
            #expect(session.eventsYieldedForTesting == 0)

            // And it did not become the next caller's response either.
            let server = respond(on: serverFD, with: #"{"type":"status","ok":false}"#)
            let received = try await session.send("{\"type\":\"status\"}\n")
            #expect(received.contains("\"ok\":false"))
            _ = await server.value

            session.close()
            await session.waitUntilFinished()
            var published: [String] = []
            for await line in session.eventLines { published.append(line) }
            #expect(published.isEmpty, "a dropped response must not reach the event stream")
        }
    }

    @Test("close really closes the descriptor, not just the counter")
    func closeReallyClosesTheDescriptor() async throws {
        let pair = try SocketPairHelper.make()
        defer { Darwin.close(pair.serverFD) }
        let session = ConnectionSession(fd: pair.clientFD, maxResponseSize: 64 * 1024)
        session.close()
        await session.waitUntilFinished()

        // The counter sits on the line after the syscall, so it reads 1 whether
        // or not the close happened. NFR-1 needs the descriptor itself.
        #expect(session.descriptorCloseCountForTesting == 1)
        // Both captured before asserting: `#expect` runs code of its own, which
        // can overwrite `errno` between the syscall and the read.
        let probe = fcntl(pair.clientFD, F_GETFD)
        let probeErrno = errno
        #expect(probe == -1)
        #expect(probeErrno == EBADF)
    }

    @Test("a response containing type event in a snippet goes to the caller")
    func nestedEventTextIsNotAnEvent() async throws {
        try await withStartedSession { session, serverFD in
            // An invented snippet, not captured text: a screenshot of a JSON
            // sample would produce exactly this shape.
            let body = #"{"type":"status","ok":true,"snippet":"the text \"type\":\"event\" here"}"#
            let server = respond(on: serverFD, with: body)
            let received = try await session.send("{\"type\":\"status\"}\n")
            #expect(received.contains("\"type\":\"status\""))
            _ = await server.value
        }
    }

    @Test("a line with no type key goes to the caller")
    func missingTypeKeyGoesToCaller() async throws {
        try await withStartedSession { session, serverFD in
            let server = respond(on: serverFD, with: #"{"unexpected":"shape"}"#)
            let received = try await session.send("{\"type\":\"status\"}\n")
            #expect(received.contains("unexpected"))
            _ = await server.value
        }
    }

    @Test("a line that is not JSON goes to the caller")
    func nonJSONGoesToCaller() async throws {
        try await withStartedSession { session, serverFD in
            let server = respond(on: serverFD, with: "not json at all")
            let received = try await session.send("{\"type\":\"status\"}\n")
            #expect(received == "not json at all")
            _ = await server.value
        }
    }

    @Test("a reader error reaches the waiting caller unchanged")
    func readerErrorReachesCaller() async throws {
        try await withStartedSession { session, serverFD in
            let server = blockingServer {
                guard readLineSync(from: serverFD) != nil else { return }
                _ = writeAll([0xFF, 0xFE, 0x0A], to: serverFD)
            }
            do {
                _ = try await session.send("{\"type\":\"status\"}\n")
                Issue.record("expected a throw")
            } catch {
                #expect(error as? IPCError == IPCError.invalidUTF8)
            }
            _ = await server.value
        }
    }

    @Test("the reader exiting finishes the session")
    func readerExitFinishesSession() async throws {
        let pair = try SocketPairHelper.make()
        let session = ConnectionSession(fd: pair.clientFD, maxResponseSize: 64 * 1024)
        session.start()
        Darwin.close(pair.serverFD)          // end of file
        await session.waitUntilFinished()
        #expect(session.state == .finished)
    }

    @Test("close after the reader has already exited touches nothing")
    func closeAfterReaderExitIsNoOp() async throws {
        let pair = try SocketPairHelper.make()
        let session = ConnectionSession(fd: pair.clientFD, maxResponseSize: 64 * 1024)
        session.start()
        Darwin.close(pair.serverFD)
        await session.waitUntilFinished()

        let shutdowns = session.shutdownCountForTesting
        let closes = session.descriptorCloseCountForTesting
        session.close()
        #expect(session.shutdownCountForTesting == shutdowns)
        #expect(session.descriptorCloseCountForTesting == closes)
    }

    @Test("the event stream finishes when the session does")
    func streamFinishesWithSession() async throws {
        let pair = try SocketPairHelper.make()
        let session = ConnectionSession(fd: pair.clientFD, maxResponseSize: 64 * 1024)
        session.start()
        var iterator = session.eventLines.makeAsyncIterator()
        Darwin.close(pair.serverFD)
        await session.waitUntilFinished()
        let next = await iterator.next()
        #expect(next == nil, "a finished session must end its stream")
    }
}

@Suite("ConnectionSession writes", .timeLimit(.minutes(1)))
@MainActor
struct ConnectionSessionWriteTests {

    @Test("send rejects on a session that is not live")
    func sendRejectsWhenNotLive() async throws {
        let pair = try SocketPairHelper.make()
        let session = ConnectionSession(fd: pair.clientFD, maxResponseSize: 64 * 1024)
        session.start()
        session.close()
        do {
            _ = try await session.send("{\"type\":\"status\"}\n")
            Issue.record("expected a throw")
        } catch {
            #expect(error as? IPCError == IPCError.notConnected)
        }
        await session.waitUntilFinished()
        Darwin.close(pair.serverFD)
    }

    @Test("the descriptor stays open while a write is in flight")
    func descriptorStaysOpenDuringWrite() async throws {
        let pair = try SocketPairHelper.make()
        setNoSigPipe(pair.clientFD)
        let session = ConnectionSession(fd: pair.clientFD, maxResponseSize: 64 * 1024)

        // No reader, deliberately. With one started, close() leaves
        // readerExited false and closeIfDone() returns on THAT guard whatever
        // writesInFlight says — so the write guard would never be the thing
        // under test. closeWithLiveReaderHoldsDescriptor covers the reader half.
        // 4 MB is far past net.local.stream.sendspace, 8 KB by default on
        // macOS, so the write cannot complete while the peer is never read.
        let big = String(repeating: "x", count: 4_000_000) + "\n"
        let sendTask = Task { try await session.send(big) }
        try await Task.sleep(for: .milliseconds(100))
        #expect(session.writesInFlightForTesting == 1)

        // Checked synchronously, with no suspension after close(). Darwin's
        // shutdown() aborts the blocked write at once, so the window where a
        // thread is still inside the syscall is microseconds wide, not
        // sleepable — but close() and the write task's decrement both run on
        // the MainActor and cannot interleave, so this observes it exactly.
        session.close()
        #expect(session.state == .closing, "must not finish while a write holds the fd")
        #expect(session.descriptorCloseCountForTesting == 0)

        // close() already broke the write via shutdown(); measured, a blocked
        // write returns EPIPE from shutdown alone with no peer close at all.
        // This line just returns the peer descriptor.
        Darwin.close(pair.serverFD)
        _ = try? await sendTask.value
        await session.waitUntilFinished()
        #expect(session.state == .finished)
        #expect(session.descriptorCloseCountForTesting == 1)
    }

    @Test("close fails a waiting caller rather than stranding it")
    func closeFailsAWaitingCaller() async throws {
        let pair = try SocketPairHelper.make()
        setNoSigPipe(pair.clientFD)
        let session = ConnectionSession(fd: pair.clientFD, maxResponseSize: 64 * 1024)
        session.start()

        // The peer never answers, so this caller is parked in `send` with its
        // waiter registered. Only teardown can resume it.
        let caller = Task { try await session.send("{\"type\":\"status\"}\n") }
        try await Task.sleep(for: .milliseconds(50))
        #expect(session.waiterCountForTesting == 1)

        session.close()
        do {
            _ = try await caller.value
            Issue.record("expected a throw")
        } catch {
            #expect(error as? IPCError == IPCError.notConnected)
        }
        #expect(session.waiterCountForTesting == 0)
        await session.waitUntilFinished()
        Darwin.close(pair.serverFD)
    }

    @Test("concurrent sends do not interleave on the wire")
    func concurrentSendsDoNotInterleave() async throws {
        let pair = try SocketPairHelper.make()
        setNoSigPipe(pair.clientFD)
        let session = ConnectionSession(fd: pair.clientFD, maxResponseSize: 64 * 1024)
        let serverFD = pair.serverFD

        // Each payload is far past net.local.stream.sendspace (8192 on macOS),
        // so the kernel copies it in chunks as the reader drains. Measured: a
        // blocking 100 KB write() still returns 100000 in ONE syscall, so
        // IO.write's loop never iterates — the interleaving happens below it.
        // Unserialized, two such writes alternate chunks and neither line
        // arrives homogeneous.
        let size = 100_000
        let first = Task { try await session.send(String(repeating: "x", count: size) + "\n") }
        let second = Task { try await session.send(String(repeating: "y", count: size) + "\n") }

        let reader = blockingServer { () -> [Bool] in
            var intact: [Bool] = []
            for _ in 0..<2 {
                guard let line = readLineSync(from: serverFD, maxBytes: 4 * size) else { break }
                intact.append(line.count == size && Set(line).count == 1)
            }
            return intact
        }
        #expect(await reader.value == [true, true],
                "a line came back mixed, so two write loops interleaved")

        session.close()
        _ = try? await first.value
        _ = try? await second.value
        await session.waitUntilFinished()
        Darwin.close(serverFD)
    }

    @Test("two overlapping sends both hold the descriptor")
    func overlappingSendsAreCounted() async throws {
        let pair = try SocketPairHelper.make()
        setNoSigPipe(pair.clientFD)
        let session = ConnectionSession(fd: pair.clientFD, maxResponseSize: 64 * 1024)
        session.start()

        let big = String(repeating: "y", count: 4_000_000) + "\n"
        let first = Task { try await session.send(big) }
        let second = Task { try await session.send(big) }
        try await Task.sleep(for: .milliseconds(100))
        #expect(session.writesInFlightForTesting == 2,
                "a flag instead of a count would read 1 here and close too early")

        session.close()
        Darwin.close(pair.serverFD)
        _ = try? await first.value
        _ = try? await second.value
        await session.waitUntilFinished()
        #expect(session.descriptorCloseCountForTesting == 1)
    }
}
