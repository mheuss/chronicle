//! The scheduled retention cleanup task.
//!
//! One deadline governs the loop: sleep until it, run, recompute. There is no
//! `tokio::time::interval` and no separate due check, and adding either back
//! would reintroduce a scheduling bug — three earlier designs did exactly that.
//! See HEU-629 before changing the shape here.
//!
//! The wait is computed from the wall clock but slept on tokio's timer, which
//! is monotonic. On macOS that timer is `CLOCK_UPTIME_RAW`, which does not
//! advance while the machine is asleep, so a laptop that suspends for hours
//! runs its next cleanup that much later than the wall clock says it is due.
//! Accepted: retention is a 30-day policy, a few hours of lateness is noise,
//! and every restart re-derives the deadline from the wall clock and resets
//! the drift.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chronicle_ipc::CancellationToken;
use chronicle_storage::{CleanupOutcome, CleanupStats, Storage, StorageError};

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

/// True if this error is a panic in a blocking storage worker.
///
/// Classifies the cleanup run and both checkpoint calls — they all cross the
/// same `spawn_blocking` boundary. `JoinError` also covers cancellation, which
/// is not a panic and not fatal; `a_cancelled_join_is_not_a_worker_panic` pins
/// that half.
fn is_worker_panic(e: &StorageError) -> bool {
    matches!(e, StorageError::Join(j) if j.is_panic())
}

