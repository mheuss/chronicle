# Testing

## Running Tests

### Unit tests (no special setup)

```bash
cd chronicle-daemon
cargo test --workspace
```

Run a single crate:

```bash
cargo test -p chronicle-capture
cargo test -p chronicle-storage
```

### Integration tests

Capture integration tests are marked `#[ignore]` because they need a real macOS
display and Screen Recording permission.

```bash
cd chronicle-daemon
DYLD_LIBRARY_PATH="/Library/Developer/CommandLineTools/usr/lib/swift-5.5/macosx" \
  cargo test -p chronicle-capture --test integration -- --ignored
```

Storage integration tests use temp directories and don't need special
permissions:

```bash
cargo test -p chronicle-storage --test integration
```

## DYLD_LIBRARY_PATH

The `objc2-screen-capture-kit` bindings ultimately rely on Swift concurrency
libraries provided by the Xcode Command Line Tools. Test binaries that
instantiate `SCStream` or `SCShareableContent` need this path set or they crash
with SIGABRT.

The `swift-5.5` segment in the path may differ by system. Check what's at:

```text
/Library/Developer/CommandLineTools/usr/lib/
```

Unit tests that don't instantiate ScreenCaptureKit types at runtime don't need
this variable.

## macOS Permissions

Two permissions are required to run the daemon or its integration tests:

- **Screen Recording** — System Settings > Privacy & Security > Screen
  Recording. Without it, `SCShareableContent::get()` fails.
- **Microphone** — System Settings > Privacy & Security > Microphone. Required
  for audio capture via AVFoundation.

Grant these to Terminal (or whichever app runs the tests).

## swift-testing --filter

`swift test --filter <name>` matches the **Swift function name** as a regex, not the `@Test("...")` display title. The display title with spaces and quotes consistently matches zero tests:

```bash
# Wrong (matches 0):
swift test --filter "two concurrent requestStatus calls are serialized FIFO"

# Right:
swift test --filter concurrentRequestsAreSerialized
```

## Ignored Tier: Transcription E2E Needs a Provisioned Model

`engine_transcribes_real_speech` (chronicle-transcription, `--ignored` tier)
requires the whisper ggml model on disk — fetch via
`chronicle-daemon/scripts/fetch-whisper-model.sh`. Without it the test FAILS
LOUDLY by design rather than skipping: a silent skip is how a
never-exercised transcription path shipped in the first place (HEU-472).

## tokio Paused Clock: advance() vs sleep() for Negative Assertions

Under `#[tokio::test(start_paused = true)]`, `tokio::time::advance()` bumps
the clock BEFORE newly spawned tasks get their first poll — a sleeper
spawned by the code under test registers its timer against the
already-advanced clock, so an "assert nothing fired" check passes even when
something was wrongly spawned. For negative assertions use `sleep().await`
instead: going idle auto-advances to the EARLIEST registered deadline, so a
rogue sleeper fires first and `try_recv()` catches it. Also prefer
`try_recv` bracketing over `recv().await` — with a never-spawned sender,
`recv().await` hangs the test instead of failing it. (Found empirically in
HEU-284's final review: the broken shape passed with a deliberately armed
sleeper.)
