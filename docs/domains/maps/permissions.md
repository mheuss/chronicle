# Domain: permissions

**Status:** Decided 2026-09-16, revised 2026-09-21, revised 2026-11-21
**Owns:** Asking macOS what this process is allowed to record, and saying so in terms the rest of the daemon can act on.
**Code:** `chronicle-daemon/src/permissions.rs`

## Boundary

- inside: querying the TCC state for microphone access — `chronicle-daemon/src/permissions.rs` — `check_microphone`
- inside: the startup gate that refuses to run without the grants — `chronicle-daemon/src/permissions.rs` — `preflight`
- inside: the vocabulary the rest of the daemon uses for a grant state — `chronicle-daemon/src/permissions.rs` — `MicrophoneStatus`
- inside: a fresh microphone read in a child process, because a running process's own read goes stale — `chronicle-daemon/src/permissions.rs` — `fresh_microphone_status`
- inside: the result of that read, where a failed read is `Unavailable` and never authorized — `chronicle-daemon/src/permissions.rs` — `PermissionRead`
- inside: the `--permission-status` helper mode that prints one grant word — `chronicle-daemon/src/permissions.rs` — `run_status_command`
- outside: acting on a denial by stopping or degrading capture — `chronicle-daemon/src/capture_supervisor.rs` — `CaptureSupervisor`
- outside: deciding whether a microphone start goes ahead on a read — `chronicle-daemon/src/mic_gate.rs` — `apply_mic_intent`
- outside: asking macOS for microphone access, which only the UI does — `chronicle-ui/Sources/ChronicleUI/MicrophonePermission.swift` — `MicrophonePermission`
- outside: the states the UI is told about, which IPC defines rather than this domain — `chronicle-ui/Sources/ChronicleUI/DaemonConnection.swift` — `MicState`

**Placement test:** Does the file ask the operating system whether we are permitted to do something? Reacting to the answer belongs to whoever asked.
**Document comparison:** differs — no existing document draws this boundary. The use-case catalogue has no permissions entry. No ADR mentions it. So this is the first time the area has been written down.

## Owned Files

- chronicle-daemon/src/permissions.rs

## Depends On

- None

## Depended On By

- capture — `chronicle-daemon/src/capture_supervisor.rs` — `fresh_microphone_status`
- capture — `chronicle-daemon/src/mic_gate.rs` — `PermissionRead`
- pipeline — `chronicle-daemon/src/main.rs` — `preflight`
- pipeline — `chronicle-daemon/src/main.rs` — `run_status_command`
- UI — `chronicle-ui/Sources/ChronicleUI/MicrophonePermission.swift` — `MicrophonePermission`

## Seams

- SEAM-permission-wire — IPC — permissions owns the grant vocabulary and IPC owns what the UI is told; no grant state crosses the wire as itself, and the only trace of one is `permission_denied`, the reply to a microphone start the gate refused on a `PermissionRead` other than `Authorized`, after which Status reports `off` — owner: permissions — `chronicle-daemon/src/permissions.rs` — `PermissionRead`

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
