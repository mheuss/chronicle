//! The scheduled retention cleanup task.
//!
//! One deadline governs the loop: sleep until it, run, recompute. There is no
//! `tokio::time::interval` and no separate due check, and adding either back
//! would reintroduce a scheduling bug — three earlier designs did exactly that.
//! See HEU-629 before changing the shape here.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chronicle_storage::{CleanupStats, Storage, StorageError};

/// How long after startup the first run may fire.
///
/// Short enough that an ordinary session reaches it. A session shorter than
/// this never cleans — an accepted residual gap. The original 15 minutes was
/// guarding against HEU-547's 249-second startup stall, which is fixed.
pub(crate) const CLEANUP_START_DELAY: Duration = Duration::from_secs(3 * 60);

/// Time from one run finishing to the next starting.
pub(crate) const CLEANUP_PERIOD: Duration = Duration::from_secs(6 * 60 * 60);

/// Config key holding the last completed run's wall-clock time, in ms.
pub(crate) const LAST_CLEANUP_KEY: &str = "last_cleanup_ms";

/// When the first run of this process is due, in wall-clock ms.
///
/// `last_raw` is the stored `last_cleanup_ms` value, exactly as read. The four
/// reading rules:
///
/// * absent — never run, due now
/// * unparseable — treated as absent, so corrupt state cannot wedge retention
///   off permanently
/// * in the future (the clock moved backwards) — due now, not waited out
/// * otherwise — due one period after the stored time
///
/// The start delay floors all four.
pub(crate) fn initial_deadline_ms(
    now_ms: i64,
    last_raw: Option<String>,
    start_delay_ms: i64,
    period_ms: i64,
) -> i64 {
    let start_delay_end = now_ms.saturating_add(start_delay_ms);
    let due = match last_raw
        .as_deref()
        .and_then(|s| s.trim().parse::<i64>().ok())
    {
        None => now_ms,
        Some(last) if last > now_ms => now_ms,
        Some(last) => last.saturating_add(period_ms),
    };
    std::cmp::max(start_delay_end, due)
}

/// The scheduler's four dependencies, behind one seam.
///
/// Production uses [`StorageCleanupOps`]. Tests supply a fake with a
/// controllable run duration, a panicking variant, and a clock that tracks
/// tokio's paused timer.
#[async_trait]
pub(crate) trait CleanupOps: Send + Sync {
    /// Wall-clock now, in ms. Injected so paused-time tests move it with the
    /// timer.
    fn now_ms(&self) -> i64;
    /// Run one cleanup pass.
    async fn run_cleanup(&self) -> Result<CleanupStats, StorageError>;
    /// Read the stored `last_cleanup_ms`, exactly as stored.
    async fn read_last_cleanup(&self) -> Result<Option<String>, StorageError>;
    /// Record a completed run's finish time.
    async fn write_last_cleanup(&self, value_ms: i64) -> Result<(), StorageError>;
}

/// The production implementation, over the real [`Storage`].
pub(crate) struct StorageCleanupOps {
    storage: Arc<Storage>,
}

impl StorageCleanupOps {
    pub(crate) fn new(storage: Arc<Storage>) -> Self {
        Self { storage }
    }
}

#[async_trait]
impl CleanupOps for StorageCleanupOps {
    fn now_ms(&self) -> i64 {
        chrono::Utc::now().timestamp_millis()
    }

    async fn run_cleanup(&self) -> Result<CleanupStats, StorageError> {
        self.storage.run_cleanup().await
    }

    async fn read_last_cleanup(&self) -> Result<Option<String>, StorageError> {
        self.storage.get_config(LAST_CLEANUP_KEY).await
    }

    async fn write_last_cleanup(&self, value_ms: i64) -> Result<(), StorageError> {
        self.storage
            .set_config(LAST_CLEANUP_KEY, &value_ms.to_string())
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_700_000_000_000;
    const DELAY: i64 = 3 * 60 * 1000;
    const PERIOD: i64 = 6 * 60 * 60 * 1000;

    #[test]
    fn an_absent_timestamp_is_due_after_the_start_delay() {
        // First ever run: due immediately, but the start delay still applies.
        assert_eq!(initial_deadline_ms(NOW, None, DELAY, PERIOD), NOW + DELAY);
    }

    #[test]
    fn an_unparseable_timestamp_is_treated_as_absent() {
        // Corrupt state must not wedge retention off permanently.
        let got = initial_deadline_ms(NOW, Some("not a number".into()), DELAY, PERIOD);
        assert_eq!(got, NOW + DELAY);
    }

    #[test]
    fn a_future_timestamp_is_due_now_rather_than_waited_out() {
        // The clock moved backwards. Waiting out the difference could park
        // retention for an arbitrarily long time.
        let got = initial_deadline_ms(NOW, Some((NOW + PERIOD * 10).to_string()), DELAY, PERIOD);
        assert_eq!(got, NOW + DELAY);
    }

    #[test]
    fn an_i64_max_timestamp_takes_the_future_branch() {
        // i64::MAX is in the future, so the future branch catches it before the
        // addition runs. The saturating_add behind it is defence for a caller
        // that ever reorders these branches — an overflow here would panic in
        // debug and wrap to a deadline in the past in release.
        let got = initial_deadline_ms(NOW, Some(i64::MAX.to_string()), DELAY, PERIOD);
        assert_eq!(got, NOW + DELAY);
    }

    #[test]
    fn an_overdue_run_waits_only_the_start_delay() {
        // Persisted run 7 hours old against a 6 hour period: due an hour ago,
        // so the start delay is what governs.
        let last = NOW - 7 * 60 * 60 * 1000;
        assert_eq!(
            initial_deadline_ms(NOW, Some(last.to_string()), DELAY, PERIOD),
            NOW + DELAY
        );
    }

    #[test]
    fn a_nearly_due_run_waits_out_its_remainder() {
        // Revision-3 regression. A process starting with a run 5h56m old must
        // wait the remaining ~4 minutes, NOT defer to the following period.
        // The bug this pins produced a 12-hour gap from a 6-hour policy.
        let last = NOW - (5 * 60 + 56) * 60 * 1000;
        let got = initial_deadline_ms(NOW, Some(last.to_string()), DELAY, PERIOD);
        assert_eq!(
            got,
            last + PERIOD,
            "must be the remainder, not a further period"
        );
        assert!(
            got > NOW + DELAY,
            "and the remainder must outlast the start delay here"
        );
    }

    #[test]
    fn surrounding_whitespace_is_tolerated() {
        // Pins the trim. Without it the value reads as unparseable, and the
        // run lands at the start delay instead of one period after the
        // stored time.
        let last = NOW - (5 * 60 + 56) * 60 * 1000;
        let got = initial_deadline_ms(NOW, Some(format!("  {last}\n")), DELAY, PERIOD);
        assert_eq!(got, last + PERIOD);
    }

    #[test]
    fn a_timestamp_equal_to_now_still_waits_a_full_period() {
        // Pins the `>` in the future guard. Relaxed to `>=`, a restart inside
        // the same millisecond as the last run would clean again after the
        // start delay rather than a full period, so a crash-restart loop
        // would clean repeatedly.
        assert_eq!(
            initial_deadline_ms(NOW, Some(NOW.to_string()), DELAY, PERIOD),
            NOW + PERIOD
        );
    }

    #[test]
    fn the_start_delay_floors_every_case() {
        // A run due 1ms from now still waits the delay out.
        let last = NOW - PERIOD + 1;
        assert_eq!(
            initial_deadline_ms(NOW, Some(last.to_string()), DELAY, PERIOD),
            NOW + DELAY
        );
    }
}
