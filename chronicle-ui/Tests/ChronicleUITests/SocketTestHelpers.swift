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
