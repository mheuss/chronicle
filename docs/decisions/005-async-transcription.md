# ADR-005: Async Transcription Behind Capture

**Status:** Accepted
**Date:** 2026-03-20

## Context

Audio transcription via whisper.cpp is CPU/GPU intensive. Running it in
real-time alongside continuous screen capture and OCR could starve the capture
pipeline of resources.

## Decision

Transcription runs asynchronously in a background queue. Audio is always
captured and stored first. Transcription catches up during idle moments or
lower-activity periods.

If the machine is busy, transcription falls behind but capture never drops.

## Consequences

**Good:**
- Screen and audio capture are never interrupted or degraded by transcription
  load.
- Transcription can use larger/better Whisper models since it doesn't need to
  run in real-time.
- Naturally adapts to machine load — transcription speeds up when CPU is
  available.

**Bad:**
- Recently captured audio won't be searchable by text immediately — there's a
  delay while transcription catches up.
- Need to track transcription backlog and potentially surface it in the UI
  ("transcription is X minutes behind").
