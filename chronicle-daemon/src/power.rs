//! macOS sleep/wake observer. Watches four NSWorkspace notifications on a
//! dedicated CFRunLoop thread and forwards them to the daemon event loop.
//!
//! The observer is fire-and-forget: it does `try_send` and drops on a full or
//! closed channel. Timeline correctness is guaranteed by SegmentAccumulator
//! gap detection (design §5.2), not by the observer reacting before the
//! machine sleeps — that is why dropping a `PowerEvent` is acceptable even
//! though dropping an audio segment is not (the audio-bridge thread's
//! `blocking_send` precedent therefore does not apply here).
//!
//! Module-wide `dead_code` allow: this module is loaded now (Task 10) so it
//! can be exercised by a smoke test, but the daemon event loop only consumes
//! it in Task 11.
#![allow(dead_code)]

use std::io;
use std::sync::mpsc as std_mpsc;
use std::thread::{self, JoinHandle};

use block2::RcBlock;
use objc2_app_kit::{
    NSWorkspace, NSWorkspaceDidWakeNotification, NSWorkspaceScreensDidSleepNotification,
    NSWorkspaceScreensDidWakeNotification, NSWorkspaceWillSleepNotification,
};
use objc2_core_foundation::CFRunLoop;
use objc2_foundation::NSNotificationName;
use tokio::sync::mpsc;

/// A macOS power transition. Fire-and-forget — no acknowledgement: timeline
/// correctness is guaranteed by SegmentAccumulator gap detection, not by the
/// observer reacting before the machine sleeps (design §5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerEvent {
    SystemSleep,
    SystemWake,
    DisplaySleep,
    DisplayWake,
}

/// Spawn the observer thread. It registers four NSWorkspace observers, then
/// runs a CFRunLoop; each notification does a `try_send` of the matching
/// `PowerEvent` (drop + warn on full — gap detection is the correctness
/// backbone, so dropping a power event is acceptable). Blocks on a startup
/// handshake and returns `Ok(handle)` once registration has succeeded. `Err`
/// means observer registration failed — the caller logs and continues
/// (capture still works).
///
/// The returned handle is intentionally not joined at shutdown; process exit
/// reaps the thread.
pub fn spawn_power_observer(tx: mpsc::Sender<PowerEvent>) -> io::Result<JoinHandle<()>> {
    let (handshake_tx, handshake_rx) = std_mpsc::sync_channel::<io::Result<()>>(0);

    let handle = thread::Builder::new()
        .name("power-observer".into())
        .spawn(move || run_observer_thread(tx, handshake_tx))?;

    match handshake_rx.recv() {
        Ok(Ok(())) => Ok(handle),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(io::Error::other("observer thread panicked during setup")),
    }
}

/// The body of the spawned observer thread. Wraps the ObjC setup phase in
/// `autoreleasepool` per docs/development/objc2-interop.md (placed at the
/// public boundary of this thread, not deeper), registers the four
/// observers, signals the handshake, then parks in `CFRunLoop::run()`.
fn run_observer_thread(
    tx: mpsc::Sender<PowerEvent>,
    handshake_tx: std_mpsc::SyncSender<io::Result<()>>,
) {
    let setup_result = objc2::rc::autoreleasepool(|_| register_observers(&tx));

    // Keep observer tokens alive for the life of the thread. Dropping them
    // would deregister the observers immediately.
    let _tokens = match setup_result {
        Ok(tokens) => {
            // Best-effort: if the parent dropped the receiver, just continue —
            // observers are still wired up, but the thread will exit cleanly
            // when CFRunLoop::run returns (it normally does not).
            let _ = handshake_tx.send(Ok(()));
            tokens
        }
        Err(e) => {
            let _ = handshake_tx.send(Err(e));
            return;
        }
    };

    // Park here for the life of the process. The observer tokens live on
    // this thread's stack (in `_tokens`); if the thread returned, the
    // tokens would drop and NSNotificationCenter would deregister the
    // observers. With `queue: None` the block fires on the posting thread
    // (NSWorkspace posts on the main thread), so this run loop is not
    // strictly needed for dispatch — it is needed to keep the tokens
    // alive. Process exit reaps the thread; CFRunLoop::run does not
    // return under normal operation.
    CFRunLoop::run();
}

/// Register the four NSWorkspace observers and return the tokens. The caller
/// must keep the returned vector alive for the life of the thread. The
/// `io::Result` is a forward-defensive contract: today every path returns
/// `Ok(...)` and this function cannot construct an `Err`. The `Result` is
/// reserved for a future case where a registration call could fail (e.g.,
/// Apple changes the API, or a future binding version surfaces failures).
fn register_observers(
    tx: &mpsc::Sender<PowerEvent>,
) -> io::Result<
    Vec<objc2::rc::Retained<objc2::runtime::ProtocolObject<dyn objc2::runtime::NSObjectProtocol>>>,
> {
    let workspace = NSWorkspace::sharedWorkspace();
    let center = workspace.notificationCenter();

    // SAFETY: these statics are AppKit-provided NSNotificationName
    // references. They are valid for the life of the process.
    let pairs: [(&'static NSNotificationName, PowerEvent); 4] = unsafe {
        [
            (NSWorkspaceWillSleepNotification, PowerEvent::SystemSleep),
            (NSWorkspaceDidWakeNotification, PowerEvent::SystemWake),
            (
                NSWorkspaceScreensDidSleepNotification,
                PowerEvent::DisplaySleep,
            ),
            (
                NSWorkspaceScreensDidWakeNotification,
                PowerEvent::DisplayWake,
            ),
        ]
    };

    let mut tokens = Vec::with_capacity(pairs.len());
    for (name, event) in pairs {
        let tx_clone = tx.clone();
        // Passing queue=None means the block is invoked synchronously on
        // the notification's posting thread (NSWorkspace posts on the
        // main thread). The block does a non-blocking try_send and logs
        // on drop. Never block here — these are OS-delivered callbacks
        // (Structured Backpressure in SCK Callbacks, applied by
        // analogy: blocking would stall AppKit notification dispatch).
        //
        // Closure capture contract: `PowerEvent` is `Copy`, so the
        // closure captures `tx_clone` by move and `event` by copy on
        // each invocation, satisfying the block API's `Fn` bound. If
        // `PowerEvent` ever becomes non-`Copy` (e.g., gains a `String`
        // payload), this needs to be reworked.
        let block = RcBlock::new(move |_note| {
            let _ = tx_clone.try_send(event).inspect_err(|e| {
                log::warn!("dropped power event {event:?}: {e}");
            });
        });

        // SAFETY: name is a valid NSNotificationName static; obj is
        // None (observe all senders); queue is None (deliver on the
        // posting thread, which is fine — the block only does a
        // non-blocking try_send). The block is held by the
        // notification center for the life of the observer token,
        // which we store in `tokens`.
        let token = unsafe {
            center.addObserverForName_object_queue_usingBlock(Some(name), None, None, &block)
        };
        tokens.push(token);
    }

    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_power_observer_returns_a_handle() {
        // This test intentionally leaks the spawned `power-observer` thread.
        // The thread parks in `CFRunLoop::run()`, which never returns under
        // normal operation; process exit reaps it. Do NOT try to `.join()`
        // the returned handle — it will deadlock.
        let (tx, _rx) = tokio::sync::mpsc::channel::<PowerEvent>(8);
        let result = spawn_power_observer(tx);
        assert!(
            result.is_ok(),
            "spawn_power_observer should return Ok(handle): {result:?}"
        );
    }
}
