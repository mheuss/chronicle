import Foundation
import Observation

/// Communicates with chronicle-daemon over a Unix domain socket.
///
/// Protocol: newline-delimited JSON request/response.
/// Socket path: ~/Library/Application Support/Chronicle/chronicle.sock
@MainActor
@Observable
final class DaemonConnection {

    // MARK: - State

    enum ConnectionState {
        case disconnected
        case connecting
        case connected
    }

    private(set) var state: ConnectionState = .disconnected
    private(set) var lastStatus: StatusResponse?

    // MARK: - Socket Path

    static let socketPath: String = {
        let appSupport = FileManager.default.urls(
            for: .applicationSupportDirectory,
            in: .userDomainMask
        ).first!
        return appSupport
            .appendingPathComponent("Chronicle")
            .appendingPathComponent("chronicle.sock")
            .path
    }()

    // MARK: - Constants

    private static let maxResponseSize = 64 * 1024

    // MARK: - Private

    private var socketHandle: FileHandle?
    private var reconnectTask: Task<Void, Never>?
    private var requestQueue: Task<Void, Never>?
    private var connectionGeneration: UInt64 = 0
    private let encoder = JSONEncoder()
    private let decoder: JSONDecoder = {
        let d = JSONDecoder()
        d.keyDecodingStrategy = .convertFromSnakeCase
        return d
    }()

    // MARK: - Init

    init() {}

    /// **Test-only.** Not for production use. Wires `DaemonConnection` to a
    /// pre-connected fd (e.g., one side of a `socketpair()`). The fd is taken
    /// over and closed when the connection deallocates.
    internal init(testingSocketFD fd: Int32) {
        var noSigPipe: Int32 = 1
        _ = setsockopt(
            fd,
            SOL_SOCKET,
            SO_NOSIGPIPE,
            &noSigPipe,
            socklen_t(MemoryLayout<Int32>.size)
        )
        self.socketHandle = FileHandle(fileDescriptor: fd, closeOnDealloc: true)
        self.state = .connected
    }

    // MARK: - Lifecycle

    /// Begin connecting to the daemon. Auto-reconnects on failure.
    func connect() {
        guard reconnectTask == nil else { return }
        reconnectTask = Task {
            var delay: Duration = .seconds(1)
            while !Task.isCancelled {
                state = .connecting
                do {
                    try await establishConnection()
                    try Task.checkCancellation()
                    state = .connected
                    delay = .seconds(1)
                    try await monitorConnection()
                } catch is CancellationError {
                    break
                } catch {
                    closeSocket()
                    state = .disconnected
                    try? await Task.sleep(for: delay)
                    delay = min(delay * 2, .seconds(30))
                }
            }
        }
    }

    /// Disconnect and stop auto-reconnect.
    func disconnect() {
        reconnectTask?.cancel()
        reconnectTask = nil
        closeSocket()
        state = .disconnected
    }

    // MARK: - Requests

    /// Send a status request and return the response.
    func requestStatus() async throws -> StatusResponse {
        let response = try await sendRequest(
            IPCRequest(type: "status"),
            expecting: StatusResponse.self
        )
        lastStatus = response
        return response
    }

