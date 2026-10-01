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

    @Test func eachBadgeHasItsOwnLook() {
        let looks: [(CaptureBadge, String, MenuBarDisplay.Tint, String)] = [
            (.disconnected, "xmark.circle.fill", .red, "Disconnected"),
            (.connecting, "ellipsis.circle.fill", .yellow, "Connecting"),
            (.waiting, "ellipsis.circle.fill", .yellow, "Waiting for status"),
            (.paused, "pause.circle.fill", .orange, "Paused"),
            (.active, "record.circle.fill", .green, "Active"),
            (.notRunning, "exclamationmark.circle.fill", .red, "Not running"),
        ]
        for (badge, symbol, tint, label) in looks {
            #expect(badge.symbol == symbol)
            #expect(badge.tint == tint)
            #expect(badge.label == label)
        }
    }
}
