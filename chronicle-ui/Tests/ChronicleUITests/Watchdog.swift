import Foundation

/// How long the test watchdog lets a test process run before ending it.
///
/// `CHRONICLE_TEST_WATCHDOG_SECONDS` sets it. A whole number of seconds from
/// 1 to 86 400 replaces the default of 120, and `0` turns the watchdog off.
/// Any other value prints a warning and keeps the default.
enum WatchdogSetting: Equatable {
    case `default`
    case seconds(Int)
    case off
    case invalid(String)

    static let variable = "CHRONICLE_TEST_WATCHDOG_SECONDS"
    static let defaultSeconds = 120
    static let maximumSeconds = 86_400

    static func parse(_ raw: String?) -> WatchdogSetting {
        guard let raw else { return .default }
        guard !raw.isEmpty, raw.utf8.allSatisfy({ $0 >= 0x30 && $0 <= 0x39 }),
              let value = Int(raw)
        else { return .invalid(raw) }
        if value == 0 { return .off }
        return value <= maximumSeconds ? .seconds(value) : .invalid(raw)
    }

    /// The deadline in seconds, or nil when the watchdog is off.
    var armedSeconds: Int? {
        switch self {
        case .default, .invalid: Self.defaultSeconds
        case .seconds(let value): value
        case .off: nil
        }
    }

    /// The line printed when the watchdog arms, or nil for the default.
    var notice: String? {
        switch self {
        case .default: nil
        case .seconds(let value): "CHRONICLE TEST WATCHDOG: N is \(value) s (\(Self.variable))\n"
        case .off: "CHRONICLE TEST WATCHDOG: off (\(Self.variable)=0)\n"
        case .invalid(let raw):
            "CHRONICLE TEST WATCHDOG: ignoring \(Self.variable)=\"\(raw)\"; N is \(Self.defaultSeconds) s\n"
        }
    }
}

enum WatchdogMessage {
    static func label(name: String, fileID: String, line: Int) -> String {
        "\(name) (\(fileID):\(line))"
    }

    static func fired(seconds: Int, inFlight: [String]) -> String {
        let head = "CHRONICLE TEST WATCHDOG FIRED: the test run passed its \(seconds) s deadline."
        if inFlight.isEmpty {
            return head + "\n  no test in flight; the hang is outside a test body\n"
        }
        return head + " In flight:\n" + inFlight.sorted().map { "  \($0)\n" }.joined()
    }
}

enum StandardError {
    /// Writes `text` to fd 2 with write(2): no stdio buffer, no stdio lock.
    /// Retries on EINTR, continues after a partial write, stops on any other error.
    static func write(_ text: String) {
        let bytes = Array(text.utf8)
        bytes.withUnsafeBytes { buffer in
            var offset = 0
            while offset < buffer.count {
                let written = Darwin.write(2, buffer.baseAddress! + offset, buffer.count - offset)
                if written > 0 {
                    offset += written
                } else if written < 0, errno == EINTR {
                    continue
                } else {
                    return
                }
            }
        }
    }
}
