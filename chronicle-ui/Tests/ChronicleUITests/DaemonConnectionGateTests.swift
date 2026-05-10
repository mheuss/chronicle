import Testing
import Foundation
import Darwin
@testable import ChronicleUI

// MARK: - Test helpers

/// Writes all `bytes` to `fd`, looping in case of partial writes.
@discardableResult
private func writeAll(_ bytes: [UInt8], to fd: Int32) -> Bool {
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
private func writeAll(_ string: String, to fd: Int32) -> Bool {
    writeAll(Array(string.utf8), to: fd)
}

/// Reads bytes from `fd` until LF (0x0A) or `maxBytes` is hit. Returns the
/// payload without the trailing LF. Returns nil on read error or EOF before
/// any bytes were read.
private func readLineSync(from fd: Int32, maxBytes: Int = 64 * 1024) -> [UInt8]? {
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
private func respondToOneRequest(on fd: Int32, with response: String) {
    _ = readLineSync(from: fd)
    writeAll(response + "\n", to: fd)
}

@Suite("DaemonConnection IPC gate")
@MainActor
struct DaemonConnectionGateTests {

    @Test("testing init produces a connected DaemonConnection")
    func testingInitProducesConnected() async throws {
        let pair = try SocketPairHelper.make()
        defer {
            // serverFD owned by the test, clientFD owned by FileHandle inside conn
            Darwin.close(pair.serverFD)
        }
        let conn = DaemonConnection(testingSocketFD: pair.clientFD)
        #expect(conn.state == .connected)
    }

    @Test("DaemonConnection sets SO_NOSIGPIPE on its socket")
    func socketHasNoSigPipeOption() throws {
        let pair = try SocketPairHelper.make()
        let conn = DaemonConnection(testingSocketFD: pair.clientFD)
        _ = conn  // hold the connection so the fd stays alive for getsockopt

        var noSigPipeValue: Int32 = 0
        var optLen = socklen_t(MemoryLayout<Int32>.size)
        let result = getsockopt(
            pair.clientFD,
            SOL_SOCKET,
            SO_NOSIGPIPE,
            &noSigPipeValue,
            &optLen
        )
        #expect(result == 0, "getsockopt failed with errno \(errno)")
        #expect(noSigPipeValue != 0, "SO_NOSIGPIPE should be enabled on the IPC socket")
    }

    @Test("write to closed peer fails cleanly without SIGPIPE crash")
    func brokenPipeDoesNotCrash() async throws {
        let pair = try SocketPairHelper.make()
        let conn = DaemonConnection(testingSocketFD: pair.clientFD)

        // Close the peer end so any write returns EPIPE.
        Darwin.close(pair.serverFD)

        do {
            _ = try await conn.requestStatus()
            Issue.record("Expected throw — peer closed")
        } catch is IPCError {
            // expected; the key assertion is "we got here, process is alive"
        }
    }

    @Test("readLine throws invalidUTF8 on bad bytes")
    func readLineThrowsOnInvalidUTF8() async throws {
        let pair = try SocketPairHelper.make()
        let conn = DaemonConnection(testingSocketFD: pair.clientFD)
        let serverFD = pair.serverFD

        // Fake daemon: read the request line, then write invalid UTF-8 + LF.
        // 0xFF and 0xFE are invalid as standalone UTF-8 leading bytes.
        let serverTask = Task.detached {
            _ = readLineSync(from: serverFD)
            writeAll([0xFF, 0xFE, 0x0A], to: serverFD)
            Darwin.close(serverFD)
        }

        do {
            _ = try await conn.requestStatus()
            Issue.record("Expected throw")
        } catch IPCError.invalidUTF8 {
            // expected
        } catch {
            Issue.record("Expected IPCError.invalidUTF8, got \(error)")
        }

        _ = await serverTask.value
    }

    @Test("sendRequest round-trip happy path")
    func sendRequestHappyPath() async throws {
        let pair = try SocketPairHelper.make()
        let conn = DaemonConnection(testingSocketFD: pair.clientFD)
        let serverFD = pair.serverFD

        let validResponse = #"{"type":"status","ok":true,"data":{"uptime_secs":42,"version":"0.1.0"}}"#
        let serverTask = Task.detached {
            respondToOneRequest(on: serverFD, with: validResponse)
            Darwin.close(serverFD)
        }

        let response = try await conn.requestStatus()
        #expect(response.type == "status")
        #expect(response.ok == true)
        #expect(response.data.uptimeSecs == 42)
        #expect(response.data.version == "0.1.0")

        _ = await serverTask.value
    }
}
