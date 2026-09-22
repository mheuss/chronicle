# Domain: transcription

**Status:** Decided 2026-09-17, revised 2026-09-21
**Owns:** Turning a stored audio segment into text, and having a working model to do it with.
**Code:** `chronicle-daemon/crates/transcription/src/lib.rs` and `chronicle-daemon/src/provisioning.rs`

## Boundary

- inside: running whisper over decoded PCM — `chronicle-daemon/crates/transcription/src/lib.rs` — `TranscriptionEngine`
- inside: decoding a stored Ogg/Opus segment to the PCM whisper needs — `chronicle-daemon/crates/transcription/src/lib.rs` — `decode_opus_16k_mono`
- inside: which model variants exist and where they live on disk — `chronicle-daemon/crates/transcription/src/lib.rs` — `model_path`
- inside: acquiring and verifying a model file — `chronicle-daemon/src/provisioning.rs` — `download_model`
- inside: holding the loaded engine and swapping it — `chronicle-daemon/src/provisioning.rs` — `EngineHandle`
- inside: the state the rest of the system reads for "is transcription working" — `chronicle-daemon/src/provisioning.rs` — `TranscriptionStatusCell`
- outside: scheduling transcription work and deciding what gets transcribed next — `chronicle-daemon/src/pipeline.rs` — `transcribe_loop`
- outside: persisting the transcript and indexing it — `chronicle-daemon/crates/storage/src/lib.rs` — `update_transcript_full`
- outside: producing the segment file this domain decodes — `chronicle-daemon/crates/audio/src/encoder.rs` — `OggOpusEncoder`
- outside: the settings file holding the `whisper_model` key alongside two of capture's — `chronicle-daemon/src/settings.rs` — `read_whisper_model`
- outside: rendering provisioning state to the user — `chronicle-ui/Sources/ChronicleUI/TranscriptionAlertState.swift` — `TranscriptionAlertState`

**Placement test:** Does the file decide which model is loaded, or turn audio into text? Scheduling the work, storing the result, and showing it to the user each belong to whoever does that.
**Document comparison:** differs — `docs/use-cases/INDEX.md` gives this domain the Code Location `chronicle-daemon/crates/transcription/`. That stops at the crate. It leaves `src/provisioning.rs` in no domain at all.

## Owned Files

- chronicle-daemon/crates/transcription/src/lib.rs
- chronicle-daemon/src/provisioning.rs

## Depends On

- IPC — `chronicle-daemon/src/provisioning.rs` — `TranscriptionStats`

## Depended On By

- pipeline — `chronicle-daemon/src/pipeline.rs` — `decode_opus_16k_mono`
- pipeline — `chronicle-daemon/src/pipeline/sinks.rs` — `EngineHandle`
- pipeline — `chronicle-daemon/src/main.rs` — `ProvisionerContext`
- IPC — `chronicle-daemon/src/ipc_handler.rs` — `TranscriptionStatusCell`

## Seams

- SEAM-opus-segment-file — audio — `audio` writes an Ogg/Opus file at 48 kHz and hand-builds its OpusHead; this domain hand-parses that header, reading pre-skip as a u16 LE at byte offset 10 in the 48 kHz domain, and neither crate imports the other in production; `audio` decides what lands on disk — owner: audio — `chronicle-daemon/crates/transcription/src/lib.rs` — `decode_opus_16k_mono`
- SEAM-transcription-wire — IPC — this domain converts its own state into IPC's independently-declared `TranscriptionState`, `TranscriptionStats` and `ModelEntry`, and decides which provisioning states exist — owner: transcription — `chronicle-daemon/src/provisioning.rs` — `TranscriptionStatusCell`

## How The Existing Documents Saw This

| Document | Said | Verdict |
|---|---|---|
| `docs/use-cases/INDEX.md:11` | Code Location `chronicle-daemon/crates/transcription/` | wrong — half the domain is `src/provisioning.rs`, which the column puts in no domain. Filed as CHR-151 |
| `docs/use-cases/transcription.md` | Six solutions, every `**Location:**` inside the crate's `lib.rs` | accurate for what it covers, silent on provisioning, download and hot-swap |
| `docs/guides/chronicle-daemon.md:251` | "`provisioning.rs` loads and calls it, and `transcription-metal` is a default feature" | accurate — the one document that already reads the two roots as one thing |
| `docs/guides/chronicle-daemon.md:255` | `chronicle-transcription` takes `chronicle-audio` as a dev-dependency for `OggOpusEncoder` | accurate — confirmed in `crates/transcription/Cargo.toml` |
| `docs/decisions/012-whisper-model-provisioning-spike.md` | Decides `model_path`, the variant allow-list, the `base` default and the settings key; its status note says CHR-71 and CHR-47 discharged download, verify and hot-swap into `src/provisioning.rs` | accurate in substance, stale in citation — of four line references in the status note, `lib.rs:31` and `settings.rs:18` are exact, `lib.rs:125` for `model_path` is now `lib.rs:113`, and `lib.rs:56` for the `base` default is now `lib.rs:55` |
| `docs/use-cases/background-work.md:49` | Puts `transcribe_loop` in `background-work`, at `src/pipeline.rs` | consistent with this boundary — the worker is outside. `pipeline` owns `pipeline.rs`, decided as row 11 later the same day |
| `docs/project-description.md:110-117` | Component 4 "Transcription Pipeline", capture prioritized over transcription | accurate |
| `docs/decisions/014-daemon-initiated-ipc-nudges.md:43` | Groups `TranscriptionState` with `MicState` under one wire rule | accurate — evidence for `SEAM-transcription-wire` |
| `docs/audits/2026-04-06-system-health-audit.md:193`, `.claude/audit/performance.md:39`, `dependency-health.md:48, 51, 54`, `dead-code.md:108` | All describe the crate as a stub with an empty body, no implementation, and no runtime path, and suggest dropping `whisper-rs` | stale, uniformly — dated 2026-04-06, before the crate was written. `lib.rs` is 1287 lines as of 2026-09-21 and six files outside the crate reference `chronicle_transcription` |
| `.claude/audit/documentation-drift.md:20` | Cites `docs/decisions/005-async-transcription.md` | stale — the file does not exist. Same deletion CHR-36 records |

`src/provisioning.rs` carries nine `design §` citations and the crate's `lib.rs`
carries none, counted 2026-09-17. That is the worst single-file concentration
behind CHR-36. That ticket is already labelled `needs-domain` and recorded
against row 6.

## Offshoots Filed

- CHR-153 — the 48 kHz sample rate is declared at three production sites, two of them inside `audio` itself