    /// Generic request helper. ALL request methods must route through this —
    /// direct calls to `write()`/`readLine()` from request paths bypass FIFO
    /// serialization and the connection-generation check.
    ///
    /// The unstructured `Task` here is intentional: caller cancellation must
    /// not abort socket I/O mid-write/read, or we'd leave the newline-delimited
    /// protocol in a corrupt state.
    ///
    /// The Task strongly captures `self` and is then stored in
    /// `self.requestQueue`, creating a temporary retain cycle. This is safe:
    /// `closeSocket()` closes the underlying fd, which aborts any blocked
    /// `Darwin.read`/`write`, which lets the task complete and the cycle break.
    private func sendRequest<Req: Encodable & Sendable, Res: Decodable & Sendable>(
        _ request: Req,
        expecting: Res.Type
    ) async throws -> Res {
        let myGeneration = connectionGeneration
        let prev = requestQueue
        let task = Task<Res, Error> {
            _ = await prev?.value  // wait for predecessor
            guard self.connectionGeneration == myGeneration else {
                throw IPCError.notConnected
            }
            let data = try self.encoder.encode(request)
            guard var line = String(data: data, encoding: .utf8) else {
                throw IPCError.encodingFailed
            }
            line.append("\n")
            try await self.write(line)
            let responseLine = try await self.readLine()
            let responseData = Data(responseLine.utf8)
            if let res = try? self.decoder.decode(Res.self, from: responseData) {
                return res
            }
            if let err = try? self.decoder.decode(ErrorResponse.self, from: responseData) {
                throw IPCError.daemonError(err.message)
            }
            do {
                return try self.decoder.decode(Res.self, from: responseData)
            } catch {
                throw IPCError.malformedResponse(String(describing: error))
            }
        }
        requestQueue = Task { _ = try? await task.value }
        return try await task.value
    }

    // MARK: - Socket Operations

    private func establishConnection() async throws {
        let path = Self.socketPath
        let fd: Int32 = try await withCheckedThrowingContinuation { cont in
            DispatchQueue.global().async {
                let fd = socket(AF_UNIX, SOCK_STREAM, 0)
                guard fd >= 0 else {
                    cont.resume(throwing: IPCError.socketCreationFailed(errno: errno))
                    return
                }

                var noSigPipe: Int32 = 1
                let setoptResult = setsockopt(
                    fd,
                    SOL_SOCKET,
                    SO_NOSIGPIPE,
                    &noSigPipe,
                    socklen_t(MemoryLayout<Int32>.size)
                )
                guard setoptResult == 0 else {
                    let setoptErrno = errno
                    Darwin.close(fd)
                    cont.resume(throwing: IPCError.socketCreationFailed(errno: setoptErrno))
                    return
                }

                var addr = sockaddr_un()
                addr.sun_family = sa_family_t(AF_UNIX)
                let pathBytes = path.utf8CString
                guard pathBytes.count <= MemoryLayout.size(ofValue: addr.sun_path) else {
                    Darwin.close(fd)
                    cont.resume(throwing: IPCError.pathTooLong)
                    return
                }
                withUnsafeMutablePointer(to: &addr.sun_path) { ptr in
                    ptr.withMemoryRebound(to: CChar.self, capacity: pathBytes.count) { dest in
                        pathBytes.withUnsafeBufferPointer { src in
                            _ = memcpy(dest, src.baseAddress!, src.count)
                        }
                    }
                }

                let result = withUnsafePointer(to: &addr) { ptr in
                    ptr.withMemoryRebound(to: sockaddr.self, capacity: 1) { sockPtr in
                        Darwin.connect(fd, sockPtr, socklen_t(MemoryLayout<sockaddr_un>.size))
                    }
                }

                guard result == 0 else {
                    let connectErrno = errno
                    Darwin.close(fd)
                    cont.resume(throwing: IPCError.connectionFailed(errno: connectErrno))
                    return
                }

                cont.resume(returning: fd)
            }
        }

        socketHandle = FileHandle(fileDescriptor: fd, closeOnDealloc: true)
    }

    private func monitorConnection() async throws {
        while !Task.isCancelled && socketHandle != nil {
            try await Task.sleep(for: .seconds(30))
            _ = try await requestStatus()
        }
    }

    private func write(_ string: String) async throws {
        guard let handle = socketHandle else {
            throw IPCError.notConnected
        }
        let fd = handle.fileDescriptor
        try await withCheckedThrowingContinuation { (cont: CheckedContinuation<Void, Error>) in
            DispatchQueue.global().async {
                let data = Array(string.utf8)
                data.withUnsafeBytes { rawBuffer in
                    var offset = 0
                    while offset < rawBuffer.count {
                        let written = Darwin.write(
                            fd,
                            rawBuffer.baseAddress! + offset,
                            rawBuffer.count - offset
                        )
                        if written <= 0 {
                            cont.resume(throwing: IPCError.writeFailed(errno: errno))
                            return
                        }
                        offset += written
                    }
                    cont.resume()
                }
            }
        }
    }

