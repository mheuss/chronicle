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

    // Phase 2 reverses the Phase 1 behavior this replaces. Reopening the
    // popover mid-download re-runs `.task` against a `.downloading` snapshot,
    // and the banner is where the progress lives — hiding it would leave a
    // download the user started with nowhere to be seen.
    @Test func showsWhenFirstSnapshotIsDownloading() {
        let alert = TranscriptionAlertState()
        alert.evaluate(status: status(.downloading))
        #expect(alert.shouldShow)
    }

    // A switch started from Settings on a healthy boot: the first snapshot is
    // `.ready`, so the missing-latch never fires, but the progress still has
    // to surface here.
    @Test func showsForAProvisionStartedAfterAHealthyBoot() {
        let alert = TranscriptionAlertState()
        alert.evaluate(status: status(.ready))
        #expect(!alert.shouldShow)
        alert.evaluate(status: status(.downloading))
        #expect(alert.shouldShow)
    }

    // The failure of a download the user started must stay on screen — that
    // is where the Retry button lives.
    @Test func staysVisibleWhenAProvisionFails() {
        let alert = TranscriptionAlertState()
        alert.evaluate(status: status(.ready))
        alert.evaluate(status: status(.downloading))
        alert.evaluate(status: status(.error))
        #expect(alert.shouldShow)
    }

    // ...and clears once it succeeds.
    @Test func dismissesWhenAProvisionSucceeds() {
        let alert = TranscriptionAlertState()
        alert.evaluate(status: status(.ready))
        alert.evaluate(status: status(.downloading))
        alert.evaluate(status: status(.ready))
        #expect(!alert.shouldShow)
    }

    // Resolving must clear the CAUSE, not set `dismissed` — otherwise the
    // first successful provision kills the banner for the whole session and
    // every later switch runs with no progress and no Retry.
    @Test func aSecondProvisionShowsAgain() {
        let alert = TranscriptionAlertState()
        alert.evaluate(status: status(.ready))
        alert.evaluate(status: status(.downloading))
        alert.evaluate(status: status(.ready))
        alert.evaluate(status: status(.downloading))
        #expect(alert.shouldShow)
    }

    // The ordinary path that made the above routine rather than rare: the
    // daemon enters `.loading` at cell creation, before the IPC server
    // starts, so merely opening the popover during a daemon boot engaged the
    // in-flight flag. Resolving it must not cost the user their banner.
    @Test func aBootLoadThenLaterSwitchStillShows() {
        let alert = TranscriptionAlertState()
        alert.evaluate(status: status(.loading))
        alert.evaluate(status: status(.ready))
        alert.evaluate(status: status(.downloading))
        #expect(alert.shouldShow)
    }

    // The sharp end: a failed SECOND switch still needs its Retry button.
    @Test func aSecondProvisionFailureStillShows() {
        let alert = TranscriptionAlertState()
        alert.evaluate(status: status(.ready))
        alert.evaluate(status: status(.downloading))
        alert.evaluate(status: status(.ready))
        alert.evaluate(status: status(.downloading))
        alert.evaluate(status: status(.error))
        #expect(alert.shouldShow)
    }

    // `dismissed` means one thing only: the user clicked X. Nothing in
    // `evaluate` may set it, or the banner stops being raisable.
    @Test func resolvingNeverSetsDismissed() {
        let alert = TranscriptionAlertState()
        alert.evaluate(status: status(.missing))
        alert.evaluate(status: status(.downloading))
        alert.evaluate(status: status(.ready))
        #expect(!alert.dismissed, "only the X button sets this")
        #expect(!alert.shouldShow, "hidden because the causes cleared, not because it was dismissed")
    }

    // Guards the resolve arm itself: a healthy first snapshot must leave the
    // state untouched rather than burning anything.
    @Test func aHealthyFirstSnapshotChangesNothing() {
        let alert = TranscriptionAlertState()
        alert.evaluate(status: status(.ready))
        #expect(!alert.dismissed)
        #expect(!alert.missingOnFirstSnapshot)
        alert.evaluate(status: status(.missing))
        #expect(!alert.shouldShow, "the boot latch does not re-arm mid-session")
    }

    // An old daemon sends no block. Nothing is known to be wrong, so no banner
    // — Settings carries the "Requires daemon update" wording instead.
    @Test func staysHiddenWhenDaemonSendsNoBlock() {
        let alert = TranscriptionAlertState()
        alert.evaluate(status: statusWithoutBlock())
        #expect(!alert.shouldShow)
    }

    // A daemon upgraded mid-session starts sending the block. The nil snapshot
    // must NOT have consumed the first-snapshot latch, or the banner could
    // never appear afterwards. Reachable by reopening the popover, where
    // `.task` re-runs with `lastStatus` already populated.
    @Test func latchSurvivesASnapshotWithNoBlock() {
        let alert = TranscriptionAlertState()
        alert.evaluate(status: statusWithoutBlock())
        alert.evaluate(status: status(.missing))
        #expect(alert.shouldShow)
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
