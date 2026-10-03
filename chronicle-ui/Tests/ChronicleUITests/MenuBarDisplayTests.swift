import Testing
@testable import ChronicleUI

@Suite("Menu bar display")
struct MenuBarDisplayTests {
    @Test func disconnectedIgnoresStatus() {
        #expect(MenuBarDisplay.from(connectionState: .disconnected,
                                    status: statusWithCapture(state: "running", paused: false))
            == MenuBarDisplay(symbol: "xmark.circle", tint: .red,
                              label: "Chronicle daemon disconnected"))
    }

    @Test func connectingIgnoresStatus() {
        #expect(MenuBarDisplay.from(connectionState: .connecting,
                                    status: statusWithCapture(state: "running", paused: false))
            == MenuBarDisplay(symbol: "ellipsis.circle", tint: .yellow,
                              label: "Chronicle daemon connecting"))
    }

    @Test func connectedWithNoStatusIsWaiting() {
        let waiting = MenuBarDisplay(symbol: "ellipsis.circle", tint: .yellow,
                                     label: "Chronicle waiting for status")
        #expect(MenuBarDisplay.from(connectionState: .connected, status: nil) == waiting)
        #expect(MenuBarDisplay.from(connectionState: .connected,
                                    status: statusWithoutCapture()) == waiting)
    }

    @Test func pausedWinsOverState() {
        let paused = MenuBarDisplay(symbol: "pause.circle.fill", tint: .orange,
                                    label: "Chronicle capture paused")
        #expect(MenuBarDisplay.from(connectionState: .connected,
                                    status: statusWithCapture(state: "paused", paused: true)) == paused)
        #expect(MenuBarDisplay.from(connectionState: .connected,
                                    status: statusWithCapture(state: "running", paused: true)) == paused)
    }

    @Test func runningIsActive() {
        #expect(MenuBarDisplay.from(connectionState: .connected,
                                    status: statusWithCapture(state: "running", paused: false))
            == MenuBarDisplay(symbol: "record.circle.fill", tint: .green,
                              label: "Chronicle capture active"))
    }

    @Test(arguments: ["unknown", "idle", "stopping", "poisoned", "frobnicating"])
    func anyOtherStateIsNotRunning(state: String) {
        #expect(MenuBarDisplay.from(connectionState: .connected,
                                    status: statusWithCapture(state: state, paused: false))
            == MenuBarDisplay(symbol: "exclamationmark.circle", tint: .red,
                              label: "Chronicle capture not running"))
    }
}
