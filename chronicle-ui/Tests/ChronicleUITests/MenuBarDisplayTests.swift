import Testing
@testable import ChronicleUI

@Suite("Menu bar display")
struct MenuBarDisplayTests {
    private func status(state: String, paused: Bool) -> StatusResponse {
        StatusResponse(type: "status", ok: true, data: StatusData(
            uptimeSecs: 0, version: "t",
            capture: CaptureStats(
                state: state, activeDisplays: 1, framesCaptured: 0, framesDropped: 0,
                framesProcessed: 0, framesFailed: 0, paused: paused),
            ocr: nil, audio: nil, storage: nil, transcription: nil))
    }

    private func statusWithoutCapture() -> StatusResponse {
        StatusResponse(type: "status", ok: true, data: StatusData(
            uptimeSecs: 0, version: "t", capture: nil, ocr: nil, audio: nil,
            storage: nil, transcription: nil))
    }

    @Test func disconnectedIgnoresStatus() {
        #expect(MenuBarDisplay.from(connectionState: .disconnected,
                                    status: status(state: "running", paused: false))
            == MenuBarDisplay(symbol: "xmark.circle", tint: .red,
                              label: "Chronicle daemon disconnected"))
    }

    @Test func connectingIgnoresStatus() {
        #expect(MenuBarDisplay.from(connectionState: .connecting,
                                    status: status(state: "running", paused: false))
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
                                    status: status(state: "paused", paused: true)) == paused)
        #expect(MenuBarDisplay.from(connectionState: .connected,
                                    status: status(state: "running", paused: true)) == paused)
    }

    @Test func runningIsActive() {
        #expect(MenuBarDisplay.from(connectionState: .connected,
                                    status: status(state: "running", paused: false))
            == MenuBarDisplay(symbol: "record.circle.fill", tint: .green,
                              label: "Chronicle capture active"))
    }

    @Test(arguments: ["unknown", "idle", "stopping", "poisoned", "frobnicating"])
    func anyOtherStateIsNotRunning(state: String) {
        #expect(MenuBarDisplay.from(connectionState: .connected,
                                    status: status(state: state, paused: false))
            == MenuBarDisplay(symbol: "exclamationmark.circle", tint: .red,
                              label: "Chronicle capture not running"))
    }
}
