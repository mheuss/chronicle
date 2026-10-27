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
  over IPC. Left-click for a search popover over screen history and audio
  transcripts, with an All / Screen / Audio filter. Right-click or `Cmd+,` for
  a Settings window (disk usage, retention, pause / resume capture). Click a
  screen result to open the screenshot in its own window.

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
    ├── transcription/     Whisper model management + speech-to-text
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
- Rust 1.89+ toolchain (to build the daemon)

## Building

**Daemon:**

```sh
cd chronicle-daemon
cargo build --release
```

**App bundle:**

Build both the daemon and the menu bar app into one bundle from the repo root:

```sh
scripts/make-app.sh           # debug build (default)
scripts/make-app.sh release   # release build
```

The script writes `dist/Chronicle.app`. It holds the menu bar app, the daemon
at `Contents/MacOS/chronicle-daemon`, and the daemon's launch agent plist. Re-run
it after any code change. The bundle is ad-hoc signed until CHR-70 adds
Developer ID signing. Nothing starts the daemon at login until CHR-120.

The UI must run from a `.app` bundle, not a bare `swift run` binary. Modern
macOS Control Center won't register a menu bar icon for a loose executable.

## Running

Start the daemon in one terminal:

```sh
cd chronicle-daemon
cargo run --release
```

Wait for the `IPC server listening` log line, then in a second terminal from
the repo root launch the UI:

```sh
open dist/Chronicle.app
```

An icon should appear in the menu bar. A red `xmark.circle` means the UI cannot
reach the daemon. A yellow `ellipsis.circle` means it is connecting or waiting
for the first status. A green `record.circle.fill` means the daemon reports
capture running. Orange `pause.circle.fill` means capture is paused. Red
`exclamationmark.circle` means the daemon is up but capture is not running.
Left-click the icon to open the search popover, or use `Cmd+,` for Settings.

### First run

Audio transcription needs a speech model, and the popover offers it the first
time you open it: "Audio transcription is off. Download the base model
(148 MB) to enable it." Click **Download**, wait for the progress bar, and
transcription is on. Settings → General has the same button and a picker if
you want a larger, more accurate model instead.

No Terminal step — the app downloads and verifies the model itself.

### Stopping

To stop the daemon cleanly, send `SIGTERM` (`Ctrl-C` in the terminal where it
runs). Pause state persists in `~/Library/Application Support/Chronicle/settings`
across restarts — if you paused capture before quitting, the daemon will boot
paused and the UI will show a banner offering to resume.

## License

MIT — see [LICENSE](LICENSE).
