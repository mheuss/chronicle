# Domain: audio

**Status:** Decided 2026-09-16, edited 2026-09-18, revised 2026-09-21
**Owns:** Getting sound off the microphone and the system, turning it into Opus segments on disk, and holding the vocabulary the rest of the daemon uses to talk about it.
**Code:** `chronicle-daemon/crates/audio/src/`

## Boundary

- inside: the dedicated microphone capture path — `chronicle-daemon/crates/audio/src/microphone.rs` — `MicrophoneCapture`
- inside: the pipeline that owns both sources and the handler token — `chronicle-daemon/crates/audio/src/engine.rs` — `AudioPipeline`
- inside: enumerating and resolving the input device — `chronicle-daemon/crates/audio/src/device.rs` — `InputDevice`
- inside: where a finished segment is written — `chronicle-daemon/crates/audio/src/lib.rs` — `segment_path`
- inside: counting what the real-time callbacks dropped — `chronicle-daemon/crates/audio/src/drops.rs` — `AudioDropCounters`
- inside: absorbed from the `audio-encoding` candidate on 2026-09-16 — RFC 7845 Opus/Ogg encoding with pre-skip, segment accumulation with timestamp tracking, and sleep-gap detection — `chronicle-daemon/crates/audio/src/encoder.rs` — `OggOpusEncoder`
- inside: absorbed from the `audio-encoding` candidate on 2026-09-16 — deciding when a segment is complete and writing it — `chronicle-daemon/crates/audio/src/accumulator.rs` — `SegmentAccumulator`
- outside: reporting both pipelines' drop counters on a timer — `chronicle-daemon/src/drop_reporter.rs` — `run_reporter`
- outside: classifying whether a row's backing file is absent — `chronicle-daemon/src/media_presence.rs` — `media_is_absent`
- outside: deciding whether audio should be running at all — `chronicle-daemon/src/capture_supervisor.rs` — `CaptureSupervisor`

**Placement test:** Does the file move sound from a device into memory, turn those samples into a segment on disk, or name a state that only an audio source can be in? Scheduling when to capture, and reporting on it afterwards, belong elsewhere.
**Document comparison:** deliberately matches — `docs/use-cases/INDEX.md` assigns the whole `crates/audio/` directory to a single domain. After the `audio-encoding` merge on 2026-09-16 that is the shape this domain has. The index has the boundary right and the name wrong.

## Owned Files

- chronicle-daemon/crates/audio/src/accumulator.rs
- chronicle-daemon/crates/audio/src/device.rs
- chronicle-daemon/crates/audio/src/drops.rs
- chronicle-daemon/crates/audio/src/encoder.rs
- chronicle-daemon/crates/audio/src/engine.rs
- chronicle-daemon/crates/audio/src/handler.rs
- chronicle-daemon/crates/audio/src/lib.rs
- chronicle-daemon/crates/audio/src/microphone.rs

## Depends On

- None

## Depended On By

- capture — `chronicle-daemon/crates/capture/src/lib.rs` — `AudioHandlerToken`
- pipeline — `chronicle-daemon/src/pipeline.rs` — `CompletedSegment`
- IPC — `chronicle-daemon/src/ipc_handler.rs` — `MicToggleOutcome`

## Seams

- SEAM-capture-audio-lifecycle — capture — the supervisor reconciles both pipelines against one desired state and owns microphone enable and disable — owner: capture — `chronicle-daemon/src/capture_supervisor.rs` — `CaptureSupervisor`
- SEAM-opus-segment-file — transcription — this domain writes an Ogg/Opus file at 48 kHz and hand-builds its OpusHead; transcription hand-parses that header, reading pre-skip as a u16 LE at byte offset 10 in the 48 kHz domain, and neither crate imports the other in production; this domain decides what lands on disk — owner: audio — `chronicle-daemon/crates/audio/src/encoder.rs` — `OggOpusEncoder`

## Why transcription Is Not A Dependent

`transcription` is deliberately absent from `## Depended On By`. Its only
`chronicle_audio` use is `OggOpusEncoder`, at four sites — `lib.rs:912`, `:946`,
`:981`, `:1209` — all inside the `#[cfg(test)] mod tests` that opens at `:535`,
and `chronicle-audio` is declared in its `[dev-dependencies]`. It does not depend
on this domain in production, even though `OggOpusEncoder` is now owned here. The
two are joined by the file format rather than by an import — see
`SEAM-opus-segment-file` above.

## How The Existing Documents Saw This

| Document | Said | Verdict |
|---|---|---|
| `docs/use-cases/INDEX.md` | Assigns `chronicle-daemon/crates/audio/` — the whole crate — to `audio-encoding` | right boundary, wrong name — the crate is one domain, called `audio`. The column is superseded regardless; see CHR-151 |
| `docs/use-cases/audio-encoding.md` | The catalogue's only audio entry, documenting three patterns: RFC 7845 pre-skip encoding, segment accumulation with timestamp tracking, sleep-gap detection | accurate about `encoder.rs` and `accumulator.rs`, silent on the other six files. Its knowledge survives the merge; only the domain name changes |
| `docs/decisions/010-dedicated-avaudioengine-for-microphone.md` | "Capture the mic on its own `AVAudioEngine`, off the `SCStream` entirely" because SCK will not release the mic on a live config change | accurate — this is why `microphone.rs` is the largest file here |
| `docs/decisions/009-typed-lifecycle-for-capture-and-audio.md` | A borrow-based `AudioHandlerToken` ties the handler's lifetime to the pipeline | accurate — and it is why `capture` owns the seam above rather than this domain |
| `docs/decisions/013-record-on-the-audio-thread-analyse-offline.md` | "Real-time audio callbacks record; they never analyse" | accurate — `drops.rs` counts, and the reporting happens off-thread in a file this domain does not own |
| `docs/project-description.md` | Names audio capture among the components | accurate |

## Offshoots Filed

- none found
