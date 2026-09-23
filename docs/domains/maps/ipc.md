# Domain: IPC

**Status:** Decided 2026-09-18, revised 2026-09-21
**Owns:** The daemon–UI wire — the request and response vocabulary, the value types carried on it, the socket transport at both ends, the dispatch that turns a request into a response and gathers what it reports, and the rules that keep the two sides talking across a version gap.
**Code:** `chronicle-daemon/crates/ipc/src/lib.rs`, `chronicle-daemon/crates/ipc/src/retention.rs`, `chronicle-daemon/crates/ipc/src/server.rs`, `chronicle-daemon/src/ipc_handler.rs`, `chronicle-daemon/src/media_presence.rs`, `chronicle-ui/Sources/ChronicleUI/DaemonConnection.swift`, `chronicle-ui/Sources/ChronicleUI/ConnectionSession.swift`

## Boundary

- inside: declaring the request and response vocabulary both processes serialize against — `chronicle-daemon/crates/ipc/src/lib.rs` — `Request`
- inside: accepting connections on the Unix socket, framing newline-delimited JSON, and bounding a request line — `chronicle-daemon/crates/ipc/src/server.rs` — `IpcServer`
- inside: the trait a daemon-side handler implements to answer a request — `chronicle-daemon/crates/ipc/src/lib.rs` — `RequestHandler`
- inside: dispatching each request variant against the daemon's runtime and shaping the response — `chronicle-daemon/src/ipc_handler.rs` — `DaemonHandler`
- inside: converting a domain's own type into the wire shape at the daemon boundary — `chronicle-daemon/src/ipc_handler.rs` — `search_hit_from_storage`
- inside: deciding whether a media file behind a row is genuinely gone, read-only, to fill a counter on the status response — `chronicle-daemon/src/media_presence.rs` — `media_is_absent`
- inside: the client's socket lifecycle, reconnect loop, and per-connection reader — `chronicle-ui/Sources/ChronicleUI/ConnectionSession.swift` — `ConnectionSession`
- inside: the client's request methods and its independently declared restatement of the same vocabulary — `chronicle-ui/Sources/ChronicleUI/DaemonConnection.swift` — `DaemonConnection`
- inside: decoding an enum value this client does not recognise without failing the response — `chronicle-ui/Sources/ChronicleUI/DaemonConnection.swift` — `DaemonErrorCode`
- inside: absorbed from the `ipc-compat` candidate on 2026-09-18 — keeping the two sides able to talk across a version gap: additive fields optional on the decoder, unknown enum values degraded rather than thrown, and a tagged payload read only under the tag that carries it — `chronicle-ui/Sources/ChronicleUI/DaemonConnection.swift` — `Retention`
- inside: absorbed from the `ipc-compat` candidate on 2026-09-18 — what a configured retention value means, what an invalid one means, and the single bound every entry point validates against — `chronicle-daemon/crates/ipc/src/retention.rs` — `classify`
- outside: the work a response reports on, and the types each domain declares for its own use — `chronicle-daemon/crates/storage/src/models.rs` — `StorageStatus`
- outside: constructing the handler, opening the channels it sends on, and starting and stopping the server — `chronicle-daemon/src/main.rs` — `main`
- outside: the view layer that calls the client and renders what it returns — `chronicle-ui/Sources/ChronicleUI/MenuBarIcon.swift` — `MenuBarIcon`
- outside: what a wire value means to the person reading it on screen — `chronicle-ui/Sources/ChronicleUI/RetentionCopy.swift` — `RetentionCopy`

**Placement test:** Does the file declare the wire vocabulary, move bytes across the socket at either end, or turn a request into a response? Producing the data a response carries belongs to the domain that produces it.
**Document comparison:** differs — `docs/use-cases/INDEX.md:12` names no `IPC` domain at all and gives `ipc-compat` the whole of `chronicle-daemon/crates/ipc/` and `chronicle-ui/Sources/ChronicleUI/`. Row 13 merged here on 2026-09-18. So the first of those two trees is now this domain. The second still spans this domain and row 14.

## Owned Files

- chronicle-daemon/crates/ipc/src/lib.rs
- chronicle-daemon/crates/ipc/src/retention.rs
- chronicle-daemon/crates/ipc/src/server.rs
- chronicle-daemon/src/ipc_handler.rs
- chronicle-daemon/src/media_presence.rs
- chronicle-ui/Sources/ChronicleUI/ConnectionSession.swift
- chronicle-ui/Sources/ChronicleUI/DaemonConnection.swift

