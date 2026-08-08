import Foundation
import Observation

/// Tracks the "we booted with no transcription model" condition that drives the
/// transcription banner in the search popover. Mirrors `StartupAlertState`: a
/// one-shot startup notification that appears only when the first status
/// snapshot *carrying a transcription block* reports `.missing`, and
/// auto-dismisses once a later snapshot shows transcription is no longer in a
/// state worth warning about.
///
/// "First snapshot carrying a block" rather than "first snapshot": a daemon
/// older than this UI sends none, and those snapshots must not consume the
/// latch — see `evaluate(status:)`.
///
/// `evaluate(status:)` is the single entry point. It's safe to call on every
/// status push; once latched, everything after it is ignored except resolve
/// edges.
///
/// Phase 1 has no call to action — the banner just tells the truth. The
/// download button arrives with Phase 2 (Task 15).
@Observable
final class TranscriptionAlertState {
    /// Set when the user hits the X button (or a resolve edge auto-dismisses
    /// below). Once true, the banner stays hidden for the rest of the session.
    var dismissed: Bool = false

    /// True iff the first status snapshot that carried a transcription block
    /// reported `.missing`. Latched on the first call to `evaluate(status:)`
    /// whose status has a block — calls before that (an older daemon) leave it
    /// unlatched — and never re-evaluated afterwards, so a model deleted
    /// mid-session does not flip this back on.
    private(set) var missingOnFirstSnapshot: Bool = false

    private var hasEvaluated: Bool = false

    /// True once a provision has been seen in flight and until it resolves.
    /// Separate from the boot latch because the two raise the banner for
    /// different reasons: the latch says "you booted with nothing", this says
    /// "something is happening right now and its progress lives here".
    ///
    /// Without it, a switch started from Settings on a healthy boot would run
    /// with no progress anywhere in the popover, and its failure would have no
    /// Retry button — `missingOnFirstSnapshot` is false on that path and never
    /// re-evaluates by design.
    private var provisionEngaged: Bool = false

    var shouldShow: Bool { (missingOnFirstSnapshot || provisionEngaged) && !dismissed }

    func evaluate(status: StatusResponse) {
        // nil = a daemon too old to send the block at all. Nothing is known to
        // be wrong, so it neither latches the banner nor dismisses it; Settings
        // carries the "Requires daemon update" wording instead.
        //
        // The `let state` on the first-snapshot arm is load-bearing: without it
        // a nil block CONSUMES the latch (hasEvaluated flips, missing stays
        // false), and a `.missing` arriving after a mid-session daemon upgrade
        // could never raise the banner again. Reachable via popover reopen,
        // where `.task` re-runs with `lastStatus` already populated.
        let state = status.data.transcription?.state
        if !hasEvaluated, let state {
            hasEvaluated = true
            missingOnFirstSnapshot = state == .missing
        }
        guard let state else { return }

        // Latches on the way in, clears on the way out. `.error` deliberately
        // leaves it set: a failed download has to keep its banner, because
        // that is where Retry lives.
        if Self.isProvisioning(state) {
            provisionEngaged = true
        }

        if missingOnFirstSnapshot || provisionEngaged, Self.isResolved(state) {
            // Transcription became workable (from anywhere). Auto-dismiss so
            // the banner doesn't linger past the condition that raised it.
            provisionEngaged = false
            dismissed = true
        }
    }

    /// Whether a state means "an operation is running right now".
    ///
    /// Exhaustive for the same reason as `isResolved`: a new state must be
    /// classified deliberately rather than defaulting to "not in flight" and
    /// silently losing its progress UI.
    private static func isProvisioning(_ state: TranscriptionState) -> Bool {
        switch state {
        case .downloading, .verifying, .loading:
            return true
        case .missing, .error, .ready, .unknown:
            return false
        }
    }

    /// Whether a state means "nothing left to warn the user about".
    ///
    /// Exhaustive on purpose — no `default:` arm. A state added to
    /// `TranscriptionState` later must fail to compile here and be classified
    /// deliberately, rather than silently inheriting resolved treatment.
    private static func isResolved(_ state: TranscriptionState) -> Bool {
        switch state {
        case .ready, .unknown:
            // `.unknown` rides with `.ready` by decision (plan Decisions,
            // 2026-08-02): a state this UI doesn't recognize is not treated as
            // a problem. Accepted trade-off — if a future state means something
            // IS broken, this stays silent rather than crying wolf.
            return true
        case .missing, .downloading, .verifying, .loading, .error:
            // Still in flight or still broken — keep the banner up.
            return false
        }
    }
}
