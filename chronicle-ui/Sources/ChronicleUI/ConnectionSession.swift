import Foundation
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

    /// The reserved `type` value marking a daemon-initiated message. HEU-386
    /// ships the Rust side and the test that the two agree.
    nonisolated static let eventDiscriminator = "event"

    let eventLines: AsyncStream<String>
    private let eventContinuation: AsyncStream<String>.Continuation
    private var readerTask: Task<Void, Never>?
    private var waiters: [CheckedContinuation<String, Error>] = []

    private lazy var io = IO(fd: fd, maxResponseSize: maxResponseSize)

    #if DEBUG
    private(set) var shutdownCountForTesting = 0
    private(set) var descriptorCloseCountForTesting = 0
    private(set) var eventsYieldedForTesting = 0
    var finishedWaiterCountForTesting: Int { finishedWaiters.count }
    var waiterCountForTesting: Int { waiters.count }
    var writesInFlightForTesting: Int { writesInFlight }
    #endif

    init(fd: Int32, maxResponseSize: Int) {
        self.fd = fd
        self.maxResponseSize = maxResponseSize
        // Bounded: nothing on this branch consumes the stream, so an unbounded
        // buffer would grow for the life of the connection.
        let (stream, continuation) = AsyncStream<String>.makeStream(
            bufferingPolicy: .bufferingNewest(16)
        )
        self.eventLines = stream
        self.eventContinuation = continuation
    }

    /// Start reading.
    ///
    /// The reader ignores cancellation: it parks in a blocking `read` that
    /// cancellation cannot interrupt, and exiting early would close a
    /// descriptor a thread is still on. `close()` is the only way to stop it.
    func start() {
        guard !readerStarted, state == .live else { return }
        markReaderStarted()
        readerTask = Task { [weak self] in
            while true {
                guard let self else { return }
                do {
                    let line = try await self.io.readLine()
                    guard self.state == .live else { break }
                    self.deliver(line)
                } catch {
                    self.readerDidExit(with: error)
                    return
                }
            }
            self?.readerDidExit(with: nil)
        }
    }

    /// Send one line and wait for its response.
    ///
    /// The waiter is registered BEFORE the write, so a response cannot arrive
    /// before there is somewhere to put it. A failed write fails the whole
    /// session: `IO.write` loops until every byte is out, so a failure can
    /// leave a partial line on the wire and the daemon reading a truncated
    /// request.
    func send(_ line: String) async throws -> String {
        guard state == .live else { throw IPCError.notConnected }
        return try await withCheckedThrowingContinuation { cont in
            waiters.append(cont)
            // A count, not a flag: `send` admits concurrent callers, and a flag
            // would let the first completion clear it while a second write
            // still holds the descriptor.
            writesInFlight += 1
            Task { [weak self] in
                guard let self else { return }
                do {
                    try await self.io.write(line)
                } catch {
                    self.fail(error)
                }
                self.writesInFlight -= 1
                self.closeIfDone()
            }
        }
    }

    private func readerDidExit(with error: Error?) {
        markReaderExited()
        if state == .live {
            state = .closing
            let cause = terminalError ?? error ?? IPCError.connectionClosed
            terminalError = cause
            shutdownDescriptor()
            resumeAllWaiters(with: cause)
        }
        closeIfDone()
    }

    private func deliver(_ line: String) {
        if Self.isDaemonInitiated(line) {
            eventContinuation.yield(line)
            #if DEBUG
            eventsYieldedForTesting += 1
            #endif
            return
        }
        guard !waiters.isEmpty else {
            // Never the line itself: a response can carry an FTS5 snippet of
            // the user's screen.
            NSLog("ChronicleUI: dropping unsolicited response line")
            return
        }
        waiters.removeFirst().resume(returning: line)
    }

    /// True when the line's TOP-LEVEL `type` is the reserved event value.
    ///
    /// This must stay a structural parse. `SearchHit.snippet` carries arbitrary
    /// OCR text, so a legitimate response can contain the characters
    /// `"type":"event"` inside a string value, and a substring match would send
    /// that response to the event stream and hang its caller.
    nonisolated static func isDaemonInitiated(_ line: String) -> Bool {
        guard let data = line.data(using: .utf8),
              let object = try? JSONSerialization.jsonObject(with: data),
              let dict = object as? [String: Any],
              let type = dict["type"] as? String
        else { return false }
        return type == eventDiscriminator
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

    private func finishStream() {
        eventContinuation.finish()
    }

    private func resumeAllWaiters(with error: Error) {
        let pending = waiters
        waiters.removeAll()
        for cont in pending { cont.resume(throwing: error) }
    }

    func markReaderStarted() { readerStarted = true }
    func markReaderExited() { readerExited = true }

    /// Raw socket I/O for this session.
    private struct IO: Sendable {
        let fd: Int32
        let maxResponseSize: Int

        func write(_ string: String) async throws {
            let fd = self.fd
            try await withCheckedThrowingContinuation { (cont: CheckedContinuation<Void, Error>) in
                DispatchQueue.global().async {
                    let data = Array(string.utf8)
                    data.withUnsafeBytes { rawBuffer in
                        var offset = 0
                        while offset < rawBuffer.count {
                            let written = Darwin.write(
                                fd,
                                rawBuffer.baseAddress! + offset,
                                rawBuffer.count - offset
                            )
                            if written < 0 {
                                if errno == EINTR { continue }
                                cont.resume(throwing: IPCError.writeFailed(errno: errno))
                                return
                            }
                            if written == 0 {
                                // POSIX write should not return 0 on non-zero count for a
                                // stream socket; treat it as a broken connection rather than
                                // an infinite loop.
                                cont.resume(throwing: IPCError.writeFailed(errno: errno))
                                return
                            }
                            offset += written
                        }
                        cont.resume()
                    }
                }
            }
        }

        func readLine() async throws -> String {
            let fd = self.fd
            let maxSize = self.maxResponseSize
            return try await withCheckedThrowingContinuation { (cont: CheckedContinuation<String, Error>) in
                DispatchQueue.global().async {
                    var buffer = [UInt8]()
                    var byte: UInt8 = 0
                    while true {
                        let bytesRead = Darwin.read(fd, &byte, 1)
                        if bytesRead < 0 {
                            if errno == EINTR { continue }
                            cont.resume(throwing: IPCError.readFailed(errno: errno))
                            return
                        }
                        if bytesRead == 0 {
                            cont.resume(throwing: IPCError.connectionClosed)
                            return
                        }
                        if byte == UInt8(ascii: "\n") {
                            break
                        }
                        buffer.append(byte)
                        if buffer.count > maxSize {
                            cont.resume(throwing: IPCError.responseTooLarge)
                            return
                        }
                    }
                    guard let line = String(bytes: buffer, encoding: .utf8) else {
                        cont.resume(throwing: IPCError.invalidUTF8)
                        return
                    }
                    cont.resume(returning: line)
                }
            }
        }
    }
}
