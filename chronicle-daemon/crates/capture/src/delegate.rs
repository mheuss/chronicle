//! Stream delegate that reports ScreenCaptureKit stopping a stream.
//!
//! When SCK stops a stream on its own, the callback moves the engine from
//! `Running` to `Failed` and sends one `StreamStopped` notice. The SDK header
//! doesn't name the callback queue, so the callback does a compare-and-swap,
//! a retain and a `try_send`, and nothing else.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AnyThread, DefinedClass, Message, define_class};
use objc2_foundation::{NSError, NSInteger};
use objc2_screen_capture_kit::{
    SCStream, SCStreamDelegate, SCStreamErrorCode, SCStreamErrorDomain,
};
use tokio::sync::mpsc;

use crate::EngineState;

/// One stream stop, as the delegate saw it.
pub struct StreamStopped {
    /// The `CaptureConfig::run` of the engine that owned the stream.
    pub run: u64,
    pub display_id: u32,
    pub error: Retained<NSError>,
    /// True for the one notice that moved its engine from `Running` to `Failed`.
    pub won_cas: bool,
}

impl StreamStopped {
    pub fn is_stream_domain(&self) -> bool {
        // SAFETY: SCStreamErrorDomain is an immutable NSString constant.
        &*self.error.domain() == unsafe { SCStreamErrorDomain }
    }

    pub fn domain(&self) -> String {
        self.error.domain().to_string()
    }

    pub fn code(&self) -> NSInteger {
        self.error.code()
    }

    pub fn description(&self) -> String {
        self.error.localizedDescription().to_string()
    }
}

/// What a stream stop means for capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopCause {
    /// SCStreamErrorUserStopped (-3817).
    UserStopped,
    Other,
}

pub fn stop_cause(is_stream_domain: bool, code: NSInteger) -> StopCause {
    if is_stream_domain && code == SCStreamErrorCode::UserStopped.0 {
        StopCause::UserStopped
    } else {
        StopCause::Other
    }
}

