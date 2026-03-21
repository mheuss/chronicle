# ADR-003: Adaptive Screen Capture Rate

**Status:** Accepted
**Date:** 2026-03-20

## Context

Fixed-interval capture (e.g., every 2 seconds) wastes storage when the screen
is static. Most of the time — reading, thinking, idle — the screen doesn't
change meaningfully between captures.

## Decision

Adaptive capture rate based on screen changes and user activity:

- Baseline interval: ~2 seconds
- Compare each frame to the previous via perceptual hash
- If the screen hasn't meaningfully changed, skip storing it
- If no keyboard/mouse input for 30+ seconds, reduce to every 10 seconds
- Resume normal rate on input

## Consequences

**Good:**
- Estimated 40-60% storage reduction compared to fixed-interval capture.
- Lower CPU usage during idle periods.
- No loss of meaningful screen changes — captures happen when things actually
  change.

**Bad:**
- More complex than a simple timer. Requires perceptual hashing and input
  monitoring.
- The "meaningful change" threshold needs tuning — too aggressive skips real
  changes, too conservative saves everything.
