# Chronicle

This is wicked alpha - more a concept than a working product. I'm still playing with 
the ideas, and having fun with it. 

An open-source macOS app that continuously captures your screen and audio, then
lets you search across everything you've seen and heard. Inspired by the
original Rewind app.

## Architecture

Chronicle uses a two-process design:

- **chronicle-daemon** (Rust): Background service that handles screen capture,
  OCR, audio capture, transcription, storage, and search. Runs as a `launchd`
  agent and listens on a Unix domain socket.

- **chronicle-ui** (Swift/SwiftUI): Menu bar app that connects to the daemon
  over IPC. Sends search queries and displays results.

The two-process split gives us crash isolation (a UI crash doesn't stop
capture), clean separation of concerns and independent testability.

## Project Structure

```
chronicle-daemon/          Rust workspace
├── src/main.rs            Daemon entry point
└── crates/
    ├── capture/           Screen capture via ScreenCaptureKit
    ├── ocr/               Text extraction via Apple Vision
    ├── audio/             Mic + system audio capture
    ├── transcription/     Speech-to-text via whisper.cpp
    ├── storage/           SQLite + FTS5 search indexes
    └── ipc/               Unix socket JSON protocol

chronicle-ui/              Swift package
└── Sources/ChronicleUI/   SwiftUI menu bar app
```

## Requirements

- macOS 14+
- Apple Silicon (for efficient local transcription)
- Screen Recording permission
- Microphone permission

## Building

**Daemon:**

```sh
cd chronicle-daemon
cargo build
```

**UI:**

```sh
cd chronicle-ui
swift build
```

## Architectural Decisions

Design decisions are documented as ADRs in [`docs/decisions/`](docs/decisions/).

## License

MIT — see [LICENSE](LICENSE).
