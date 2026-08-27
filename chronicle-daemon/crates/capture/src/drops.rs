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

/// Record one discarded frame.
///
/// Takes the error rather than a pre-classified cause so the callback carries
/// no logic at all: every step from `TrySendError` to counter field sits
/// inside something a test can call without a real `CMSampleBuffer`.
pub(crate) fn count_frame_drop<T>(
    counters: &CaptureDropCounters,
    err: &tokio::sync::mpsc::error::TrySendError<T>,
) {
    match err {
        tokio::sync::mpsc::error::TrySendError::Full(_) => {
            counters.full.fetch_add(1, Ordering::Relaxed)
        }
        tokio::sync::mpsc::error::TrySendError::Closed(_) => {
            counters.closed.fetch_add(1, Ordering::Relaxed)
        }
    };
}

/// Send one frame and account for the outcome.
///
/// The whole "try_send, then classify, then count" path in one place, so a test
/// can drive it with a real channel. The callback body reduces to a call, which
/// is the point: deleting a counter increment *inside* an ObjC delivery
/// callback is invisible to every test, because the callback needs a real
/// `CMSampleBuffer` to invoke. Here it is not invisible.
///
/// `captured`/`dropped` are the engine's per-engine counters (`CaptureStats`
/// reports `dropped` over IPC, unchanged); `drops` is the process-lifetime set
/// the reporter reads.
pub(crate) fn send_frame<T>(
    sender: &tokio::sync::mpsc::Sender<T>,
    frame: T,
    captured: &AtomicU64,
    dropped: &AtomicU64,
    drops: &CaptureDropCounters,
) {
    match sender.try_send(frame) {
        Ok(()) => {
            captured.fetch_add(1, Ordering::Relaxed);
        }
        Err(e) => {
            dropped.fetch_add(1, Ordering::Relaxed);
            count_frame_drop(drops, &e);
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

    #[tokio::test]
    async fn try_send_error_maps_to_the_right_counter() {
        // Drive real TrySendError values: the classification is the step that
        // would invert silently.
        let (full_tx, _full_rx) = tokio::sync::mpsc::channel::<u8>(1);
        full_tx.try_send(1).unwrap();
        let full_err = full_tx.try_send(2).unwrap_err();

        let (closed_tx, closed_rx) = tokio::sync::mpsc::channel::<u8>(1);
        drop(closed_rx);
        let closed_err = closed_tx.try_send(1).unwrap_err();

        let c = CaptureDropCounters::default();

        count_frame_drop(&c, &full_err);
        assert_eq!(c.snapshot().full, 1);
        assert_eq!(c.snapshot().closed, 0, "full must not bump closed");

        count_frame_drop(&c, &closed_err);
        assert_eq!(c.snapshot().closed, 1);
        assert_eq!(c.snapshot().full, 1, "closed must not bump full");
    }

    #[tokio::test]
    async fn send_frame_counts_success_full_and_closed() {
        let captured = AtomicU64::new(0);
        let dropped = AtomicU64::new(0);
        let drops = CaptureDropCounters::default();

        // Success bumps captured only.
        let (tx, rx) = tokio::sync::mpsc::channel::<u8>(1);
        send_frame(&tx, 1, &captured, &dropped, &drops);
        assert_eq!(captured.load(Ordering::Relaxed), 1);
        assert_eq!(dropped.load(Ordering::Relaxed), 0);
        assert_eq!(drops.snapshot(), CaptureDropSnapshot::default());

        // Channel now full: bumps dropped and `full`, never captured.
        send_frame(&tx, 2, &captured, &dropped, &drops);
        assert_eq!(
            captured.load(Ordering::Relaxed),
            1,
            "a drop is not a capture"
        );
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
        assert_eq!(drops.snapshot().full, 1);
        assert_eq!(drops.snapshot().closed, 0);

        // Receiver gone: bumps dropped and `closed`.
        drop(rx);
        send_frame(&tx, 3, &captured, &dropped, &drops);
        assert_eq!(dropped.load(Ordering::Relaxed), 2);
        assert_eq!(drops.snapshot().closed, 1);
        assert_eq!(drops.snapshot().full, 1, "closed must not bump full");
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
