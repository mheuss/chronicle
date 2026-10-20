//! Re-feeds audio rows that were never transcribed through `transcribe_loop`.
//!
//! A row is pending while `Storage::pending_transcriptions` returns it: no
//! `whisper_model`, no transcript, and fewer than `MAX_TRANSCRIBE_ATTEMPTS`
//! attempts. That predicate is the checkpoint, so a restart resumes
//! with nothing extra stored.

use std::sync::Arc;
use std::time::Duration;

use chronicle_ipc::CancellationToken;
use chronicle_storage::Storage;
use tokio::sync::{mpsc, oneshot};

use crate::pipeline::sinks::{BackfillJob, BackfillOutcome};
use crate::provisioning::EngineHandle;

/// Attempts after which an unstamped row is given up on. Live and backfill jobs
/// both check it before running a row.
pub const MAX_TRANSCRIBE_ATTEMPTS: u32 = 3;
/// How often to look for a loaded model while none is loaded.
pub const IDLE_POLL: Duration = Duration::from_secs(30);
/// Wait between passes. A pass tries each row at most once, so this also
/// spaces retries.
pub const PASS_INTERVAL: Duration = Duration::from_secs(600);
/// Rows read per query.
const BATCH: u32 = 32;

/// How a pass ended.
enum PassEnd {
    /// Out of rows, stopped at an engine failure, or the query failed.
    Done,
    /// Cancelled, or the worker is gone.
    Stopped,
}

/// Send pending rows to `transcribe_loop` one at a time while a model is
/// loaded, until `cancel` fires.
pub async fn backfill_loop(
    engine: Arc<EngineHandle>,
    storage: Arc<Storage>,
    tx: mpsc::Sender<BackfillJob>,
    cancel: CancellationToken,
) {
    loop {
        // The pass decides the wait, so a model that loads between two checks
        // still gets `IDLE_POLL`, not a full `PASS_INTERVAL`.
        let wait = if engine.is_loaded() {
            match run_pass(&storage, &tx, &cancel).await {
                PassEnd::Stopped => break,
                PassEnd::Done => PASS_INTERVAL,
            }
        } else {
            IDLE_POLL
        };
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(wait) => {}
        }
    }
    log::info!("Backfill loop exiting");
}

/// One walk over the pending rows in id order. Each row is sent at most once.
async fn run_pass(
    storage: &Storage,
    tx: &mpsc::Sender<BackfillJob>,
    cancel: &CancellationToken,
) -> PassEnd {
    let mut cursor = 0;
    let mut sent = 0usize;
    let mut skipped = 0usize;
    let end = 'pass: loop {
        // Cancel-aware: the query waits on the connection pool. A dropped
        // query finishes on its blocking thread and changes nothing.
        let listed = tokio::select! {
            biased;
            _ = cancel.cancelled() => break 'pass PassEnd::Stopped,
            listed = storage.pending_transcriptions(cursor, MAX_TRANSCRIBE_ATTEMPTS, BATCH) => listed,
        };
        let rows = match listed {
            Ok(rows) => rows,
            Err(e) => {
                log::warn!("backfill could not list pending segments: {e}");
                break PassEnd::Done;
            }
        };
        if rows.is_empty() {
            break PassEnd::Done;
        }
        for row in rows {
            cursor = row.id;
            let (reply_tx, reply_rx) = oneshot::channel();
            let job = BackfillJob {
                row_id: row.id,
                audio_path: row.audio_path,
                reply: reply_tx,
                cancel: cancel.clone(),
            };
            let delivered = tokio::select! {
                biased;
                _ = cancel.cancelled() => false,
                result = tx.send(job) => result.is_ok(),
            };
            if !delivered {
                break 'pass PassEnd::Stopped;
            }
            sent += 1;
            let outcome = tokio::select! {
                biased;
                _ = cancel.cancelled() => None,
                reply = reply_rx => reply.ok(),
            };
            match outcome {
                None => break 'pass PassEnd::Stopped,
                Some(BackfillOutcome::EngineFailed) => {
                    log::warn!(
                        "backfill pass stopped at segment {} after an engine failure",
                        row.id
                    );
                    break 'pass PassEnd::Done;
                }
                Some(BackfillOutcome::Skipped) => skipped += 1,
                Some(_) => {}
            }
        }
    };
    if sent > 0 {
        log::info!("backfill pass sent {sent} segment(s), {skipped} skipped");
    }
    end
}
