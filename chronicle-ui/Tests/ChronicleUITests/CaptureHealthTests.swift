import Testing
@testable import ChronicleUI

@Suite("Capture health")
struct CaptureHealthTests {
    private func status(state: String, paused: Bool) -> StatusResponse {
        StatusResponse(type: "status", ok: true, data: StatusData(
            uptimeSecs: 0, version: "t",
            capture: CaptureStats(
                state: state, activeDisplays: 1, framesCaptured: 0, framesDropped: 0,
                framesProcessed: 0, framesFailed: 0, paused: paused),
            ocr: nil, audio: nil, storage: nil, transcription: nil))
    }

    @Test func noStatusIsNoStatus() {
        #expect(CaptureHealth.classify(nil) == .noStatus)
    }

    @Test func pausedWinsOverState() {
        #expect(CaptureHealth.classify(status(state: "running", paused: true)) == .paused)
    }

    @Test func runningIsRunning() {
        #expect(CaptureHealth.classify(status(state: "running", paused: false)) == .running)
    }

    @Test(arguments: ["unknown", "idle", "frobnicating"])
    func anyOtherStateIsNotRunning(state: String) {
        #expect(CaptureHealth.classify(status(state: state, paused: false)) == .notRunning)
    }
}