/// The whole callback body, as a plain fn so tests reach it without SCK.
/// A full or closed channel drops the notice. The CAS still stands.
pub(crate) fn on_stop(
    state: &AtomicU8,
    notices: &mpsc::Sender<StreamStopped>,
    run: u64,
    display_id: u32,
    error: Retained<NSError>,
) -> bool {
    let won_cas = state
        .compare_exchange(
            EngineState::Running as u8,
            EngineState::Failed as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_ok();
    let _ = notices.try_send(StreamStopped {
        run,
        display_id,
        error,
        won_cas,
    });
    won_cas
}

pub(crate) struct StreamDelegateIvars {
    display_id: u32,
    run: u64,
    state: Arc<AtomicU8>,
    notices: mpsc::Sender<StreamStopped>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements. We don't implement Drop.
    #[unsafe(super(NSObject))]
    #[name = "ChronicleStreamDelegate"]
    #[ivars = StreamDelegateIvars]
    pub(crate) struct StreamDelegate;

    // SAFETY: NSObjectProtocol has no extra requirements.
    unsafe impl NSObjectProtocol for StreamDelegate {}

    // SAFETY: The signature matches the optional stream:didStopWithError:.
    // The body retains the error and touches only Sync ivars, so any queue
    // is fine.
    unsafe impl SCStreamDelegate for StreamDelegate {
        #[unsafe(method(stream:didStopWithError:))]
        unsafe fn stream_did_stop_with_error(&self, _stream: &SCStream, error: &NSError) {
            let ivars = self.ivars();
            on_stop(
                &ivars.state,
                &ivars.notices,
                ivars.run,
                ivars.display_id,
                error.retain(),
            );
        }
    }
);

impl StreamDelegate {
    /// Build the delegate for one display's stream. `state` is the engine's
    /// state atom.
    pub(crate) fn for_display(
        display_id: u32,
        run: u64,
        notices: mpsc::Sender<StreamStopped>,
        state: Arc<AtomicU8>,
    ) -> Retained<Self> {
        let this = Self::alloc().set_ivars(StreamDelegateIvars {
            display_id,
            run,
            state,
            notices,
        });
        unsafe { objc2::msg_send![super(this), init] }
    }

    /// Get a reference suitable for passing to `initWithFilter:configuration:delegate:`.
    pub(crate) fn as_protocol_object(&self) -> &ProtocolObject<dyn SCStreamDelegate> {
        ProtocolObject::from_ref(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_foundation::NSString;

    fn user_stopped() -> Retained<NSError> {
        // SAFETY: SCStreamErrorDomain is an immutable NSString constant.
        NSError::new(SCStreamErrorCode::UserStopped.0, unsafe {
            SCStreamErrorDomain
        })
    }

    #[test]
    fn a_stop_while_running_fails_the_engine_and_wins() {
        let state = AtomicU8::new(EngineState::Running as u8);
        let (tx, mut rx) = mpsc::channel(4);
        assert!(on_stop(&state, &tx, 3, 7, user_stopped()));
        assert_eq!(
            EngineState::from_u8(state.load(Ordering::Acquire)),
            EngineState::Failed
        );
        let n = rx.try_recv().expect("one notice");
        assert_eq!((n.run, n.display_id, n.won_cas), (3, 7, true));
        assert!(rx.try_recv().is_err(), "exactly one notice");
    }

    #[test]
    fn a_stop_in_any_other_state_changes_nothing() {
        for start in [
            EngineState::Stopping,
            EngineState::Idle,
            EngineState::Poisoned,
        ] {
            let state = AtomicU8::new(start as u8);
            let (tx, mut rx) = mpsc::channel(4);
            assert!(!on_stop(&state, &tx, 1, 7, user_stopped()), "{start:?}");
            assert_eq!(EngineState::from_u8(state.load(Ordering::Acquire)), start);
            let n = rx.try_recv().expect("the stop is still reported");
            assert!(!n.won_cas, "{start:?}");
        }
    }

    #[test]
    fn three_stops_on_one_engine_win_once() {
        let state = AtomicU8::new(EngineState::Running as u8);
        let (tx, _rx) = mpsc::channel(4);
        let wins = (0..3u32)
            .filter(|&display_id| on_stop(&state, &tx, 1, display_id, user_stopped()))
            .count();
        assert_eq!(wins, 1);
    }

    #[test]
    fn a_full_or_closed_channel_still_fails_the_engine() {
        let state = AtomicU8::new(EngineState::Running as u8);
        let (tx, rx) = mpsc::channel(1);
        tx.try_send(StreamStopped {
            run: 0,
            display_id: 0,
            error: user_stopped(),
            won_cas: false,
        })
        .unwrap();
        assert!(on_stop(&state, &tx, 1, 7, user_stopped()), "full channel");
        drop(rx);
        let state = AtomicU8::new(EngineState::Running as u8);
        assert!(on_stop(&state, &tx, 1, 7, user_stopped()), "closed channel");
    }

    #[test]
    fn the_delegate_registers_and_shares_the_engine_state() {
        let state = Arc::new(AtomicU8::new(EngineState::Running as u8));
        let (tx, _rx) = mpsc::channel(4);
        let delegate = StreamDelegate::for_display(7, 3, tx, Arc::clone(&state));
        let _protocol: &ProtocolObject<dyn SCStreamDelegate> = delegate.as_protocol_object();
        assert!(Arc::ptr_eq(&state, &delegate.ivars().state));
        assert_eq!((delegate.ivars().display_id, delegate.ivars().run), (7, 3));
    }

    #[test]
    fn only_a_stream_domain_user_stop_is_user_stopped() {
        assert_eq!(
            stop_cause(true, SCStreamErrorCode::UserStopped.0),
            StopCause::UserStopped
        );
        assert_eq!(
            stop_cause(false, SCStreamErrorCode::UserStopped.0),
            StopCause::Other
        );
        assert_eq!(
            stop_cause(true, SCStreamErrorCode::SystemStoppedStream.0),
            StopCause::Other
        );
        assert_eq!(
            stop_cause(true, SCStreamErrorCode::AttemptToStopStreamState.0),
            StopCause::Other
        );
    }

    #[test]
    fn a_notice_reads_its_error_fields() {
        let state = AtomicU8::new(EngineState::Running as u8);
        let (tx, mut rx) = mpsc::channel(4);
        on_stop(&state, &tx, 1, 7, user_stopped());
        let n = rx.try_recv().unwrap();
        assert!(n.is_stream_domain());
        assert_eq!(n.code(), -3817);
        assert_eq!(n.domain(), unsafe { SCStreamErrorDomain }.to_string());
        assert!(!n.description().is_empty());

        let other = NSError::new(-3817, &NSString::from_str("NSOSStatusErrorDomain"));
        on_stop(&state, &tx, 1, 7, other);
        assert!(!rx.try_recv().unwrap().is_stream_domain());
    }
}
