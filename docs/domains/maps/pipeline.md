# Domain: pipeline

**Status:** Decided 2026-09-17, revised 2026-09-18, revised 2026-09-21
**Owns:** The daemon binary itself — the stage loops that move a captured item to storage, the contracts they run on, the counters they report through, and the process that wires them together.
**Code:** `chronicle-daemon/src/pipeline.rs`, `chronicle-daemon/src/pipeline/`, `chronicle-daemon/src/main.rs`, `chronicle-daemon/src/settings.rs`, `chronicle-daemon/src/drop_reporter.rs`

## Boundary

- inside: moving a captured frame to storage and enqueueing the work that follows it — `chronicle-daemon/src/pipeline.rs` — `capture_store_loop`
- inside: the contracts a producer enqueues through, including the typed channel-full outcome — `chronicle-daemon/src/pipeline/sinks.rs` — `OcrEnqueueResult`
- inside: crossing from a synchronous producer thread to the async runtime — `chronicle-daemon/src/pipeline.rs` — `bridge_audio_segments`
- inside: counting what each stage dropped — `chronicle-daemon/src/pipeline/counters.rs` — `PipelineCounters`
- inside: turning the real-time callbacks' drop counters into at most one log line per period, off the threads that count — `chronicle-daemon/src/drop_reporter.rs` — `run_reporter`
- inside: caching foreground-app lookups behind a clock so every frame does not pay for one — `chronicle-daemon/src/pipeline/metadata.rs` — `CachingAppMetadataProvider`
- inside: constructing every domain and ordering startup and shutdown between them — `chronicle-daemon/src/main.rs` — `main`
- inside: persisting the daemon's process-level settings, whichever domain each key belongs to — `chronicle-daemon/src/settings.rs` — `read_capture_paused`
- outside: producing the frames and segments these loops consume — `chronicle-daemon/src/capture_runtime.rs` — `CaptureRuntime`
- outside: the work a stage performs once scheduled — `chronicle-daemon/crates/ocr/src/lib.rs` — `extract_text`
- outside: where an item ends up — `chronicle-daemon/crates/storage/src/lib.rs` — `Storage`
- outside: deciding when recurring work is due — `chronicle-daemon/src/retention_task.rs` — `run_cleanup_loop`

**Placement test:** Does the file carry an item between two stages, define the contract a stage runs on, or start and stop the process? Doing a stage's actual work belongs to that stage's domain.
**Document comparison:** differs — `docs/use-cases/INDEX.md:7` gives this domain the Code Location `chronicle-daemon/src/`. That is the whole daemon source tree. It now spans seven domains. Six of the nine solutions in `docs/use-cases/pipeline.md` are located in files owned elsewhere.

## Owned Files

- chronicle-daemon/src/drop_reporter.rs
- chronicle-daemon/src/main.rs
- chronicle-daemon/src/pipeline.rs
- chronicle-daemon/src/pipeline/counters.rs
- chronicle-daemon/src/pipeline/metadata.rs
- chronicle-daemon/src/pipeline/sinks.rs
- chronicle-daemon/src/settings.rs

## Depends On

- storage — `chronicle-daemon/src/pipeline.rs` — `Storage`
- transcription — `chronicle-daemon/src/pipeline.rs` — `decode_opus_16k_mono`
- OCR — `chronicle-daemon/src/pipeline.rs` — `ocr_loop`
- capture — `chronicle-daemon/src/main.rs` — `CaptureSupervisor`
- audio — `chronicle-daemon/src/pipeline.rs` — `CompletedSegment`
- power — `chronicle-daemon/src/main.rs` — `spawn_power_observer`
- permissions — `chronicle-daemon/src/main.rs` — `preflight`
- background-work — `chronicle-daemon/src/main.rs` — `run_cleanup_loop`
- IPC — `chronicle-daemon/src/main.rs` — `IpcServer`

## Depended On By

- capture — `chronicle-daemon/src/capture_runtime.rs` — `OcrSink`
- capture — `chronicle-daemon/src/capture_supervisor.rs` — `AppMetadataProvider`
- IPC — `chronicle-daemon/src/ipc_handler.rs` — `PipelineCounters`

## Seams

- SEAM-stage-enqueue — capture — a producer holds an `Arc<dyn OcrSink>` and enqueues through a bounded channel whose full case is a typed result rather than a block or a panic; this domain decides the trait and the outcome vocabulary, the producer decides what it sends — owner: pipeline — `chronicle-daemon/src/pipeline/sinks.rs` — `OcrSink`
- SEAM-ocr-extraction — OCR — a blocking synchronous call taking a path and returning text, invoked from this domain's async loop through `spawn_blocking`; OCR decides the signature and the error vocabulary, this domain decides the scheduling — owner: OCR — `chronicle-daemon/src/pipeline.rs` — `ocr_loop`
- SEAM-power-event — power — power names the transition and this domain decides what it means: a two-variant `PowerEvent` delivered fire-and-forget, since `try_send` at `power.rs:141` logs at warn and discards its error, so an event is dropped rather than queued when the bounded channel at `main.rs:376` is full — owner: power — `chronicle-daemon/src/main.rs` — `power_rx`

## How The Existing Documents Saw This

| Document | Said | Verdict |
|---|---|---|
| `docs/use-cases/INDEX.md:7` | Code Location `chronicle-daemon/src/` | wrong — that path is the whole daemon source tree, which spans seven domains. Filed as CHR-151 |
| `docs/use-cases/pipeline.md:18, 89, 134` | Locates shutdown at `main.rs:main`, per-item log throttling at `pipeline.rs`, and channel bridging at `pipeline.rs:bridge_audio_segments` | accurate — these three are this domain |
| `docs/use-cases/pipeline.md:155, 187, 218, 241, 274, 298` | Locates six more solutions at `capture_runtime.rs`, `crates/capture/src/handler.rs` twice, `capture_supervisor.rs`, `power.rs`, and `provisioning.rs` | stale as a grouping — all six are owned by `capture`, `power` and `transcription`. The patterns are real; the catalogue grouped them by shape rather than by owner |
| `docs/use-cases/background-work.md:20` | Locates "Budgeted Start-Retry via Detached Nudges" at `main.rs:note_reconcile_outcome` plus `capture_supervisor.rs:StartRetry` | accurate, and evidence for CHR-58 — one behaviour split across this domain's file and `capture`'s |
| `docs/guides/chronicle-daemon.md:48-114` | Documents the Startup Sequence, Channel Topology and Shutdown as properties of the binary | accurate — and the closest existing description of why `main.rs` is one file |
| `docs/guides/chronicle-daemon.md:219` | Gives an "Adding a new pipeline stage" procedure | accurate |

`main.rs` holds two helpers that are other domains' logic rather than
composition. One is `note_reconcile_outcome` (`:231`), half of a
`background-work` solution. The other is `handle_provision_event` (`:155`) with
`begin_model_switch` (`:188`), the `transcription` model-switch path. Recorded
on CHR-58 rather than filed separately, since that ticket already names the file
as accumulating responsibilities. The map records who owns the file today, not who should.

## Offshoots Filed

- CHR-58 — `main.rs` carries two other domains' logic; labelled `needs-domain` with the measurement attached
