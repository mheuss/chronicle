//! Off-thread reporting for real-time drop counters.
//!
//! The capture and audio callbacks count; nothing else. This module turns
//! those counts into at most one log line per period — see ADR-013.

// Nothing in a non-test build calls this yet: the task that spawns the
// reporter in `main` lands next on this branch and removes this attribute.
// Scoped to the module rather than the crate so it cannot mask anything else.
#![allow(dead_code)]

/// One period's reading of every drop counter the daemon tracks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct DropTotals {
    pub mic_full: u64,
    pub mic_closed: u64,
    pub mic_convert_failed: u64,
    pub system_full: u64,
    pub system_closed: u64,
    pub frames_full: u64,
    pub frames_closed: u64,
}

/// Previous-period totals. `Default` means "nothing seen yet", which is why
/// counters that are already non-zero on the first observation are reported
/// in full rather than swallowed as a baseline.
#[derive(Default)]
pub(crate) struct ReporterState {
    previous: DropTotals,
}

impl ReporterState {
    /// Fold one reading in. Returns the line to log, or `None` when nothing
    /// was dropped this period.
    ///
    /// The counters are monotonic by construction — they are created once and
    /// never rebuilt — so this is plain subtraction with no restart case. If a
    /// future change makes a counter rebuildable these become underflow panics
    /// in debug and wrap in release, which is the loud failure we want. Do not
    /// "harden" them with `saturating_sub`: that converts a broken invariant
    /// into quiet wrong numbers.
    pub(crate) fn observe(&mut self, current: DropTotals) -> Option<String> {
        // Fixed order: the output is greppable and the tests can assert on it.
        let fields: [(&str, u64, u64); 7] = [
            (
                "mic_full",
                current.mic_full - self.previous.mic_full,
                current.mic_full,
            ),
            (
                "mic_closed",
                current.mic_closed - self.previous.mic_closed,
                current.mic_closed,
            ),
            (
                "mic_convert_failed",
                current.mic_convert_failed - self.previous.mic_convert_failed,
                current.mic_convert_failed,
            ),
            (
                "system_full",
                current.system_full - self.previous.system_full,
                current.system_full,
            ),
            (
                "system_closed",
                current.system_closed - self.previous.system_closed,
                current.system_closed,
            ),
            (
                "frames_full",
                current.frames_full - self.previous.frames_full,
                current.frames_full,
            ),
            (
                "frames_closed",
                current.frames_closed - self.previous.frames_closed,
                current.frames_closed,
            ),
        ];
        self.previous = current;

        let deltas: Vec<String> = fields
            .iter()
            .filter(|(_, delta, _)| *delta > 0)
            .map(|(name, delta, _)| format!("{name}={delta}"))
            .collect();
        if deltas.is_empty() {
            return None;
        }
        let totals: Vec<String> = fields
            .iter()
            .filter(|(_, delta, _)| *delta > 0)
            .map(|(name, _, total)| format!("{name}={total}"))
            .collect();

        Some(format!(
            "dropped since the previous report: {} (totals: {})",
            deltas.join(", "),
            totals.join(", "),
        ))
    }
}

/// How often the reporter reads the counters.
///
/// Timing resolution *inside* a drop burst is HEU-548's job, not this
/// ticket's, so 30 s is the right granularity for what HEU-653 owns.
#[allow(dead_code)] // read by the reporter task, which a later task adds
pub(crate) const REPORT_PERIOD: std::time::Duration = std::time::Duration::from_secs(30);

#[cfg(test)]
mod tests {
    use super::*;

    fn totals(mic_full: u64, system_full: u64, frames_full: u64) -> DropTotals {
        DropTotals {
            mic_full,
            system_full,
            frames_full,
            ..Default::default()
        }
    }

    #[test]
    fn first_observation_all_zero_reports_nothing() {
        let mut state = ReporterState::default();
        assert_eq!(state.observe(DropTotals::default()), None);
    }

    #[test]
    fn first_observation_non_zero_reports_full_values() {
        // A burst during model load must not be absorbed as a baseline.
        let mut state = ReporterState::default();
        let line = state.observe(totals(12, 0, 0)).expect("expected a line");
        assert!(line.contains("mic_full=12"), "{line}");
        assert!(line.contains("totals: mic_full=12"), "{line}");
    }

    #[test]
    fn ordinary_growth_reports_the_difference() {
        let mut state = ReporterState::default();
        state.observe(totals(10, 0, 0));
        let line = state.observe(totals(14, 0, 0)).expect("expected a line");
        assert!(line.contains("mic_full=4"), "{line}");
        assert!(line.contains("totals: mic_full=14"), "{line}");
    }

    #[test]
    fn several_fields_share_one_line_in_fixed_order() {
        let mut state = ReporterState::default();
        let line = state.observe(totals(1, 2, 3)).expect("expected a line");
        let mic = line.find("mic_full").unwrap();
        let sys = line.find("system_full").unwrap();
        let frames = line.find("frames_full").unwrap();
        assert!(mic < sys && sys < frames, "fixed field order: {line}");
    }

    #[test]
    fn a_field_that_stops_growing_is_omitted() {
        let mut state = ReporterState::default();
        state.observe(totals(5, 0, 0));
        let line = state.observe(totals(5, 2, 0)).expect("expected a line");
        assert!(!line.contains("mic_full=0"), "{line}");
        assert!(line.contains("system_full=2"), "{line}");
    }

    #[test]
    fn no_growth_anywhere_reports_nothing() {
        let mut state = ReporterState::default();
        state.observe(totals(5, 5, 5));
        assert_eq!(state.observe(totals(5, 5, 5)), None);
    }

    #[test]
    fn every_field_maps_to_its_own_name() {
        // Without this, swapping mic_closed and system_closed in `observe`
        // passes every other test in this module.
        let cases: [(&str, fn(&mut DropTotals)); 7] = [
            ("mic_full", |t| t.mic_full = 1),
            ("mic_closed", |t| t.mic_closed = 1),
            ("mic_convert_failed", |t| t.mic_convert_failed = 1),
            ("system_full", |t| t.system_full = 1),
            ("system_closed", |t| t.system_closed = 1),
            ("frames_full", |t| t.frames_full = 1),
            ("frames_closed", |t| t.frames_closed = 1),
        ];
        for (name, set) in cases {
            let mut totals = DropTotals::default();
            set(&mut totals);
            let mut state = ReporterState::default();
            let line = state
                .observe(totals)
                .unwrap_or_else(|| panic!("{name}: no line"));
            assert!(line.contains(&format!("{name}=1")), "{name}: {line}");
            // Exactly one field reported — no bleed into a neighbour.
            assert_eq!(line.matches('=').count(), 2, "{name}: {line}");
        }
    }

    #[test]
    fn totals_are_cumulative_not_per_period() {
        let mut state = ReporterState::default();
        state.observe(totals(100, 0, 0));
        let line = state.observe(totals(103, 0, 0)).expect("expected a line");
        assert!(line.contains("mic_full=3"), "delta: {line}");
        assert!(line.contains("totals: mic_full=103"), "total: {line}");
    }
}
