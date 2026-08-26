//! Counters for frames the ScreenCaptureKit delivery callback discarded.

use std::sync::atomic::{AtomicU64, Ordering};

/// Frames the SCK screen callback discarded, split by cause.
///
/// Separate from — and in addition to — the engine's own `frames_dropped`,
/// which stays per-engine because `CaptureStats` reports it over IPC and that
/// behaviour is unchanged. The handler bumps both.
///
/// Owned by the daemon's `main` and passed in through `CaptureConfig`, because
/// `CaptureEngine` is rebuilt on every resume. An engine-owned counter would
/// reset each cycle, which is exactly the loss this design removes.
#[derive(Default)]
pub struct CaptureDropCounters {
    /// Channel full — downstream is slow.
    pub full: AtomicU64,
    /// Channel closed — downstream is gone.
    pub closed: AtomicU64,
}

/// Point-in-time copy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CaptureDropSnapshot {
    pub full: u64,
    pub closed: u64,
}

impl CaptureDropCounters {
    pub fn snapshot(&self) -> CaptureDropSnapshot {
        CaptureDropSnapshot {
            full: self.full.load(Ordering::Relaxed),
            closed: self.closed.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn snapshot_reads_both_causes() {
        // Distinct values: a swapped pair inside snapshot() must fail this
        // test, and two fields sharing a value would hide exactly that.
        let c = CaptureDropCounters::default();
        c.full.fetch_add(7, Ordering::Relaxed);
        c.closed.fetch_add(2, Ordering::Relaxed);

        let s = c.snapshot();

        assert_eq!(s.full, 7);
        assert_eq!(s.closed, 2);
    }

    #[test]
    fn default_config_gets_its_own_counters() {
        let a = crate::CaptureConfig::default();
        let b = crate::CaptureConfig::default();
        a.drop_counters.full.fetch_add(1, Ordering::Relaxed);
        assert_eq!(
            b.drop_counters.snapshot().full,
            0,
            "each Default config must get a fresh set, or tests would share state"
        );
    }
}
