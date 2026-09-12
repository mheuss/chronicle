import Darwin

/// One connection's descriptor, reader, and waiting callers.
///
/// Created `live`, leaves `live` exactly once, ends `finished`. `close()` is
/// the edge into `closing`, and the descriptor is released only once nothing
/// holds it.
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
    var finishedWaiterCountForTesting: Int { finishedWaiters.count }
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
        let cause = terminalError ?? IPCError.notConnected
        terminalError = cause
        if !readerStarted { readerExited = true }
        shutdownDescriptor()
        resumeAllWaiters(with: cause)
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
    ///
    /// A task that is ALREADY cancelled on entry survives only because nothing
    /// suspends between installing the handler and registering the
    /// continuation: `onCancel` fires synchronously, and the MainActor task it
    /// enqueues cannot run until registration has happened. Adding an `await`
    /// anywhere before `finishedWaiters[id] = cont` hangs this call forever.
    /// `alreadyCancelledWaiterReturns` is what hangs when that happens — the
    /// suite's time limit cannot convert it into a failure, so a stalled
    /// `swift test` with no results is the symptom to read it by.
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

    /// The only `shutdown` call site, so the DEBUG counter cannot drift from it.
    private func shutdownDescriptor() {
        shutdown(fd, SHUT_RDWR)
        #if DEBUG
        shutdownCountForTesting += 1
        #endif
    }

    /// No event stream yet, so there is nothing to finish.
    private func finishStream() {}

    /// No request waiters yet, so there is nothing to fail.
    private func resumeAllWaiters(with error: Error) {}

    func markReaderStarted() { readerStarted = true }
    func markReaderExited() { readerExited = true }
}
