//! Counters for buffers the real-time audio callbacks discarded.

use std::sync::atomic::{AtomicU64, Ordering};

/// Buffers the real-time callbacks discarded.
///
/// Incremented on the audio thread with a single relaxed `fetch_add` —
/// ADR-013 allows counting there and nothing else. All reporting happens
/// off-thread in the daemon's drop reporter.
///
/// Owned by `AudioPipeline`, which is constructed once and never rebuilt, so
/// these counts are monotonic for the life of the process. That is what lets
/// the reporter compute deltas by plain subtraction with no restart case.
#[derive(Default)]
pub struct AudioDropCounters {
    /// Mic buffer dropped: encoding channel full (downstream slow).
    pub mic_full: AtomicU64,
    /// Mic buffer dropped: encoding channel closed (downstream gone).
    pub mic_closed: AtomicU64,
    /// Mic buffer dropped: `AVAudioConverter` returned `Error`.
    pub mic_convert_failed: AtomicU64,
    /// System audio buffer dropped: encoding channel full.
    pub system_full: AtomicU64,
    /// System audio buffer dropped: encoding channel closed.
    pub system_closed: AtomicU64,
}

/// Point-in-time copy, mirroring `CountersSnapshot` in the daemon's
/// `pipeline/counters.rs`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AudioDropSnapshot {
    pub mic_full: u64,
    pub mic_closed: u64,
    pub mic_convert_failed: u64,
    pub system_full: u64,
    pub system_closed: u64,
}

impl AudioDropCounters {
    pub fn snapshot(&self) -> AudioDropSnapshot {
        AudioDropSnapshot {
            mic_full: self.mic_full.load(Ordering::Relaxed),
            mic_closed: self.mic_closed.load(Ordering::Relaxed),
            mic_convert_failed: self.mic_convert_failed.load(Ordering::Relaxed),
            system_full: self.system_full.load(Ordering::Relaxed),
            system_closed: self.system_closed.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_reads_every_field() {
        let c = AudioDropCounters::default();
        c.mic_full.fetch_add(3, Ordering::Relaxed);
        c.mic_closed.fetch_add(1, Ordering::Relaxed);
        c.mic_convert_failed.fetch_add(4, Ordering::Relaxed);
        c.system_full.fetch_add(1, Ordering::Relaxed);
        c.system_closed.fetch_add(5, Ordering::Relaxed);

        let s = c.snapshot();

        assert_eq!(s.mic_full, 3);
        assert_eq!(s.mic_closed, 1);
        assert_eq!(s.mic_convert_failed, 4);
        assert_eq!(s.system_full, 1);
        assert_eq!(s.system_closed, 5);
    }

    #[test]
    fn snapshot_of_fresh_counters_is_all_zero() {
        assert_eq!(
            AudioDropCounters::default().snapshot(),
            AudioDropSnapshot::default()
        );
    }
}
