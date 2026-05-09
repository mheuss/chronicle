# QA Testing Guide — Capture Pipeline Engine Hardening (HEU-407)

**Date:** 2026-04-20
**Branch:** mrheuss/heu-407-capture-pipeline-engine-hardening
**Commits on branch:** up through `3b3a0ee` (Commit 4)

## Overview

This branch hardens the capture pipeline in three ways:

1. **Structured engine lifecycle.** `CaptureEngine` now exposes typed states (Running / Stopping / Idle / Poisoned), collects teardown errors into a `PartialTeardown` variant, and lets the daemon exit with code 3 when teardown fails. Graceful shutdown now logs "Capture engine stopped cleanly" instead of just "stopped".
2. **Live observability.** The IPC `status` endpoint payload grows four nested objects (`capture`, `ocr`, `audio`, `storage`) carrying live counters and engine state. The daemon reads these via `ArcSwap` + atomic probes, with 1 Hz and 30 s background refreshers.
3. **Test seams.** `capture_store_loop`'s inner logic is split into an `insert_and_enqueue` helper that accepts trait objects (`ScreenshotSink`, `OcrSink`, `AppMetadataProvider`), so the pipeline can be unit-tested without SQLite or SCK. A TTL-cached metadata provider (250 ms) replaces per-frame `CGWindowListCopyWindowInfo` sweeps.

The Swift client's `StatusData` grows matching optional nested fields, so older daemon payloads continue to decode.

## Prerequisites

- **Environment:** macOS 14+ with Screen Recording and Microphone permissions granted to the terminal app (System Settings → Privacy & Security).
- **Tools:** `sqlite3`, `python3`, a second terminal window, `jq` (optional but handy).
- **ccache caveat (local only):** A release build currently fails on this machine because Homebrew's `ccache` is linked against a missing `libfmt.11.dylib` (Homebrew upgraded `fmt` to 12). Fix with `brew reinstall ccache`, or unlink it for the build: `brew unlink ccache && cargo build --release && brew link ccache`. Debug builds are not affected.
- **Clean slate (optional):** `rm -rf ~/Library/Application\ Support/Chronicle/` to start from a fresh DB and directory tree.

## Build

From the monorepo root:

```bash
cargo build --manifest-path chronicle-daemon/Cargo.toml
cd chronicle-ui && swift build
```

Both should succeed. If you want a release binary, see the ccache caveat above.

## Test Scenarios

### Happy Path

#### Scenario 1: Daemon starts cleanly

1. In a permitted terminal, run:
   ```bash
   cd chronicle-daemon && RUST_LOG=info cargo run
   ```
2. Watch the startup log.

**Expected result:** Log includes (roughly in this order):

```text
INFO  chronicle-daemon starting
INFO  IPC server started
INFO  Audio pipeline created
INFO  Capture engine started (audio on primary display)
```

No `partial teardown` or `entering Poisoned` lines. The daemon keeps running.

#### Scenario 2: IPC status returns the new nested shape

1. With the daemon running, in a second terminal:
   ```bash
   python3 - <<'PY'
   import json, socket, os
   sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
   sock.connect(os.path.expanduser(
       "~/Library/Application Support/Chronicle/chronicle.sock"
   ))
   sock.send(b'{"type":"status"}\n')
   buf = b""
   while not buf.endswith(b"\n"):
       buf += sock.recv(1024)
   print(json.dumps(json.loads(buf), indent=2))
   PY
   ```

**Expected result:** The JSON has this shape (values will differ):

```json
{
  "type": "status",
  "ok": true,
  "data": {
    "uptime_secs": 5,
    "version": "0.1.0",
    "capture": {
      "state": "running",
      "active_displays": 1,
      "frames_captured": 2,
      "frames_dropped": 0,
      "frames_processed": 2,
      "frames_failed": 0
    },
    "ocr": { "enqueued": 2, "dropped": 0 },
    "audio": { "segments_persisted": 0 },
    "storage": { "db_size_bytes": 24576 }
  }
}
```

Key checks:
- `capture.state == "running"`
- `capture.active_displays >= 1`
- `frames_captured` and `frames_processed` are small positive integers
- `frames_dropped == 0` and `frames_failed == 0`
- `storage.db_size_bytes > 0`

#### Scenario 3: Counters increment over time

1. Leave the daemon running for ~90 seconds with audio playing.
2. Call the status endpoint again (same Python snippet).

**Expected result:**

- `capture.frames_captured` and `capture.frames_processed` both grew, roughly in lockstep (default capture is 0.5 fps, so expect ~45 new frames in 90 s across all displays).
- `ocr.enqueued` grew by about the same number.
- `audio.segments_persisted` grew by 2 or 3 (one segment per 30 s).
- `storage.db_size_bytes` grew (refreshed every 30 s, so may lag by up to half a minute).
- `frames_dropped` and `frames_failed` stay at 0.