    private func readLine() async throws -> String {
        guard let handle = socketHandle else {
            throw IPCError.notConnected
        }
        let fd = handle.fileDescriptor
        let maxSize = Self.maxResponseSize
        return try await withCheckedThrowingContinuation { (cont: CheckedContinuation<String, Error>) in
            DispatchQueue.global().async {
                var buffer = [UInt8]()
                var byte: UInt8 = 0
                while true {
                    let bytesRead = Darwin.read(fd, &byte, 1)
                    if bytesRead <= 0 {
                        cont.resume(throwing: IPCError.connectionClosed)
                        return
                    }
                    if byte == UInt8(ascii: "\n") {
                        break
                    }
                    buffer.append(byte)
                    if buffer.count > maxSize {
                        cont.resume(throwing: IPCError.responseTooLarge)
                        return
                    }
                }
                guard let line = String(bytes: buffer, encoding: .utf8) else {
                    cont.resume(throwing: IPCError.invalidUTF8)
                    return
                }
                cont.resume(returning: line)
            }
        }
    }

    private func closeSocket() {
        if let handle = socketHandle {
            // closeOnDealloc is true, but we close explicitly for immediate cleanup
            try? handle.close()
            socketHandle = nil
        }
        connectionGeneration &+= 1
        requestQueue = nil
    }
}

// MARK: - Protocol Types

struct IPCRequest: Codable, Sendable {
    let type: String
}

struct StatusResponse: Codable, Sendable {
    let type: String
    let ok: Bool
    let data: StatusData
}

struct StatusData: Codable, Sendable {
    let uptimeSecs: UInt64
    let version: String
    let capture: CaptureStats?
    let ocr: OcrStats?
    let audio: AudioStats?
    let storage: StorageStats?
}

struct CaptureStats: Codable, Sendable {
    let state: String
    let activeDisplays: Int
    let framesCaptured: UInt64
    let framesDropped: UInt64
    let framesProcessed: UInt64
    let framesFailed: UInt64
}

struct OcrStats: Codable, Sendable {
    let enqueued: UInt64
    let dropped: UInt64
}

struct AudioStats: Codable, Sendable {
    let segmentsPersisted: UInt64
}

struct StorageStats: Codable, Sendable {
    let dbSizeBytes: UInt64
}

struct ErrorResponse: Codable, Sendable {
    let type: String
    let ok: Bool
    let message: String
}

// MARK: - Errors

enum IPCError: Error, LocalizedError {
    case socketCreationFailed(errno: Int32)
    case pathTooLong
    case connectionFailed(errno: Int32)
    case notConnected
    case writeFailed(errno: Int32)
    case connectionClosed
    case encodingFailed
    case responseTooLarge
    case daemonError(String)
    case invalidUTF8
    case malformedResponse(String)

    var errorDescription: String? {
        switch self {
        case .socketCreationFailed(let e): "Failed to create socket: \(String(cString: strerror(e)))"
        case .pathTooLong: "Socket path too long"
        case .connectionFailed(let e): "Connection failed: \(String(cString: strerror(e)))"
        case .notConnected: "Not connected to daemon"
        case .writeFailed(let e): "Write failed: \(String(cString: strerror(e)))"
        case .connectionClosed: "Connection closed by daemon"
        case .encodingFailed: "Failed to encode request"
        case .responseTooLarge: "Response exceeded maximum size"
        case .daemonError(let msg): "Daemon error: \(msg)"
        case .invalidUTF8: "Response contained invalid UTF-8"
        case .malformedResponse(let detail): "Malformed daemon response: \(detail)"
        }
    }
}
