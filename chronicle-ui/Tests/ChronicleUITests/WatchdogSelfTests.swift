import Foundation
import Synchronization
import Testing

@Suite("Watchdog self-test", .watchdog,
       .enabled(if: ProcessInfo.processInfo.environment["CHRONICLE_TEST_WATCHDOG_SELFTEST"] == "1"))
struct WatchdogSelfTests {
    static let parked = Mutex<[CheckedContinuation<Void, Never>]>([])

    @Test func watchdogFiresOnAParkedContinuation() async {
        await withCheckedContinuation { cont in Self.parked.withLock { $0.append(cont) } }
    }

    @Test func watchdogFiresWhileTheCooperativePoolIsBlocked() async {
        let gate = DispatchSemaphore(value: 0)
        for index in 0..<(ProcessInfo.processInfo.activeProcessorCount + 2) {
            Task.detached {
                StandardError.write("CHRONICLE TEST WATCHDOG SELFTEST: blocker \(index) entered\n")
                Self.block(on: gate)
            }
        }
        await withCheckedContinuation { cont in Self.parked.withLock { $0.append(cont) } }
    }

    private static func block(on gate: DispatchSemaphore) {
        gate.wait()
    }
}
