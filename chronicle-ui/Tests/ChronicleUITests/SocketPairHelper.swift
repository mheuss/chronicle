import Foundation
import Darwin

/// Creates a connected AF_UNIX SOCK_STREAM socketpair for tests. The two fds
/// are full-duplex; either side can read or write.
enum SocketPairHelper {
    enum Error: Swift.Error {
        case socketpairFailed(errno: Int32)
        case receiveTimeoutFailed(errno: Int32)
    }

    static func make() throws -> (clientFD: Int32, serverFD: Int32) {
        var fds: [Int32] = [0, 0]
        let result = fds.withUnsafeMutableBufferPointer { buf in
            socketpair(AF_UNIX, SOCK_STREAM, 0, buf.baseAddress)
        }
        guard result == 0 else { throw Error.socketpairFailed(errno: errno) }
        // The server side only — bounding reads on the client side would change
        // the behaviour of the code under test. A test server whose bytes never
        // arrive otherwise parks a thread for the whole run, and the suite's
        // .timeLimit cannot convert that into a failure: it works by
        // cancellation, which a thread inside read() never observes.
        guard setReceiveTimeout(fds[1], seconds: 10) else {
            let timeoutErrno = errno
            Darwin.close(fds[0])
            Darwin.close(fds[1])
            throw Error.receiveTimeoutFailed(errno: timeoutErrno)
        }
        return (clientFD: fds[0], serverFD: fds[1])
    }
}
