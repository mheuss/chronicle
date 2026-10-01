import Testing
@testable import ChronicleUI

@Suite("Capture health")
struct CaptureHealthTests {
    @Test func noStatusIsNoStatus() {
        #expect(CaptureHealth.classify(nil) == .noStatus)
    }

    @Test func statusWithoutCaptureIsNoStatus() {
        #expect(CaptureHealth.classify(statusWithoutCapture()) == .noStatus)
    }

    @Test func pausedWinsOverState() {
        #expect(CaptureHealth.classify(statusWithCapture(state: "running", paused: true)) == .paused)
    }

    @Test func runningIsRunning() {
        #expect(CaptureHealth.classify(statusWithCapture(state: "running", paused: false)) == .running)
    }

    @Test(arguments: ["unknown", "idle", "frobnicating"])
    func anyOtherStateIsNotRunning(state: String) {
        #expect(CaptureHealth.classify(statusWithCapture(state: state, paused: false)) == .notRunning)
    }
}
