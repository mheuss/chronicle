//! Pure decisions for the permission monitor's reads (design §3, §4).
//! Nothing here touches the supervisor, the audio pipeline or the settings file.

use std::future::Future;
use std::time::Duration;

use chronicle_ipc::MicState;
use tokio::sync::watch;

use crate::capture_supervisor::{Gates, ReconcileOutcome};
use crate::permissions::PermissionRead;

/// How often the monitor reads (design §2, AD6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MonitorMode {
    Gated,
    Permitted,
    Idle,
}

impl MonitorMode {
    pub(crate) fn interval(self) -> Option<Duration> {
        match self {
            Self::Gated => Some(Duration::from_secs(2)),
            Self::Permitted => Some(Duration::from_secs(5)),
            Self::Idle => None,
        }
    }
}

pub(crate) fn mode_for(g: Gates) -> MonitorMode {
    if g.user_paused || g.system_asleep {
        MonitorMode::Idle
    } else if g.sr_denied || g.sr_stale {
        MonitorMode::Gated
    } else {
        MonitorMode::Permitted
    }
}

/// What the monitor watches: its mode, the supervisor's idle epoch and the
/// event loop's mic generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MonitorState {
    pub mode: MonitorMode,
    pub epoch: u64,
    pub mic_generation: u64,
}

pub(crate) fn state_for(g: Gates, mic_generation: u64) -> MonitorState {
    MonitorState {
        mode: mode_for(g),
        epoch: g.idle_epoch,
        mic_generation,
    }
}

/// Publish the state only when it changed.
/// An unchanged state must not interrupt the monitor's wait.
pub(crate) fn publish_state(tx: &watch::Sender<MonitorState>, gates: Gates, mic_generation: u64) {
    let state = state_for(gates, mic_generation);
    tx.send_if_modified(|m| {
        if *m == state {
            false
        } else {
            *m = state;
            true
        }
    });
}

/// The screen arm acts on a read only if it was taken in the current idle epoch.
/// Capture must also be neither paused nor asleep. See design §3 Step 1 and AD14.
pub(crate) fn accept_screen_read(read_epoch: u64, g: Gates) -> bool {
    read_epoch == g.idle_epoch && !g.user_paused && !g.system_asleep
}

/// The mic arm acts on a read only if no mic command has succeeded since the
/// read started (design §3, AD20).
pub(crate) fn accept_mic_read(read_generation: u64, current_generation: u64) -> bool {
    read_generation == current_generation
}

/// Publish the new state before the reconcile runs.
/// The monitor then stops reading before a teardown begins (design §1.2, AD21).
pub(crate) async fn publish_then_reconcile(
    tx: &watch::Sender<MonitorState>,
    gates: Gates,
    mic_generation: u64,
    reconcile: impl Future<Output = ReconcileOutcome>,
) -> ReconcileOutcome {
    publish_state(tx, gates, mic_generation);
    reconcile.await
}

/// Unavailable reads in a row that set the Screen Recording gate (AD7).
pub(crate) const UNAVAILABLE_LIMIT: u8 = 3;

/// Decides the Screen Recording gate from each read (design §3 Step 1).
#[derive(Debug, Default)]
pub(crate) struct SrGate {
    unavailable_streak: u8,
}

impl SrGate {
    /// Returns `Some(new_denied)` when `set_sr_denied` must run.
    pub(crate) fn decide(
        &mut self,
        denied_now: bool,
        stale_now: bool,
        read: PermissionRead,
    ) -> Option<bool> {
        match read {
            PermissionRead::Authorized => {
                self.unavailable_streak = 0;
                (denied_now || stale_now).then_some(false)
            }
            PermissionRead::Denied | PermissionRead::Restricted | PermissionRead::NotDetermined => {
                self.unavailable_streak = 0;
                (!denied_now).then_some(true)
            }
            PermissionRead::Unavailable => {
                self.unavailable_streak = self.unavailable_streak.saturating_add(1);
                log::warn!(
                    "screen recording read unavailable ({}/{UNAVAILABLE_LIMIT})",
                    self.unavailable_streak
                );
                (self.unavailable_streak >= UNAVAILABLE_LIMIT && !denied_now).then_some(true)
            }
        }
    }
}

