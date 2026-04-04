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
        ]
        for (error, expected) in errors {
            #expect(error.errorDescription == expected)
        }
    }
}
