import Foundation
import Observation

/// Tracks the "we booted with capture paused" condition that drives the paused
/// banner in the search popover. The banner is a one-shot startup notification:
/// it appears only when the daemon's FIRST status snapshot shows paused, and
/// it auto-dismisses the moment capture resumes (from the banner itself, from
/// Settings, or from any other path).
///
/// `evaluate(status:)` is the single entry point. It's safe to call on every
/// status push; the latch logic inside ignores everything after the first
/// snapshot except resume edges.
@Observable
final class StartupAlertState {
    /// Set when the user hits the X button (or `resumeCapture` succeeds, via
    /// the auto-dismiss edge below). Once true, the banner stays hidden for
    /// the rest of the session even if capture goes back to paused.
    var dismissed: Bool = false

    /// True iff the FIRST observed status snapshot reported `paused == true`.
    /// Latched on the very first call to `evaluate(status:)` and never
    /// re-evaluated — a later manual pause does not flip this back on.
    private(set) var pausedOnBoot: Bool = false

    private var hasEvaluated: Bool = false

    func evaluate(status: StatusResponse) {
        let paused = status.data.capture?.paused == true
        if !hasEvaluated {
            hasEvaluated = true
            pausedOnBoot = paused
        } else if pausedOnBoot && !paused {
            // Capture resumed (from anywhere). Auto-dismiss so the banner
            // doesn't linger after the condition that triggered it is gone.
            dismissed = true
        }
    }
}
