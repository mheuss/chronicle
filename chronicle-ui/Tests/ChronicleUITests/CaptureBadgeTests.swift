import Testing
@testable import ChronicleUI

@Suite("Settings capture badge")
struct CaptureBadgeTests {
    @Test func disconnectedIgnoresStatus() {
        #expect(CaptureBadge.from(connectionState: .disconnected,
                                  status: statusWithCapture(state: "running", paused: false))
            == .disconnected)
    }

    @Test func connectingIgnoresStatus() {
        #expect(CaptureBadge.from(connectionState: .connecting,
                                  status: statusWithCapture(state: "running", paused: false))
            == .connecting)
    }

    @Test func connectedWithNoStatusIsWaiting() {
        #expect(CaptureBadge.from(connectionState: .connected, status: nil) == .waiting)
        #expect(CaptureBadge.from(connectionState: .connected, status: statusWithoutCapture())
            == .waiting)
    }

    @Test func pausedRunningAndNotRunning() {
        #expect(CaptureBadge.from(connectionState: .connected,
                                  status: statusWithCapture(state: "running", paused: true))
            == .paused)
        #expect(CaptureBadge.from(connectionState: .connected,
                                  status: statusWithCapture(state: "running", paused: false))
            == .active)
        #expect(CaptureBadge.from(connectionState: .connected,
                                  status: statusWithCapture(state: "unknown", paused: false))
            == .notRunning)
    }

    private static func expectedLook(
        _ badge: CaptureBadge
    ) -> (symbol: String, tint: MenuBarDisplay.Tint, label: String) {
        switch badge {
        case .disconnected: return ("xmark.circle.fill", .red, "Disconnected")
        case .connecting: return ("ellipsis.circle.fill", .yellow, "Connecting")
        case .waiting: return ("ellipsis.circle.fill", .yellow, "Waiting for status")
        case .paused: return ("pause.circle.fill", .orange, "Paused")
        case .active: return ("record.circle.fill", .green, "Active")
        case .notRunning: return ("exclamationmark.circle.fill", .red, "Not running")
        }
    }

    @Test(arguments: CaptureBadge.allCases)
    func eachBadgeHasItsOwnLook(badge: CaptureBadge) {
        let look = Self.expectedLook(badge)
        #expect(badge.symbol == look.symbol)
        #expect(badge.tint == look.tint)
        #expect(badge.label == look.label)
    }
}
