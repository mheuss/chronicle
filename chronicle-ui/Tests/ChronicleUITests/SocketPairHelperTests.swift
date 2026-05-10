import Testing
import Foundation
import Darwin
@testable import ChronicleUI

@Suite("SocketPairHelper", .timeLimit(.minutes(1)))
struct SocketPairHelperTests {

    @Test("returns two fds that can communicate")
    func roundTrip() throws {
        let pair = try SocketPairHelper.make()
        defer {
            Darwin.close(pair.clientFD)
            Darwin.close(pair.serverFD)
        }
        let payload: [UInt8] = [1, 2, 3, 4, 5]
        let written = payload.withUnsafeBufferPointer { buf in
            Darwin.write(pair.clientFD, buf.baseAddress, buf.count)
        }
        #expect(written >= 0, "write failed with errno \(errno)")
        #expect(written == payload.count)
        var readBuf = [UInt8](repeating: 0, count: payload.count)
        let readBytes = readBuf.withUnsafeMutableBufferPointer { buf in
            Darwin.read(pair.serverFD, buf.baseAddress, buf.count)
        }
        #expect(readBytes >= 0, "read failed with errno \(errno)")
        #expect(readBytes == payload.count)
        #expect(readBuf == payload)
    }
}