## Depends On

- storage — `chronicle-daemon/src/ipc_handler.rs` — `Storage`
- pipeline — `chronicle-daemon/src/ipc_handler.rs` — `PipelineCounters`
- transcription — `chronicle-daemon/src/ipc_handler.rs` — `TranscriptionStatusCell`
- capture — `chronicle-daemon/src/ipc_handler.rs` — `EngineState`
- audio — `chronicle-daemon/src/ipc_handler.rs` — `MicToggleOutcome`
- permissions — `chronicle-daemon/src/ipc_handler.rs` — `MicrophoneStatus`

## Depended On By

- pipeline — `chronicle-daemon/src/main.rs` — `IpcServer`
- capture — `chronicle-daemon/src/capture_supervisor.rs` — `map_outcome`
- storage — `chronicle-daemon/crates/storage/src/lib.rs` — `Retention`
- background-work — through this crate's re-export of `tokio_util`'s type at `crates/ipc/src/lib.rs:352`, not the wire — `chronicle-daemon/src/retention_task.rs` — `CancellationToken`
- transcription — `chronicle-daemon/src/provisioning.rs` — `TranscriptionStats`
- UI — `chronicle-ui/Sources/ChronicleUI/ChronicleApp.swift` — `DaemonConnection`

## Seams

- SEAM-permission-wire — permissions — permissions owns the grant vocabulary, IPC owns what the UI is told, and the contract is the mapping between them: `map_outcome` reads `MicrophoneStatus` only on a failed toggle, folding `Denied`, `Restricted` and `NotDetermined` into `PermissionDenied` and `Authorized` into `Error`, so no grant state crosses the wire as itself — owner: permissions — `chronicle-daemon/crates/ipc/src/lib.rs` — `MicState`
- SEAM-storage-status-wire — storage — `models.rs` declares `StorageStatus` and this domain declares `StorageStats` independently, neither importing the other, and this domain converts between them at `src/ipc_handler.rs`; storage decides what a status can report — owner: storage — `chronicle-daemon/crates/ipc/src/lib.rs` — `StorageStats`
- SEAM-transcription-wire — transcription — transcription converts its own state into this domain's independently declared `TranscriptionState`, `TranscriptionStats` and `ModelEntry`, and decides which provisioning states exist — owner: transcription — `chronicle-daemon/crates/ipc/src/lib.rs` — `TranscriptionStats`
- SEAM-daemon-connection-api — UI — the view layer holds one `DaemonConnection`, calls its request methods and reads its published state, and never touches the socket; this domain decides the method surface and the error vocabulary, UI decides what it renders — owner: IPC — `chronicle-ui/Sources/ChronicleUI/DaemonConnection.swift` — `DaemonConnection`

## How The Existing Documents Saw This

