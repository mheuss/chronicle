//! Lifecycle wrapper around CaptureEngine + its downstream consumer tasks.
//!
//! Without this wrapper, naively cycling `CaptureEngine::start()` between
//! pauses orphans `capture_store_loop` — the old engine's `frame_rx`
//! channel closes when the engine drops, the loop exits, and the next
//! engine's frames have no consumer. `CaptureRuntime` keeps the engine,
//! the store loop's JoinHandle, and the OCR loop's JoinHandle as one
//! atomic unit. Pause stops them together; resume constructs a fresh
//! set.
//!
//! Shutdown semantics:
//! - `stop().await`: explicit, deterministic. Stops the engine
//!   synchronously, then DROPS the engine (which closes its `_sender`
//!   field and lets the frame channel close), then awaits both downstream
//!   joins. The store loop exits when its receiver sees the closed
//!   channel; the OCR loop exits when the store loop drops its sender.
//!   Dropping the engine before awaiting is load-bearing: `engine.stop()`
//!   alone does NOT close the frame channel, because the engine keeps a
//!   `Sender` clone alive in `_sender` until the engine itself drops.
//! - `drop` without `stop`: aborts both JoinHandles via `JoinHandle::abort`
//!   for best-effort cleanup. The engine's own `Drop` impl tears down
//!   SCStreams. Final state is not guaranteed clean — prefer `stop().await`.

// T12 wires this into main.rs. Until then, the wrapper is unreferenced; allow
// dead code at module scope so clippy --all-targets -D warnings stays clean.
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;

use chronicle_capture::{CaptureConfig, CaptureEngine, CaptureError};
use chronicle_storage::Storage;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::pipeline;
use crate::pipeline::counters::PipelineCounters;
use crate::pipeline::sinks::{AppMetadataProvider, OcrSink};

/// Errors a `CaptureRuntime` can produce.
#[derive(Debug, thiserror::Error)]
pub enum CaptureRuntimeError {
    #[error("capture engine: {0}")]
    Engine(#[from] CaptureError),
}

pub type Result<T> = std::result::Result<T, CaptureRuntimeError>;

/// Owns the capture engine and the downstream tasks that consume its frames.
///
/// The engine and handles are wrapped in `Option` so `stop()` can `.take()`
/// them — `JoinHandle::await` consumes the handle by move, and dropping the
/// engine before awaiting is required to close its `_sender` field. Rust
/// forbids moving fields out of a `Drop` type, so `Option::take` is the
/// idiom. After a clean `stop()` the options are `None`, so the `Drop` impl
/// finds nothing to abort and the engine is already gone.
pub struct CaptureRuntime<'a, M: AppMetadataProvider + 'static + ?Sized> {
    engine: Option<CaptureEngine<'a>>,
    store_handle: Option<JoinHandle<()>>,
    ocr_handle: Option<JoinHandle<()>>,
    /// Anchors the type parameter without holding a value.
    _meta_marker: std::marker::PhantomData<Arc<M>>,
}

impl<'a, M: AppMetadataProvider + 'static + ?Sized> CaptureRuntime<'a, M> {
    /// Construct the engine, take its frame_rx, and spawn the store + OCR loops.
    pub fn start(
        config: CaptureConfig<'a>,
        storage: Arc<Storage>,
        meta: Arc<M>,
        counters: Arc<PipelineCounters>,
        ocr_channel_capacity: usize,
    ) -> Result<Self> {
        let (engine, frame_rx) = CaptureEngine::start(config)?;

        let (ocr_tx, ocr_rx) = mpsc::channel::<(i64, PathBuf)>(ocr_channel_capacity);
        let ocr_sink: Arc<dyn OcrSink> = Arc::new(pipeline::sinks::TokioOcrSink(ocr_tx));

        let store_storage = Arc::clone(&storage);
        let store_meta = Arc::clone(&meta);
        let store_counters = Arc::clone(&counters);
        let store_handle = tokio::spawn(pipeline::capture_store_loop(
            store_storage,
            frame_rx,
            ocr_sink,
            store_meta,
            store_counters,
        ));

        let ocr_storage = Arc::clone(&storage);
        let ocr_handle = tokio::spawn(pipeline::ocr_loop(ocr_storage, ocr_rx));

        Ok(Self {
            engine: Some(engine),
            store_handle: Some(store_handle),
            ocr_handle: Some(ocr_handle),
            _meta_marker: std::marker::PhantomData,
        })
    }

