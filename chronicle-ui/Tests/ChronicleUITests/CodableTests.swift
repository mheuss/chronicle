import Testing
import Foundation
import Darwin
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

    @Test("readFailed description includes errno text")
    func readFailedDescription() {
        let err = IPCError.readFailed(errno: EBADF)
        #expect(err.errorDescription?.hasPrefix("Read failed: ") == true)
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
              "frames_failed": 4,
              "paused": false
            },
            "ocr": {
              "enqueued": 1200,
              "dropped": 3
            },
            "audio": {
              "segments_persisted": 55,
              "mic_state": "on"
            },
            "storage": {
              "db_size_bytes": 52428800,
              "total_disk_usage_bytes": 104857600,
              "screenshot_count": 1024,
              "audio_segment_count": 32,
              "oldest_entry_ms": null,
              "retention_days": 30
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
        #expect(audio.micState == .on)

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

    @Test("MicState decodes from wire values")
    func micStateDecodesFromWireValues() throws {
        let decoder = JSONDecoder()
        #expect(try decoder.decode(MicState.self, from: Data("\"off\"".utf8)) == .off)
        #expect(try decoder.decode(MicState.self, from: Data("\"on\"".utf8)) == .on)
        #expect(try decoder.decode(MicState.self, from: Data("\"permission_denied\"".utf8)) == .permissionDenied)
        #expect(try decoder.decode(MicState.self, from: Data("\"error\"".utf8)) == .error)
    }

    @Test("SetMicEnabledRequest encodes to wire format")
    func setMicEnabledRequestEncodes() throws {
        let request = SetMicEnabledRequest(type: "set_mic_enabled", enabled: true)
        let data = try JSONEncoder().encode(request)
        let dict = try JSONSerialization.jsonObject(with: data) as! [String: Any]
        #expect(dict["type"] as? String == "set_mic_enabled")
        #expect(dict["enabled"] as? Bool == true)
    }

    @Test("SetMicEnabledResponse decodes from wire format")
    func setMicEnabledResponseDecodes() throws {
        let json = """
        {"type":"set_mic_enabled","ok":true,"state":"on"}
        """
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let response = try decoder.decode(SetMicEnabledResponse.self, from: Data(json.utf8))
        #expect(response.type == "set_mic_enabled")
        #expect(response.ok == true)
        #expect(response.state == .on)
    }

    @Test("SearchResponse decodes from Rust wire format")
    func searchResponseDecodes() throws {
        let json = """
        {"type":"search","ok":true,"hits":[
          {"id":1,"source":"screen","timestamp_ms":1700000000000,
           "app_name":"Terminal","app_bundle_id":"com.apple.Terminal",
           "window_title":"zsh","image_path":"/x.heif",
           "snippet":"hello <b>world</b>","rank":-1.5}
        ]}
        """
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let resp = try decoder.decode(SearchResponse.self, from: Data(json.utf8))
        #expect(resp.type == "search")
        #expect(resp.ok == true)
        #expect(resp.hits.count == 1)
        let hit = resp.hits[0]
        #expect(hit.id == 1)
        #expect(hit.source == .screen)
        #expect(hit.timestampMs == 1_700_000_000_000)
        #expect(hit.appName == "Terminal")
        #expect(hit.snippet == "hello <b>world</b>")
    }

    @Test("GetScreenshotResponse with null hit decodes")
    func getScreenshotNullHit() throws {
        let json = """
        {"type":"get_screenshot","ok":true,"hit":null}
        """
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let resp = try decoder.decode(GetScreenshotResponse.self, from: Data(json.utf8))
        #expect(resp.hit == nil)
    }

    @Test("PauseResumeResponse decodes both variants")
    func pauseResumeResponseDecodes() throws {
        let pauseJson = """
        {"type":"pause_capture","ok":true,"paused":true}
        """
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let p = try decoder.decode(PauseResumeResponse.self, from: Data(pauseJson.utf8))
        #expect(p.type == "pause_capture")
        #expect(p.paused == true)

        let resumeJson = """
        {"type":"resume_capture","ok":true,"paused":false}
        """
        let r = try decoder.decode(PauseResumeResponse.self, from: Data(resumeJson.utf8))
        #expect(r.type == "resume_capture")
        #expect(r.paused == false)
    }

    @Test("Expanded StatusData decodes new fields")
    func expandedStatusDataDecodes() throws {
        let json = """
        {"type":"status","ok":true,"data":{
          "uptime_secs":42,"version":"0.1.0",
          "capture":{"state":"paused","active_displays":1,
            "frames_captured":0,"frames_dropped":0,
            "frames_processed":0,"frames_failed":0,"paused":true},
          "ocr":{"enqueued":0,"dropped":0},
          "audio":{"segments_persisted":0,"mic_state":"off"},
          "storage":{"db_size_bytes":1024,"total_disk_usage_bytes":2048,
            "screenshot_count":50,"audio_segment_count":5,
            "oldest_entry_ms":1700000000000,"retention_days":14}
        }}
        """
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let resp = try decoder.decode(StatusResponse.self, from: Data(json.utf8))
        let capture = try #require(resp.data.capture)
        let storage = try #require(resp.data.storage)
        #expect(capture.paused == true)
        #expect(capture.state == "paused")
        #expect(storage.totalDiskUsageBytes == 2048)
        #expect(storage.screenshotCount == 50)
        #expect(storage.retentionDays == 14)
        #expect(storage.oldestEntryMs == 1_700_000_000_000)
    }

    @Test("StatusData decodes the transcription block")
    func statusDataDecodesTranscriptionBlock() throws {
        // The sizes below are deliberately arbitrary wire values, NOT manifest
        // constants — small is really 488M. Leave them at 466; they exist to
        // exercise the decoder, and "correcting" them buys nothing.
        let json = """
        {"type":"status","ok":true,"data":{"uptime_secs":5,"version":"0.1.0",
         "transcription":{"state":"downloading","variant":"small",
         "loaded_variant":"base","error":null,
         "download_bytes":1024,"download_total_bytes":466000000,
         "models":[{"variant":"base","downloaded":true,"size_bytes":148000000},
                   {"variant":"small","downloaded":false,"size_bytes":466000000},
                   {"variant":"medium","downloaded":false,"size_bytes":1530000000}]}}}
        """
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let resp = try decoder.decode(StatusResponse.self, from: Data(json.utf8))
        let t = try #require(resp.data.transcription)
        #expect(t.state == .downloading)
        #expect(t.variant == "small")
        #expect(t.loadedVariant == "base")
        #expect(t.downloadBytes == 1024)
        // Asserted explicitly: the property is UInt64?, so a snake_case
        // mapping drift would degrade to nil and this test would still pass
        // without it. Mirrors the fixture above — see the note there before
        // changing either number.
        #expect(t.downloadTotalBytes == 466_000_000)
        #expect(t.models.count == 3)
        #expect(t.models[0].downloaded == true)
    }

    @Test("StatusData tolerates a daemon without the block")
    func statusDataToleratesMissingTranscription() throws {
        let json = """
        {"type":"status","ok":true,"data":{"uptime_secs":5,"version":"0.1.0"}}
        """
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let resp = try decoder.decode(StatusResponse.self, from: Data(json.utf8))
        #expect(resp.data.transcription == nil, "old daemon → nil, no crash")
    }

    @Test("Old UI schema ignores the transcription block")
    func oldUISchemaIgnoresTranscriptionBlock() throws {
        // NFR-7, reverse direction: an old UI must ignore a new daemon's extra
        // fields. Stub mirrors StatusData exactly as it exists BEFORE this task
        // (no `transcription` property).
        struct OldStatusData: Codable {
            let uptimeSecs: UInt64
            let version: String
            let capture: CaptureStats?
            let ocr: OcrStats?
            let audio: AudioStats?
            let storage: StorageStats?
        }
        struct OldStatusResponse: Codable {
            let type: String
            let ok: Bool
            let data: OldStatusData
        }
        let json = """
        {"type":"status","ok":true,"data":{"uptime_secs":5,"version":"0.1.0",
         "transcription":{"state":"downloading","variant":"small",
         "loaded_variant":"base","error":null,
         "download_bytes":1024,"download_total_bytes":466000000,
         "models":[{"variant":"base","downloaded":true,"size_bytes":148000000},
                   {"variant":"small","downloaded":false,"size_bytes":466000000},
                   {"variant":"medium","downloaded":false,"size_bytes":1530000000}]}}}
        """
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let resp = try decoder.decode(OldStatusResponse.self, from: Data(json.utf8))
        #expect(resp.data.version == "0.1.0",
                "new transcription block ignored, not a decode failure")
    }

    @Test("Unknown transcription state degrades, not throws")
    func unknownTranscriptionStateDegrades() throws {
        // A future daemon may add a 7th state. A closed enum would throw and
        // kill the WHOLE StatusResponse decode (status polling goes dark) —
        // decode to .unknown instead. Recorded decision from the Task 2
        // wire-type review; formal cross-version policy is HEU-456.
        let json = """
        {"type":"status","ok":true,"data":{"uptime_secs":5,"version":"0.1.0",
         "transcription":{"state":"defragmenting","variant":"base",
         "loaded_variant":null,"error":null,
         "download_bytes":null,"download_total_bytes":null,"models":[]}}}
        """
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let resp = try decoder.decode(StatusResponse.self, from: Data(json.utf8))
        let t = try #require(resp.data.transcription)
        #expect(t.state == .unknown)
        #expect(resp.data.version == "0.1.0", "rest of the payload intact")
    }

    @Test("Failed switch keeps the previously loaded variant")
    func failedSwitchRetainsLoadedVariant() throws {
        // Design §4.2 keys the error banner copy on this branch:
        // "Switch to {variant} failed — still using {loaded_variant}".
        // `variant` is the attempted one; `loaded_variant` is what still runs.
        let json = """
        {"type":"status","ok":true,"data":{"uptime_secs":5,"version":"0.1.0",
         "transcription":{"state":"error","variant":"small",
         "loaded_variant":"base","error":"download failed: connection reset",
         "download_bytes":null,"download_total_bytes":null,
         "models":[{"variant":"base","downloaded":true,"size_bytes":148000000}]}}}
        """
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let resp = try decoder.decode(StatusResponse.self, from: Data(json.utf8))
        let t = try #require(resp.data.transcription)
        #expect(t.state == .error)
        #expect(t.variant == "small", "attempted variant")
        #expect(t.loadedVariant == "base", "still-running variant")
        #expect(t.error == "download failed: connection reset")
        #expect(t.downloadBytes == nil)
        #expect(t.downloadTotalBytes == nil)
    }

    // MARK: - Forward compatibility on closed wire enums
    //
    // A closed String enum on a wire field throws on an unknown value, and the
    // throw takes the WHOLE response decode with it — not just that field.
    // TranscriptionState already degrades to .unknown for this reason. These
    // two cover the enums that still had the bug.
    //
    // The fix must ship BEFORE the daemon emits the new value: it only
    // protects UIs built after it. Fixing it alongside the daemon change
    // protects nothing, because the UI that breaks is the one already
    // installed. Formal negotiation is HEU-456; this is just the decoder
    // being careful.

    @Test("Unknown search hit source degrades, not throws")
    func unknownSearchHitSourceDegrades() throws {
        // Rust reserves SearchHitSource::Audio for HEU-470. `source` is
        // non-optional, so a closed enum here takes the entire SearchResponse
        // down and search goes dark — every hit lost, not just the audio one.
        let json = """
        {"type":"search","ok":true,"hits":[
          {"id":1,"source":"screen","timestamp_ms":1700000000000,
           "app_name":"Terminal","app_bundle_id":"com.apple.Terminal",
           "window_title":"zsh","image_path":"/x.heif",
           "snippet":"hello <b>world</b>","rank":-1.5},
          {"id":2,"source":"audio","timestamp_ms":1700000000001,
           "app_name":null,"app_bundle_id":null,
           "window_title":null,"image_path":"","snippet":"spoken <b>words</b>",
           "rank":-1.2}
        ]}
        """
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let resp = try decoder.decode(SearchResponse.self, from: Data(json.utf8))
        #expect(resp.hits.count == 2, "the known hit survives the unknown one")
        #expect(resp.hits[0].source == .screen)
        #expect(resp.hits[1].source == .unknown)
        #expect(resp.hits[1].snippet == "spoken <b>words</b>",
                "rest of the unknown-source hit still decodes")
    }

    @Test("Unknown mic state degrades, not throws")
    func unknownMicStateDegrades() throws {
        // StatusData.audio is optional, but that does NOT save you: a present
        // key whose value fails to decode throws rather than degrading to nil.
        // Verified — an unknown mic_state killed the whole StatusData decode
        // with DecodingError.dataCorrupted at path audio.micState.
        let json = """
        {"type":"status","ok":true,"data":{"uptime_secs":5,"version":"0.1.0",
         "audio":{"segments_persisted":5,"mic_state":"muted"}}}
        """
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let resp = try decoder.decode(StatusResponse.self, from: Data(json.utf8))
        let audio = try #require(resp.data.audio, "status survives, audio intact")
        #expect(audio.micState == .unknown)
        #expect(audio.segmentsPersisted == 5, "sibling field still decodes")
        #expect(resp.data.version == "0.1.0", "rest of the payload intact")
    }
}
