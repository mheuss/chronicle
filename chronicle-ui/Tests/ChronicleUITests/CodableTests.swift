import Testing
import Foundation
@testable import ChronicleUI

@Suite("IPC Protocol Codable Tests")
struct CodableTests {

    @Test("StatusResponse decodes from Rust wire format")
    func statusResponseDecodesFromRustJSON() throws {
        let json = """
        {"type":"status","ok":true,"data":{"uptime_secs":3412,"version":"0.1.0"}}
        """
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let response = try decoder.decode(StatusResponse.self, from: Data(json.utf8))
        #expect(response.type == "status")
        #expect(response.ok == true)
        #expect(response.data.uptimeSecs == 3412)
        #expect(response.data.version == "0.1.0")
    }

    @Test("StatusData decodes zero uptime")
    func statusDataDecodesZeroUptime() throws {
        let json = """
        {"type":"status","ok":true,"data":{"uptime_secs":0,"version":"0.0.1"}}
        """
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let response = try decoder.decode(StatusResponse.self, from: Data(json.utf8))
        #expect(response.data.uptimeSecs == 0)
    }

    @Test("IPCRequest encodes to expected JSON wire format")
    func ipcRequestEncodesToExpectedJSON() throws {
        let request = IPCRequest(type: "status")
        let data = try JSONEncoder().encode(request)
        let dict = try JSONSerialization.jsonObject(with: data) as! [String: String]
        #expect(dict == ["type": "status"])
    }

    @Test("StatusResponse rejects malformed JSON")
    func statusResponseRejectsMalformedJSON() {
        let json = """
        {"type":"status","ok":true}
        """
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        #expect(throws: DecodingError.self) {
            _ = try decoder.decode(StatusResponse.self, from: Data(json.utf8))
        }
    }

    @Test("IPCError provides human-readable descriptions")
    func ipcErrorDescriptions() {
        let errors: [(IPCError, String)] = [
            (.pathTooLong, "Socket path too long"),
            (.notConnected, "Not connected to daemon"),
            (.connectionClosed, "Connection closed by daemon"),
            (.encodingFailed, "Failed to encode request"),
            (.invalidUTF8, "Response contained invalid UTF-8"),
        ]
        for (error, expected) in errors {
            #expect(error.errorDescription == expected)
        }
    }

    @Test("malformedResponse description includes detail")
    func malformedResponseDescription() {
        let detail = "keyNotFound(\"data\")"
        let err = IPCError.malformedResponse(detail)
        #expect(err.errorDescription == "Malformed daemon response: keyNotFound(\"data\")")
    }

    @Test("StatusData decodes nested capture/ocr/audio/storage stats")
    func statusDataDecodesNestedStats() throws {
        let json = """
        {
          "type": "status",
          "ok": true,
          "data": {
            "uptime_secs": 120,
            "version": "0.1.0",
            "capture": {
              "state": "running",
              "active_displays": 2,
              "frames_captured": 3456,
              "frames_dropped": 12,
              "frames_processed": 3440,
              "frames_failed": 4
            },
            "ocr": {
              "enqueued": 1200,
              "dropped": 3
            },
            "audio": {
              "segments_persisted": 55
            },
            "storage": {
              "db_size_bytes": 52428800
            }
          }
        }
        """
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let response = try decoder.decode(StatusResponse.self, from: Data(json.utf8))

        #expect(response.data.uptimeSecs == 120)
        #expect(response.data.version == "0.1.0")

        let capture = try #require(response.data.capture)
        #expect(capture.state == "running")
        #expect(capture.activeDisplays == 2)
        #expect(capture.framesCaptured == 3456)
        #expect(capture.framesDropped == 12)
        #expect(capture.framesProcessed == 3440)
        #expect(capture.framesFailed == 4)

        let ocr = try #require(response.data.ocr)
        #expect(ocr.enqueued == 1200)
        #expect(ocr.dropped == 3)

        let audio = try #require(response.data.audio)
        #expect(audio.segmentsPersisted == 55)

        let storage = try #require(response.data.storage)
        #expect(storage.dbSizeBytes == 52428800)
    }

    @Test("StatusData decodes when nested stats are absent")
    func statusDataDecodesWithoutNestedStats() throws {
        let json = """
        {"type":"status","ok":true,"data":{"uptime_secs":42,"version":"0.1.0"}}
        """
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let response = try decoder.decode(StatusResponse.self, from: Data(json.utf8))
        #expect(response.data.capture == nil)
        #expect(response.data.ocr == nil)
        #expect(response.data.audio == nil)
        #expect(response.data.storage == nil)
    }
}
