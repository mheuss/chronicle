//! macOS system sleep/wake observer using IOKit `IORegisterForSystemPower`.
//!
//! `NSWorkspace.willSleepNotification` is a thin wrapper around this same
//! IOKit API, but its dispatch goes through main-thread Cocoa run-loop
//! plumbing that a bare Rust daemon doesn't bootstrap (no `.app` bundle, no
//! `NSApplicationMain`, no `NSPrincipalClass` in an Info.plist). Three
//! NSWorkspace approaches all registered cleanly but never delivered events.
//! Going direct to IOKit bypasses the wrapper. See the design Addendum dated
//! 2026-05-26 for the full investigation.
//!
//! **System sleep only.** `IORegisterForSystemPower` does not fire on display
//! sleep, and `IODisplayWrangler` is absent on Apple Silicon — display-sleep
//! awareness is filed as HEU-496.

#![allow(non_upper_case_globals, non_camel_case_types)]

use std::ffi::c_void;
use std::io;
use std::ptr;
use std::sync::mpsc as std_mpsc;
use std::thread::{self, JoinHandle};

use tokio::sync::mpsc;

// --- IOKit / CoreFoundation raw FFI ---
//
// Raw FFI rather than a bindings crate keeps this module self-contained and
// avoids pulling in another dependency for what is a tiny surface area:
// register, get run-loop source, ack sleep messages, drive a run loop.

type io_object_t = u32;
type io_service_t = io_object_t;
type io_connect_t = io_object_t;

#[repr(C)]
struct IONotificationPort {
    _opaque: [u8; 0],
}
type IONotificationPortRef = *mut IONotificationPort;

#[repr(C)]
struct CFRunLoopSource_ {
    _o: [u8; 0],
}
type CFRunLoopSourceRef = *mut CFRunLoopSource_;

#[repr(C)]
struct CFRunLoop_ {
    _o: [u8; 0],
}
type CFRunLoopRef = *mut CFRunLoop_;

#[repr(C)]
struct CFString_ {
    _o: [u8; 0],
}
type CFStringRef = *const CFString_;

type IOServiceInterestCallback = extern "C" fn(
    refcon: *mut c_void,
    service: io_service_t,
    message_type: u32,
    message_argument: *mut c_void,
);

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IORegisterForSystemPower(
        refcon: *mut c_void,
        port: *mut IONotificationPortRef,
        callback: IOServiceInterestCallback,
        notifier: *mut io_object_t,
    ) -> io_connect_t;
    fn IONotificationPortGetRunLoopSource(port: IONotificationPortRef) -> CFRunLoopSourceRef;
    fn IOAllowPowerChange(kernel_port: io_connect_t, notification_id: isize) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    fn CFRunLoopAddSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
    fn CFRunLoopRun();
    static kCFRunLoopDefaultMode: CFStringRef;
}

// IOMessage codes from `<IOKit/IOMessage.h>`. Stable across macOS versions.
const kIOMessageCanSystemSleep: u32 = 0xe000_0270;
const kIOMessageSystemWillSleep: u32 = 0xe000_0280;
const kIOMessageSystemHasPoweredOn: u32 = 0xe000_0300;

/// A macOS system power transition. Fire-and-forget — the supervisor uses
/// `SegmentAccumulator` gap-detection (design §5.2) as the timeline-
/// correctness backbone, so a dropped event cannot corrupt the timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerEvent {
    SystemSleep,
    SystemWake,
}

/// Refcon held by the C callback. Owns the channel sender for forwarding
/// events and the kernel `root_port` needed to ack sleep messages.
struct Refcon {
    tx: mpsc::Sender<PowerEvent>,
    root_port: io_connect_t,
}

extern "C" fn power_callback(
    refcon: *mut c_void,
    _service: io_service_t,
    message_type: u32,
    message_argument: *mut c_void,
) {
    // SAFETY: `refcon` points to the `Refcon` we leaked at registration time.
    // The box lives for the life of the process — the observer thread holds
    // the backing memory until process exit.
    let r = unsafe { &*(refcon as *const Refcon) };

    // Acknowledge sleep votes and announcements first, before anything that
    // could delay (channel send, logging). The kernel gives us ~30s before it
    // considers us non-responsive, but there's no reason to use any of that —
    // the supervisor's downstream work runs asynchronously on the daemon
    // event loop.
    if message_type == kIOMessageCanSystemSleep || message_type == kIOMessageSystemWillSleep {
        // SAFETY: `root_port` came from `IORegisterForSystemPower`;
        // `notification_id` is the value the system handed us as a
        // pointer-sized integer.
        unsafe {
            IOAllowPowerChange(r.root_port, message_argument as isize);
        }
    }

    let event = match message_type {
        kIOMessageSystemWillSleep => PowerEvent::SystemSleep,
        kIOMessageSystemHasPoweredOn => PowerEvent::SystemWake,
        // `kIOMessageCanSystemSleep` is the vote phase — we ack and stay
        // silent. Wake pre-announcement (`SystemWillPowerOn`) is
        // informational; we use `HasPoweredOn` as the actionable wake signal.
        _ => return,
    };

    let _ = r.tx.try_send(event).inspect_err(|e| {
        log::warn!("dropped power event {event:?}: {e}");
    });
}

