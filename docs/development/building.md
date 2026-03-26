# Building

## Prerequisites

- Rust toolchain (edition 2024)
- Xcode Command Line Tools (provides Swift runtime + macOS frameworks)
- macOS 14.0+ (for ScreenCaptureKit audio support)

No external services, Docker, databases, or env files needed. SQLite is bundled
via the `rusqlite` `bundled-full` feature.

## Build Commands

### Daemon (Rust)

```bash
cd chronicle-daemon
cargo build
```

### UI (Swift)

```bash
cd chronicle-ui
swift build
```

## Workspace Structure

The daemon is a Cargo workspace with these crates:

| Crate | Purpose |
|-------|---------|
| `chronicle-daemon` | Entry point, orchestration |
| `chronicle-capture` | Screen capture via ScreenCaptureKit |
| `chronicle-ocr` | Text extraction via Vision framework |
| `chronicle-audio` | Audio capture via AVFoundation |
| `chronicle-transcription` | Speech-to-text via whisper.cpp |
| `chronicle-storage` | SQLite + FTS5 storage engine |
| `chronicle-ipc` | JSON-over-Unix-socket IPC |

## Storage Location

Chronicle stores data at:

```
~/Library/Application Support/Chronicle/
├── chronicle.db           # SQLite (metadata + FTS5 index)
├── chronicle.sock         # Unix socket (daemon <-> UI IPC)
├── screenshots/YYYY/MM/DD/  # HEIF images
└── audio/YYYY/MM/DD/        # Audio segments
```
