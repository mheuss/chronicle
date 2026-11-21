//! The microphone permission gate (design §1.2). Every mic start goes through
//! `apply_mic_intent`. Every caller runs on the event loop task, because the
//! gate writes settings (see the single-writer note in `main.rs`).

use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};

use chronicle_audio::{AudioPipeline, MicToggleOutcome};
use chronicle_ipc::MicState;

use crate::ipc_handler::map_outcome;
use crate::permissions::PermissionRead;
use crate::settings;

/// The two pipeline calls the gate makes, so tests can use a fake.
pub(crate) trait MicControl {
    fn set_microphone_enabled(&self, enabled: bool) -> MicToggleOutcome;
    fn drop_microphone(&self) -> MicToggleOutcome;
}

impl MicControl for AudioPipeline {
    fn set_microphone_enabled(&self, enabled: bool) -> MicToggleOutcome {
        AudioPipeline::set_microphone_enabled(self, enabled)
    }

    fn drop_microphone(&self) -> MicToggleOutcome {
        AudioPipeline::drop_microphone(self)
    }
}

/// One gate decision: the IPC reply, and the value `Status` stores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GateOutcome {
    pub reply: MicState,
    pub status: MicState,
}

impl GateOutcome {
    fn same(state: MicState) -> Self {
        Self {
            reply: state,
            status: state,
        }
    }
}

/// Apply `wanted` under a fresh permission read. `permission` runs only for a
/// start, so a stop never spawns the helper.
pub(crate) fn apply_mic_intent(
    audio: &impl MicControl,
    base_dir: &Path,
    wanted: bool,
    permission: impl FnOnce() -> PermissionRead,
) -> GateOutcome {
    if !wanted {
        // Every stop drops the engine, so the next allowed start builds a new
        // one (design §1.1).
        return GateOutcome::same(map_outcome(audio.drop_microphone()));
    }
    match permission() {
        PermissionRead::Authorized => {
            GateOutcome::same(map_outcome(audio.set_microphone_enabled(true)))
        }
        read => {
            let _ = audio.drop_microphone();
            // Architectural Decision 11: a failed read is not a permission
            // event, so only a real refusal clears the intent (AD 10).
            if read != PermissionRead::Unavailable {
                settings::write_mic_setting(base_dir, false);
            }
            log::info!("mic gate: refused start, permission={read:?}");
            GateOutcome {
                reply: MicState::PermissionDenied,
                status: MicState::Off,
            }
        }
    }
}

