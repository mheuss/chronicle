# ADR-001: Two-Process Architecture

> Historical naming note:
> this ADR captures the original Rewind-era naming and rationale.
> The current implementation uses `chronicle-daemon` and `chronicle-ui`.

**Status:** Accepted
**Date:** 2026-03-20

## Context

Rewind needs to run continuously in the background capturing screens and audio,
while also providing a UI for searching and browsing history. We needed to
decide whether this should be one process or two.

## Decision

Two separate processes: a Rust daemon (`rewind-daemon`) for all capture,
processing, and storage, and a Swift menu bar app (`rewind-ui`) for the
interface. They communicate over a Unix domain socket.

We considered three approaches:

1. **Rust daemon + Swift UI** (chosen) — two processes, clean separation
2. **Single Swift app with embedded Rust library** — one process, tighter
   coupling
3. **Tauri app** — Rust + web UI, avoids Swift entirely

## Consequences

**Good:**
- If the UI crashes, capture keeps running. You don't lose recordings.
- The daemon can be developed, tested, and debugged independently of the UI.
- The daemon starts via `launchd` at login — no dependency on the UI being open.
- Clear architectural story for contributors: Rust does systems work, Swift
  does UI.

**Bad:**
- Two things to build and deploy instead of one.
- IPC adds a communication layer to design and maintain.
- Install/setup is slightly more involved than a single app bundle.