If `frames_dropped` is climbing, OCR is backpressured; investigate the OCR channel. If `frames_failed` is climbing, capture is failing post-SCK — check the log for `process_frame` errors.

#### Scenario 4: Graceful shutdown logs "cleanly"

1. With the daemon running, press Ctrl+C.
2. Watch the log.

**Expected result:**

```text
INFO  Shutdown signal received
INFO  Capture engine stopped cleanly
INFO  Capture engine stopped
INFO  Audio pipeline stopped
INFO  chronicle-daemon stopped
```

Then:

```bash
echo $?   # should print 0
```

The "stopped cleanly" line is the signal that no teardown errors surfaced. If you see `engine stop failed; entering Poisoned` instead, the engine poisoned itself on shutdown — skip to Scenario 7.

#### Scenario 5: Status after shutdown is unreachable

1. Immediately after shutdown, re-run the Python status snippet from Scenario 2.

**Expected result:** The script errors with `ConnectionRefusedError` or similar. The socket is gone because the daemon cleaned up. If it connects and returns data, a previous daemon is still running.

### Swift client

#### Scenario 6: Swift tests decode both old and new payloads

1. From the monorepo root:
   ```bash
   cd chronicle-ui && swift test
   ```

**Expected result:** 7 tests pass, including:
- `statusDataDecodesNestedStats` — the full nested payload decodes.
- `statusDataDecodesWithoutNestedStats` — the legacy two-field payload decodes with all nested fields `nil`.

Pre-existing deprecation warnings on `@Test` / `@Suite` are fine — they track a Swift-Testing package migration, not this branch.

### Poisoned-engine paths

These scenarios require a failure to trigger. If the engine never poisons in normal use (the happy case), you can skip them and rely on the unit tests at `chronicle-daemon/crates/capture/src/engine.rs:stop_success_transitions_to_idle` and friends to cover the state machine.

#### Scenario 7: Engine poison triggers exit code 3

The engine only enters `Poisoned` if stream teardown returns an error during `stop()`. This is rare in a healthy environment. To force it, you would need to kill SCK mid-shutdown or run under sandbox-induced TCC denial — both invasive. Accept the unit-test coverage unless you have a specific reproducer.

**If you reproduce it, expected behavior:**

- Log includes: `engine stop failed; entering Poisoned. survivors=[...] stop_errors=[...]`
- Log includes: `daemon exiting with code 3 (engine poisoned)`
- `echo $?` prints `3`.

#### Scenario 8: Partial startup failure surfaces on stderr and exits 3

Again, hard to force in a healthy env. The code path lives at `chronicle-daemon/src/main.rs:61-75` — it logs `capture startup rollback failed; exiting. survivors=... stop_errors=... original=...` and calls `process::exit(3)`.

### Database sanity (spot check)

#### Scenario 9: Screenshots and audio segments continue to land in SQLite

1. Let the daemon run for ~2 minutes.
2. In another terminal:
   ```bash
   sqlite3 ~/Library/Application\ Support/Chronicle/chronicle.db <<'SQL'
   SELECT COUNT(*) AS screenshots FROM screenshots;
   SELECT COUNT(*) AS audio_segments FROM audio_segments;
   SELECT MAX(timestamp) FROM screenshots;
   SQL
   ```

**Expected result:** Both counts are positive. The max screenshot timestamp is within a few seconds of now. Nothing on this branch changed the DB schema, so these should behave exactly like `main`.

## Regressions to watch for

These are things that should NOT happen on this branch. If any do, it's a regression introduced since `main`:

- `capture.state` stuck at `"unknown"` after 2+ seconds of uptime. The refresher publishes a snapshot every second; `"unknown"` past the first tick means the 1 Hz refresher loop is dead.
- `storage.db_size_bytes` stuck at 0 for more than 35 seconds. The 30 s storage refresher calls `storage.status()` and stores `db_size_bytes`; a zero past the first interval means the refresher isn't running.
- `frames_processed` growing but `ocr.enqueued` flat. OCR is being dropped — check `ocr.dropped` and the log for `ocr channel full` warnings.
- Shutdown log shows `engine stop failed` when you didn't force a failure.
- Swift UI fails to decode the status payload (`keyNotFound` or similar). The decoder strategy is `.convertFromSnakeCase` at `chronicle-ui/Sources/ChronicleUI/DaemonConnection.swift:47`; the nested fields are all optional at `:253-256`.
- Status endpoint takes noticeably longer than it did on `main`. The handler reads one `ArcSwap` and one atomic plus a snapshot of the counters — all lock-free. A slow response would indicate a regression somewhere else.

## Cleanup

```bash
rm -rf ~/Library/Application\ Support/Chronicle/
```

Re-grant any permissions you revoked during testing. If you unlinked ccache, re-link it: `brew link ccache`.