/// Run retention cleanup on a schedule until cancelled.
///
/// One deadline governs the loop — see the module docs before changing its
/// shape.
pub(crate) async fn run_cleanup_loop<O>(
    ops: Arc<O>,
    cancel: CancellationToken,
) -> Result<(), StorageError>
where
    O: CleanupOps + 'static,
{
    // Read once. From here the loop owns its schedule; the persisted value
    // exists for the NEXT process, not for this one.
    //
    // Fail open on a read error: treat it as "never ran". The cost of being
    // wrong is one redundant idempotent cleanup. The cost of failing closed is
    // retention silently never running after a transient boot error.
    //
    // A PANIC is not a read error. `get_config` also runs in `spawn_blocking`,
    // so a panic in it arrives here as `StorageError::Join` — and swallowing
    // that as "never ran" would hide a genuinely broken storage layer behind a
    // routine-looking fallback.
    let last_raw = match ops.read_last_cleanup().await {
        Ok(v) => v,
        Err(e) if is_worker_panic(&e) => {
            log::error!("retention: checkpoint read panicked; stopping the scheduler: {e}");
            return Err(e);
        }
        Err(e) => {
            log::warn!("retention: reading {LAST_CLEANUP_KEY} failed, treating as never run: {e}");
            None
        }
    };

    let mut next_attempt = initial_deadline_ms(
        ops.now_ms(),
        last_raw,
        CLEANUP_START_DELAY.as_millis() as i64,
        CLEANUP_PERIOD.as_millis() as i64,
    );

    loop {
        // Both terms are wall-clock, but the sleep below is monotonic, so a
        // backwards step in the wall clock is slept out and parks retention
        // until the next restart. `initial_deadline_ms` guards that hazard at
        // task start and this path does not — the asymmetry is deferred to
        // HEU-630 with the fix and its test, not overlooked.
        let wait = Duration::from_millis(next_attempt.saturating_sub(ops.now_ms()).max(0) as u64);
        tokio::select! {
            // `biased` so a ready cancellation is never passed over in favour
            // of a simultaneously-ready deadline.
            biased;
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(wait) => {}
        }
        if cancel.is_cancelled() {
            break;
        }

        let result = ops.run_cleanup().await;

        // One clock read, used for both the next deadline and the persisted
        // checkpoint. Two separate `now_ms()` calls would drift apart if the
        // wall clock moved between them, making in-process cadence and restart
        // cadence disagree for no reason. No test pins this: under paused time
        // two adjacent reads return the same value, so splitting them is an
        // invisible mutant.
        let completed_at = ops.now_ms();

        // Rescheduled unconditionally, BEFORE inspecting the result. Without
        // this, an outcome that records no timestamp leaves the deadline in the
        // past and the loop spins at full speed.
        next_attempt = completed_at.saturating_add(CLEANUP_PERIOD.as_millis() as i64);

        match result {
            Ok(stats) => {
                if stats.outcome == CleanupOutcome::Completed {
                    // The log line goes AFTER the write attempt so it cannot
                    // claim success while the checkpoint silently failed.
                    match ops.write_last_cleanup(completed_at).await {
                        Ok(()) => log::info!("retention cleanup finished: {stats:?}"),
                        // Same reasoning as the read above: `set_config` runs in
                        // `spawn_blocking` too, and a panic there is a broken
                        // storage layer, not a checkpoint that failed to stick.
                        Err(e) if is_worker_panic(&e) => {
                            log::error!(
                                "retention cleanup finished: {stats:?}; checkpoint write panicked, stopping the scheduler: {e}"
                            );
                            return Err(e);
                        }
                        Err(e) => log::warn!(
                            "retention cleanup finished: {stats:?}; {LAST_CLEANUP_KEY} not recorded: {e}"
                        ),
                    }
                } else {
                    log::info!("retention cleanup finished: {stats:?}");
                }
            }
            Err(e) if is_worker_panic(&e) => {
                log::error!("retention cleanup worker panicked; stopping the scheduler: {e}");
                return Err(e);
            }
            Err(e) => log::error!("retention cleanup failed: {e}"),
        }
    }
    Ok(())
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

#[cfg(test)]
mod loop_tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use chronicle_ipc::CancellationToken;
    use chronicle_storage::{CleanupOutcome, CleanupStats};

    const NOW: i64 = 1_700_000_000_000;

    /// A real `JoinError` carrying a panic.
    ///
    /// The production paths all panic inside `spawn_blocking`, which tokio
    /// converts to a `JoinError`. Reproduce that shape rather than panicking
    /// directly, so the tests exercise the branch the real error takes.
    async fn join_error_from_panic() -> tokio::task::JoinError {
        tokio::task::spawn_blocking(|| panic!("storage worker died"))
            .await
            .unwrap_err()
    }

    /// A fake whose clock tracks tokio's paused timer.
    ///
    /// `now_ms` is NOT a stored constant — it is the base plus however much
    /// tokio's clock has advanced. That is what lets a test assert the *next*
    /// deadline against a run of nonzero duration.
    struct FakeOps {
        origin: tokio::time::Instant,
        runs: AtomicUsize,
        run_duration: Duration,
        outcome: CleanupOutcome,
        /// Return a `Join` error carrying a real panic — fatal to the loop.
        panic_on_run: bool,
        /// Return an ordinary error — the loop must keep going.
        run_fails: bool,
        /// Fail the checkpoint read — the loop must fail open.
        read_fails: bool,
        /// Panic the checkpoint read — fatal, unlike `read_fails`.
        read_panics: bool,
        /// Fail the checkpoint write — logged, and the loop keeps going.
        write_fails: bool,
        /// Panic the checkpoint write — fatal, unlike `write_fails`.
        write_panics: bool,
        stored: Mutex<Option<String>>,
        writes: Mutex<Vec<i64>>,
    }

    impl FakeOps {
        fn new() -> Self {
            Self {
                origin: tokio::time::Instant::now(),
                runs: AtomicUsize::new(0),
                run_duration: Duration::ZERO,
                outcome: CleanupOutcome::Completed,
                panic_on_run: false,
                run_fails: false,
                read_fails: false,
                read_panics: false,
                write_fails: false,
                write_panics: false,
                stored: Mutex::new(None),
                writes: Mutex::new(Vec::new()),
            }
        }
        fn run_count(&self) -> usize {
            self.runs.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl CleanupOps for FakeOps {
        fn now_ms(&self) -> i64 {
            NOW + self.origin.elapsed().as_millis() as i64
        }
        async fn run_cleanup(&self) -> Result<CleanupStats, StorageError> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(self.run_duration).await;
            if self.panic_on_run {
                return Err(StorageError::Join(join_error_from_panic().await));
            }
            if self.run_fails {
                // An ORDINARY error — not a Join — so `is_worker_panic` is
                // false and the loop must survive it.
                return Err(StorageError::Other("db down".into()));
            }
            Ok(CleanupStats {
                outcome: self.outcome,
                ..CleanupStats::default()
            })
        }
        async fn read_last_cleanup(&self) -> Result<Option<String>, StorageError> {
            if self.read_panics {
                return Err(StorageError::Join(join_error_from_panic().await));
            }
            if self.read_fails {
                return Err(StorageError::Other("pool exhausted".into()));
            }
            Ok(self.stored.lock().unwrap().clone())
        }
        async fn write_last_cleanup(&self, value_ms: i64) -> Result<(), StorageError> {
            if self.write_panics {
                return Err(StorageError::Join(join_error_from_panic().await));
            }
            if self.write_fails {
                return Err(StorageError::Other("disk full".into()));
            }
            self.writes.lock().unwrap().push(value_ms);
            *self.stored.lock().unwrap() = Some(value_ms.to_string());
            Ok(())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_first_run_fires_at_the_start_delay_and_not_before() {
        let ops = Arc::new(FakeOps::new());
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_cleanup_loop(Arc::clone(&ops), cancel.clone()));

        // sleep(), not advance(): going idle auto-advances to the EARLIEST
        // deadline, so a task scheduled too soon delivers its run before this
        // sleep completes and the assertion catches it. advance() would bump
        // the clock past the sleeper before it is first polled, hiding the bug.
        // Same reasoning as the StartRetry tests in main.rs.
        tokio::time::sleep(Duration::from_secs(2 * 60)).await;
        assert_eq!(ops.run_count(), 0, "must not run before the start delay");

        tokio::time::sleep(Duration::from_secs(2 * 60)).await;
        assert_eq!(ops.run_count(), 1, "must run once the start delay elapses");

        cancel.cancel();
        task.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_run_of_nonzero_duration_is_followed_one_period_later() {
        // Revision-2 regression. With a ticker anchored at tick time and the
        // timestamp written at completion, the next tick saw PERIOD minus the
        // run duration elapsed, said "not yet", and pushed the run to the tick
        // after — a 12-hour cadence from a 6-hour policy. Only expressible
        // because the clock is injected and the fake run consumes real
        // (paused) time.
        let mut fake = FakeOps::new();
        fake.run_duration = Duration::from_secs(90);
        let ops = Arc::new(fake);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_cleanup_loop(Arc::clone(&ops), cancel.clone()));

        // Start delay + run, then just under one period.
        tokio::time::sleep(Duration::from_secs(3 * 60 + 90 + 6 * 60 * 60 - 60)).await;
        assert_eq!(ops.run_count(), 1, "the second run must not have fired yet");

        tokio::time::sleep(Duration::from_secs(120)).await;
        assert_eq!(
            ops.run_count(),
            2,
            "the second run must fire one period after the first finished"
        );

        cancel.cancel();
        task.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_disabled_outcome_reschedules_rather_than_spinning() {
        // Without reschedule-before-inspect, a Disabled run records no
        // timestamp, leaves the deadline in the past, and the loop hammers the
        // database. Assert exactly one run per period, not a burst.
        let mut fake = FakeOps::new();
        fake.outcome = CleanupOutcome::Disabled;
        let ops = Arc::new(fake);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_cleanup_loop(Arc::clone(&ops), cancel.clone()));

        tokio::time::sleep(Duration::from_secs(3 * 60 + 60)).await;
        assert_eq!(ops.run_count(), 1, "one run, not a spin");
        assert!(
            ops.writes.lock().unwrap().is_empty(),
            "a disabled run must not persist last_cleanup_ms"
        );

        tokio::time::sleep(Duration::from_secs(6 * 60 * 60)).await;
        assert_eq!(ops.run_count(), 2, "and the next comes a period later");

        cancel.cancel();
        task.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_completed_run_persists_its_finish_time() {
        let mut fake = FakeOps::new();
        fake.run_duration = Duration::from_secs(90);
        let ops = Arc::new(fake);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_cleanup_loop(Arc::clone(&ops), cancel.clone()));

        tokio::time::sleep(Duration::from_secs(3 * 60 + 90 + 1)).await;

        let writes = ops.writes.lock().unwrap().clone();
        assert_eq!(writes.len(), 1);
        // The recorded time is the run's COMPLETION, not its start: start delay
        // plus the run's own 90 seconds.
        assert_eq!(writes[0], NOW + (3 * 60 + 90) * 1000);

        cancel.cancel();
        task.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_while_sleeping_exits_without_running() {
        let ops = Arc::new(FakeOps::new());
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_cleanup_loop(Arc::clone(&ops), cancel.clone()));

        tokio::time::sleep(Duration::from_secs(60)).await;
        cancel.cancel();
        task.await.unwrap().unwrap();

        assert_eq!(
            ops.run_count(),
            0,
            "cancelling before the deadline must not run cleanup"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_worker_panic_stops_the_loop() {
        let mut fake = FakeOps::new();
        fake.panic_on_run = true;
        let ops = Arc::new(fake);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_cleanup_loop(Arc::clone(&ops), cancel.clone()));

        // One run, then the loop must be gone — not retrying every period.
        tokio::time::sleep(Duration::from_secs(3 * 60 + 1)).await;
        assert_eq!(ops.run_count(), 1);

        // The task ends on its own, without anyone cancelling it, AND it ends
        // in a failed state — that Err is what HEU-630's join will turn into a
        // nonzero exit code. A loop that stopped but returned Ok would look
        // like an orderly shutdown.
        let outcome = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("a panicked worker must stop the loop")
            .unwrap();
        assert!(
            matches!(outcome, Err(StorageError::Join(ref j)) if j.is_panic()),
            "a panicked worker must leave the task failed, got: {outcome:?}"
        );

        tokio::time::sleep(Duration::from_secs(12 * 60 * 60)).await;
        assert_eq!(
            ops.run_count(),
            1,
            "a panicked cleanup must not retry every period"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_during_a_run_stops_the_loop_before_the_next() {
        // Design §5 lists "a ready cancellation is taken over a
        // simultaneously-ready deadline (`biased`)". That exact race is not
        // constructible in a test — a select only randomises among branches
        // that are ready in the SAME poll, and paused time gives no way to
        // arrange that deterministically. A test that pretended to would be
        // flaky rather than useful.
        //
        // What this test pins: cancelling mid-run makes the loop exit after the
        // in-flight run returns rather than sleeping on to the next deadline.
        // It isolates no clause of its own — `cancellation_while_sleeping_exits_
        // without_running` kills every mutant it does — so it earns its place by
        // covering the mid-run entry point, not by discriminating a guard.
        //
        // What it does NOT pin, verified by mutation: neither `biased;` nor the
        // `is_cancelled()` recheck. Delete either, or both, and this suite stays
        // green, because by the time the loop reaches the next select the
        // deadline is a period out and the cancel branch is the only ready one.
        //
        // The two guards are not interchangeable. The recheck is load-bearing:
        // it closes a window `biased;` cannot reach, where cancel lands after
        // the select resolves on the sleep branch but before `run_cleanup()` is
        // entered. `biased;` is redundant given the recheck — if the deadline
        // branch ever won the coin flip, the recheck breaks immediately — and is
        // kept only because the design names it as a required property.
        //
        // The reachable race is a deschedule, not a zero `wait`: `wait` is
        // floored at the start delay and every reschedule is a full period out,
        // so zero would need the wall clock to jump hours between two adjacent
        // statements. The real window is the loop going unpolled while the timer
        // fires and `cancel()` lands in the same gap.
        //
        // `docs/development/storage.md` ("two guards on one invariant means the
        // outer one is untestable") prescribes deleting the redundant guard, not
        // documenting it. That rule is consciously overridden here because the
        // design names `biased;` by hand. Do not read the green suite as
        // evidence `biased;` does anything.
        let mut fake = FakeOps::new();
        fake.run_duration = Duration::from_secs(60);
        let ops = Arc::new(fake);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_cleanup_loop(Arc::clone(&ops), cancel.clone()));

        // Land inside the first run: past the 3-minute delay, before the run's
        // own 60 seconds are up.
        tokio::time::sleep(Duration::from_secs(3 * 60 + 30)).await;
        assert_eq!(ops.run_count(), 1, "the first run must be in flight");
        cancel.cancel();

        // The in-flight run finishes (no stop flag until HEU-630), then the
        // loop must exit rather than sleeping to the next deadline.
        tokio::time::timeout(Duration::from_secs(120), task)
            .await
            .expect("the loop must exit after the in-flight run returns")
            .unwrap()
            .unwrap();
        assert_eq!(ops.run_count(), 1, "no run may start after cancellation");
    }

    #[tokio::test(start_paused = true)]
    async fn an_ordinary_failure_reschedules_rather_than_retrying() {
        // The non-panic error branch. Reschedule-before-inspect is what stops a
        // failing run from spinning: the deadline is already moved forward by
        // the time the error is matched. Without it, a database that is down
        // hammers itself in a tight loop.
        //
        // Distinct from the panic test: that one STOPS the loop, this one must
        // keep it going at the normal cadence.
        let mut fake = FakeOps::new();
        fake.run_fails = true;
        let ops = Arc::new(fake);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_cleanup_loop(Arc::clone(&ops), cancel.clone()));

        tokio::time::sleep(Duration::from_secs(3 * 60 + 1)).await;
        assert_eq!(ops.run_count(), 1);
        assert!(
            ops.writes.lock().unwrap().is_empty(),
            "a failed run must not persist last_cleanup_ms"
        );

        // Nothing for almost a period — no immediate retry.
        tokio::time::sleep(Duration::from_secs(6 * 60 * 60 - 60)).await;
        assert_eq!(ops.run_count(), 1, "a failure must not retry immediately");

        tokio::time::sleep(Duration::from_secs(120)).await;
        assert_eq!(
            ops.run_count(),
            2,
            "and the loop keeps running at the normal cadence"
        );

        cancel.cancel();
        task.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_stored_timestamp_places_the_first_deadline() {
        // The loop reads the persisted value once at start. A run 5h56m old
        // against a 6h period waits its ~4 minute remainder.
        let fake = FakeOps::new();
        *fake.stored.lock().unwrap() = Some((NOW - (5 * 60 + 56) * 60 * 1000).to_string());
        let ops = Arc::new(fake);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_cleanup_loop(Arc::clone(&ops), cancel.clone()));

        tokio::time::sleep(Duration::from_secs(3 * 60)).await;
        assert_eq!(
            ops.run_count(),
            0,
            "not yet — the remainder outlasts the start delay"
        );

        tokio::time::sleep(Duration::from_secs(2 * 60)).await;
        assert_eq!(
            ops.run_count(),
            1,
            "runs at the remainder, not a further period"
        );

        cancel.cancel();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_cancelled_join_is_not_a_worker_panic() {
        // The other half of `is_worker_panic`. A `JoinError` from an aborted
        // task is not a panic and must not be fatal: at shutdown the runtime
        // can abort a queued blocking task, and classifying that as a panic
        // would turn an orderly exit into a nonzero exit code once HEU-630
        // joins this loop.
        let handle = tokio::spawn(async { std::future::pending::<()>().await });
        handle.abort();
        let e = StorageError::Join(handle.await.unwrap_err());
        assert!(
            !is_worker_panic(&e),
            "a cancelled join is not a worker panic"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_panicking_checkpoint_read_stops_the_loop() {
        // The read fails OPEN on an ordinary error — see `a_failed_read_fails_open`.
        // A panic is not that. `get_config` runs in `spawn_blocking` too, so a
        // panic arrives as `StorageError::Join`, and treating it as "never ran"
        // would hide a broken storage layer behind a routine-looking fallback.
        let mut fake = FakeOps::new();
        fake.read_panics = true;
        let ops = Arc::new(fake);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_cleanup_loop(Arc::clone(&ops), cancel.clone()));

        let outcome = tokio::time::timeout(Duration::from_secs(60), task)
            .await
            .expect("a panicked checkpoint read must stop the loop")
            .unwrap();
        assert!(
            matches!(outcome, Err(StorageError::Join(ref j)) if j.is_panic()),
            "must leave the task failed, got: {outcome:?}"
        );
        assert_eq!(ops.run_count(), 0, "and no cleanup may run");
    }

    #[tokio::test(start_paused = true)]
    async fn a_panicking_checkpoint_write_stops_the_loop() {
        // Same classification as the read and the run: `set_config` crosses the
        // same `spawn_blocking` boundary. Distinct from an ordinary write
        // failure, which the next test pins as non-fatal.
        let mut fake = FakeOps::new();
        fake.write_panics = true;
        let ops = Arc::new(fake);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_cleanup_loop(Arc::clone(&ops), cancel.clone()));

        let outcome = tokio::time::timeout(Duration::from_secs(4 * 60), task)
            .await
            .expect("a panicked checkpoint write must stop the loop")
            .unwrap();
        assert!(
            matches!(outcome, Err(StorageError::Join(ref j)) if j.is_panic()),
            "must leave the task failed, got: {outcome:?}"
        );
        assert_eq!(ops.run_count(), 1, "after exactly one run");
    }

    #[tokio::test(start_paused = true)]
    async fn an_ordinary_write_failure_keeps_the_loop_running() {
        // Bounded and self-correcting: the next process starts from an older
        // timestamp and runs once more than it strictly needed to. Making it
        // fatal would disable retention over a transient database error.
        let mut fake = FakeOps::new();
        fake.write_fails = true;
        let ops = Arc::new(fake);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_cleanup_loop(Arc::clone(&ops), cancel.clone()));

        tokio::time::sleep(Duration::from_secs(3 * 60 + 1)).await;
        assert_eq!(ops.run_count(), 1);

        tokio::time::sleep(Duration::from_secs(6 * 60 * 60)).await;
        assert_eq!(
            ops.run_count(),
            2,
            "a write failure must not stop the schedule"
        );

        cancel.cancel();
        task.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_read_fails_open() {
        // A transient database error at boot must not wedge retention off —
        // that is the exact bug this ticket exists to fix. Treat it as "never
        // ran".
        let mut fake = FakeOps::new();
        fake.read_fails = true;
        let ops = Arc::new(fake);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_cleanup_loop(Arc::clone(&ops), cancel.clone()));

        tokio::time::sleep(Duration::from_secs(3 * 60 + 1)).await;
        assert_eq!(
            ops.run_count(),
            1,
            "a failed read schedules at the start delay"
        );

        cancel.cancel();
        task.await.unwrap().unwrap();
    }
}
