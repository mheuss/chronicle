# Rewind — Project Description

> Historical design note from the Rewind-era concept phase.
> It does not describe the current Chronicle implementation.
> For current behavior, start with `README.md` and `docs/guides/`.

An open-source macOS app that continuously captures your screen and audio, then
lets you search across everything you've seen and heard. Inspired by the
original Rewind app (discontinued after Meta acquisition).

This is a personal tool built for daily use, open-sourced for other developers
who miss Rewind. Not commercial. Intended as a showcase project demonstrating
systems-level Rust, native macOS integration, and AI-assisted development.

---

## Architecture

Two-process design:

**rewind-daemon (Rust)** — Background process managed by `launchd`. Handles all
capture, processing, storage, and search. Starts at login, runs continuously.
Listens on a Unix domain socket for queries from the UI.

**rewind-ui (Swift/SwiftUI)** — Thin menu bar app. Connects to the daemon over
IPC. Sends search queries, displays results. v1 is a search popover; future
versions add timeline scrubbing and full-screen playback.

**Why two processes:** Crash isolation (UI crash doesn't stop capture), clean
separation of concerns, independent testability, and a natural fit for an
always-running background service. The architecture is easy to explain to
contributors.

**IPC:** JSON-over-Unix-domain-socket. Newline-delimited JSON request/response.
Simple to implement, simple to debug (`socat`, `nc`). gRPC is overkill for a
single local client.

---

## Components

### 1. Screen Capture Engine

Continuous screen capture using `ScreenCaptureKit` (macOS 12.3+).

- Captures all connected displays, not just the active one
- One `SCStream` per display, each delivering frames independently
- Adaptive capture rate:
  - Baseline: screenshot every ~2 seconds
  - Compares each frame to the previous via perceptual hash (pHash/dHash)
  - If the screen hasn't meaningfully changed, skip storing it
  - If no keyboard/mouse input for 30+ seconds, reduce to every 10 seconds
  - Resume normal rate on input
- Images stored as HEIF (native macOS compression, good quality-to-size ratio)
- Stored in date-organized directory structure

**Metadata captured per screenshot:**

| Field | Description |
|---|---|
| `id` | Unique identifier |
| `timestamp` | Millisecond precision |
| `display_id` | Which monitor |
| `app_name` | Foreground app on this display |
| `app_bundle_id` | Bundle identifier |
| `window_title` | Focused window title |
| `image_path` | Path to compressed screenshot |
| `ocr_text` | Extracted text (FTS5 indexed) |
| `phash` | Perceptual hash for deduplication |
| `resolution` | Display resolution and scale factor |

Principle: capture all available metadata upfront. Metadata is cheap to store
and gives flexibility as the product matures.

### 2. OCR Pipeline

On-device text recognition using Apple's `Vision` framework
(`VNRecognizeTextRequest`).

- Runs on each stored screenshot after capture
- Extracted text indexed in SQLite FTS5 for full-text search
- Runs asynchronously — capture is never blocked by OCR

### 3. Audio Capture

Two independent audio streams:

- **Microphone** — via `AVAudioEngine`. Captures the user's voice.
- **System audio** — via `ScreenCaptureKit` audio (macOS 13+). Captures audio
  from all running apps, including backgrounded meeting apps. This handles the
  "presenting in Zoom while focused on another app" scenario — teammates' audio
  is still captured.

Audio stored as compressed segments (AAC or Opus), chunked into 30-second or
1-minute blocks. Each segment tagged with source (mic vs. system).

**Metadata per audio segment:**

| Field | Description |
|---|---|
| `id` | Unique identifier |
| `start_timestamp` | Start of segment |
| `end_timestamp` | End of segment |
| `source` | `mic` or `system` |
| `audio_path` | Path to compressed audio file |
| `transcript` | Transcribed text (FTS5 indexed) |
| `whisper_model` | Model used for transcription |
| `language` | Detected language |

### 4. Transcription Pipeline

Local audio transcription using `whisper-rs` (whisper.cpp bindings).

- Runs asynchronously in a background queue
- Audio is captured first, transcription catches up during idle moments
- Capture is always prioritized over transcription for CPU/GPU resources
- Transcripts indexed in FTS5 alongside OCR text

### 5. Storage Engine

SQLite as the single source of truth. One database file holds all metadata and
search indexes. Media files (screenshots, audio) live on disk.

**Disk layout:**

```
~/Library/Application Support/Rewind/
├── rewind.db
├── screenshots/
│   └── YYYY/MM/DD/
│       └── {timestamp}_{display_id}.heif
└── audio/
    └── YYYY/MM/DD/
        └── {timestamp}_{source}.opus
```

**Search:**

- Unified full-text search across OCR text and audio transcripts
- FTS5 virtual tables for both screenshot text and audio transcripts
- Results ranked by relevance, ordered by timestamp
- Optional filter: screen-only or audio-only
- Each result includes: matched text snippet, timestamp, source type, app context

**Retention:**

- Configurable retention period (default: 30 days)
- Background cleanup job deletes expired data (database rows + files on disk)

### 6. IPC Server

Unix domain socket at `~/Library/Application Support/Rewind/rewind.sock`.

**Operations:**

| Command | Description |
|---|---|
| `search` | Full-text search with optional filters |
| `get_screenshot` | Fetch screenshot by ID |
| `get_audio` | Fetch audio segment by ID |
| `get_timeline` | Screenshot metadata for a time range + display |
| `status` | Daemon health, capture status, disk usage |
| `config` | Get/set configuration |
| `pause` / `resume` | Pause and resume capture |

### 7. UI — Menu Bar App (v1)

Swift/SwiftUI menu bar app.

**Menu bar icon:**
- Indicates capture status (active / paused)
- Click opens search popover

**Search popover:**
- Text input with debounced search-as-you-type
- Filter toggle: All / Screen / Audio
- Text-first results list: matched snippet, timestamp, app name, source icon
- Click screen result → shows full screenshot
- Click audio result → plays audio from that timestamp

**Settings (gear icon or right-click):**
- Capture status and disk usage
- Retention period setting
- Pause/resume toggle

### 8. Future Features (NOT in v1)

- Timeline scrubber overlay (full-screen playback mode)
- Global hotkey activation
- App exclusion list (don't capture certain apps)
- Quick-jump controls (jump to time periods, apps, meetings)
- Encryption at rest
- Visual thumbnail search results
- Pause hotkey

---

## Tech Stack

**Rust daemon:**
- `objc2` crate family — Apple framework bindings (ScreenCaptureKit, Vision, AVFoundation)
- `whisper-rs` — Local transcription via whisper.cpp
- `rusqlite` — SQLite with FTS5
- `core-graphics`, `core-foundation` — Lower-level macOS bindings

**Swift UI:**
- SwiftUI for the menu bar app and popover
- Communicates with daemon via Unix socket (Foundation networking)

**Build:**
- Rust: Cargo
- Swift: Xcode / Swift Package Manager
- Daemon installed as a `launchd` agent

---

## Key Decisions

These will be documented as ADRs in `docs/decisions/` for contributors.

| # | Decision | Rationale |
|---|---|---|
| 1 | Rust daemon + Swift UI (two processes) | Crash isolation, clean separation, natural fit for background service |
| 2 | Capture all displays, not just active | "Active" is ambiguous, context spans screens, frame diffing keeps cost low |
| 3 | Adaptive capture rate | Reduces storage 40-60% without losing meaningful changes |
| 4 | JSON-over-Unix-socket for IPC | Simple to implement and debug, gRPC is overkill for single local client |
| 5 | Capture all metadata upfront | Cheap to store, gives flexibility as product matures |
| 6 | Text-first search results | Faster to scan than thumbnails, click to expand for visual context |
| 7 | Unified search with optional filter | One search bar covers screen + audio, filter when you know what you want |
| 8 | SQLite + FTS5 for search | Single-file database, excellent full-text search, no external dependencies |
| 9 | Async transcription behind capture | Capture never drops, transcription catches up during idle |
| 10 | ~/Library/Application Support/ for storage | Follows macOS conventions, keeps home directory clean |
| 11 | HEIF for screenshot compression | Native macOS format, good quality-to-size ratio |
| 12 | Configurable retention (default 30 days) | Balances storage with utility, user decides |

---

## Out of Scope for v1

- Encryption at rest (planned for future)
- App exclusion / privacy filtering (metadata captured, filtering added later)
- Timeline scrubber / full-screen playback
- Global hotkey
- Cross-platform support
- Cloud sync
- AI-powered features (summarization, Q&A over history)

---

## Minimum Requirements

- macOS 13+ (for ScreenCaptureKit audio capture)
- Apple Silicon (for efficient whisper.cpp transcription)
- Screen Recording permission
- Microphone permission
