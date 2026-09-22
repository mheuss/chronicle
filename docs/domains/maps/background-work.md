# Domain: background-work

**Status:** Decided 2026-09-17, revised 2026-09-18
**Owns:** Deciding when recurring work runs, and whether it is due again after a restart.
**Code:** `chronicle-daemon/src/retention_task.rs`

## Boundary

- inside: deciding when recurring work is next due, across a restart — `chronicle-daemon/src/retention_task.rs` — `initial_deadline_ms`
- inside: the loop that drives a recurring job and stops it at shutdown — `chronicle-daemon/src/retention_task.rs` — `run_cleanup_loop`
- inside: the abstraction that keeps the scheduler ignorant of the work it schedules — `chronicle-daemon/src/retention_task.rs` — `CleanupOps`
- inside: recording that a run happened, so a restart resumes the schedule rather than restarting it — `chronicle-daemon/src/retention_task.rs` — `LAST_CLEANUP_KEY`
- outside: the deletion work itself, batching and the orphan sweep — `chronicle-daemon/crates/storage/src/retention.rs` — `run_cleanup_interruptible`
- outside: what a configured retention value means, and what an invalid one means — `chronicle-daemon/crates/ipc/src/retention.rs` — `Retention`
- outside: the other recurring worker the catalogue groups under this name — `chronicle-daemon/src/pipeline.rs` — `transcribe_loop`
- outside: wiring the scheduler to its one implementation at startup — `chronicle-daemon/src/main.rs` — `run_cleanup_loop`

**Placement test:** Does the file decide *when* work runs, how often, or whether it is due again after a restart? Doing the work belongs to whoever owns the work.
**Document comparison:** deliberately matches — `docs/use-cases/background-work.md:107` locates its surviving-restarts pattern at `retention_task.rs:run_cleanup_loop`, which is this boundary exactly. It departs from `docs/use-cases/INDEX.md:8` only in excluding `src/pipeline.rs`, which register row 11 reaches and this row does not.

## Owned Files

- chronicle-daemon/src/retention_task.rs

## Depends On

- storage — `chronicle-daemon/src/retention_task.rs` — `Storage`
- IPC — the scheduler names no wire type; this is `chronicle-ipc`'s re-export of `tokio_util`'s cancellation token, which the daemon has no other path to — `chronicle-daemon/src/retention_task.rs` — `CancellationToken`

## Depended On By

- pipeline — `chronicle-daemon/src/main.rs` — `run_cleanup_loop`

## Seams

- SEAM-cleanup-schedule-key — storage — the `last_cleanup_ms` row in storage's `config` table, written and read only by this domain, which decides the key's name and its units; storage owns the table the row sits in — owner: background-work — `chronicle-daemon/src/retention_task.rs` — `LAST_CLEANUP_KEY`

## How The Existing Documents Saw This

| Document | Said | Verdict |
|---|---|---|
| `docs/use-cases/background-work.md:93-107` | "Recurring Background Work That Survives Restarts", located at `retention_task.rs:run_cleanup_loop` | accurate, and the closest existing statement of this boundary |
| `docs/use-cases/background-work.md:20, 49` | Files two other solutions under the same name, located in `main.rs` + `capture_supervisor.rs` and in `pipeline.rs:transcribe_loop` | stale as a boundary claim — those files belong to `capture` and to row 11. The patterns are real; the grouping is by shape, not by ownership |
| `docs/use-cases/INDEX.md:8` | Code Location `chronicle-daemon/src/retention_task.rs`, `chronicle-daemon/src/pipeline.rs` | half right — the first path is this domain, the second is row 11's. The column is superseded regardless; see CHR-151 |
| `docs/guides/storage-engine.md:63, 71` | Lists `retention.rs` as an inner module of the storage crate, beside `screenshots`, `audio` and `search` | accurate — and it is why `crates/storage/src/retention.rs` is outside this domain |
| `docs/guides/storage-engine.md:139-140` | "Retention cleanup and orphan sweeping are two independent operations in `retention.rs`, with different callers. They are not two phases of one pass." | accurate — both callers are outside this domain |
| `docs/use-cases/storage.md:89-99` | Files "Batched Cleanup for SQLite Variable Limits" under `storage` | accurate |
| `docs/decisions/015-shared-value-types-live-in-the-ipc-crate.md` | `Retention` lives in `chronicle-ipc` and `chronicle-storage` depends on it; the backwards layering is deliberate | accurate — and it is why the type is outside this domain rather than owned with the schedule |
| `docs/guides/storage-engine.md:146` | The schedule resumes from `last_cleanup_ms` in the `config` table | accurate — recorded here as `SEAM-cleanup-schedule-key` |

The design's §3.1 manifest and `docs/use-cases/INDEX.md:8` disagree about
`src/pipeline.rs`: the index gives it to this domain, the manifest to `pipeline`.
Surfaced here because this is the first of the two rows to be walked. The
register already follows the manifest — `src/pipeline.rs` is reachable from
row 11's Roots and from no other row — and this sitting does not reopen it.

## Offshoots Filed

- none found
