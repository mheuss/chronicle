import Foundation
import Synchronization
@_spi(Experimental) import Testing

/// How long the test watchdog lets a test process run before ending it.
///
/// `CHRONICLE_TEST_WATCHDOG_SECONDS` sets it. A whole number of seconds from
/// 1 to 86 400 replaces the default of 120, and `0` turns the watchdog off.
/// Any other value is invalid: it keeps the default and carries a warning.
/// CI's main Test step sets neither this nor `CHRONICLE_TEST_WATCHDOG_SELFTEST`,
/// because `scripts/check-watchdog.sh` owns both.
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

    /// The line to print when the watchdog arms, or nil for the default.
    var notice: String? {
        switch self {
        case .default: nil
        case .seconds(let value): "CHRONICLE TEST WATCHDOG: N is \(value) s (\(Self.variable))\n"
        case .off: "CHRONICLE TEST WATCHDOG: off (\(Self.variable)=0)\n"
        case .invalid(let raw):
            "CHRONICLE TEST WATCHDOG: ignoring \(Self.variable)=\(raw.debugDescription); N is \(Self.defaultSeconds) s\n"
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

/// One per test process. The timer runs on its own Dispatch queue, never on
/// the Swift cooperative pool, because a stalled pool is one of the hangs it
/// has to end.
final class TestWatchdog: Sendable {
    static let shared = TestWatchdog()

    private struct State {
        var armed = false
        var nextToken: UInt64 = 0
        var inFlight: [UInt64: String] = [:]
    }

    private let state = Mutex(State())
    private let timerQueue = DispatchQueue(label: "chronicle.test-watchdog")
    private let backupQueue = DispatchQueue(label: "chronicle.test-watchdog.backup")
    private let timer: DispatchSourceTimer
    private let backupTimer: DispatchSourceTimer

    private init() {
        timer = DispatchSource.makeTimerSource(queue: timerQueue)
        backupTimer = DispatchSource.makeTimerSource(queue: backupQueue)
    }

    func armOnce() {
        let first = state.withLock { state in
            guard !state.armed else { return false }
            state.armed = true
            return true
        }
        guard first else { return }
        let setting = WatchdogSetting.parse(ProcessInfo.processInfo.environment[WatchdogSetting.variable])
        if let seconds = setting.armedSeconds {
            timer.schedule(deadline: .now() + .seconds(seconds))
            timer.setEventHandler { [self] in fire(seconds: seconds) }
            timer.resume()
            // write(2) to a full pipe can block the fire, so a second timer ends
            // the process 2 s later whether or not the message got out.
            backupTimer.schedule(deadline: .now() + .seconds(seconds + 2))
            backupTimer.setEventHandler { _exit(1) }
            backupTimer.resume()
        }
        // After arming: a write to a full stderr pipe can block.
        if let notice = setting.notice { StandardError.write(notice) }
    }

    func begin(_ label: String) -> UInt64 {
        state.withLock { state in
            let token = state.nextToken
            state.nextToken += 1
            state.inFlight[token] = label
            return token
        }
    }

    func end(_ token: UInt64) {
        state.withLock { _ = $0.inFlight.removeValue(forKey: token) }
    }

    func inFlight() -> [String] {
        state.withLock { Array($0.inFlight.values) }
    }

    private func fire(seconds: Int) {
        StandardError.write(WatchdogMessage.fired(seconds: seconds, inFlight: inFlight()))
        _exit(1)
    }
}

/// Arms the process watchdog when the runner plans a tagged test, and records
/// each test function while it runs so a fire can name it.
struct WatchdogTrait: SuiteTrait, TestTrait, CustomExecutionTrait {
    /// Nested suites that both carry the trait give a test two copies of it.
    @TaskLocal private static var registeredTest: Test.ID?

    var isRecursive: Bool { true }

    func prepare(for test: Test) async throws {
        TestWatchdog.shared.armOnce()
    }

    func execute(
        _ function: @escaping @Sendable () async throws -> Void,
        for test: Test,
        testCase: Test.Case?
    ) async throws {
        guard !test.isSuite, testCase == nil, Self.registeredTest != test.id else {
            return try await function()
        }
        let label = WatchdogMessage.label(
            name: test.displayName ?? test.name,
            fileID: test.sourceLocation.fileID,
            line: test.sourceLocation.line
        )
        let token = TestWatchdog.shared.begin(label)
        defer { TestWatchdog.shared.end(token) }
        try await Self.$registeredTest.withValue(test.id) { try await function() }
    }
}

extension Trait where Self == WatchdogTrait {
    static var watchdog: Self { Self() }
}
