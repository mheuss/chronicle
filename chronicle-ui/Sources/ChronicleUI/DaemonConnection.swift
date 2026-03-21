import Foundation

/// Communicates with chronicle-daemon over a Unix domain socket.
///
/// Protocol: newline-delimited JSON request/response.
/// Socket path: ~/Library/Application Support/Chronicle/chronicle.sock
final class DaemonConnection: Sendable {
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
}
