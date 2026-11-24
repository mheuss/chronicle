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

- macOS 27+
- Apple Silicon (for efficient local transcription)
- Screen Recording permission
- Microphone permission
- Rust 1.98.0. rustup installs it from `rust-toolchain.toml` on the first build.
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
   login. A copy opened from anywhere else does not register itself. Its
   Settings toggle still reads on. Switch it off and back on to register
   that copy.
2. Grant Screen Recording. On first launch Chronicle opens a Set Up Chronicle
   window. Click Open System Settings, then turn on Chronicle under Privacy &
   Security → Screen & System Audio Recording. If Chronicle isn't in the list,
   click +, then choose Chronicle.app in Applications. If you press Open
   System Settings again, macOS may say Chronicle would like to record this
   computer's screen and audio. Click Open System Settings there, not Deny.
   Capture starts a few seconds after you turn Chronicle on, without a
   restart.
3. When capture starts, macOS asks whether "dev.heuss.chronicle.daemon" may
   bypass the private window picker. That is Chronicle's capture process.
   Click Allow. If macOS then offers to quit and reopen Chronicle, click
   Later.
4. The window then offers the microphone. Turn it on or skip it, then click
   Done. Done needs Screen Recording. If you close the window first, the menu
   bar popover shows "Finish setting up Chronicle…" to reopen it. Chronicle
   keeps running without Screen Recording. Search still works for anything
   already saved.
5. If Settings → Run at login shows "Needs approval in System Settings",
   click Open Login Items and enable Chronicle.

To turn it off, switch off "Capture in the background" under Settings → Run at
login. Do this before you delete the app. Moving it to the Trash does not
remove the login items. launchd keeps starting the daemon from the Trash
until you empty it. This holds after a restart. Removing Chronicle from Open at
Login in System Settings lasts only until you next open the app or bring it to
the front. While the toggle is on, that adds it back.

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

A dev daemon runs with `CHRONICLE_DATA_DIR` set. See Data location for why.
It takes its own instance lock, so it starts even while the agent runs. Both
would then capture. Switch the agent off first. To run the daemon by hand:

1. Switch off "Capture in the background" in Settings. That stops the agent
   and unregisters it.
2. Run the daemon with `cargo run` as under Running, with `CHRONICLE_DATA_DIR`
   exported. Stop it when done.
3. Switch the toggle back on. That registers and starts the agent again.

This restarts the agent without toggling it. A Screen Recording grant no
longer needs it. Capture starts on its own a few seconds after the grant.

```sh
launchctl kickstart -k gui/$(id -u)/dev.heuss.chronicle.daemon
```

## Running

A dev run needs `CHRONICLE_DATA_DIR` exported in both terminals. See Data
location below for why. Start the daemon in one terminal:

```sh
export CHRONICLE_DATA_DIR="$HOME/chronicle-dev"
cd chronicle-daemon
cargo run --release
```

Wait for the `IPC server listening` log line, then in a second terminal from
the repo root launch the UI:

```sh
export CHRONICLE_DATA_DIR="$HOME/chronicle-dev"
open dist/Chronicle.app
```

An icon should appear in the menu bar. Each state has its own shape:

- Green `record.circle.fill`: capture is running.
- Orange `pause.circle.fill`: capture is paused.
- Orange `power.circle`: background capture is off.
- Yellow `ellipsis.circle`: Chronicle is starting.
- Yellow `questionmark.circle`: Chronicle can't check Screen Recording right now.
- Red `lock.circle`: Chronicle needs Screen Recording.
- Red `xmark.circle`: Chronicle isn't running.
- Red `exclamationmark.circle`: the daemon is up but capture is not running.

Left-click the icon to open the search popover, or use `Cmd+,` for Settings.

### First run

Audio transcription needs a speech model, and the popover offers it the first
time you open it: "Audio transcription is off. Download the base model
(148 MB) to enable it." Click **Download**, wait for the progress bar, and
transcription is on. Settings → General has the same button and a picker if
you want a larger, more accurate model instead.

No Terminal step — the app downloads and verifies the model itself.

### Data location

Chronicle keeps its database, screenshots, audio, models, settings and IPC
socket in `~/Library/Group Containers/7TJEE2YF4G.chronicle`. This is the data
root. The socket path must fit in 103 bytes, so your home folder name can be at
most 41 bytes. Beyond that the daemon fails at every start and logs an error that
names the path.

For development, `CHRONICLE_DATA_DIR` moves that directory. It must be an
absolute path. Set it for both dev processes. A relative value stops the daemon
at startup, and the UI cannot connect. An app opened from Finder or at login
never has the variable. Neither does the login agent. Both use the group
container.

macOS refuses a daemon started with `cargo run` in the group container, so set
the variable for every dev run. Quit any running Chronicle before `open`,
because `open` passes the variable only to a new process.

```sh
export CHRONICLE_DATA_DIR="$HOME/chronicle-dev"
(cd chronicle-daemon && cargo run --release)
# In a second terminal, with the same export:
open dist/Chronicle.app
```

### Stopping

To stop the daemon cleanly, send `SIGTERM` (`Ctrl-C` in the terminal where it
runs). Pause state persists across restarts in `settings` under the data root. Data
location says where that is. If you paused capture before quitting, the daemon
will boot paused and the UI will show a banner offering to resume.

## License

MIT — see [LICENSE](LICENSE).
