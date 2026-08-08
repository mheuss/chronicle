import Foundation
import Observation

/// Decides whether the search popover shows its transcription banner.
///
/// Two independent conditions raise it, and keeping them separate is the whole
/// design: `bootedWithoutModel` says "you started with nothing", while
/// `provisionEngaged` says "something is happening or has just failed, and its
/// progress and Retry live here". Either one is enough.
///
/// It is NOT one-shot. An earlier version auto-dismissed on resolve, which
/// meant the first successful provision silenced every later one — see the
/// comment on the resolve edge in `evaluate(status:)`.
///
/// "First snapshot carrying a block" rather than "first snapshot": a daemon
/// older than this UI sends none, and those snapshots must not consume the
/// boot latch — see `evaluate(status:)`.
///
/// `evaluate(status:)` is the single entry point and is safe to call on every
/// status push.
@Observable
final class TranscriptionAlertState {
    /// Set ONLY by the X button. Nothing in `evaluate` sets it — that
    /// conflation is what made the banner un-raisable after the first resolve.
    ///
    /// Cleared when genuinely new information arrives — a new operation
    /// starting, or one failing — so a dismissal silences what the user put
    /// away rather than everything after it.
    ///
    /// The clear is edge-triggered, keyed on `lastRaisingState`. The re-entry
    /// it guards against is a popover REOPEN: `evaluate` runs from a
    /// `.task(id:)` keyed on the state, so an unchanged state does not re-run
    /// it, but re-appearing re-runs the task with whatever state is current.
    /// Level-triggered, that would silently undo a dismissal.
    var dismissed: Bool = false

    /// True iff the first status snapshot that carried a transcription block
    /// reported `.missing`. Latched on the first call to `evaluate(status:)`
    /// whose status has a block — calls before that (an older daemon) leave it
    /// unlatched — and never re-latched afterwards, so a model deleted
    /// mid-session does not flip this back on.
    ///
    /// Cleared once transcription resolves, because by then the condition it
    /// describes is over. Only the class clears it; `hasEvaluated` is what
    /// prevents re-latching, so clearing here cannot resurrect the latch.
    private(set) var bootedWithoutModel: Bool = false

    private var hasEvaluated: Bool = false

    /// True while an operation is in flight, and through a failure — `.error`
    /// keeps it set because that is the state that owns the Retry button.
    /// Cleared only on resolve.
    ///
    /// Without it, a switch started from Settings on a healthy boot would run
    /// with no progress anywhere in the popover and its failure would offer no
    /// Retry: `bootedWithoutModel` is false on that path and never re-latches.
    private var provisionEngaged: Bool = false

    /// The last state that put the banner on screen, so `evaluate` can tell an
    /// edge from a level. Without it, a popover reopen — which re-runs
    /// `.task(id:)` with the state unchanged — is indistinguishable from a
    /// genuine transition.
    private var lastRaisingState: TranscriptionState?

    var shouldShow: Bool { (bootedWithoutModel || provisionEngaged) && !dismissed }

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
            bootedWithoutModel = state == .missing
        }
        guard let state else { return }

        // Latches on the way in, clears on the way out. `.error` raises it too
        // and keeps it set: a failure is where Retry lives, and a fast one can
        // land between two 30 s status ticks without any in-flight snapshot
        // ever being observed — so keying only on `isProvisioning` would leave
        // the user with a broken switch and no way to retry it.
        if Self.raisesBanner(state) {
            // A dismissal covers the thing that was on screen, not everything
            // that follows. Two edges bring genuinely NEW information and so
            // clear it:
            //
            //   • a new operation starting. `!provisionEngaged` catches the
            //     usual case, but not a retry after a failure — `.error` is
            //     not a resolve, so the flag is still set. Coming out of
            //     `.error` into an in-flight state can only happen because
            //     the daemon accepted a fresh request, so that is the edge.
            //   • an operation failing. Dismissing "Downloading base model…"
            //     is not dismissing "it failed" — and the Retry button lives
            //     only on the banner, so swallowing this left no way back
            //     inside the session.
            //
            // Both are edges, not levels. `.downloading → .verifying →
            // .loading` is one operation progressing and must NOT re-raise a
            // banner the user put away; nor may a popover reopen, which
            // re-runs `.task` with the state unchanged.
            let startsNewOperation =
                !provisionEngaged || (lastRaisingState == .error && Self.isProvisioning(state))
            let justFailed = state == .error && lastRaisingState != .error
            if startsNewOperation || justFailed {
                dismissed = false
            }
            provisionEngaged = true
            lastRaisingState = state
        }

        if Self.isResolved(state) {
            // Transcription became workable (from anywhere). Clear the two
            // CAUSES rather than setting `dismissed`, so the banner can be
            // raised again by a later provision.
            //
            // Setting `dismissed` here is what the plan's Step 3 said, and it
            // was wrong once "in flight" joined the same predicate as a
            // session-sticky dismiss: the first resolve killed the banner for
            // the rest of the session. Not a rare path — the daemon enters
            // `Loading` at cell creation before the IPC server starts, so
            // merely opening the popover during a daemon boot engaged the
            // flag, `.ready` burned the dismiss, and every later switch ran
            // with no progress and no Retry. `dismissed` now means only what
            // its name says: the user clicked X.
            //
            // `hasEvaluated` is deliberately NOT cleared, so the boot latch
            // still never re-arms — a model deleted mid-session does not
            // raise the banner again, which is the documented contract above.
            provisionEngaged = false
            bootedWithoutModel = false
        }
    }

    /// Whether a state is worth putting the banner on screen for: an
    /// operation running, or one that failed and needs Retry.
    ///
    /// Exhaustive for the same reason as `isResolved` — a new state must be
    /// classified deliberately rather than defaulting to "say nothing".
    private static func raisesBanner(_ state: TranscriptionState) -> Bool {
        switch state {
        case .downloading, .verifying, .loading, .error:
            return true
        case .missing, .ready, .unknown:
            // `.missing` is the boot latch's job, not this one — raising here
            // would re-show the banner every time a dismissed missing-model
            // status is polled.
            return false
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
