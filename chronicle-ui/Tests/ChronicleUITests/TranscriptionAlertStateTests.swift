import Testing
@testable import ChronicleUI

// Swift Testing, matching the suite (codex plan review, important 1).
@Suite("TranscriptionAlertState latch")
struct TranscriptionAlertStateTests {
    private func status(_ state: TranscriptionState) -> StatusResponse {
        // Minimal literal — only transcription matters to the latch.
        StatusResponse(type: "status", ok: true, data: StatusData(
            uptimeSecs: 0, version: "t", capture: nil, ocr: nil, audio: nil,
            storage: nil,
            transcription: TranscriptionStats(
                state: state, variant: "base", loadedVariant: nil, error: nil,
                downloadBytes: nil, downloadTotalBytes: nil, models: [])))
    }

    /// An old daemon sends no transcription block at all.
    private func statusWithoutBlock() -> StatusResponse {
        StatusResponse(type: "status", ok: true, data: StatusData(
            uptimeSecs: 0, version: "t", capture: nil, ocr: nil, audio: nil,
            storage: nil, transcription: nil))
    }

    @Test func showsWhenFirstSnapshotIsMissing() {
        let alert = TranscriptionAlertState()
        alert.evaluate(status: status(.missing))
        #expect(alert.shouldShow)
    }

    @Test func staysHiddenWhenFirstSnapshotIsReady() {
        let alert = TranscriptionAlertState()
        alert.evaluate(status: status(.ready))
        #expect(!alert.shouldShow)
    }

    @Test func autoDismissesOnReadyEdge() {
        let alert = TranscriptionAlertState()
        alert.evaluate(status: status(.missing))
        alert.evaluate(status: status(.ready))
        #expect(!alert.shouldShow)
    }

    @Test func manualDismissSticks() {
        let alert = TranscriptionAlertState()
        alert.evaluate(status: status(.missing))
        alert.dismissed = true
        alert.evaluate(status: status(.missing))
        #expect(!alert.shouldShow)
    }

    // Pins the settled cross-version choice (Decisions, 2026-08-02): a state
    // this UI doesn't recognize is treated as ready, not as a problem.
    @Test func unknownIsTreatedAsReady() {
        let alert = TranscriptionAlertState()
        alert.evaluate(status: status(.unknown))
        #expect(!alert.shouldShow)
    }

    // And it auto-dismisses on the unknown edge, same as the ready edge —
    // otherwise a newer daemon would strand the banner open forever.
    @Test func autoDismissesOnUnknownEdge() {
        let alert = TranscriptionAlertState()
        alert.evaluate(status: status(.missing))
        alert.evaluate(status: status(.unknown))
        #expect(!alert.shouldShow)
    }

    // The latch only fires on `.missing`, so an in-flight state on the first
    // snapshot must not raise the banner — Phase 2 owns progress UI.
    @Test func staysHiddenWhenFirstSnapshotIsDownloading() {
        let alert = TranscriptionAlertState()
        alert.evaluate(status: status(.downloading))
        #expect(!alert.shouldShow)
    }

    // An old daemon sends no block. Nothing is known to be wrong, so no banner
    // — Settings carries the "Requires daemon update" wording instead.
    @Test func staysHiddenWhenDaemonSendsNoBlock() {
        let alert = TranscriptionAlertState()
        alert.evaluate(status: statusWithoutBlock())
        #expect(!alert.shouldShow)
    }

    // A still-unresolved state must NOT auto-dismiss — the banner has to
    // survive until transcription actually works.
    @Test func staysVisibleWhileStillUnresolved() {
        let alert = TranscriptionAlertState()
        alert.evaluate(status: status(.missing))
        alert.evaluate(status: status(.error))
        #expect(alert.shouldShow)
    }
}
