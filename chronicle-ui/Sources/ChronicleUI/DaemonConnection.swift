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
    private let encoder = JSONEncoder()
    private let decoder: JSONDecoder = {
        let d = JSONDecoder()
        d.keyDecodingStrategy = .convertFromSnakeCase
        return d
    }()

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
        let request = IPCRequest(type: "status")
        let data = try encoder.encode(request)
        guard var line = String(data: data, encoding: .utf8) else {
            throw IPCError.encodingFailed
        }
        line.append("\n")

        try await write(line)
        let responseLine = try await readLine()
        let responseData = Data(responseLine.utf8)

        // Try decoding as status response first, then as error response
        if let response = try? decoder.decode(StatusResponse.self, from: responseData) {
            lastStatus = response
            return response
        }
        if let errorResp = try? decoder.decode(ErrorResponse.self, from: responseData) {
            throw IPCError.daemonError(errorResp.message)
        }
        // Neither parsed — surface as a decoding error
        let response = try decoder.decode(StatusResponse.self, from: responseData)
        lastStatus = response
        return response
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
                    Darwin.close(fd)
                    cont.resume(throwing: IPCError.connectionFailed(errno: errno))
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
                        if written < 0 {
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
                let line = String(bytes: buffer, encoding: .utf8) ?? ""
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
    }
}

// MARK: - Protocol Types

struct IPCRequest: Codable {
    let type: String
}

struct StatusResponse: Codable {
    let type: String
    let ok: Bool
    let data: StatusData
}

struct StatusData: Codable {
    let uptimeSecs: UInt64
    let version: String
}

struct ErrorResponse: Codable {
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
        }
    }
}
