//! Owns the capture runtime and reconciles it against three independent
//! stop reasons: user pause, system sleep, display sleep.

/// What `reconcile()` decided to do, given the flags and current run state.
#[derive(Debug, PartialEq, Eq)]
enum ReconcileAction {
    Start,
    Stop,
    None,
}

/// Pure decision: capture should run only when none of the three flags is set.
fn decide(
    user_paused: bool,
    system_asleep: bool,
    display_asleep: bool,
    running: bool,
) -> ReconcileAction {
    let should_run = !user_paused && !system_asleep && !display_asleep;
    match (should_run, running) {
        (true, false) => ReconcileAction::Start,
        (false, true) => ReconcileAction::Stop,
        _ => ReconcileAction::None,
    }
}

#[cfg(test)]
mod tests {
    use super::{decide, ReconcileAction};

    #[test]
    fn decide_truth_table() {
        // All flags false + not running → should start
        assert_eq!(
            decide(false, false, false, false),
            ReconcileAction::Start
        );

        // All flags false + already running → no change
        assert_eq!(
            decide(false, false, false, true),
            ReconcileAction::None
        );

        // user_paused alone: running → stop
        assert_eq!(
            decide(true, false, false, true),
            ReconcileAction::Stop
        );

        // user_paused alone: not running → no change
        assert_eq!(
            decide(true, false, false, false),
            ReconcileAction::None
        );

        // system_asleep alone: running → stop
        assert_eq!(
            decide(false, true, false, true),
            ReconcileAction::Stop
        );

        // system_asleep alone: not running → no change
        assert_eq!(
            decide(false, true, false, false),
            ReconcileAction::None
        );

        // display_asleep alone: running → stop
        assert_eq!(
            decide(false, false, true, true),
            ReconcileAction::Stop
        );

        // display_asleep alone: not running → no change
        assert_eq!(
            decide(false, false, true, false),
            ReconcileAction::None
        );

        // All three flags set + running → stop
        assert_eq!(
            decide(true, true, true, true),
            ReconcileAction::Stop
        );

        // All three flags set + not running → no change
        assert_eq!(
            decide(true, true, true, false),
            ReconcileAction::None
        );

        // Mix: user_paused + system_asleep, running → stop
        assert_eq!(
            decide(true, true, false, true),
            ReconcileAction::Stop
        );

        // Mix: user_paused + system_asleep, not running → no change
        assert_eq!(
            decide(true, true, false, false),
            ReconcileAction::None
        );
    }
}