    /// Status probe — survives engine drop because it holds `Arc`s into
    /// the engine's atomic counters. The 1Hz refresher in main() captures
    /// this and continues to read it across pause/resume.
    pub fn status_probe(&self) -> chronicle_capture::EngineStatusProbe {
        // Always Some until stop() consumes self.
        self.engine
            .as_ref()
            .expect("engine present until stop() consumes self")
            .status_probe()
    }

    /// Stop the engine, then await the downstream task joins.
    ///
    /// Order matters in two ways:
    ///
    /// 1. `engine.stop()` synchronously tears down SCStreams but only
    ///    clears the engine's `streams`/`handlers`/`display_ids`. The
    ///    engine's `_sender` field (a clone of the frame channel sender)
    ///    is NOT dropped by `stop()`. The channel only closes when every
    ///    `Sender` clone — including `_sender` — is dropped.
    ///
    /// 2. We must therefore drop the engine BEFORE awaiting the store
    ///    handle. If we await first, `frame_rx.recv()` stays Pending
    ///    forever (the engine's `_sender` keeps the channel open) and
    ///    the store handle never resolves — deadlock.
    ///
    /// Once the engine is dropped and the channel closes,
    /// `capture_store_loop` sees `recv()` return None and exits. When
    /// the store loop returns, its `ocr_sink` (holding the OCR sender)
    /// drops, which signals `ocr_loop` to exit. We `await` both joins
    /// to confirm clean teardown.
    pub async fn stop(mut self) -> Result<()> {
        let store_handle = self.store_handle.take();
        let ocr_handle = self.ocr_handle.take();

        // Take the engine, stop it, then explicitly drop it. The drop is
        // what closes `_sender` and lets the frame channel close.
        let stop_result = {
            let mut engine = self.engine.take().expect("engine present on first stop");
            let r = engine.stop();
            drop(engine);
            r
        };

        if let Some(h) = store_handle
            && let Err(e) = h.await
        {
            log::error!("capture_store_loop join failed: {e}");
        }
        if let Some(h) = ocr_handle
            && let Err(e) = h.await
        {
            log::error!("ocr_loop join failed: {e}");
        }
        stop_result.map_err(CaptureRuntimeError::Engine)
    }
}

impl<M: AppMetadataProvider + 'static + ?Sized> Drop for CaptureRuntime<'_, M> {
    fn drop(&mut self) {
        // Best-effort: abort downstream tasks on drop. The engine's own
        // Drop tears down SCStreams. Callers should prefer stop().await
        // for clean shutdown; this Drop path exists only so a panic
        // mid-pause doesn't strand the tasks indefinitely.
        //
        // After a clean stop() the options are None and these abort()
        // calls are no-ops.
        if let Some(h) = self.store_handle.take() {
            h.abort();
        }
        if let Some(h) = self.ocr_handle.take() {
            h.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_runtime_error_renders_human_readable() {
        let e = CaptureRuntimeError::Engine(CaptureError::NoDisplays);
        assert!(e.to_string().contains("no displays"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stop_does_not_deadlock_when_owner_holds_sender_until_drop() {
        // This test simulates CaptureEngine's `_sender` lifecycle: a single
        // sender clone is held inside an "owner" struct and only drops when
        // the owner drops, NOT when an explicit `stop()` method runs on the
        // owner. CaptureRuntime::stop must drop its engine BEFORE awaiting
        // the store handle, or the receiver-side task hangs forever.
        use tokio::sync::mpsc;
        let (tx, mut rx) = mpsc::channel::<()>(8);

        struct Owner {
            _sender: mpsc::Sender<()>,
        }
        impl Owner {
            fn stop(&mut self) { /* clears streams etc but NOT _sender */
            }
        }
        let mut owner = Some(Owner { _sender: tx });

        let recv_task = tokio::spawn(async move {
            while rx.recv().await.is_some() {}
            // Returns when channel closes (= all senders dropped).
        });

        // Mirror CaptureRuntime::stop: pull owner out of Option, stop it,
        // drop it (which drops _sender), then await the receiver task.
        let mut owned = owner.take().unwrap();
        owned.stop();
        drop(owned);

        // Use a timeout to fail loudly instead of hanging the test runner.
        let res = tokio::time::timeout(std::time::Duration::from_secs(2), recv_task).await;
        assert!(res.is_ok(), "stop deadlocked or recv_task never completed");
    }
}
