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

/// Sets `fd` to non-blocking mode. Used in the FIFO probe.
@discardableResult
private func setNonBlocking(_ fd: Int32) -> Bool {
    let flags = fcntl(fd, F_GETFL, 0)
    if flags < 0 { return false }
    return fcntl(fd, F_SETFL, flags | O_NONBLOCK) == 0
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

    @Test("sendRequest wraps decode failures in IPCError.malformedResponse")
    func sendRequestWrapsMalformedResponse() async throws {
        let pair = try SocketPairHelper.make()
        let conn = DaemonConnection(testingSocketFD: pair.clientFD)
        let serverFD = pair.serverFD

        // Valid UTF-8, valid JSON, but does NOT match StatusResponse OR
        // ErrorResponse schemas — so we exit through the catch arm.
        let badResponse = #"{"unexpected":"shape"}"#
        let serverTask = Task.detached {
            respondToOneRequest(on: serverFD, with: badResponse)
            Darwin.close(serverFD)
        }

        do {
            _ = try await conn.requestStatus()
            Issue.record("Expected throw")
        } catch IPCError.malformedResponse(let detail) {
            #expect(!detail.isEmpty)
        } catch {
            Issue.record("Expected IPCError.malformedResponse, got \(error)")
        }

        _ = await serverTask.value
    }

    @Test("sendRequest surfaces daemon-side errors as IPCError.daemonError")
    func sendRequestPropagatesDaemonError() async throws {
        let pair = try SocketPairHelper.make()
        let conn = DaemonConnection(testingSocketFD: pair.clientFD)
        let serverFD = pair.serverFD

        // Valid ErrorResponse JSON. After the Task 7 refactor, sendRequest
        // must still route this through IPCError.daemonError(message), not
        // wrap it in malformedResponse.
        let errorResponse = #"{"type":"error","ok":false,"message":"internal failure"}"#
        let serverTask = Task.detached {
            respondToOneRequest(on: serverFD, with: errorResponse)
            Darwin.close(serverFD)
        }

        do {
            _ = try await conn.requestStatus()
            Issue.record("Expected throw")
        } catch IPCError.daemonError(let message) {
            #expect(message == "internal failure")
        } catch {
            Issue.record("Expected IPCError.daemonError, got \(error)")
        }

        _ = await serverTask.value
    }

    @Test("two concurrent requestStatus calls are serialized FIFO")
    func concurrentRequestsAreSerialized() async throws {
        let pair = try SocketPairHelper.make()
        let conn = DaemonConnection(testingSocketFD: pair.clientFD)
        let serverFD = pair.serverFD

        // Server fd is non-blocking so the probe step can detect "no data"
        // immediately rather than blocking forever.
        #expect(setNonBlocking(serverFD))

        let r1 = #"{"type":"status","ok":true,"data":{"uptime_secs":1,"version":"0.0.1"}}"#
        let r2 = #"{"type":"status","ok":true,"data":{"uptime_secs":2,"version":"0.0.2"}}"#

        // Server task returns true if it observed early request-2 bytes
        // (i.e., FIFO is broken). Returns false under correct FIFO behavior.
        let serverTask: Task<Bool, Never> = Task.detached {
            // 1. Read first request line, busy-looping on EAGAIN.
            var byte: UInt8 = 0
            while true {
                let n = Darwin.read(serverFD, &byte, 1)
                if n == 1 {
                    if byte == 0x0A { break }
                } else if n < 0 && errno == EAGAIN {
                    try? await Task.sleep(for: .milliseconds(1))
                } else {
                    return false
                }
            }

            // 2. Sleep 50ms. Under broken FIFO, this is enough time for
            //    caller B's task to write its request to the socket.
            //    Under correct FIFO, caller B is suspended awaiting caller A.
            try? await Task.sleep(for: .milliseconds(50))

            // 3. Probe: are there any bytes queued from request 2?
            var probe = [UInt8](repeating: 0, count: 256)
            let bytesProbed = probe.withUnsafeMutableBufferPointer { buf in
                Darwin.read(serverFD, buf.baseAddress, buf.count)
            }
            let sawEarlySecondRequest = bytesProbed > 0

            // 4. Send response 1 (lets caller A's task return; caller B's
            //    task is now allowed to start its own write).
            writeAll(r1 + "\n", to: serverFD)

            // 5. Read second request line. (If FIFO is broken, the probe in
            //    step 3 may have already consumed all of request 2; in that
            //    case the test will fail at the assertion below regardless,
            //    so we don't need to recover those bytes.)
            if !sawEarlySecondRequest {
                while true {
                    let n = Darwin.read(serverFD, &byte, 1)
                    if n == 1 {
                        if byte == 0x0A { break }
                    } else if n < 0 && errno == EAGAIN {
                        try? await Task.sleep(for: .milliseconds(1))
                    } else {
                        break
                    }
                }
            }

            // 6. Send response 2.
            writeAll(r2 + "\n", to: serverFD)
            return sawEarlySecondRequest
        }

        async let resp1 = conn.requestStatus()
        async let resp2 = conn.requestStatus()
        let (got1, got2) = try await (resp1, resp2)
        let sawEarly = await serverTask.value

        #expect(sawEarly == false,
                "FIFO broken: request 2 bytes arrived at server before response 1 was sent")
        #expect(got1.data.uptimeSecs == 1)
        #expect(got2.data.uptimeSecs == 2)
    }

    @Test("queued request after disconnect throws notConnected")
    func queuedRequestAfterDisconnectThrowsNotConnected() async throws {
        let pair = try SocketPairHelper.make()
        let conn = DaemonConnection(testingSocketFD: pair.clientFD)
        let serverFD = pair.serverFD

        // Fake daemon never replies — first request will hang in readLine.
        // We enqueue a second request behind it, close the server peer to
        // unblock the first task's read with EOF, then disconnect, and
        // assert the second one throws notConnected (generation mismatch).

        let firstTask = Task { try await conn.requestStatus() }
        // Give the first request enough time to take the queue head and start
        // its write+read. 50ms is plenty for a local socketpair.
        try await Task.sleep(for: .milliseconds(50))

        let secondTask = Task { try await conn.requestStatus() }
        try await Task.sleep(for: .milliseconds(50))

        // Close the server peer FIRST. This sends EOF to the client, so the
        // first task's blocking Darwin.read returns 0 → IPCError.connectionClosed
        // is raised. This guarantees firstTask completes and unblocks the
        // chain wrapper that secondTask awaits on.
        Darwin.close(serverFD)

        // Now disconnect. closeSocket() bumps connectionGeneration. By the
        // time secondTask resumes from `await prev?.value`, its captured
        // myGeneration no longer matches → throws notConnected.
        conn.disconnect()

        do {
            _ = try await secondTask.value
            Issue.record("Expected throw — generation should have changed")
        } catch IPCError.notConnected {
            // expected
        } catch {
            Issue.record("Expected IPCError.notConnected, got \(error)")
        }

        // First task surfaces as connectionClosed (EOF from server close) or
        // notConnected (if disconnect won the race). We don't assert which —
        // only that it doesn't hang forever, which is now guaranteed because
        // the server peer is closed before we await.
        _ = try? await firstTask.value
    }
}
