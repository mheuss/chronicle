import Foundation
import Darwin

/// Creates a connected AF_UNIX SOCK_STREAM socketpair for tests. The two fds
/// are full-duplex; either side can read or write.
enum SocketPairHelper {
    enum Error: Swift.Error {
        case socketpairFailed(errno: Int32)
    }

    static func make() throws -> (clientFD: Int32, serverFD: Int32) {
        var fds: [Int32] = [0, 0]
        let result = fds.withUnsafeMutableBufferPointer { buf in
            socketpair(AF_UNIX, SOCK_STREAM, 0, buf.baseAddress)
        }
        guard result == 0 else { throw Error.socketpairFailed(errno: errno) }
        return (clientFD: fds[0], serverFD: fds[1])
    }
}