| Document | Said | Verdict |
|---|---|---|
| `docs/project-description.md:34` | "JSON-over-Unix-domain-socket. Newline-delimited JSON request/response." | accurate — still the protocol |
| `docs/project-description.md:152` | The socket is at `~/Library/Application Support/Rewind/rewind.sock` | wrong — both sides compose `Chronicle/chronicle.sock`, the daemon at `src/main.rs:348` and the UI at `DaemonConnection.swift:25-34`. Pre-existing Rewind-era drift already recorded in `.claude/audit/documentation-drift.md:52` |
| `docs/guides/chronicle-daemon.md:11, 249` | "The current IPC surface is status-only" | wrong — `Request` declares seven variants and `Response` eight. Already recorded in `docs/audits/2026-04-06-system-health-audit.md:90-95` |
| `docs/guides/chronicle-daemon.md:261-262` | The UI "does not depend on the daemon as a library — only on the IPC protocol" | accurate — and it is why this domain owns both ends |
| `docs/guides/chronicle-daemon.md:200-205` | `IpcServer` is the single-instance guard, socket-based rather than a global lock | accurate |
| `docs/use-cases/INDEX.md:12` | Gives `ipc-compat` the code locations `chronicle-daemon/crates/ipc/` and `chronicle-ui/Sources/ChronicleUI/` | wrong as a grouping — those two trees span this domain, row 13 and row 14. Already covered by CHR-151, which re-scoped that column as a whole |
| `docs/use-cases/pipeline.md:21-22` | Shutdown stops the IPC server first, awaiting socket-file cleanup | accurate — the ordering is `pipeline`'s, the `shutdown` it calls is this domain's |
| `docs/decisions/015-shared-value-types-live-in-the-ipc-crate.md:19, 73-74` | `chronicle-ipc` is a leaf crate and keeping it a leaf is load-bearing | accurate — its `Cargo.toml` names no workspace crate, and the crate's source names none. The arrow it describes is recorded here as storage depending on this domain |
| `docs/decisions/015-shared-value-types-live-in-the-ipc-crate.md:38, 68, 84` | A value type both a domain crate and the wire need lives in `chronicle-ipc`; `MAX_RETENTION_DAYS` moved here from `chronicle-storage` | accurate — and it is why the merged `ipc-compat` candidate's one remaining file sits in this crate rather than in `storage` |
| `docs/use-cases/ipc-compat.md` | Five solutions for daemon/UI version skew | accurate as patterns, and split between this domain and UI — three name `DaemonConnection.swift` and `ConnectionSession.swift`, which this domain owns; two name `TranscriptionAlertState.swift` and `TranscriptionBannerCopy.swift`, which UI owns. None names `crates/ipc/src/retention.rs`, the file the candidate had left |
| `docs/use-cases/ipc-compat.md:54` | Names CHR-77 as the formal policy for degrading unknown wire enum values | accurate — CHR-77 is Backlog, and the rule is applied ad hoc in five hand-written decoders in `DaemonConnection.swift` |
| `docs/decisions/014-daemon-initiated-ipc-nudges.md` | Events reach "a connection that sent `subscribe`" | stale as a description of the code — `grep -rni 'subscribe'` over `chronicle-daemon/src`, `chronicle-daemon/crates` and `chronicle-ui/Sources` returns nothing in production source. The client half exists in `ConnectionSession.swift`; the daemon half is CHR-93, Todo |
| `.claude/audit/api-contracts.md:3-8` | Scopes the IPC contract to `crates/ipc/src/lib.rs`, `server.rs`, `ipc_handler.rs` and `DaemonConnection.swift` | accurate — four of this domain's seven files, and the closest existing statement of this boundary |
| `.claude/audit/dry-violations.md:76` | `DaemonConnection.swift` mirrors the Rust wire types, and that duplication "is currently the wire contract itself" rather than a defect | accurate — this domain owns both sides of the mirror, which is what makes the duplication a contract rather than a copy |
| `.claude/audit/architecture-design.md:86` | `ipc` is a leaf crate with `chronicle-daemon` as the orchestration root | accurate of the crate, incomplete as a description of this domain — `ipc_handler.rs` sits in the root package and names five workspace crates |

`ipc_handler.rs` carries two reasons to change: a 227-line match on the seven
`Request` variants at `:311-537`, and 222 lines of daemon runtime wiring at
`:74-134` and `:136-296` that only `main.rs` constructs. That wiring is three
`mpsc` channels to `main()`'s event loop, two readiness atomics, three reply
timeouts. `DaemonConnection.swift` splits the same way at its own
`// MARK: - Protocol Types` divider on line 465. The class runs `:10-463`. The
23 wire types run `:467-801`. Both are recorded as offshoots. The map
records who owns the file today, not who should.

`media_presence.rs` appears in row 4's Roots. `audio` did not claim it. The
revision pass placed it here on 2026-09-18. Its only production caller is
`ipc_handler.rs:551`. It exists to fill the `media_absent` counter on the
status response. It reads the filesystem and changes nothing. So it crosses no
boundary that `storage` owns.

## Offshoots Filed

- CHR-56 — `ipc_handler.rs` mixes protocol dispatch with daemon runtime wiring; labelled `needs-domain` with the measurement added as a comment rather than a duplicate ticket
- CHR-154 — `DaemonConnection.swift` holds the transport class and the 23 wire types in one file
- CHR-155 — this crate's bare re-export of `tokio_util`'s `CancellationToken` at `crates/ipc/src/lib.rs:352` is the daemon's only path to the type, so the map shows a background-work dependency on IPC that is a build artifact rather than a protocol edge
