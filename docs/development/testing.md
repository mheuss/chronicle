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

```
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
