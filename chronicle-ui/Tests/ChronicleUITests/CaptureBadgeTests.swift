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

    private static let looks: [CaptureBadge: (symbol: String, tint: MenuBarDisplay.Tint, label: String)] = [
        .disconnected: ("xmark.circle.fill", .red, "Disconnected"),
        .connecting: ("ellipsis.circle.fill", .yellow, "Connecting"),
        .waiting: ("ellipsis.circle.fill", .yellow, "Waiting for status"),
        .paused: ("pause.circle.fill", .orange, "Paused"),
        .active: ("record.circle.fill", .green, "Active"),
        .notRunning: ("exclamationmark.circle.fill", .red, "Not running"),
    ]

    @Test(arguments: CaptureBadge.allCases)
    func eachBadgeHasItsOwnLook(badge: CaptureBadge) throws {
        let look = try #require(Self.looks[badge])
        #expect(badge.symbol == look.symbol)
        #expect(badge.tint == look.tint)
        #expect(badge.label == look.label)
    }
}
