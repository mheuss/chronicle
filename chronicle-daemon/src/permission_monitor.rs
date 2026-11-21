//! Reads both permissions through the helper on a timer (design §2).
//! It only sends reads. Acting on them is left to the event loop (design §3, N5).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use chronicle_ipc::CancellationToken;
use tokio::sync::{mpsc, watch};

use crate::permission_gate::MonitorState;
use crate::permissions::{self, PermissionRead};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PermissionName {
    ScreenRecording,
    Microphone,
}

/// One read, when it finished, and the idle epoch and mic generation seen
/// when it started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TimedRead {
    pub read: PermissionRead,
    pub at_ms: i64,
    pub epoch: u64,
    pub mic_generation: u64,
}

pub(crate) type ReadFuture = Pin<Box<dyn Future<Output = PermissionRead> + Send>>;
pub(crate) type Reader = Arc<dyn Fn(PermissionName) -> ReadFuture + Send + Sync>;

/// The production reader: CHR-95's helper on a blocking thread. `run_helper`
/// kills and reaps a child that misses its 1 s limit.
pub(crate) fn helper_reader() -> Reader {
    Arc::new(|name| {
        Box::pin(async move {
            tokio::task::spawn_blocking(move || match name {
                PermissionName::ScreenRecording => permissions::fresh_screen_recording_status(),
                PermissionName::Microphone => permissions::fresh_microphone_status(),
            })
            .await
            .unwrap_or(PermissionRead::Unavailable)
        })
    })
}

