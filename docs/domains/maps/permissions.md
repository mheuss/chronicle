# Domain: permissions

**Status:** Decided 2026-09-16, revised 2026-09-21
**Owns:** Asking macOS what this process is allowed to record, and saying so in terms the rest of the daemon can act on.
**Code:** `chronicle-daemon/src/permissions.rs`

## Boundary

- inside: querying the TCC state for microphone access — `chronicle-daemon/src/permissions.rs` — `check_microphone`
- inside: the startup gate that refuses to run without the grants — `chronicle-daemon/src/permissions.rs` — `preflight`
- inside: the vocabulary the rest of the daemon uses for a grant state — `chronicle-daemon/src/permissions.rs` — `MicrophoneStatus`
- outside: acting on a denial by stopping or degrading capture — `chronicle-daemon/src/capture_supervisor.rs` — `CaptureSupervisor`
- outside: deciding what a failed toggle tells the user, reading a grant state to choose — `chronicle-daemon/src/ipc_handler.rs` — `map_outcome`
- outside: the states the UI is told about, which IPC defines rather than this domain — `chronicle-ui/Sources/ChronicleUI/DaemonConnection.swift` — `MicState`

**Placement test:** Does the file ask the operating system whether we are permitted to do something? Reacting to the answer belongs to whoever asked.
**Document comparison:** differs — no existing document draws this boundary. The use-case catalogue has no permissions entry. No ADR mentions it. So this is the first time the area has been written down.

## Owned Files

- chronicle-daemon/src/permissions.rs

## Depends On

- None

## Depended On By

- capture — `chronicle-daemon/src/capture_supervisor.rs` — `check_microphone`
- IPC — `chronicle-daemon/src/ipc_handler.rs` — `MicrophoneStatus`
- pipeline — `chronicle-daemon/src/main.rs` — `preflight`

## Seams

- SEAM-permission-wire — IPC — permissions owns the grant vocabulary, IPC owns what the UI is told, and the contract is the mapping between them: `map_outcome` reads `MicrophoneStatus` only on a failed toggle, folding `Denied`, `Restricted` and `NotDetermined` into `PermissionDenied` and `Authorized` into `Error`, so no grant state crosses the wire as itself — owner: permissions — `chronicle-daemon/src/permissions.rs` — `MicrophoneStatus`

## How The Existing Documents Saw This

| Document | Said | Verdict |
|---|---|---|
| `docs/use-cases/` | Nothing — the catalogue has no permissions entry and `INDEX.md` assigns it no code location | stale by omission; the area predates the catalogue's coverage |
| `docs/decisions/` | Nothing — no ADR mentions permissions | accurate, in that no decision was ever needed here |
| `docs/guides/chronicle-daemon.md` | Names permissions among the daemon's startup concerns | accurate |
| `docs/guides/screen-capture.md` | Refers to permissions in relation to ScreenCaptureKit access | accurate |
| `docs/project-description.md` | Mentions permissions as a daemon startup requirement | accurate |

## Offshoots Filed

- CHR-152 — `StartupAlertState.swift` holds no permission state, so the candidate manifest's Swift pairing is wrong
