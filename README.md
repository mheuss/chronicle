# Chronicle

This is wicked alpha - more a concept than a working product. I'm still playing with 
the ideas, and having fun with it. 

An open-source macOS app that continuously captures your screen and audio, then
stores metadata locally for future search and playback features. Inspired by
the original Rewind app.

## Architecture

Chronicle uses a two-process design:

- **chronicle-daemon** (Rust): Background service that handles screen capture,
  OCR, audio capture, storage, and IPC. Listens on a Unix domain socket. Today
  it is started manually in development; persistent background install will
  land alongside `.app` bundle packaging.

- **chronicle-ui** (Swift/SwiftUI): Menu bar app that connects to the daemon
  over IPC. Left-click for a search popover (OCR-indexed screen history),
  right-click or `Cmd+,` for a Settings window (disk usage, retention, pause /
  resume capture). Click a search result to open the screenshot in its own
  window.

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
    ├── transcription/     Placeholder crate for future speech-to-text work
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
- Rust 1.88+ toolchain (to build the daemon)

## Building

**Daemon:**

```sh
cd chronicle-daemon
cargo build --release
```

**UI:**

The UI needs to run from a `.app` bundle, not a bare `swift run` binary —
modern macOS Control Center won't register a menu bar icon for a loose
executable. A helper script assembles a minimal bundle from the SPM build:

```sh
cd chronicle-ui
./scripts/make-app.sh           # debug build (default)
./scripts/make-app.sh release   # or release
```

The script writes to `.build/Chronicle.app`. Re-run it after any code change.
(Proper code-signed packaging and an autostart `SMAppService` are tracked
separately under HEU-420.)

## Running

Start the daemon in one terminal:

```sh
cd chronicle-daemon
cargo run --release
```

Wait for the `IPC server listening` log line, then launch the UI:

```sh
open chronicle-ui/.build/Chronicle.app
```

A green `record.circle` icon should appear in the menu bar. Left-click it to
open the search popover, or use `Cmd+,` for Settings.

To stop the daemon cleanly, send `SIGTERM` (`Ctrl-C` in the terminal where it
runs). Pause state persists in `~/Library/Application Support/Chronicle/settings`
across restarts — if you paused capture before quitting, the daemon will boot
paused and the UI will show a banner offering to resume.

## License

MIT — see [LICENSE](LICENSE).