pub(crate) fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Each tick reads Screen Recording and sends it, then reads the mic and
/// sends it. Ticks start at least one interval apart, measured from the last
/// tick's start. Every read taken is sent and awaited, in order (design AD13).
pub(crate) async fn run(
    reader: Reader,
    mut mode_rx: watch::Receiver<MonitorState>,
    screen_tx: mpsc::Sender<TimedRead>,
    mic_tx: mpsc::Sender<TimedRead>,
    cancel: CancellationToken,
    now_ms: fn() -> i64,
) {
    // When the last tick started. None after Idle. Leaving Idle then reads at once.
    let mut last_tick: Option<tokio::time::Instant> = None;
    loop {
        let mode = mode_rx.borrow_and_update().mode;
        let Some(interval) = mode.interval() else {
            log::info!("permission monitor: idle");
            last_tick = None;
            tokio::select! {
                _ = cancel.cancelled() => return,
                changed = mode_rx.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    continue;
                }
            }
        };

        // Wait out the current mode's interval from the last tick. A mode
        // change restarts this wait with the new interval (design §2, N1b).
        if let Some(t) = last_tick {
            // `biased` checks a mode change before an expired sleep.
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return,
                changed = mode_rx.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    continue;
                }
                _ = tokio::time::sleep_until(t + interval) => {}
            }
        }

        // Check the mode again right before the first spawn (design §2).
        let state = *mode_rx.borrow();
        if state.mode.interval().is_none() {
            continue;
        }
        last_tick = Some(tokio::time::Instant::now());
        let screen = reader(PermissionName::ScreenRecording).await;
        log::info!("permission read: ScreenRecording -> {screen:?}");
        let sent = screen_tx
            .send(TimedRead {
                read: screen,
                at_ms: now_ms(),
                epoch: state.epoch,
                mic_generation: state.mic_generation,
            })
            .await;
        if sent.is_err() {
            return;
        }

        // Check the mode again before the second spawn (design §2).
        let state = *mode_rx.borrow();
        if state.mode.interval().is_some() {
            let mic = reader(PermissionName::Microphone).await;
            log::info!("permission read: Microphone -> {mic:?}");
            let sent = mic_tx
                .send(TimedRead {
                    read: mic,
                    at_ms: now_ms(),
                    epoch: state.epoch,
                    mic_generation: state.mic_generation,
                })
                .await;
            if sent.is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission_gate::MonitorMode;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::time::Instant;

    type Log = Arc<Mutex<Vec<(PermissionName, Duration)>>>;

    fn st(mode: MonitorMode) -> MonitorState {
        MonitorState {
            mode,
            epoch: 0,
            mic_generation: 0,
        }
    }

    /// Returns `screen` for Screen Recording and Authorized for the mic, and
    /// logs each call's time since `start`.
    fn fake(log: Log, screen: Arc<Mutex<PermissionRead>>, start: Instant) -> Reader {
        Arc::new(move |name| {
            log.lock().unwrap().push((name, start.elapsed()));
            let read = match name {
                PermissionName::ScreenRecording => *screen.lock().unwrap(),
                PermissionName::Microphone => PermissionRead::Authorized,
            };
            Box::pin(async move { read })
        })
    }

    struct Harness {
        log: Log,
        screen: Arc<Mutex<PermissionRead>>,
        mode_tx: watch::Sender<MonitorState>,
        screen_rx: mpsc::Receiver<TimedRead>,
        // Kept so the monitor's mic sends don't fail.
        _mic_rx: mpsc::Receiver<TimedRead>,
        cancel: CancellationToken,
    }

    fn start(mode: MonitorMode) -> Harness {
        let log: Log = Arc::default();
        let screen = Arc::new(Mutex::new(PermissionRead::Authorized));
        let (mode_tx, mode_rx) = watch::channel(st(mode));
        let (screen_tx, screen_rx) = mpsc::channel(64);
        let (mic_tx, _mic_rx) = mpsc::channel(64);
        let cancel = CancellationToken::new();
        let reader = fake(Arc::clone(&log), Arc::clone(&screen), Instant::now());
        tokio::spawn(run(
            reader,
            mode_rx,
            screen_tx,
            mic_tx,
            cancel.clone(),
            || 0,
        ));
        Harness {
            log,
            screen,
            mode_tx,
            screen_rx,
            _mic_rx,
            cancel,
        }
    }

    fn screen_reads(log: &Log) -> Vec<u64> {
        log.lock()
            .unwrap()
            .iter()
            .filter(|(n, _)| *n == PermissionName::ScreenRecording)
            .map(|(_, t)| t.as_secs())
            .collect()
    }

    #[tokio::test(start_paused = true)]
    async fn gated_reads_every_two_seconds() {
        let h = start(MonitorMode::Gated);
        tokio::time::sleep(Duration::from_millis(4_500)).await;
        assert_eq!(screen_reads(&h.log), vec![0, 2, 4]);
        assert_eq!(h.log.lock().unwrap().len(), 6, "two helper reads per tick");
        h.cancel.cancel();
    }

    #[tokio::test(start_paused = true)]
    async fn gated_to_permitted_keeps_five_second_spacing() {
        let h = start(MonitorMode::Gated);
        tokio::time::sleep(Duration::from_secs(1)).await;
        h.mode_tx.send_replace(st(MonitorMode::Permitted));
        tokio::time::sleep(Duration::from_millis(5_500)).await;
        assert_eq!(screen_reads(&h.log), vec![0, 5]);
        h.cancel.cancel();
    }

    #[tokio::test(start_paused = true)]
    async fn permitted_to_gated_keeps_two_second_spacing() {
        let h = start(MonitorMode::Permitted);
        tokio::time::sleep(Duration::from_secs(1)).await;
        h.mode_tx.send_replace(st(MonitorMode::Gated));
        tokio::time::sleep(Duration::from_millis(3_500)).await;
        assert_eq!(screen_reads(&h.log), vec![0, 2, 4]);
        h.cancel.cancel();
    }

    #[tokio::test(start_paused = true)]
    async fn permitted_reads_every_five_seconds() {
        let h = start(MonitorMode::Permitted);
        tokio::time::sleep(Duration::from_millis(10_500)).await;
        assert_eq!(screen_reads(&h.log), vec![0, 5, 10]);
        h.cancel.cancel();
    }

    #[tokio::test(start_paused = true)]
    async fn idle_never_reads() {
        let h = start(MonitorMode::Idle);
        tokio::time::sleep(Duration::from_secs(30)).await;
        assert!(h.log.lock().unwrap().is_empty());
        h.cancel.cancel();
    }

    #[tokio::test(start_paused = true)]
    async fn leaving_idle_reads_at_once() {
        let h = start(MonitorMode::Idle);
        tokio::time::sleep(Duration::from_secs(10)).await;
        h.mode_tx.send_replace(st(MonitorMode::Permitted));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(screen_reads(&h.log), vec![10]);
        h.cancel.cancel();
    }

    #[tokio::test(start_paused = true)]
    async fn screen_is_published_before_the_mic_read() {
        let (mode_tx, mode_rx) = watch::channel(st(MonitorMode::Permitted));
        let (screen_tx, screen_rx) = mpsc::channel(64);
        let screen_rx = Arc::new(Mutex::new(screen_rx));
        let (mic_tx, _mic_rx) = mpsc::channel(64);
        let seen_before_mic = Arc::new(Mutex::new(None));
        let seen = Arc::clone(&seen_before_mic);
        let rx = Arc::clone(&screen_rx);
        let reader: Reader = Arc::new(move |name| {
            if name == PermissionName::Microphone {
                *seen.lock().unwrap() = Some(rx.lock().unwrap().try_recv().is_ok());
            }
            Box::pin(async { PermissionRead::Denied })
        });
        let cancel = CancellationToken::new();
        tokio::spawn(run(
            reader,
            mode_rx,
            screen_tx,
            mic_tx,
            cancel.clone(),
            || 0,
        ));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(*seen_before_mic.lock().unwrap(), Some(true));
        cancel.cancel();
        drop(mode_tx);
    }

    #[tokio::test(start_paused = true)]
    async fn going_idle_between_reads_skips_the_mic() {
        let (mode_tx, mode_rx) = watch::channel(st(MonitorMode::Permitted));
        let mode_tx = Arc::new(mode_tx);
        let (screen_tx, _screen_rx) = mpsc::channel(64);
        let (mic_tx, _mic_rx) = mpsc::channel(64);
        let mic_reads = Arc::new(AtomicUsize::new(0));
        let (tx, count) = (Arc::clone(&mode_tx), Arc::clone(&mic_reads));
        let reader: Reader = Arc::new(move |name| {
            match name {
                PermissionName::ScreenRecording => {
                    tx.send_replace(st(MonitorMode::Idle));
                }
                PermissionName::Microphone => {
                    count.fetch_add(1, Ordering::SeqCst);
                }
            }
            Box::pin(async { PermissionRead::Authorized })
        });
        let cancel = CancellationToken::new();
        tokio::spawn(run(
            reader,
            mode_rx,
            screen_tx,
            mic_tx,
            cancel.clone(),
            || 0,
        ));
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(mic_reads.load(Ordering::SeqCst), 0);
        cancel.cancel();
    }

    #[tokio::test(start_paused = true)]
    async fn revoke_is_published_within_one_tick() {
        let mut h = start(MonitorMode::Permitted);
        tokio::time::sleep(Duration::from_secs(1)).await;
        let revoked_at = Instant::now();
        *h.screen.lock().unwrap() = PermissionRead::Denied;
        while h.screen_rx.recv().await.unwrap().read != PermissionRead::Denied {}
        assert!(revoked_at.elapsed() <= Duration::from_secs(5));
        h.cancel.cancel();
    }

    #[tokio::test(start_paused = true)]
    async fn a_reader_that_never_answers_blocks_further_reads() {
        let (_mode_tx, mode_rx) = watch::channel(st(MonitorMode::Gated));
        let (screen_tx, _screen_rx) = mpsc::channel(64);
        let (mic_tx, _mic_rx) = mpsc::channel(64);
        let calls = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&calls);
        let reader: Reader = Arc::new(move |_| {
            c.fetch_add(1, Ordering::SeqCst);
            Box::pin(std::future::pending())
        });
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run(
            reader,
            mode_rx,
            screen_tx,
            mic_tx,
            cancel.clone(),
            || 0,
        ));
        tokio::time::sleep(Duration::from_secs(30)).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "no second read starts while one hangs"
        );
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn idle_set_before_the_wait_ends_spawns_nothing() {
        let h = start(MonitorMode::Gated);
        tokio::time::sleep(Duration::from_millis(1_999)).await;
        h.mode_tx.send_replace(MonitorState {
            mode: MonitorMode::Idle,
            epoch: 1,
            mic_generation: 0,
        });
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert_eq!(screen_reads(&h.log), vec![0], "no spawn after Idle began");
        h.cancel.cancel();
    }

    #[tokio::test(start_paused = true)]
    async fn reads_are_stamped_at_spawn_start() {
        let (mode_tx, mode_rx) = watch::channel(st(MonitorMode::Permitted));
        let mode_tx = Arc::new(mode_tx);
        let (screen_tx, mut screen_rx) = mpsc::channel(64);
        let (mic_tx, _mic_rx) = mpsc::channel(64);
        let tx = Arc::clone(&mode_tx);
        // The epoch and the mic generation move to 1 while the screen read is in flight.
        let reader: Reader = Arc::new(move |name| {
            if name == PermissionName::ScreenRecording {
                tx.send_replace(MonitorState {
                    mode: MonitorMode::Permitted,
                    epoch: 1,
                    mic_generation: 1,
                });
            }
            Box::pin(async { PermissionRead::Authorized })
        });
        let cancel = CancellationToken::new();
        tokio::spawn(run(
            reader,
            mode_rx,
            screen_tx,
            mic_tx,
            cancel.clone(),
            || 0,
        ));
        let first = screen_rx.recv().await.unwrap();
        assert_eq!(
            (first.epoch, first.mic_generation),
            (0, 0),
            "stamped at spawn start"
        );
        cancel.cancel();
    }

    #[tokio::test(start_paused = true)]
    async fn queued_reads_keep_their_order() {
        let h = start(MonitorMode::Gated);
        // Nothing drains the channel while the reads change.
        tokio::time::sleep(Duration::from_secs(1)).await;
        *h.screen.lock().unwrap() = PermissionRead::Denied;
        tokio::time::sleep(Duration::from_secs(2)).await;
        *h.screen.lock().unwrap() = PermissionRead::Authorized;
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        let mut rx = h.screen_rx;
        let mut reads = Vec::new();
        while let Ok(t) = rx.try_recv() {
            reads.push(t.read);
        }
        assert_eq!(
            reads,
            vec![
                PermissionRead::Authorized,
                PermissionRead::Denied,
                PermissionRead::Authorized
            ],
            "no read is dropped"
        );
        h.cancel.cancel();
    }

    #[tokio::test(start_paused = true)]
    async fn one_read_in_flight_at_a_time() {
        let (_mode_tx, mode_rx) = watch::channel(st(MonitorMode::Gated));
        let (screen_tx, _screen_rx) = mpsc::channel(64);
        let (mic_tx, _mic_rx) = mpsc::channel(64);
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let (now, max) = (Arc::clone(&in_flight), Arc::clone(&max_seen));
        // Each read takes 3 s, longer than the 2 s Gated interval.
        let reader: Reader = Arc::new(move |_| {
            let (now, max) = (Arc::clone(&now), Arc::clone(&max));
            Box::pin(async move {
                let n = now.fetch_add(1, Ordering::SeqCst) + 1;
                max.fetch_max(n, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_secs(3)).await;
                now.fetch_sub(1, Ordering::SeqCst);
                PermissionRead::Unavailable
            })
        });
        let cancel = CancellationToken::new();
        tokio::spawn(run(
            reader,
            mode_rx,
            screen_tx,
            mic_tx,
            cancel.clone(),
            || 0,
        ));
        tokio::time::sleep(Duration::from_secs(20)).await;
        assert_eq!(max_seen.load(Ordering::SeqCst), 1);
        cancel.cancel();
    }
}
