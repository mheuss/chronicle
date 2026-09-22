# Chronicle Domain Map

> **Authoritative.** The walk is complete and the closure checks pass.

**Written 2026-09-21**, from the fourteen-sitting walk recorded in `REGISTER.md`.

Given a source file, this says which domain owns it. Eleven domains own all 64
production source files — every Rust, Swift, SQL and plist file whose content
ships inside a binary Chronicle installs, enumerated in `INVENTORY.txt`. Each
is owned exactly once, so the Exclusions table below is empty.

Eleven domains, fourteen candidates: the walk opened with fourteen and three of
them merged into others, which the Candidate Outcomes table records.

**To place a file.** For a file that already exists, read the `## Owned Files`
section of the domain files — it is an exact list, and between them the eleven
cover all 64. For a *new* file, use the placement test at the top of each domain
file, which is a question you ask of the file to decide where it belongs.

**Why a boundary sits where it does** is in `DECISIONS.md`, alongside the
corrections made along the way and a list of what the walk left unfinished.

**A seam** is a contract between two domains: owning one means the owning domain
decides the contract's shape and the other endpoint follows. Not every dependency
is a seam. Domains depend on each other in more places than this table has rows —
`pipeline` alone names nine — and a seam is only where a contract had to be
agreed. For the full picture, read each domain file's `## Depends On` and
`## Depended On By`.

## Domains

| Domain | Owns | File |
|---|---|---|
| capture | Turning connected displays into encoded frames, and deciding when that should be happening. | maps/capture.md |
| permissions | Asking macOS what this process is allowed to record, and saying so in terms the rest of the daemon can act on. | maps/permissions.md |
| OCR | Turning an image file into text, and nothing about when that should happen. | maps/ocr.md |
| audio | Getting sound off the microphone and the system, turning it into Opus segments on disk, and holding the vocabulary the rest of the daemon uses to talk about it. | maps/audio.md |
| power | Hearing the operating system say the machine is going to sleep or has woken, and saying so in one word. | maps/power.md |
| transcription | Turning a stored audio segment into text, and having a working model to do it with. | maps/transcription.md |
| background-work | Deciding when recurring work runs, and whether it is due again after a restart. | maps/background-work.md |
| storage | Persisting captures and querying them back — the SQLite schema, the rows, the FTS5 indexes, and the media tree on disk. | maps/storage.md |
| pipeline | The daemon binary itself — the stage loops that move a captured item to storage, the contracts they run on, the counters they report through, and the process that wires them together. | maps/pipeline.md |
| IPC | The daemon–UI wire — the request and response vocabulary, the value types carried on it, the socket transport at both ends, the dispatch that turns a request into a response and gathers what it reports, and the rules that keep the two sides talking across a version gap. | maps/ipc.md |
| UI | Everything the person sees and interacts with — the menu bar app's scenes, its views, the state that decides when an alert fires, the copy that describes a wire value, and the bundle keys that make the process a menu bar agent. | maps/ui.md |

## Seams

| ID | Endpoint A | Endpoint B | Contract | Owner | Evidence |
|---|---|---|---|---|---|
| SEAM-capture-audio-lifecycle | capture | audio | One supervisor reconciles both pipelines against a single desired state, and owns microphone enable and disable. | capture | `CaptureSupervisor` |
| SEAM-cleanup-schedule-key | background-work | storage | A `last_cleanup_ms` row in storage's `config` table, written and read only by the scheduler, which decides the key's name and its units. storage owns the table the row sits in. | background-work | `LAST_CLEANUP_KEY`, `set_config` |
| SEAM-daemon-connection-api | IPC | UI | The view layer holds one `DaemonConnection`, calls its request methods and reads its published state, and never touches the socket. IPC decides the method surface and the error vocabulary. | IPC | `DaemonConnection` |
| SEAM-ocr-extraction | OCR | pipeline | A blocking synchronous call taking a path and returning text, invoked from pipeline's async loop through `spawn_blocking`. OCR decides the signature and the error vocabulary; pipeline decides the scheduling. | OCR | `extract_text`, `ocr_loop` |
| SEAM-opus-segment-file | audio | transcription | An Ogg/Opus file at 48 kHz whose OpusHead audio writes by hand and transcription parses by hand, reading pre-skip as a u16 LE in the 48 kHz domain at byte offset 10. Neither crate imports the other in production, so the file format is the whole contract, and audio decides what lands on disk. | audio | `OggOpusEncoder`, `decode_opus_16k_mono` |
| SEAM-permission-wire | permissions | IPC | permissions owns the grant vocabulary and IPC owns what the UI is told. The contract is the mapping between them, and it is lossy: `map_outcome` reads `MicrophoneStatus` only on a failed toggle, so no grant state crosses the wire as itself. | permissions | `MicrophoneStatus`, `MicState` |
| SEAM-power-event | power | pipeline | A two-variant `PowerEvent` over a bounded channel, delivered fire-and-forget — an event is dropped rather than queued when the channel is full. power names the transition; pipeline decides what it means. | power | `PowerEvent`, `power_rx` |
| SEAM-stage-enqueue | capture | pipeline | capture holds an `Arc<dyn OcrSink>` and enqueues through a bounded channel whose full case is a typed result rather than a block or a panic. pipeline decides the trait and the outcome vocabulary; capture decides what it sends. | pipeline | `OcrSink`, `OcrEnqueueResult` |
| SEAM-storage-status-wire | storage | IPC | `StorageStatus` and `StorageStats` are declared independently, neither importing the other, and the daemon converts between them at `ipc_handler.rs`. storage decides what a status can report. | storage | `StorageStatus`, `StorageStats` |
| SEAM-transcription-wire | transcription | IPC | transcription converts its own state into IPC's independently declared `TranscriptionState`, `TranscriptionStats` and `ModelEntry`, and decides which provisioning states exist. | transcription | `TranscriptionStatusCell`, `TranscriptionStats` |

## Candidate Outcomes

All fourteen candidates the walk opened with. No candidate was split and none was
dropped, so no row here records a cross-cutting concern.

| Candidate | Outcome | Where it ended up |
|---|---|---|
| capture | confirmed | maps/capture.md |
| permissions | confirmed | maps/permissions.md |
| OCR | confirmed | maps/ocr.md |
| audio | confirmed | maps/audio.md |
| audio-encoding | merged | into audio — its two files are the encoder and the segment accumulator, which are how audio produces what it owns rather than a separate concern |
| power | confirmed | maps/power.md |
| transcription | confirmed | maps/transcription.md |
| background-work | confirmed | maps/background-work.md |
| storage | confirmed | maps/storage.md |
| search | merged | into storage — `search.rs` queries the tables storage owns. The three Swift files its Roots named all went to UI; the wire types its Symbols named, `SearchHit`, `SearchHitSource` and `SearchResponse`, are declared in `DaemonConnection.swift`, which IPC owns |
| pipeline | confirmed | maps/pipeline.md |
| IPC | confirmed | maps/ipc.md |
| ipc-compat | merged | into IPC — after sitting 12 its Roots were down to one file, the ADR-015 shared value type, which is not a compatibility mechanism |
| UI | confirmed | maps/ui.md |

## Exclusions

Empty, and that is the result rather than an omission. All 64 paths in
`INVENTORY.txt` are owned by exactly one confirmed domain, so no path needs an
exclusion reason. Closure check 8 enforces that rather than taking it on trust.

The header row below stays with no rows under it. `check8_coverage.sh` parses this
table by discarding the first pipe-led line as a header, so removing it would make
the first exclusion anyone adds later invisible to the check.

| Path | Reason |
|---|---|
