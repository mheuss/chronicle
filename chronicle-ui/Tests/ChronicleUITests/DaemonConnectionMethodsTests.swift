import Testing
import Foundation
import Darwin
@testable import ChronicleUI

// MARK: - Test helpers (file-private)

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

private func respondToOneRequest(on fd: Int32, with response: String) {
    _ = readLineSync(from: fd)
    writeAll(response + "\n", to: fd)
}

@Suite("DaemonConnection new methods", .timeLimit(.minutes(1)))
@MainActor
struct DaemonConnectionMethodsTests {

    @Test("search request encodes correctly and decodes hits")
    func searchRoundTrip() async throws {
        let pair = try SocketPairHelper.make()
        let conn = DaemonConnection(testingSocketFD: pair.clientFD)
        let serverFD = pair.serverFD

        let reply = """
        {"type":"search","ok":true,"hits":[{"id":7,"source":"screen","timestamp_ms":1700000000000,"app_name":"X","app_bundle_id":null,"window_title":null,"image_path":"/x.heif","snippet":"hello","rank":-1.0}]}
        """
        let daemonTask = Task.detached {
            let reqBytes = readLineSync(from: serverFD)
            let req = reqBytes.flatMap { String(bytes: $0, encoding: .utf8) } ?? ""
            #expect(req.contains("\"type\":\"search\""))
            #expect(req.contains("\"query\":\"hello\""))
            writeAll(reply + "\n", to: serverFD)
            Darwin.close(serverFD)
        }

        let hits = try await conn.search("hello", limit: 50, offset: 0)
        _ = await daemonTask.value

        #expect(hits.count == 1)
        #expect(hits[0].id == 7)
        #expect(hits[0].snippet == "hello")
    }

    @Test("setWhisperModel sends the variant and decodes the reply")
    func setWhisperModelRoundTrip() async throws {
        let pair = try SocketPairHelper.make()
        let conn = DaemonConnection(testingSocketFD: pair.clientFD)
        let serverFD = pair.serverFD

        let reply = """
        {"type":"set_whisper_model","ok":true,"status":{"state":"downloading","variant":"small","loaded_variant":null,"error":null,"download_bytes":0,"download_total_bytes":488000000,"models":[]}}
        """
        // Two requests, not one: an accepted switch triggers a status refresh
        // so the banner and Settings update without waiting for the next poll
        // tick. Answering both keeps the refresh on the tested path instead of
        // letting `try?` quietly swallow a closed socket.
        let statusReply = """
        {"type":"status","ok":true,"data":{"uptime_secs":1,"version":"t"}}
        """
        let daemonTask = Task.detached {
            let reqBytes = readLineSync(from: serverFD)
            let req = reqBytes.flatMap { String(bytes: $0, encoding: .utf8) } ?? ""
            #expect(req.contains("\"type\":\"set_whisper_model\""))
            #expect(req.contains("\"variant\":\"small\""))
            writeAll(reply + "\n", to: serverFD)
            // Assert what the SECOND request was, not just that one arrived —
            // otherwise an implementation that re-sent set_whisper_model
            // instead of refreshing status would pass.
            let refreshBytes = readLineSync(from: serverFD)
            let refresh = refreshBytes.flatMap { String(bytes: $0, encoding: .utf8) } ?? ""
            #expect(refresh.contains("\"type\":\"status\""), "an accepted switch refreshes status")
            writeAll(statusReply + "\n", to: serverFD)
            Darwin.close(serverFD)
        }

        let response = try await conn.setWhisperModel("small")
        _ = await daemonTask.value

        // The refresh has to actually land, or the banner waits a poll tick.
        #expect(conn.lastStatus?.data.uptimeSecs == 1, "the refreshed status was stored")
        #expect(response.ok)
        #expect(response.status.state == .downloading)
        #expect(response.status.variant == "small")
        #expect(response.status.downloadTotalBytes == 488_000_000)
    }