/// True when the mic arm must call `reapply_mic_gate` (design §3 Step 2).
pub(crate) fn mic_tick_calls_gate(
    read: PermissionRead,
    atom: MicState,
    intent_on: bool,
    capture_running: bool,
) -> bool {
    match read {
        // Drop, whatever the atom holds (AD17).
        PermissionRead::Denied | PermissionRead::Restricted | PermissionRead::NotDetermined => true,
        // Drop, except from Error. The gate would store Off there. The next
        // Authorized read would then restart the mic.
        PermissionRead::Unavailable => atom != MicState::Error,
        // Restart, only after a drop caused by an Unavailable read. Never from Error.
        PermissionRead::Authorized => atom == MicState::Off && intent_on && capture_running,
    }
}

/// Tracks the first capture start after a grant (design §4).
#[derive(Debug, Default)]
pub(crate) struct PostGrant {
    pending: bool,
}

impl PostGrant {
    pub(crate) fn granted(&mut self) {
        self.pending = true;
    }

    /// Returns the exit code the daemon must exit with, if any.
    pub(crate) fn observe(&mut self, outcome: &ReconcileOutcome) -> Option<u8> {
        if !self.pending {
            return None;
        }
        match outcome {
            ReconcileOutcome::Started => {
                self.pending = false;
                None
            }
            ReconcileOutcome::StartFailed { .. } => {
                self.pending = false;
                Some(EXIT_RESTART_FOR_PERMISSIONS)
            }
            _ => None,
        }
    }
}

/// Exit code 4: restart for fresh permissions (design §4).
pub(crate) const EXIT_RESTART_FOR_PERMISSIONS: u8 = 4;

/// True when a gate change counts as a grant and arms `PostGrant`: the gate
/// was denied and the read cleared it. A read that only clears `sr_stale`
/// is not a grant (design §4, R1).
pub(crate) fn grant_cleared(denied_now: bool, new_denied: bool) -> bool {
    denied_now && !new_denied
}

#[cfg(test)]
mod tests {
    use super::*;
    use PermissionRead::*;

    fn gates(user_paused: bool, system_asleep: bool, sr_denied: bool, sr_stale: bool) -> Gates {
        Gates {
            user_paused,
            system_asleep,
            sr_denied,
            sr_stale,
            idle_epoch: 0,
        }
    }

    #[test]
    fn mode_table() {
        assert_eq!(
            mode_for(gates(false, false, false, false)),
            MonitorMode::Permitted
        );
        assert_eq!(
            mode_for(gates(false, false, true, false)),
            MonitorMode::Gated
        );
        assert_eq!(
            mode_for(gates(false, false, false, true)),
            MonitorMode::Gated
        );
        assert_eq!(mode_for(gates(true, false, true, true)), MonitorMode::Idle);
        assert_eq!(
            mode_for(gates(false, true, false, false)),
            MonitorMode::Idle
        );
        assert_eq!(MonitorMode::Gated.interval(), Some(Duration::from_secs(2)));
        assert_eq!(
            MonitorMode::Permitted.interval(),
            Some(Duration::from_secs(5))
        );
        assert_eq!(MonitorMode::Idle.interval(), None);
    }

    #[test]
    fn publish_state_skips_an_unchanged_state() {
        let (tx, mut rx) = watch::channel(state_for(gates(false, false, false, false), 0));
        publish_state(&tx, gates(false, false, false, false), 0);
        assert!(!rx.has_changed().unwrap());
        publish_state(
            &tx,
            Gates {
                idle_epoch: 1,
                ..gates(true, false, false, true)
            },
            2,
        );
        assert!(rx.has_changed().unwrap());
        assert_eq!(
            *rx.borrow_and_update(),
            MonitorState {
                mode: MonitorMode::Idle,
                epoch: 1,
                mic_generation: 2
            }
        );
    }

