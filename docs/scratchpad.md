# Rewind — Project Scratchpad

A macOS app that continuously captures your screen and audio, lets you scroll
back through time, and search across everything you've seen and heard.

Inspired by the original Rewind app (now discontinued after Meta acquisition).

## Core Concept

- Continuous screen capture (periodic screenshots, not video)
- Audio capture (microphone and system audio)
- OCR on every screenshot for full-text search
- Audio transcription for full-text search
- Timeline UI to scrub back and forward through your day
- Search bar to find any text you've seen or heard

## Architecture

```
┌─────────────────────────────┐
│  Swift/SwiftUI UI layer     │  timeline overlay, search, settings
├─────────────────────────────┤
│  Rust core library (dylib)  │  capture, OCR, transcription,
│                             │  compression, storage, search
├─────────────────────────────┤
│  Apple frameworks via objc2 │  ScreenCaptureKit, Vision, AVFoundation
│  SQLite + FTS5              │  full-text search index
│  whisper-rs                 │  local audio transcription
└─────────────────────────────┘
```

**Rust backend** — handles all the heavy lifting: screen capture via Apple
frameworks (through objc2 bindings), OCR, audio transcription, frame
compression/deduplication, storage, and search indexing.

**Swift UI layer** — thin native layer for the timeline overlay and settings.
Calls into the Rust core via FFI.

## Multi-Monitor

Capture all screens, not just the active one. Reasons:

- Defining "active screen" is ambiguous (mouse vs. focused window can differ)
- Context often spans screens (docs on one, code on another)
- Frame diffing keeps the incremental storage cost low (~20-30% per additional
  screen after deduplication, not a full multiple)

ScreenCaptureKit enumerates all displays via `SCShareableContent`. One
`SCStream` per display, each delivering frames independently.

## Timeline UI

- Floating `NSPanel` overlay triggered by hotkey or menu bar icon
- Horizontal scrubber at the bottom to drag through time
- Search bar above the timeline
- Quick-jump controls for navigating to specific time periods
- Goes full-screen over the relevant display during playback
- Each screen's history shown on its own display

## Data Model (rough)

```
screenshot
├── timestamp (ms)
├── display_id
├── image_data (compressed, delta-encoded)
└── ocr_text (indexed in FTS5)

audio_segment
├── start_timestamp
├── end_timestamp
├── source (mic / system)
├── audio_data
└── transcript (indexed in FTS5)
```

## Storage

- Local only (encryption to be added later)
- HEIF compression for screenshots
- Delta encoding / frame diffing to deduplicate near-identical captures
- SQLite + FTS5 for the search index
- Retention policy TBD

## Key Apple Frameworks

- **ScreenCaptureKit** (macOS 12.3+) — screen capture
- **Vision** (`VNRecognizeTextRequest`) — on-device OCR
- **AVFoundation** — audio capture
- **ScreenCaptureKit audio** (macOS 13+) — system audio capture

## Key Rust Crates

- `objc2` family — Apple framework bindings
- `whisper-rs` — local transcription via whisper.cpp
- `rusqlite` — SQLite with FTS5
- `core-graphics`, `core-foundation` — lower-level macOS bindings

## Open Questions

- Capture interval — every 1 second? 2 seconds? Adaptive based on screen
  changes?
- Retention policy — how many days/weeks to keep?
- Storage budget target — GB per month?
- Hotkey for activating the timeline overlay
- Menu bar icon design / always-visible indicator
- How to handle full-screen apps and spaces
- How to handle sleep/wake and screen lock (don't capture lock screen)
- App filtering — ability to exclude certain apps from capture?
