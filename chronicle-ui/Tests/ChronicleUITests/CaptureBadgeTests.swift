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
}
