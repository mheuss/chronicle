import Foundation
import Darwin

// MARK: - Socket test helpers

/// Writes all `bytes` to `fd`, looping in case of partial writes.
@discardableResult
func writeAll(_ bytes: [UInt8], to fd: Int32) -> Bool {
    var offset = 0
    while offset < bytes.count {
        let n = bytes.withUnsafeBufferPointer { buf -> Int in
            Darwin.write(fd, buf.baseAddress!.advanced(by: offset), bytes.count - offset)
        }
        if n <= 0 { return false }
        offset += n
    }
    return true
}

@discardableResult
func writeAll(_ string: String, to fd: Int32) -> Bool {
    writeAll(Array(string.utf8), to: fd)
}

/// Reads bytes from `fd` until LF (0x0A) or `maxBytes` is hit. Returns the
/// payload without the trailing LF. Returns nil on read error or EOF before
/// any bytes were read.
func readLineSync(from fd: Int32, maxBytes: Int = 64 * 1024) -> [UInt8]? {
    var buffer: [UInt8] = []
    var byte: UInt8 = 0
    while buffer.count < maxBytes {
        let n = Darwin.read(fd, &byte, 1)
        if n <= 0 { return buffer.isEmpty ? nil : buffer }
        if byte == 0x0A { return buffer }
        buffer.append(byte)
    }
    return buffer
}

/// Reads one newline-delimited request from `fd`, then writes `response`
/// followed by LF. Closes neither side.
func respondToOneRequest(on fd: Int32, with response: String) {
    _ = readLineSync(from: fd)
    writeAll(response + "\n", to: fd)
}

/// Sets `fd` to non-blocking mode. Used in the FIFO probe.
@discardableResult
func setNonBlocking(_ fd: Int32) -> Bool {
    let flags = fcntl(fd, F_GETFL, 0)
    if flags < 0 { return false }
    return fcntl(fd, F_SETFL, flags | O_NONBLOCK) == 0
}

/// Sets `SO_NOSIGPIPE` on `fd`, so a write to a shut-down or closed peer
/// returns `EPIPE` instead of killing the test process. Production sets this in
/// `establishConnection`; a session built straight on a `socketpair` needs it
/// applied by hand.
@discardableResult
func setNoSigPipe(_ fd: Int32) -> Bool {
    var on: Int32 = 1
    return setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &on, socklen_t(MemoryLayout<Int32>.size)) == 0
}

/// Bounds every blocking `read` on `fd`, so a test whose bytes never arrive
/// fails on its assertion instead of parking a thread for the whole run.
@discardableResult
func setReceiveTimeout(_ fd: Int32, seconds: Int32) -> Bool {
    var tv = timeval(tv_sec: Int(seconds), tv_usec: 0)
    return setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, socklen_t(MemoryLayout<timeval>.size)) == 0
}

/// Runs a blocking socket-server body off the Swift concurrency cooperative
/// pool, returning a `Task` so callers can still `await` it.
///
/// `Task.detached` is the trap this exists to avoid. It parks the blocking
/// `read`/`write` on a cooperative-pool thread, and that pool is only about as
/// wide as the core count. Enough blocked servers and the concurrency runtime
/// deadlocks outright — including the MainActor work that would have unblocked
/// them. Diagnosed on HEU-724 with `sample`: every stalled thread sat in
/// `readLineSync` on `com.apple.root.default-qos.cooperative`. The dispatch
/// global pool grows instead of deadlocking, so blocking on it is safe.
func blockingServer<T: Sendable>(_ body: @escaping @Sendable () -> T) -> Task<T, Never> {
    // Detached on purpose. A plain `Task` would inherit the calling suite's
    // MainActor and not reach the dispatch hop until the MainActor next
    // yields, making the helper's correctness depend on each caller. Detached
    // costs nothing here because this body suspends immediately rather than
    // blocking — the blocking happens on the dispatch queue.
    Task.detached {
        await withCheckedContinuation { (cont: CheckedContinuation<T, Never>) in
            DispatchQueue.global().async { cont.resume(returning: body()) }
        }
    }
}

/// Polls until `condition` holds, or gives up after `ticks` 10 ms waits.
///
/// A fixed sleep is a guess about how fast the reader drains under a loaded
/// parallel suite, and it guesses wrong often enough to flake. This converts
/// that into a bounded wait, so a genuine regression still fails the caller's
/// assertion rather than passing on a lucky schedule.
@MainActor
func waitUntil(ticks: Int = 200, _ condition: @MainActor () -> Bool) async {
    var n = 0
    while !condition() && n < ticks {
        try? await Task.sleep(for: .milliseconds(10))
        n += 1
    }
}