/// Re-apply the gate to the saved intent, then store the `Status` value. It
/// writes the intent only to clear it on a missing grant, and it does that
/// whether or not the engine runs. With `capture_running` false it never
/// starts the engine. Callers must be on the event loop task, as
/// `CaptureSupervisor::start` is (design §1.2).
///
/// A periodic caller must call this on every read other than `Authorized`,
/// whatever `mic_state` is. Calling it only while the mic is `on` misses a
/// revoke that follows an `Unavailable` stop, and a later grant would then
/// restart the mic without a click. The one exception is an `Unavailable` read
/// while `mic_state` is `error`: skip it, since it cannot clear intent and
/// would turn `error` into `off`. A caller that already holds a read passes it
/// as `|| read`, so the read that decided the call is the one acted on.
pub(crate) fn reapply_mic_gate(
    audio: &impl MicControl,
    base_dir: &Path,
    permission: impl FnOnce() -> PermissionRead,
    capture_running: bool,
    mic_state: &AtomicU8,
) -> MicState {
    let intent = settings::read_mic_setting(base_dir);
    let outcome = if capture_running {
        apply_mic_intent(audio, base_dir, intent, permission)
    } else if !intent {
        apply_mic_intent(audio, base_dir, false, permission)
    } else {
        // Capture is stopped, so an Authorized read becomes a plain stop.
        // Any other read still goes through the refusal path.
        match permission() {
            PermissionRead::Authorized => {
                apply_mic_intent(audio, base_dir, false, || PermissionRead::Authorized)
            }
            read => apply_mic_intent(audio, base_dir, true, || read),
        }
    };
    mic_state.store(outcome.status as u8, Ordering::Release);
    outcome.status
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Records each call. Answers an enable with `start`, and a stop or a
    /// drop with `stop`.
    struct FakeMic {
        calls: RefCell<Vec<&'static str>>,
        start: MicToggleOutcome,
        stop: MicToggleOutcome,
    }

    impl FakeMic {
        fn new() -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                start: MicToggleOutcome::Enabled,
                stop: MicToggleOutcome::Disabled,
            }
        }
    }

    impl MicControl for FakeMic {
        fn set_microphone_enabled(&self, enabled: bool) -> MicToggleOutcome {
            self.calls
                .borrow_mut()
                .push(if enabled { "start" } else { "stop" });
            if enabled {
                self.start.clone()
            } else {
                self.stop.clone()
            }
        }

        fn drop_microphone(&self) -> MicToggleOutcome {
            self.calls.borrow_mut().push("drop");
            self.stop.clone()
        }
    }

    fn intent_on() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        settings::write_mic_setting(dir.path(), true);
        dir
    }

    #[test]
    fn stop_never_reads_permission() {
        let dir = intent_on();
        let mic = FakeMic::new();
        let out = apply_mic_intent(&mic, dir.path(), false, || panic!("read on a stop"));
        assert_eq!(out, GateOutcome::same(MicState::Off));
        assert_eq!(*mic.calls.borrow(), vec!["drop"]);
    }

    #[test]
    fn failed_stop_is_error_under_every_permission() {
        // The stop path never reads, so no permission can turn a failed stop
        // into permission_denied.
        for read in [
            PermissionRead::Authorized,
            PermissionRead::NotDetermined,
            PermissionRead::Denied,
            PermissionRead::Restricted,
            PermissionRead::Unavailable,
        ] {
            let dir = intent_on();
            let mut mic = FakeMic::new();
            mic.stop = MicToggleOutcome::Failed { reason: "x".into() };
            let out = apply_mic_intent(&mic, dir.path(), false, || -> PermissionRead {
                panic!("read on a stop ({read:?})")
            });
            assert_eq!(out, GateOutcome::same(MicState::Error), "{read:?}");
        }
    }

    #[test]
    fn authorized_start_reports_on() {
        let dir = intent_on();
        let mic = FakeMic::new();
        let out = apply_mic_intent(&mic, dir.path(), true, || PermissionRead::Authorized);
        assert_eq!(out, GateOutcome::same(MicState::On));
        assert_eq!(*mic.calls.borrow(), vec!["start"]);
        assert!(settings::read_mic_setting(dir.path()));
    }

    #[test]
    fn authorized_start_failure_reports_error() {
        let dir = intent_on();
        let mut mic = FakeMic::new();
        mic.start = MicToggleOutcome::Failed { reason: "x".into() };
        let out = apply_mic_intent(&mic, dir.path(), true, || PermissionRead::Authorized);
        assert_eq!(out, GateOutcome::same(MicState::Error));
        assert!(
            settings::read_mic_setting(dir.path()),
            "a failed start is not a permission event, so intent stays"
        );
    }

    #[test]
    fn missing_grant_drops_the_engine_and_clears_intent() {
        for read in [
            PermissionRead::NotDetermined,
            PermissionRead::Denied,
            PermissionRead::Restricted,
        ] {
            let dir = intent_on();
            let mic = FakeMic::new();
            let out = apply_mic_intent(&mic, dir.path(), true, || read);
            assert_eq!(
                out,
                GateOutcome {
                    reply: MicState::PermissionDenied,
                    status: MicState::Off
                },
                "{read:?}"
            );
            assert_eq!(*mic.calls.borrow(), vec!["drop"], "{read:?}");
            assert!(
                !settings::read_mic_setting(dir.path()),
                "{read:?} must clear intent"
            );
        }
    }

    #[test]
    fn unavailable_read_drops_the_engine_and_keeps_intent() {
        let dir = intent_on();
        let mic = FakeMic::new();
        let out = apply_mic_intent(&mic, dir.path(), true, || PermissionRead::Unavailable);
        assert_eq!(
            out,
            GateOutcome {
                reply: MicState::PermissionDenied,
                status: MicState::Off
            }
        );
        assert_eq!(*mic.calls.borrow(), vec!["drop"]);
        assert!(
            settings::read_mic_setting(dir.path()),
            "Unavailable must keep intent"
        );
    }

    #[test]
    fn unavailable_read_after_an_authorized_start_stops_the_mic() {
        // The in-process read would still say Authorized here. The gate never
        // sees it: the failed helper read alone decides (design N9).
        let dir = intent_on();
        let mic = FakeMic::new();
        apply_mic_intent(&mic, dir.path(), true, || PermissionRead::Authorized);
        let out = apply_mic_intent(&mic, dir.path(), true, || PermissionRead::Unavailable);
        assert_eq!(out.status, MicState::Off);
        assert_eq!(*mic.calls.borrow(), vec!["start", "drop"]);
    }

    #[test]
    fn reapply_starts_for_saved_intent_and_stores_status() {
        let dir = intent_on();
        let mic = FakeMic::new();
        let atom = AtomicU8::new(MicState::Off as u8);
        let s = reapply_mic_gate(&mic, dir.path(), || PermissionRead::Authorized, true, &atom);
        assert_eq!(s, MicState::On);
        assert_eq!(atom.load(Ordering::Acquire), MicState::On as u8);
        assert!(
            settings::read_mic_setting(dir.path()),
            "reapply must not write intent on a start"
        );
    }

    #[test]
    fn reapply_never_starts_when_capture_is_not_running() {
        let dir = intent_on();
        let mic = FakeMic::new();
        let atom = AtomicU8::new(MicState::On as u8);
        let s = reapply_mic_gate(
            &mic,
            dir.path(),
            || PermissionRead::Authorized,
            false,
            &atom,
        );
        assert_eq!(s, MicState::Off);
        assert_eq!(atom.load(Ordering::Acquire), MicState::Off as u8);
        assert_eq!(*mic.calls.borrow(), vec!["drop"]);
        assert!(
            settings::read_mic_setting(dir.path()),
            "a not-running stop keeps intent"
        );
    }

    #[test]
    fn reapply_with_capture_stopped_and_unavailable_keeps_intent() {
        let dir = intent_on();
        let mic = FakeMic::new();
        let atom = AtomicU8::new(MicState::On as u8);
        let s = reapply_mic_gate(
            &mic,
            dir.path(),
            || PermissionRead::Unavailable,
            false,
            &atom,
        );
        assert_eq!(s, MicState::Off);
        assert_eq!(*mic.calls.borrow(), vec!["drop"]);
        assert!(
            settings::read_mic_setting(dir.path()),
            "Unavailable keeps intent"
        );
    }

    #[test]
    fn reapply_on_a_revoke_stops_and_clears_intent() {
        let dir = intent_on();
        let mic = FakeMic::new();
        let atom = AtomicU8::new(MicState::On as u8);
        let s = reapply_mic_gate(&mic, dir.path(), || PermissionRead::Denied, true, &atom);
        assert_eq!(s, MicState::Off);
        assert_eq!(atom.load(Ordering::Acquire), MicState::Off as u8);
        assert!(!settings::read_mic_setting(dir.path()));
    }

    #[test]
    fn reapply_clears_intent_when_the_engine_is_already_off() {
        // An earlier Unavailable read stopped the engine and kept the intent.
        // A later Denied read must still clear it (Architectural Decision 10).
        let dir = intent_on();
        let mic = FakeMic::new();
        let atom = AtomicU8::new(MicState::Off as u8);
        let s = reapply_mic_gate(&mic, dir.path(), || PermissionRead::Denied, true, &atom);
        assert_eq!(s, MicState::Off);
        assert_eq!(*mic.calls.borrow(), vec!["drop"]);
        assert!(!settings::read_mic_setting(dir.path()));
    }

    #[test]
    fn reapply_clears_intent_while_capture_is_stopped() {
        let dir = intent_on();
        let mic = FakeMic::new();
        let atom = AtomicU8::new(MicState::Off as u8);
        let s = reapply_mic_gate(
            &mic,
            dir.path(),
            || PermissionRead::Restricted,
            false,
            &atom,
        );
        assert_eq!(s, MicState::Off);
        assert_eq!(*mic.calls.borrow(), vec!["drop"]);
        assert!(!settings::read_mic_setting(dir.path()));
    }

    #[test]
    fn reapply_skips_the_read_for_intent_off() {
        let dir = tempfile::tempdir().unwrap();
        let mic = FakeMic::new();
        let atom = AtomicU8::new(MicState::On as u8);
        let s = reapply_mic_gate(
            &mic,
            dir.path(),
            || panic!("read with intent off"),
            true,
            &atom,
        );
        assert_eq!(s, MicState::Off);
        assert_eq!(atom.load(Ordering::Acquire), MicState::Off as u8);
        assert_eq!(*mic.calls.borrow(), vec!["drop"]);
    }

    #[test]
    fn reapply_skips_the_read_for_intent_off_while_capture_is_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let mic = FakeMic::new();
        let atom = AtomicU8::new(MicState::Off as u8);
        let s = reapply_mic_gate(
            &mic,
            dir.path(),
            || panic!("read with intent off"),
            false,
            &atom,
        );
        assert_eq!(s, MicState::Off);
        assert_eq!(*mic.calls.borrow(), vec!["drop"]);
    }

    #[test]
    fn reapply_with_intent_off_stops() {
        let dir = tempfile::tempdir().unwrap();
        let mic = FakeMic::new();
        let atom = AtomicU8::new(MicState::Off as u8);
        let s = reapply_mic_gate(&mic, dir.path(), || PermissionRead::Authorized, true, &atom);
        assert_eq!(s, MicState::Off);
        assert_eq!(*mic.calls.borrow(), vec!["drop"]);
    }
}