    @Test("getScreenshot returns a hit when daemon finds the id")
    func getScreenshotFound() async throws {
        let pair = try SocketPairHelper.make()
        let conn = DaemonConnection(testingSocketFD: pair.clientFD)
        let serverFD = pair.serverFD

        let reply = """
        {"type":"get_screenshot","ok":true,"hit":{"id":42,"source":"screen","timestamp_ms":1700000001000,"app_name":"Safari","app_bundle_id":"com.apple.Safari","window_title":"Home","image_path":"/42.heif","snippet":"the quick brown fox","rank":0.0}}
        """
        let daemonTask = Task.detached {
            let reqBytes = readLineSync(from: serverFD)
            let req = reqBytes.flatMap { String(bytes: $0, encoding: .utf8) } ?? ""
            #expect(req.contains("\"type\":\"get_screenshot\""))
            #expect(req.contains("\"id\":42"))
            writeAll(reply + "\n", to: serverFD)
            Darwin.close(serverFD)
        }

        let hit = try await conn.getScreenshot(id: 42)
        _ = await daemonTask.value

        let unwrapped = try #require(hit)
        #expect(unwrapped.id == 42)
        #expect(unwrapped.appName == "Safari")
    }

    @Test("getScreenshot returns nil when daemon reports no hit")
    func getScreenshotNotFound() async throws {
        let pair = try SocketPairHelper.make()
        let conn = DaemonConnection(testingSocketFD: pair.clientFD)
        let serverFD = pair.serverFD

        let reply = #"{"type":"get_screenshot","ok":true,"hit":null}"#
        let daemonTask = Task.detached {
            respondToOneRequest(on: serverFD, with: reply)
            Darwin.close(serverFD)
        }

        let hit = try await conn.getScreenshot(id: 999)
        _ = await daemonTask.value

        #expect(hit == nil)
    }

    @Test("pauseCapture returns true and triggers a status refresh")
    func pauseCaptureRoundTrip() async throws {
        let pair = try SocketPairHelper.make()
        let conn = DaemonConnection(testingSocketFD: pair.clientFD)
        let serverFD = pair.serverFD

        let pauseReply = #"{"type":"pause_capture","ok":true,"paused":true}"#
        let statusReply = #"{"type":"status","ok":true,"data":{"uptime_secs":1,"version":"0.1.0"}}"#

        let daemonTask = Task.detached {
            // First request: pause_capture
            let reqBytes = readLineSync(from: serverFD)
            let req = reqBytes.flatMap { String(bytes: $0, encoding: .utf8) } ?? ""
            #expect(req.contains("\"type\":\"pause_capture\""))
            writeAll(pauseReply + "\n", to: serverFD)

            // Second request: automatic status refresh
            respondToOneRequest(on: serverFD, with: statusReply)
            Darwin.close(serverFD)
        }

        let paused = try await conn.pauseCapture()
        _ = await daemonTask.value

        #expect(paused == true)
    }

    @Test("resumeCapture returns false when daemon confirms capture is running")
    func resumeCaptureRoundTrip() async throws {
        let pair = try SocketPairHelper.make()
        let conn = DaemonConnection(testingSocketFD: pair.clientFD)
        let serverFD = pair.serverFD

        let resumeReply = #"{"type":"resume_capture","ok":true,"paused":false}"#
        let statusReply = #"{"type":"status","ok":true,"data":{"uptime_secs":2,"version":"0.1.0"}}"#

        let daemonTask = Task.detached {
            // First request: resume_capture
            let reqBytes = readLineSync(from: serverFD)
            let req = reqBytes.flatMap { String(bytes: $0, encoding: .utf8) } ?? ""
            #expect(req.contains("\"type\":\"resume_capture\""))
            writeAll(resumeReply + "\n", to: serverFD)

            // Second request: automatic status refresh
            respondToOneRequest(on: serverFD, with: statusReply)
            Darwin.close(serverFD)
        }

        let paused = try await conn.resumeCapture()
        _ = await daemonTask.value

        #expect(paused == false)
    }
}
