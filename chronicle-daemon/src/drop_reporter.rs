//! Off-thread reporting for real-time drop counters.
//!
//! The capture and audio callbacks count; nothing else. This module turns
//! those counts into at most one log line per period — see ADR-013.

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
    /// future change makes a counter rebuildable, this underflows: a panic in
    /// debug, and in release a wrap to a nonsensical delta near `u64::MAX` that
    /// is glaring in the log line. Neither is silent, which is the point. Do
    /// not "harden" them with `saturating_sub` — that turns a broken invariant
    /// into quiet wrong numbers. `checked_sub().expect()` was considered and
    /// rejected: in release — which is what ships — it panics the reporter
    /// task, and the design treats a reporter
    /// panic as fatal to the loop, so a counter bug would take drop reporting
    /// down entirely rather than printing one absurd number.
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

/// Flatten the two crates' snapshots into one reading.
///
/// The only place two field vocabularies meet: audio's `mic_*`/`system_*` and
/// capture's bare `full`/`closed`. Lives here rather than inline in `main` so
/// it can be tested — a swapped pair here mislabels counters in every log line
/// for the life of the process, and nothing downstream would notice.
pub(crate) fn totals_from(
    audio: chronicle_audio::AudioDropSnapshot,
    capture: chronicle_capture::CaptureDropSnapshot,
) -> DropTotals {
    DropTotals {
        mic_full: audio.mic_full,
        mic_closed: audio.mic_closed,
        mic_convert_failed: audio.mic_convert_failed,
        system_full: audio.system_full,
        system_closed: audio.system_closed,
        frames_full: capture.full,
        frames_closed: capture.closed,
    }
}

/// Run until signalled, logging at most one line per period.
///
/// `read_totals` is a closure rather than the counter `Arc`s directly so the
/// shutdown contract can be tested without a capture engine or an audio
/// pipeline.
pub(crate) async fn run_reporter<F>(
    read_totals: F,
    shutdown: tokio::sync::oneshot::Receiver<()>,
    period: std::time::Duration,
) where
    F: Fn() -> DropTotals,
{
    run_reporter_with_sink(read_totals, shutdown, period, |line| {
        log::log!(REPORT_LEVEL, "{line}")
    })
    .await
}

/// As `run_reporter`, with the line sink injected so tests can capture output
/// without installing a global logger.
pub(crate) async fn run_reporter_with_sink<F, S>(
    read_totals: F,
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
    period: std::time::Duration,
    emit: S,
) where
    F: Fn() -> DropTotals,
    S: Fn(String),
{
    let mut state = ReporterState::default();
    let mut ticker = tokio::time::interval(period);
    // Not the default `Burst`: after a stall it fires catch-up ticks back to
    // back, and while drops are ongoing each one carries a real delta — so the
    // "at most one line per period" contract would break exactly when the log
    // is busiest. `Delay` restarts the period from the tick that actually ran.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Unlike the storage refresher in main(), the immediate first tick is NOT
    // skipped. That one skips it to avoid double-priming a snapshot; this
    // reporter is silent when nothing was dropped, so an immediate first read
    // costs nothing and catches a burst that predates the task.
    loop {
        tokio::select! {
            // Signalled and sender-dropped collapse to the same arm on
            // purpose: either way nothing will signal us again, so the right
            // move is to flush and exit rather than tick on unreachable.
            _ = &mut shutdown => break,
            _ = ticker.tick() => {
                if let Some(line) = state.observe(read_totals()) {
                    emit(line);
                }
            }
        }
    }
    // Final read AFTER the producers have stopped — see the shutdown ordering
    // in main(). Without this a drop storm during teardown would never be
    // reported, which is the failure mode this module exists to fix.
    if let Some(line) = state.observe(read_totals()) {
        emit(line);
    }
}

/// The level the reporter logs at.
///
/// Named and pinned by a test rather than inlined: HEU-653 exists because
/// `warn!`/`info!` lines were invisible, so the level *is* the deliverable.
/// Dropping it to `debug!` would leave the daemon silent by default under the
/// `warn,chronicle=info` filter, and every test would still pass.
pub(crate) const REPORT_LEVEL: log::Level = log::Level::Warn;

/// Build the reporter's counter-read closure from the two producers' handles.
///
/// Extracted so the read side is testable in isolation: passing a fresh `Arc`
/// to either argument leaves half the report reading zero forever, and nothing
/// downstream notices.
///
/// Note what this does NOT cover. `main` choosing which `Arc` to pass is still
/// unpinned — swap `audio_pipeline.drop_counters()` at the call site for a
/// fresh `AudioDropCounters::default()` and every test in the tree still
/// passes. The capture side has no such gap, because the supervisor owns its
/// `Arc` and `supervisor_config_carries_its_own_counters` asserts `ptr_eq`
/// against it. Closing the audio equivalent means lifting that choice out of
/// `async fn main()`, which no test calls.
pub(crate) fn counter_reader(
    audio: std::sync::Arc<chronicle_audio::AudioDropCounters>,
    capture: std::sync::Arc<chronicle_capture::CaptureDropCounters>,
) -> impl Fn() -> DropTotals {
    move || totals_from(audio.snapshot(), capture.snapshot())
}

