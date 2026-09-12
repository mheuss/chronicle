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
            // No test here starts a real reader, so nothing is parked on
            // clientFD and marking the reader exited is accurate. Without it a
            // test that calls markReaderStarted() leaks clientFD for the run.
            session.markReaderExited()
            session.close()
            session.closeIfDone()
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
            session.markReaderStarted()
            session.close()
            #expect(session.state == .closing)
            #expect(session.descriptorCloseCountForTesting == 0,
                    "closing the fd with a thread parked on it is the AD-3 hazard")

            session.markReaderExited()
            session.closeIfDone()
            #expect(session.state == .finished)
            #expect(session.descriptorCloseCountForTesting == 1)
        }
    }
}