/// Spawn the IOKit power observer thread. Returns once the kernel-side
/// registration handshake completes, so a caller seeing `Ok` can log
/// "power observer started" with confidence.
///
/// The returned `JoinHandle` is intentionally not joined at shutdown —
/// today's daemon relies on process exit to reap background threads, and the
/// CFRunLoop on this thread does not return under normal operation.
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

/// Observer thread body. Registers with IOKit, attaches the notification
/// source to this thread's CFRunLoop, signals the handshake, then parks in
/// `CFRunLoopRun`. The leaked `Refcon` owns the channel sender and is held
/// alive by the running thread.
fn run_observer_thread(
    tx: mpsc::Sender<PowerEvent>,
    handshake_tx: std_mpsc::SyncSender<io::Result<()>>,
) {
    // Leak the refcon: it must outlive every callback invocation, and this
    // thread is never torn down. Process exit reclaims the memory.
    let refcon: *mut Refcon = Box::leak(Box::new(Refcon { tx, root_port: 0 }));

    let mut port: IONotificationPortRef = ptr::null_mut();
    let mut notifier: io_object_t = 0;

    // SAFETY: All four out-params are valid stack pointers. The callback
    // type matches IOKit's `IOServiceInterestCallback`. `refcon` points into
    // the leaked Box that lives until process exit.
    let root_port = unsafe {
        IORegisterForSystemPower(
            refcon as *mut c_void,
            &mut port,
            power_callback,
            &mut notifier,
        )
    };

    if root_port == 0 {
        // MACH_PORT_NULL — registration failed. Caller logs and continues.
        let _ = handshake_tx.send(Err(io::Error::other(
            "IORegisterForSystemPower returned MACH_PORT_NULL",
        )));
        return;
    }

    // SAFETY: `refcon` points to the leaked Box created above. No callback
    // has fired yet (no run loop attached), so this write does not race.
    unsafe {
        (*refcon).root_port = root_port;
    }

    // Attach the notification source to THIS thread's run loop. IOKit
    // dispatch is not main-thread-bound — that's the whole point of
    // switching from NSWorkspace.
    //
    // SAFETY: `port` came from `IORegisterForSystemPower` above;
    // `CFRunLoopGetCurrent` returns the calling thread's run loop (creating
    // one lazily on first call); `kCFRunLoopDefaultMode` is a CoreFoundation
    // static valid for the life of the process.
    unsafe {
        let source = IONotificationPortGetRunLoopSource(port);
        if source.is_null() {
            let _ = handshake_tx.send(Err(io::Error::other(
                "IONotificationPortGetRunLoopSource returned NULL",
            )));
            return;
        }
        let rl = CFRunLoopGetCurrent();
        CFRunLoopAddSource(rl, source, kCFRunLoopDefaultMode);
    }

    // Registration handshake succeeded. The caller can now log "power
    // observer started" and start its event loop.
    let _ = handshake_tx.send(Ok(()));

    // Park here for the life of the process. Power callbacks dispatch on
    // this thread via the run-loop source attached above.
    //
    // SAFETY: `CFRunLoopRun` is safe to call — it runs the current thread's
    // run loop until something stops it. Under normal operation nothing does
    // and the call returns only at process exit.
    unsafe {
        CFRunLoopRun();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_power_observer_returns_a_handle() {
        // Intentionally leaks the spawned `power-observer` thread. The
        // thread parks in `CFRunLoopRun`, which does not return under normal
        // operation; process exit reaps it. Do NOT try to `.join()` the
        // returned handle — it will deadlock.
        let (tx, _rx) = tokio::sync::mpsc::channel::<PowerEvent>(8);
        let result = spawn_power_observer(tx);
        assert!(
            result.is_ok(),
            "spawn_power_observer should return Ok(handle): {result:?}"
        );
    }
}
