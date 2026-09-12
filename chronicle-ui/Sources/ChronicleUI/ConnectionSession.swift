import Foundation
import Darwin

/// One connection's descriptor, reader, and waiting callers.
///
/// Created `live`, leaves `live` exactly once, ends `finished`. Two edges reach
/// `closing`: `close()`, and the reader exiting on its own. The second is the
/// common one — a daemon restart takes it.
@MainActor
final class ConnectionSession {
    enum State { case live, closing, finished }

    let fd: Int32
    let maxResponseSize: Int

    private(set) var state: State = .live
    private(set) var terminalError: Error?

    private var readerStarted = false
    private var readerExited = false
    private var writesInFlight = 0

    private var finishedWaiters: [Int: CheckedContinuation<Void, Never>] = [:]
    private var nextWaiterID = 0

    #if DEBUG
    private(set) var shutdownCountForTesting = 0
    private(set) var descriptorCloseCountForTesting = 0
    #endif

    init(fd: Int32, maxResponseSize: Int) {
        self.fd = fd
        self.maxResponseSize = maxResponseSize
    }

    /// Tear the connection down. Wakes the reader with an end of file rather
    /// than closing the descriptor — per design AD-3 the last user closes it.
    func close() {
        guard state == .live else { return }
        state = .closing
        terminalError = terminalError ?? IPCError.notConnected
        if !readerStarted { readerExited = true }
        shutdown(fd, SHUT_RDWR)
        #if DEBUG
        shutdownCountForTesting += 1
        #endif
        resumeAllWaiters(with: terminalError!)
        closeIfDone()
    }

    /// Record a terminal cause, then tear down. The first cause wins, so a
    /// caller learns why its connection died rather than a flat `notConnected`.
    func fail(_ error: Error) {
        terminalError = terminalError ?? error
        close()
    }

    /// Wait for `finished`.
    ///
    /// Latched, so a completion landing before the caller registers is not
    /// missed. Cancellation-aware, so this can never be the unresumed
    /// continuation that hangs a task group.
    func waitUntilFinished() async {
        if state == .finished { return }
        let id = nextWaiterID
        nextWaiterID += 1
        await withTaskCancellationHandler {
            await withCheckedContinuation { cont in
                if state == .finished {
                    cont.resume()
                    return
                }
                finishedWaiters[id] = cont
            }
        } onCancel: {
            Task { @MainActor in
                // Removing by id is what makes this safe: it takes only its own
                // entry, so a later real finish cannot resume it a second time.
                if let cont = self.finishedWaiters.removeValue(forKey: id) {
                    cont.resume()
                }
            }
        }
    }

    /// Transition to `finished` once nothing holds the descriptor.
    func closeIfDone() {
        guard state == .closing, readerExited, writesInFlight == 0 else { return }
        state = .finished
        Darwin.close(fd)
        #if DEBUG
        descriptorCloseCountForTesting += 1
        #endif
        finishStream()
        let waiters = finishedWaiters
        finishedWaiters.removeAll()
        for (_, cont) in waiters { cont.resume() }
    }

    /// Task 3 replaces this with the event stream's `finish()`.
    private func finishStream() {}

    /// Task 3 gives this a body once request waiters exist.
    private func resumeAllWaiters(with error: Error) {}

    func markReaderStarted() { readerStarted = true }
    func markReaderExited() { readerExited = true }
}
