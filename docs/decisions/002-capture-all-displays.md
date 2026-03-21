# ADR-002: Capture All Displays

**Status:** Accepted
**Date:** 2026-03-20

## Context

Most users have multiple monitors. We needed to decide whether to capture only
the "active" display or all connected displays.

## Decision

Capture all displays simultaneously. One `SCStream` per display via
ScreenCaptureKit.

## Consequences

**Good:**
- No need to define "active" — the mouse and focused window can be on different
  screens, making any heuristic wrong some of the time.
- Context often spans screens (docs on one, code on another). Capturing both
  means search finds everything.
- Frame diffing keeps the incremental storage cost low. A mostly-static second
  screen adds ~20-30% storage after deduplication, not a full 2x.

**Bad:**
- More frames to process (OCR, compression, storage).
- Slightly higher baseline CPU usage.