/// How often the reporter reads the counters.
///
/// Timing resolution *inside* a drop burst is HEU-548's job, not this
/// ticket's, so 30 s is the right granularity for what HEU-653 owns.
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

    /// Test-only holder so the reporter's read closure can be driven.
    #[derive(Default)]
    struct TestTotals(std::sync::Mutex<DropTotals>);

    impl TestTotals {
        fn get(&self) -> DropTotals {
            *self.0.lock().unwrap()
        }
        fn set(&self, t: DropTotals) {
            *self.0.lock().unwrap() = t;
        }
    }

    #[test]
    fn totals_from_maps_every_field_to_its_own_name() {
        // Seven distinct values: any swapped pair must fail. Same reasoning as
        // the snapshot tests in each crate, one layer up — this is the layer
        // where the two field vocabularies get translated.
        let audio = chronicle_audio::AudioDropSnapshot {
            mic_full: 1,
            mic_closed: 2,
            mic_convert_failed: 3,
            system_full: 4,
            system_closed: 5,
        };
        let capture = chronicle_capture::CaptureDropSnapshot { full: 6, closed: 7 };

        let t = totals_from(audio, capture);

        assert_eq!(t.mic_full, 1);
        assert_eq!(t.mic_closed, 2);
        assert_eq!(t.mic_convert_failed, 3);
        assert_eq!(t.system_full, 4);
        assert_eq!(t.system_closed, 5);
        assert_eq!(t.frames_full, 6);
        assert_eq!(t.frames_closed, 7);
    }

    #[tokio::test(start_paused = true)]
    async fn a_tick_emits_without_waiting_for_shutdown() {
        // Deleting `emit(line)` from the tick arm leaves the state advancing,
        // so the shutdown test alone still passes and the reporter goes mute
        // for the whole process. This is the test that notices.
        let (tx, rx) = tokio::sync::oneshot::channel();
        let counters = std::sync::Arc::new(TestTotals::default());
        let read = std::sync::Arc::clone(&counters);
        let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = std::sync::Arc::clone(&lines);

        let handle = tokio::spawn(run_reporter_with_sink(
            move || read.get(),
            rx,
            REPORT_PERIOD,
            move |line| sink.lock().unwrap().push(line),
        ));

        // The immediate first tick reads zeroes and stays silent.
        tokio::task::yield_now().await;
        assert!(lines.lock().unwrap().is_empty(), "zero must be silent");

        counters.set(DropTotals {
            frames_full: 4,
            ..Default::default()
        });

        // `sleep`, never `advance` — an idle runtime auto-advances to the
        // earliest deadline, which is what makes this deterministic. See
        // docs/development/paused-time-testing.md.
        tokio::time::sleep(REPORT_PERIOD + std::time::Duration::from_millis(1)).await;

        {
            let seen = lines.lock().unwrap();
            assert_eq!(seen.len(), 1, "a tick must emit; got {seen:?}");
            assert!(seen[0].contains("frames_full=4"), "{}", seen[0]);
        }

        // And the final flush adds nothing when nothing grew since.
        tx.send(()).unwrap();
        handle.await.unwrap();
        assert_eq!(lines.lock().unwrap().len(), 1);
    }

    #[test]
    fn reporter_logs_at_warn() {
        // The level is the deliverable: this ticket exists because warn/info
        // lines were invisible. `debug!` here would be silent under the
        // daemon's own default filter.
        assert_eq!(REPORT_LEVEL, log::Level::Warn);
    }

    #[test]
    fn counter_reader_reads_the_allocations_it_was_given() {
        // Hand it one Arc from each crate, write through the originals, and
        // confirm the closure sees it. A fresh Arc on either side reads zero
        // forever and nothing else would notice.
        let audio = std::sync::Arc::new(chronicle_audio::AudioDropCounters::default());
        let capture = std::sync::Arc::new(chronicle_capture::CaptureDropCounters::default());
        let read = counter_reader(
            std::sync::Arc::clone(&audio),
            std::sync::Arc::clone(&capture),
        );

        assert_eq!(read(), DropTotals::default());

        audio
            .mic_full
            .fetch_add(3, std::sync::atomic::Ordering::Relaxed);
        capture
            .closed
            .fetch_add(4, std::sync::atomic::Ordering::Relaxed);

        let t = read();
        assert_eq!(t.mic_full, 3, "audio side must reach the closure");
        assert_eq!(t.frames_closed, 4, "capture side must reach the closure");
    }

    #[test]
    fn report_period_is_thirty_seconds() {
        // Pinned deliberately: the design argues 30 s is right because burst
        // timing inside a drop storm is HEU-548's scope, not this ticket's.
        assert_eq!(REPORT_PERIOD, std::time::Duration::from_secs(30));
    }

    #[tokio::test(start_paused = true)]
    async fn pending_drops_are_reported_on_shutdown() {
        // `tokio::time::interval` fires its FIRST tick immediately, and under
        // start_paused an idle runtime auto-advances to the earliest pending
        // deadline — so a long period does not mean "never ticks"
        // (docs/development/paused-time-testing.md). Assert on content, which
        // is robust either way: a tick before the counter is set emits nothing
        // (silent at zero); a tick after it emits the line and the final flush
        // then emits nothing (no growth). Exactly one line either way.
        let (tx, rx) = tokio::sync::oneshot::channel();
        let counters = std::sync::Arc::new(TestTotals::default());
        let read = std::sync::Arc::clone(&counters);
        let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = std::sync::Arc::clone(&lines);

        let handle = tokio::spawn(run_reporter_with_sink(
            move || read.get(),
            rx,
            std::time::Duration::from_secs(3600),
            move |line| sink.lock().unwrap().push(line),
        ));

        tokio::task::yield_now().await;
        counters.set(DropTotals {
            mic_full: 9,
            ..Default::default()
        });

        tx.send(()).unwrap();
        handle.await.unwrap();

        let lines = lines.lock().unwrap();
        assert_eq!(lines.len(), 1, "exactly one line expected; got {lines:?}");
        assert!(lines[0].contains("mic_full=9"), "{}", lines[0]);
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
        type Setter = fn(&mut DropTotals);
        let cases: [(&str, Setter); 7] = [
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
