# QA Testing Guide — Audio Storage Integration (HEU-282)

**Date:** 2026-03-29
**Branch:** mrheuss/HEU-282-audio-storage-integration-wire-audio-pipeline
**Author:** Generated from implementation context

## Overview

The Chronicle daemon now records audio from the microphone and system output as 30-second Opus segments and stores them alongside screenshot data. Completed audio files are moved from a temporary staging area to permanent storage and tracked in the database. This runs continuously in the background until the daemon is stopped.

## Prerequisites

- **Environment:** Local macOS machine (macOS 14+)
- **Permissions:** The terminal app running the daemon must have **both** Screen Recording and Microphone permissions (System Settings > Privacy & Security). ScreenCaptureKit does not trigger the Microphone TCC prompt automatically — you must manually add your terminal app via the "+" button in System Settings > Privacy & Security > Microphone. Without Screen Recording the daemon fails at startup. Without Microphone there is no mic audio.
- **Build:** From the `chronicle-daemon/` directory, run `cargo build`. The workspace `.cargo/config.toml` sets the Swift rpath — without it the binary crashes at launch with a `dyld` error.
- **Clean state:** Remove any previous data before testing: `rm -rf ~/Library/Application\ Support/Chronicle/`
- **Tools:** A second terminal window, `sqlite3` (pre-installed on macOS)
- **Audio source:** Have something playing audio (a YouTube video, music) for system audio tests. Speak into the mic for microphone tests.

## Test Scenarios

### Happy Path

#### Scenario 1: Daemon starts both pipelines

1. In a terminal with Screen Recording + Microphone permissions, run:
   `cd chronicle-daemon && RUST_LOG=debug ./target/debug/chronicle-daemon`
2. Watch the startup log

**Expected result:** Log shows:
```
INFO  chronicle-daemon starting
INFO  Started capture on display ...
INFO  Capture engine started
INFO  Audio engine started
```
Both engines start without errors. If you see `database is locked` errors, a previous daemon is still running — kill it with `pkill chronicle-daemon` and try again.

#### Scenario 2: Audio segments are captured and stored

1. With the daemon running, play some audio and/or speak into the microphone
2. Wait at least 35 seconds (one full 30-second segment cycle plus encoding buffer)
3. Watch the log for audio segment entries

**Expected result:** Log shows lines like:
```
DEBUG Stored audio segment 1 (source=mic, ts=...)
DEBUG Stored audio segment 2 (source=system, ts=...)
```
Segment IDs increment. Both `mic` and `system` sources appear.

#### Scenario 3: Audio files exist on disk with correct dates

1. With the daemon still running, open a second terminal
2. Run: `find ~/Library/Application\ Support/Chronicle/audio -name "*.opus" | head -10`
3. Pick one file and confirm it's non-empty: `ls -la <path>`

**Expected result:** Opus files exist under `audio/YYYY/MM/DD/` directories with today's date. File names follow `<timestamp>_mic.opus` or `<timestamp>_system.opus`. Files are non-zero size.

#### Scenario 4: Staging directory is flat and empty after processing

1. Run: `ls ~/Library/Application\ Support/Chronicle/audio-staging/`

**Expected result:** Empty or contains only a file currently being written. No date subdirectories (`YYYY/MM/DD/`) — staging is flat. Completed segments are moved out promptly.

#### Scenario 5: Database records match files on disk

1. Run:
   ```
   sqlite3 ~/Library/Application\ Support/Chronicle/chronicle.db "SELECT id, source, start_timestamp, end_timestamp, audio_path FROM audio_segments ORDER BY id DESC LIMIT 5;"
   ```
2. Pick an `audio_path` from the results and verify the file exists: `ls -la <audio_path>`

**Expected result:** Each row has `source` of `mic` or `system`, timestamps ~30 seconds apart, and an `audio_path` pointing to an existing file.

#### Scenario 6: Transcript fields are NULL

1. Run:
   ```
   sqlite3 ~/Library/Application\ Support/Chronicle/chronicle.db \
     "SELECT id, transcript, whisper_model, language FROM audio_segments LIMIT 3;"
   ```

**Expected result:** `transcript`, `whisper_model`, and `language` are all NULL. (Transcription is HEU-240, not yet implemented.)

### Shutdown

#### Scenario 7: Clean shutdown drains remaining segments

1. With the daemon running and audio playing, press Ctrl+C
2. Watch the log output

**Expected result:** Log shows this sequence:
```
INFO  Shutdown signal received
INFO  Capture engine stopped
INFO  Audio engine stopped
INFO  Audio bridge thread exiting (sync channel closed)
INFO  Audio store loop exiting (channel closed)
INFO  chronicle-daemon stopped
```
No panics or errors. Clean exit.

#### Scenario 8: No orphaned files after shutdown

1. After shutdown, check staging: `ls ~/Library/Application\ Support/Chronicle/audio-staging/`
2. Count DB records: `sqlite3 ~/Library/Application\ Support/Chronicle/chronicle.db "SELECT count(*) FROM audio_segments;"`
3. Count files: `find ~/Library/Application\ Support/Chronicle/audio -name "*.opus" | wc -l`

**Expected result:** Staging is empty. DB record count matches file count on disk.

### Error Cases

#### Scenario 9: Daemon without Screen Recording permission

1. Revoke Screen Recording for the terminal app
2. Start the daemon: `RUST_LOG=debug ./target/debug/chronicle-daemon`

**Expected result:** Fails at startup with a clear error about screen capture content being unavailable. Does not crash with an unhandled panic.

### Edge Cases

#### Scenario 10: Long-running stability (5+ minutes)

1. Start the daemon and let it run for at least 5 minutes with audio playing
2. Monitor memory usage in Activity Monitor
3. Confirm segments continue appearing in the log at regular intervals

**Expected result:** Segments at ~30-second intervals. Stable memory (not growing unboundedly). No errors.

#### Scenario 11: Rapid Ctrl+C (immediate shutdown)

1. Start the daemon
2. Press Ctrl+C within 1-2 seconds, before any segments complete

**Expected result:** Clean shutdown, no panics, even with no segments produced.

## Known Issues

- **HEU-308:** ScreenCaptureKit does not trigger the macOS Microphone TCC prompt. The terminal app must be added manually to the Microphone privacy list. Without this, the audio engine's SCStream fails with a generic "Start stream failed" error.
- **HEU-309:** The audio engine creates a separate SCStream with a 2x2 pixel throwaway video context for audio-only capture. This should be consolidated into the existing screen capture streams. Blocker for shipping.

## Cleanup

- Remove test data: `rm -rf ~/Library/Application\ Support/Chronicle/`
- Re-grant any permissions you revoked during testing