    #[test]
    fn a_mic_read_from_an_older_generation_is_dropped() {
        assert!(
            !accept_mic_read(0, 1),
            "a read taken before the user's command"
        );
        assert!(accept_mic_read(1, 1));
    }

    #[tokio::test(start_paused = true)]
    async fn publish_happens_before_a_held_open_reconcile() {
        let (tx, rx) = watch::channel(state_for(gates(false, false, false, false), 0));
        let (release, held) = tokio::sync::oneshot::channel::<()>();
        let paused = Gates {
            idle_epoch: 1,
            ..gates(true, false, false, true)
        };
        let task = tokio::spawn(async move {
            publish_then_reconcile(&tx, paused, 0, async move {
                held.await.unwrap();
                ReconcileOutcome::Stopped
            })
            .await
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(
            rx.borrow().mode,
            MonitorMode::Idle,
            "the monitor sees Idle during the teardown"
        );
        release.send(()).unwrap();
        assert_eq!(task.await.unwrap(), ReconcileOutcome::Stopped);
    }

    #[test]
    fn a_read_from_an_older_epoch_is_dropped() {
        // The in-flight case: the read started in epoch 0, then a pause and
        // a resume moved the supervisor to epoch 1.
        assert!(!accept_screen_read(
            0,
            Gates {
                idle_epoch: 1,
                ..gates(false, false, false, true)
            }
        ));
        assert!(accept_screen_read(
            1,
            Gates {
                idle_epoch: 1,
                ..gates(false, false, false, true)
            }
        ));
    }

    #[test]
    fn a_read_arriving_while_idle_is_dropped() {
        // The queued case: the read was published before the pause. The loop
        // picked it up after the pause arm ran.
        assert!(!accept_screen_read(0, gates(true, false, false, true)));
        assert!(!accept_screen_read(0, gates(false, true, false, true)));
    }

    #[test]
    fn sr_gate_table() {
        let mut g = SrGate::default();
        assert_eq!(g.decide(false, false, Authorized), None);
        assert_eq!(g.decide(true, false, Authorized), Some(false));
        assert_eq!(g.decide(false, true, Authorized), Some(false));
        for read in [Denied, Restricted, NotDetermined] {
            assert_eq!(g.decide(false, false, read), Some(true));
            assert_eq!(g.decide(true, false, read), None);
        }
    }

    #[test]
    fn unavailable_never_clears_the_gate() {
        let mut g = SrGate::default();
        for _ in 0..5 {
            assert_eq!(g.decide(true, true, Unavailable), None);
        }
    }

    #[test]
    fn three_unavailable_reads_set_the_gate() {
        let mut g = SrGate::default();
        assert_eq!(g.decide(false, false, Unavailable), None);
        assert_eq!(g.decide(false, false, Unavailable), None);
        assert_eq!(g.decide(false, false, Unavailable), Some(true));
    }

    #[test]
    fn a_real_read_resets_the_streak() {
        let mut g = SrGate::default();
        g.decide(false, false, Unavailable);
        g.decide(false, false, Unavailable);
        assert_eq!(g.decide(false, false, Authorized), None);
        assert_eq!(g.decide(false, false, Unavailable), None);
        assert_eq!(g.decide(false, false, Unavailable), None);
        assert_eq!(g.decide(false, false, Unavailable), Some(true));
    }

    const READS: [PermissionRead; 5] = [Authorized, Denied, Restricted, NotDetermined, Unavailable];
    const ATOMS: [MicState; 4] = [
        MicState::Off,
        MicState::On,
        MicState::PermissionDenied,
        MicState::Error,
    ];

    /// The design's two cases (design §3 Step 2), written as data rather
    /// than as the function's own match.
    fn expected(read: PermissionRead, atom: MicState, intent: bool, running: bool) -> bool {
        let drop = read != Authorized && !(read == Unavailable && atom == MicState::Error);
        let restart = read == Authorized && atom == MicState::Off && intent && running;
        drop || restart
    }

    #[test]
    fn mic_tick_table() {
        for read in READS {
            for atom in ATOMS {
                for intent in [false, true] {
                    for running in [false, true] {
                        assert_eq!(
                            mic_tick_calls_gate(read, atom, intent, running),
                            expected(read, atom, intent, running),
                            "read={read:?} atom={atom:?} intent={intent} running={running}"
                        );
                    }
                }
            }
        }
    }

    /// CHR-95's gate result for one call when every audio pipeline call
    /// succeeds: the atom it stores and whether it keeps the saved intent.
    /// A failed start, or a failed drop on a plain stop, stores Error instead,
    /// which this model omits.
    fn gate_result(read: PermissionRead, intent: bool, running: bool) -> (MicState, bool) {
        let missing = matches!(read, Denied | Restricted | NotDetermined);
        let atom = if read == Authorized && intent && running {
            MicState::On
        } else {
            MicState::Off
        };
        (atom, intent && !missing)
    }

    /// Run a sequence of reads through the tick and the modelled gate.
    fn run_ticks(
        reads: &[PermissionRead],
        mut atom: MicState,
        mut intent: bool,
        running: bool,
    ) -> (MicState, bool) {
        for &read in reads {
            if mic_tick_calls_gate(read, atom, intent, running) {
                (atom, intent) = gate_result(read, intent, running);
            }
        }
        (atom, intent)
    }

    /// The regression sequences (design §3 Step 2, Drop case).
    #[test]
    fn mic_tick_sequences() {
        // On, Unavailable, Denied, Authorized: the mic ends off, intent cleared.
        assert_eq!(
            run_ticks(&[Unavailable, Denied, Authorized], MicState::On, true, true),
            (MicState::Off, false)
        );
        // A Screen Recording teardown left the atom Off in the tick of a mic
        // Denied, with capture not running: the intent is still cleared.
        assert_eq!(
            run_ticks(&[Denied], MicState::Off, true, false),
            (MicState::Off, false)
        );
        // An Unavailable stop alone keeps the intent, and the next Authorized
        // read restarts the mic.
        assert_eq!(
            run_ticks(&[Unavailable, Authorized], MicState::On, true, true),
            (MicState::On, true)
        );
        // From Error, a revoke still clears the intent.
        // The grant after it starts nothing.
        assert_eq!(
            run_ticks(&[Denied, Authorized], MicState::Error, true, true),
            (MicState::Off, false)
        );
        // Error never restarts on an Authorized read.
        assert_eq!(
            run_ticks(&[Authorized], MicState::Error, true, true),
            (MicState::Error, true)
        );
        // An Unavailable read from Error makes no call. The grant after it
        // restarts nothing.
        assert_eq!(
            run_ticks(&[Unavailable, Authorized], MicState::Error, true, true),
            (MicState::Error, true)
        );
    }

    #[test]
    fn post_grant_waits_for_an_actual_start() {
        let mut p = PostGrant::default();
        p.granted();
        assert_eq!(p.observe(&ReconcileOutcome::NoChange), None);
        assert_eq!(p.observe(&ReconcileOutcome::Started), None);
        // Cleared: a later failure is ordinary.
        assert_eq!(
            p.observe(&ReconcileOutcome::StartFailed {
                partial_teardown: false
            }),
            None
        );
    }

    #[test]
    fn only_clearing_a_real_denial_is_a_grant() {
        // (denied_now, new_denied) -> granted
        assert!(grant_cleared(true, false), "denied to allowed is a grant");
        assert!(
            !grant_cleared(false, false),
            "clearing only sr_stale is not a grant (R1)"
        );
        assert!(!grant_cleared(false, true), "a denial is not a grant");
        assert!(!grant_cleared(true, true), "still denied");
    }

    #[test]
    fn post_grant_start_failure_exits() {
        let mut p = PostGrant::default();
        assert_eq!(
            p.observe(&ReconcileOutcome::StartFailed {
                partial_teardown: false
            }),
            None
        );
        p.granted();
        assert_eq!(p.observe(&ReconcileOutcome::NoChange), None);
        assert_eq!(
            p.observe(&ReconcileOutcome::StartFailed {
                partial_teardown: false
            }),
            Some(4)
        );
    }
}
