import Foundation

/// Finds Chronicle's data root, the directory that holds the daemon's socket.
enum DataRoot {
    static let appGroupID = "7TJEE2YF4G.chronicle"
    static let socketName = "ipc.sock"

    static func resolve(
        environment: [String: String],
        containerURL: (String) -> URL?
    ) -> Result<URL, DataRootError> {
        if let dir = environment["CHRONICLE_DATA_DIR"], !dir.isEmpty {
            guard dir.hasPrefix("/") else {
                return .failure(.relativeOverride(dir))
            }
            return .success(URL(fileURLWithPath: dir))
        }
        guard let url = containerURL(appGroupID) else {
            return .failure(.containerUnavailable)
        }
        return .success(url)
    }
}

enum DataRootError: Error, LocalizedError, Equatable {
    case relativeOverride(String)
    case containerUnavailable

    var errorDescription: String? {
        switch self {
        case .relativeOverride(let dir): "CHRONICLE_DATA_DIR must be an absolute path, got \"\(dir)\""
        case .containerUnavailable: "No App Group container for \(DataRoot.appGroupID)"
        }
    }
}
