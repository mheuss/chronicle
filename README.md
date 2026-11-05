# Chronicle

This is wicked alpha - more a concept than a working product. I'm still playing with 
the ideas, and having fun with it. 

An open-source macOS app that continuously captures your screen and audio, then
stores metadata locally for future search and playback features. Inspired by
the original Rewind app.

## Architecture

Chronicle uses a two-process design:

- **chronicle-daemon** (Rust): Background service that handles screen capture,
  OCR, audio capture, storage, and IPC. Listens on a Unix domain socket. It
  runs as a per-user login agent that the app registers. During development
  you can run it by hand. See Installing.

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
- A Developer ID Application certificate to build `Chronicle.app`. It needs a
  paid Apple Developer Program membership.

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
scripts/make-app.sh dist      # release build, then a notarized DMG
```

The script writes `dist/Chronicle.app`. It holds the menu bar app, the daemon
at `Contents/MacOS/chronicle-daemon`, and the daemon's launch agent plist. Re-run
it after any code change. Every mode signs the bundle with a Developer ID
certificate. `dist` also writes a notarized `dist/Chronicle-<version>.dmg`. See
[Signing and Notarization](docs/guides/signing-and-notarization.md) for the
one-time setup. Opening the app from an Applications folder registers the
daemon to start at login (see Installing).

The UI must run from a `.app` bundle, not a bare `swift run` binary. Modern
macOS Control Center won't register a menu bar icon for a loose executable.

## Installing

1. Copy `Chronicle.app` to `/Applications` or `~/Applications` and open it.
   macOS shows "App Background Activity: Chronicle can run in the
   background." The daemon and the menu bar app start now and at every
   login. A copy opened from anywhere else does not register itself. Use
   the Settings toggle there instead.
2. Grant Screen Recording: System Settings → Privacy & Security → Screen &
   System Audio Recording. Click +, choose Chronicle.app in Applications,
   and choose Later when macOS offers to quit it. Until you do, the daemon
   exits at startup. launchd retries it every 30 s.
3. When capture starts, macOS asks whether "dev.heuss.chronicle.daemon" may
   bypass the private window picker. That is Chronicle's capture process.
   Click Allow.
4. If Settings → Run at login shows "Needs approval in System Settings",
   click Open Login Items and enable Chronicle.

To turn it off, switch off "Capture in the background" under Settings → Run at
login. Moving the app to the Trash also removes both login items. Removing
Chronicle from Open at Login in System Settings lasts only until you next open
the app or bring it to the front. While the toggle is on, that adds it back.

System Settings still lists Chronicle under Background App Activity after you
turn it off. That entry is the permission, not a running process. Nothing
starts at login while the toggle is off.

Logs go to the unified log under the agent:

```sh
log show --predicate 'subsystem == "dev.heuss.chronicle"' --last 1h --info
```

Console shows the same entries with that subsystem filter. A terminal run
still logs to stderr. The unified log omits the microphone's name and UID.

### Developing with the agent installed

The agent holds the daemon's instance lock. A manual daemon exits straight
away while the agent runs. To run the daemon by hand:

1. Switch off "Capture in the background" in Settings. That stops the agent
   and unregisters it.
2. Run the daemon with `cargo run` as under Running. Stop it when done.
3. Switch the toggle back on. That registers and starts the agent again.

If the agent's own copy exited because a manual daemon held the lock, it stays
registered but stopped until the next login. Start it again without toggling:

```sh
launchctl kickstart -k gui/$(id -u)/dev.heuss.chronicle.daemon
```

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
