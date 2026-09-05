//! Transcription pipeline for Chronicle.
//!
//! This crate turns a persisted Ogg/Opus audio segment into text. It owns:
//!
//! - [`Transcriber`] — the trait the daemon depends on, so the pipeline can be
//!   tested against a fake without a model on disk.
//! - [`TranscriptionEngine`] — the whisper.cpp implementation (Metal by default;
//!   build with `--no-default-features --features cpu` for CPU-only — the daemon
//!   forwards this as its `transcription-cpu` feature). Loads a ggml model once
//!   and shares it.
//! - [`decode_opus_16k_mono`] — Opus → 16 kHz mono f32, the format whisper wants.
//! - The model-path convention and the [`ModelVariant`] allow-list, so the daemon's
//!   startup presence check and the loader agree on what is valid and where to look.
//!
//! The daemon drives all of this from `pipeline::transcribe_loop`; nothing here
//! spawns tasks or touches the database.

use std::fmt;
use std::path::{Path, PathBuf};

use ogg::reading::PacketReader;
use opus::{Channels, Decoder};
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState,
};

/// Subdirectory under the Chronicle base dir where ggml model files live.
pub const MODELS_SUBDIR: &str = "models";

/// Allow-list of whisper.cpp model variants the daemon understands.
pub const SUPPORTED_VARIANTS: &[&str] = &["base", "small", "medium"];

/// An allow-listed whisper.cpp model variant.
///
/// The field is private and [`parse_variant`] is the only public constructor,
/// so a `ModelVariant` can never hold anything outside [`SUPPORTED_VARIANTS`].
/// That is what makes [`model_path`] safe against path traversal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelVariant(&'static str);

impl ModelVariant {
    /// The variant's allow-listed name (`base`, `small`, or `medium`).
    pub fn as_str(&self) -> &'static str {
        self.0
    }
}

impl fmt::Display for ModelVariant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// Default variant used when settings is missing, empty, or invalid.
pub const DEFAULT_VARIANT: ModelVariant = ModelVariant("base");

/// Pinned download source for one model variant. SHA1s are upstream's
/// published digests. `size_bytes` is the advertised size, used for the disk
/// precheck before a file exists locally.
/// Keep in lockstep with scripts/fetch-whisper-model.sh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelInfo {
    pub variant: ModelVariant,
    pub url: &'static str,
    pub sha1: &'static str,
    pub size_bytes: u64,
}

static MANIFEST: [ModelInfo; 3] = [
    ModelInfo {
        variant: ModelVariant("base"),
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.bin",
        sha1: "465707469ff3a37a2b9b8d8f89f2f99de7299dac",
        size_bytes: 148_000_000,
    },
    ModelInfo {
        variant: ModelVariant("small"),
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.bin",
        sha1: "55356645c2b361a969dfd0ef2c5a50d530afd8d5",
        // Content-Length 487,601,967, rounded UP: the disk precheck reads
        // this, and rounding down would let a download ENOSPC.
        size_bytes: 488_000_000,
    },
    ModelInfo {
        variant: ModelVariant("medium"),
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-medium.bin",
        sha1: "fd9727b6e1217c2f614f9b698455c4ffd82463b4",
        // Upstream Content-Length 1,533,763,059, rounded UP (see small).
        size_bytes: 1_534_000_000,
    },
];

/// Pinned manifest entry for a variant. Total; a test pins manifest coverage.
pub fn model_info(variant: ModelVariant) -> &'static ModelInfo {
    MANIFEST
        .iter()
        .find(|m| m.variant == variant)
        .expect("manifest covers every allow-listed variant")
}

/// Parse a raw string into one of the supported variants. Exact match only;
/// this is the sole public constructor of [`ModelVariant`].
pub fn parse_variant(s: &str) -> Option<ModelVariant> {
    SUPPORTED_VARIANTS
        .iter()
        .copied()
        .find(|v| *v == s)
        .map(ModelVariant)
}

/// Path to the ggml model file for a given variant. Existence is not checked;
/// see [`model_present`].
pub fn model_path(base_dir: &Path, variant: ModelVariant) -> PathBuf {
    base_dir
        .join(MODELS_SUBDIR)
        .join(format!("ggml-{variant}.bin"))
}

/// Quick existence check used by daemon startup to decide whether to log
/// a "model missing, transcription idle" warning.
pub fn model_present(base_dir: &Path, variant: ModelVariant) -> bool {
    model_path(base_dir, variant).is_file()
}

/// Stream-SHA1 a file and compare against `expected`, ignoring hex case.
/// Returns `Err` only for I/O failures; a mismatch is `Ok(false)`.
///
/// **Blocking and CPU-bound. Call it under `tokio::task::spawn_blocking`.**
/// The `sha1` crate has no aarch64 hardware backend, so this is seconds of
/// un-yielding work on the 1.5 GB `medium` model. Called from an async path it
/// starves the 1 Hz IPC status poll that renders the `Verifying` state.
pub fn verify_sha1(path: &Path, expected: &str) -> std::io::Result<bool> {
    use sha1::{Digest, Sha1};
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha1::new();
    std::io::copy(&mut file, &mut hasher)?;
    let actual = format!("{:x}", hasher.finalize());
    Ok(actual.eq_ignore_ascii_case(expected))
}

/// Shortest PCM whisper will decode. Below this, `full()` fails before it
/// touches the state — an empty slice returns early, and 1..=40 samples fails
/// language auto-detect. Measured against a real `base` model: 40 errors, 41
/// succeeds.
const MIN_DECODABLE_SAMPLES: usize = 41;

/// Rejection message for an *empty* buffer. Matches whisper-rs's own
/// `WhisperError::NoSamples` wording, so this case reads to a caller exactly as
/// it did before HEU-664.
const EMPTY_MSG: &str = "Input sample buffer was empty.";

/// The short-PCM guard `transcribe` runs before touching the state slot, as a
/// function of length alone so it can be unit-tested without a model.
fn short_buffer_error(len: usize) -> Option<TranscriptionError> {
    if len == 0 {
        Some(TranscriptionError::Whisper(EMPTY_MSG.into()))
    } else if len < MIN_DECODABLE_SAMPLES {
        Some(TranscriptionError::Whisper(format!(
            "input sample buffer too short: got {len}, need at least {MIN_DECODABLE_SAMPLES} samples"
        )))
    } else {
        None
    }
}

/// A finished transcription.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transcript {
    pub text: String,
    pub language: Option<String>,
}

/// Errors from model loading, Opus decoding, or the whisper call.
#[derive(Debug, thiserror::Error)]
pub enum TranscriptionError {
    #[error("model load failed: {0}")]
    ModelLoad(String),
    #[error("opus decode failed: {0}")]
    Decode(String),
    #[error("whisper failed: {0}")]
    Whisper(String),
}

/// True when `s` is entirely one bracketed whisper marker: `[BLANK_AUDIO]`,
/// `[Motor]`, `[_BEG_]`. whisper.cpp emits these as ordinary segment text, and
/// without this check they land in `audio_fts` as searchable words.
///
/// A heuristic biased toward keeping text, not a definition of a marker. It is
/// structural rather than an allow-list because the marker vocabulary is not
/// stable across models. It knowingly errs both ways:
///
/// - **Misses** markers with internal whitespace or non-ASCII content, such as
///   `[ Inaudible ]` and `[報告 ]` from live rows. Widening the rule would put
///   real speech at risk: live row 1501 is `[ Background noise ]` wrapped
///   around ordinary conversation.
/// - **Over-matches** a bracketed ASCII token like `[sic]` when whisper puts it
///   in its own segment. Pinned as a known loss by
///   `concat_segment_text_drops_bracketed_token_split_into_own_segment`
///   (HEU-622).
///
/// Each condition rules out a different false positive. Non-empty: `[]` is
/// punctuation. ASCII: `set_language(None)` means any language can come back,
/// and a bracketed CJK or Thai phrase has no whitespace, so this is the only
/// check that keeps it. Whitespace-free: a bracketed aside inside a segment of
/// speech survives.
fn is_whisper_marker(s: &str) -> bool {
    let Some(inner) = s
        .trim()
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
    else {
        return false;
    };
    !inner.is_empty() && inner.is_ascii() && !inner.contains(char::is_whitespace)
}

/// Concatenate whisper sub-segment texts and trim, dropping whisper's own
/// markers, so a segment whisper filled with `[BLANK_AUDIO]` reads as empty.
/// An empty result means "no usable speech".
///
/// whisper-rs 0.14.4 exposes no per-segment `no_speech_prob`, so this filter
/// plus `suppress_blank` is the whole guard against music and noise (HEU-472).
pub fn concat_segment_text<'a>(segments: impl IntoIterator<Item = &'a str>) -> String {
    let mut out = String::new();
    for text in segments {
        if is_whisper_marker(text) {
            continue;
        }
        out.push_str(text);
    }
    out.trim().to_string()
}

const DECODE_RATE: usize = 16_000;
const ENCODE_RATE: usize = 48_000;
/// Max samples libopus can return for one packet at 16 kHz mono (120 ms).
const MAX_SAMPLES_PER_PACKET: usize = 1_920;

/// Decode an Ogg/Opus segment to 16 kHz mono f32 PCM.
///
/// libopus resamples and downmixes to the decoder's output format itself, so
/// there is no separate resampler and the OpusHead channel count is not
/// checked. Pre-skip is dropped per the OpusHead, and the final granule
/// position clamps the tail.
pub fn decode_opus_16k_mono(path: &Path) -> Result<Vec<f32>, TranscriptionError> {
    let file = std::fs::File::open(path)
        .map_err(|e| TranscriptionError::Decode(format!("open {}: {e}", path.display())))?;
    let mut reader = PacketReader::new(std::io::BufReader::new(file));

    // Packet 1: OpusHead. pre_skip is a u16 LE at byte offset 10 (48 kHz domain).
    let head = reader
        .read_packet()
        .map_err(|e| TranscriptionError::Decode(format!("read OpusHead: {e}")))?
        .ok_or_else(|| TranscriptionError::Decode("empty stream".into()))?;
    if head.data.len() < 12 || &head.data[0..8] != b"OpusHead" {
        return Err(TranscriptionError::Decode("missing OpusHead".into()));
    }
    let pre_skip = u16::from_le_bytes([head.data[10], head.data[11]]) as usize;

    // Packet 2: OpusTags — skip.
    reader
        .read_packet()
        .map_err(|e| TranscriptionError::Decode(format!("read OpusTags: {e}")))?
        .ok_or_else(|| TranscriptionError::Decode("missing OpusTags".into()))?;

    let mut decoder = Decoder::new(DECODE_RATE as u32, Channels::Mono)
        .map_err(|e| TranscriptionError::Decode(format!("decoder create: {e}")))?;

    let mut pcm: Vec<f32> = Vec::new();
    let mut buf = vec![0f32; MAX_SAMPLES_PER_PACKET];
    let mut last_granule: u64 = 0;
    while let Some(packet) = reader
        .read_packet()
        .map_err(|e| TranscriptionError::Decode(format!("read packet: {e}")))?
    {
        let n = decoder
            .decode_float(&packet.data, &mut buf, false)
            .map_err(|e| TranscriptionError::Decode(format!("decode: {e}")))?;
        // The opus 0.3.1 binding casts libopus's return straight to usize, so a
        // negative error code (e.g. OPUS_INVALID_PACKET on a corrupt packet)
        // becomes a huge value rather than an Err. Reject anything past the
        // buffer instead of panicking on the out-of-bounds slice below.
        if n > buf.len() {
            return Err(TranscriptionError::Decode(format!(
                "decode returned {n} samples (buffer holds {})",
                buf.len()
            )));
        }
        pcm.extend_from_slice(&buf[..n]);
        last_granule = packet.absgp_page();
    }

    // Drop encoder pre-skip (scale 48 kHz → 16 kHz).
    let skip16 = pre_skip * DECODE_RATE / ENCODE_RATE;
    if skip16 >= pcm.len() {
        pcm.clear();
    } else {
        pcm.drain(..skip16);
    }

    // Clamp to the granule-reported length. Our encoder writes the padded
    // final-frame length into the granule, so for our own segments this never
    // fires; it guards a future producer that writes a true end-trimmed value.
    // Divide, never multiply: `last_granule` is a raw u64 off the Ogg page and
    // a page on which no packet completes carries `u64::MAX`, so multiplying
    // first would overflow.
    let total16 = (last_granule.saturating_sub(pre_skip as u64)
        / (ENCODE_RATE / DECODE_RATE) as u64) as usize;
    if total16 > 0 && pcm.len() > total16 {
        pcm.truncate(total16);
    }

    Ok(pcm)
}

/// A type that can turn 16 kHz mono PCM into a [`Transcript`]. The trait makes
/// the transcribe loop testable with a fake.
///
/// # Contract
///
/// Implementations return **speech text only**, with engine-specific markers
/// already removed. The rule for what counts as a marker is engine-specific, so
/// the filtering cannot live in the caller.
///
/// An empty or whitespace-only result means **no usable speech was found**.
/// That is an answer, not a failure: `pipeline::transcribe_loop` records it as
/// an attempt with a NULL transcript (HEU-620). Return `Err` only when
/// transcription could not be performed.
///
/// `language` on an empty result need not be `None`. whisper reports a
/// detection even for silence, and `Storage::update_transcript_full` clears
/// `language` whenever the transcript collapses to nothing.
pub trait Transcriber: Send + Sync {
    fn transcribe(&self, pcm_16k_mono: &[f32]) -> Result<Transcript, TranscriptionError>;
    /// The resolved model variant, written to the transcript row.
    fn variant(&self) -> &str;
}

/// Holds one long-lived `T` behind a `Mutex`, created on first use.
///
/// Generic so lazy creation, invalidation and poison recovery are testable
/// with a fake `T` and no whisper model. Those paths are otherwise reachable
/// only from an ignored real-model test.
struct StateSlot<T> {
    slot: std::sync::Mutex<Option<T>>,
    /// How many values this slot has built. Stays at 1 for an engine's life
    /// unless a state is discarded and rebuilt.
    creations: std::sync::atomic::AtomicUsize,
}

impl<T> StateSlot<T> {
    fn new() -> Self {
        Self {
            slot: std::sync::Mutex::new(None),
            creations: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn creation_count(&self) -> usize {
        self.creations.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Run `use_state` against the held value, creating it first if absent.
    fn with<R, E>(
        &self,
        make: impl FnOnce() -> Result<T, E>,
        use_state: impl FnOnce(&mut T) -> Result<R, E>,
    ) -> Result<R, E> {
        let mut slot = match self.slot.lock() {
            Ok(slot) => slot,
            Err(poisoned) => {
                let mut slot = poisoned.into_inner();
                // LOAD-BEARING: empty the slot before the `is_none` check
                // below. A panic leaves the poisoned mutex holding the
                // corrupted state. Keep it and `make()` never runs, so
                // `use_state` gets the corrupted state on this very call.
                *slot = None;
                self.slot.clear_poison();
                log::warn!("discarding transcription state after a panic");
                slot
            }
        };

        if slot.is_none() {
            // Not logged here: the caller reports this failure with its cause,
            // so a second detail-free line would precede every one that
            // matters.
            *slot = Some(make()?);
            self.creations
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let state = slot.as_mut().expect("populated above");

        let result = use_state(state);
        if result.is_err() {
            // A state that errored is never reused. Upstream documents no
            // reuse guarantee after an error, so do not invent one.
            //
            // Emptied before the log so a panicking logger cannot poison the
            // mutex with the errored state still in it.
            *slot = None;
            // Whisper-specific wording on purpose: this string is the anchor
            // HEU-664's live verification greps for. The count is the check.
            // One build per engine, plus one per discard that a later call
            // rebuilds.
            log::warn!(
                "discarding transcription state after an error (built {} so far)",
                self.creation_count()
            );
        }
        result
    }
}

/// whisper.cpp engine. The `WhisperContext` (the loaded model) is created once
/// and shared via `Arc`. One `WhisperState` is created on first use and reused
/// for the life of the engine. Creating one allocates the whole GGML compute
/// backend, so doing it per call rebuilt Metal on every segment (HEU-664).
///
/// The slot holds its mutex across the whole decode, so concurrent
/// `transcribe` calls serialize. That costs nothing today: `transcribe_loop`
/// awaits each job before pulling the next. Per-worker states would bring
/// back the per-state backend allocation this exists to remove.
///
/// The trade is memory. The backend, KV caches and mel buffer stay resident
/// between segments and while the daemon is idle.
pub struct TranscriptionEngine {
    // Declared before `ctx` so it drops first, which is correct regardless of
    // how `WhisperState` holds its context. Do not reorder.
    state: StateSlot<WhisperState>,
    ctx: WhisperContext,
    variant: String,
}

impl TranscriptionEngine {
    /// Load the ggml model for `variant`, on Metal when that feature is on.
    /// On `Err` the caller logs and stays idle.
    pub fn load(base_dir: &Path, variant: ModelVariant) -> Result<Self, TranscriptionError> {
        let path = model_path(base_dir, variant);
        let path_str = path
            .to_str()
            .ok_or_else(|| TranscriptionError::ModelLoad("model path is not UTF-8".into()))?;

        let mut params = WhisperContextParameters::default();
        params.use_gpu(cfg!(feature = "metal"));

        let ctx = WhisperContext::new_with_params(path_str, params)
            .map_err(|e| TranscriptionError::ModelLoad(e.to_string()))?;

        Ok(Self {
            state: StateSlot::new(),
            ctx,
            variant: variant.as_str().to_string(),
        })
    }
}

impl Transcriber for TranscriptionEngine {
    fn transcribe(&self, pcm_16k_mono: &[f32]) -> Result<Transcript, TranscriptionError> {
        // Guard before the slot, not inside it. Whisper rejects a too-short
        // buffer before it does any GPU work, so letting that `Err` reach
        // `StateSlot::with` would discard a state that never ran, and the next
        // segment would pay a full backend rebuild for nothing.
        //
        // Nothing the daemon writes lands in that band today: the Opus encoder
        // pads to whole frames, so the shortest decodable segment is 216
        // samples. That floor belongs to another crate and nothing pins it.
        if let Some(e) = short_buffer_error(pcm_16k_mono.len()) {
            return Err(e);
        }

        self.state.with(
            || {
                self.ctx
                    .create_state()
                    .map_err(|e| TranscriptionError::Whisper(e.to_string()))
            },
            |state| {
                let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
                params.set_translate(false);
                // language = None means "auto-detect AND transcribe". (set_detect_language(true)
                // is detect-ONLY — it returns 0 segments and no text. HEU-472 T11.)
                params.set_language(None);
                params.set_suppress_blank(true);
                params.set_n_threads(4); // Metal does the work; keep CPU threads modest
                // Pinned, not inherited: the state is shared across segments,
                // so this stops one segment's prompt history priming the next.
                // It is already the whisper.cpp default, but whisper-rs
                // documents it as `false` (whisper_params.rs:119-121). That
                // doc is wrong.
                //
                // Segments are still not fully independent. The state's
                // decoder RNG is seeded once and never reset, so temperature
                // fallback in one segment can shift later ones. Accepted:
                // whisper.cpp exposes no reseed. Mechanism and line cites are
                // on HEU-664.
                params.set_no_context(true);

                state
                    .full(params, pcm_16k_mono)
                    .map_err(|e| TranscriptionError::Whisper(e.to_string()))?;

                let n = state
                    .full_n_segments()
                    .map_err(|e| TranscriptionError::Whisper(e.to_string()))?;
                let mut texts: Vec<String> = Vec::with_capacity(n as usize);
                for i in 0..n {
                    // Lossy decode: a multibyte codepoint split across a segment boundary
                    // degrades to U+FFFD rather than failing the whole transcript (this
                    // feature auto-detects language, so non-Latin scripts are expected).
                    texts.push(
                        state
                            .full_get_segment_text_lossy(i)
                            .map_err(|e| TranscriptionError::Whisper(e.to_string()))?,
                    );
                }

                let text = concat_segment_text(texts.iter().map(String::as_str));
                let language = state
                    .full_lang_id_from_state()
                    .ok()
                    .and_then(whisper_rs::get_lang_str)
                    .map(|s| s.to_string());

                Ok(Transcript { text, language })
            },
        )
    }

    fn variant(&self) -> &str {
        &self.variant
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::tempdir;

    /// Construct a `ModelVariant` for tests via the real allow-list parser.
    fn variant(name: &str) -> ModelVariant {
        parse_variant(name).expect("test variant must be allow-listed")
    }

    /// The guard short-circuits before whisper-rs, so nothing else can catch
    /// its message drifting from `WhisperError::NoSamples` on a patch bump.
    #[test]
    fn empty_msg_matches_whisper_rs() {
        assert_eq!(EMPTY_MSG, whisper_rs::WhisperError::NoSamples.to_string());
    }

    #[test]
    fn short_buffer_guard_splits_empty_from_too_short() {
        let msg = |len| match short_buffer_error(len) {
            Some(TranscriptionError::Whisper(m)) => Some(m),
            Some(other) => panic!("unexpected variant: {other:?}"),
            None => None,
        };
        assert_eq!(msg(0).as_deref(), Some("Input sample buffer was empty."));
        assert_eq!(
            msg(1).as_deref(),
            Some("input sample buffer too short: got 1, need at least 41 samples")
        );
        assert_eq!(
            msg(40).as_deref(),
            Some("input sample buffer too short: got 40, need at least 41 samples")
        );
        assert_eq!(msg(41), None);
    }

    #[test]
    fn state_slot_creates_once_and_reuses() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let creations = AtomicUsize::new(0);
        let slot: StateSlot<u32> = StateSlot::new();

        for _ in 0..5 {
            let out: Result<u32, ()> = slot.with(
                || {
                    creations.fetch_add(1, Ordering::Relaxed);
                    Ok(7)
                },
                |state| Ok(*state),
            );
            assert_eq!(out, Ok(7));
        }

        assert_eq!(
            creations.load(Ordering::Relaxed),
            1,
            "five calls must share one state"
        );
        assert_eq!(
            slot.creation_count(),
            1,
            "the slot's own counter must agree"
        );
    }

    #[test]
    fn state_slot_discards_a_state_that_errored() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let creations = AtomicUsize::new(0);
        let slot: StateSlot<u32> = StateSlot::new();
        let make = || {
            creations.fetch_add(1, Ordering::Relaxed);
            Ok(7)
        };

        let first: Result<u32, &str> = slot.with(make, |_| Err("boom"));
        assert_eq!(first, Err("boom"));

        let second: Result<u32, &str> = slot.with(make, |state| Ok(*state));
        assert_eq!(second, Ok(7));

        assert_eq!(
            creations.load(Ordering::Relaxed),
            2,
            "the errored state must be dropped, forcing a rebuild"
        );
        assert_eq!(
            slot.creation_count(),
            2,
            "the slot's own counter must agree"
        );
    }

    #[test]
    fn state_slot_recovers_from_a_poisoned_lock() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let creations = AtomicUsize::new(0);
        let slot: StateSlot<u32> = StateSlot::new();
        let make = || {
            creations.fetch_add(1, Ordering::Relaxed);
            Ok(7)
        };

        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _: Result<u32, ()> = slot.with(make, |_| panic!("decoder exploded"));
        }));
        assert!(poisoned.is_err(), "the closure must have panicked");

        let after: Result<u32, ()> = slot.with(make, |state| Ok(*state));
        assert_eq!(after, Ok(7), "a panic must not disable later calls");
        assert_eq!(creations.load(Ordering::Relaxed), 2);
        assert_eq!(
            slot.creation_count(),
            2,
            "the slot's own counter must agree"
        );
    }

    #[test]
    fn a_failed_rebuild_leaves_the_slot_empty() {
        let slot: StateSlot<u32> = StateSlot::new();

        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _: Result<u32, &str> = slot.with(|| Ok(7), |_| panic!("decoder exploded"));
        }));
        assert!(poisoned.is_err());

        // Rebuild fails. The suspect state must NOT survive into the next call.
        let failed: Result<u32, &str> = slot.with(|| Err("no gpu"), |state| Ok(*state));
        assert_eq!(failed, Err("no gpu"));

        // Next call gets a fresh state, never the one the panic left behind.
        let recovered: Result<u32, &str> = slot.with(|| Ok(99), |state| Ok(*state));
        assert_eq!(
            recovered,
            Ok(99),
            "a failed rebuild must leave the slot empty, not the old state"
        );
    }

    #[test]
    fn verify_sha1_accepts_matching_digest() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("f");
        std::fs::write(&p, b"hello world").unwrap();
        // Known vector: sha1("hello world")
        assert!(verify_sha1(&p, "2aae6c35c94fcfb415dbe95f408b9ce91ee846ed").unwrap());
    }

    #[test]
    fn verify_sha1_rejects_mismatch() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("f");
        std::fs::write(&p, b"hello world").unwrap();
        assert!(!verify_sha1(&p, "0000000000000000000000000000000000000000").unwrap());
    }

    #[test]
    fn verify_sha1_errors_on_missing_file() {
        assert!(verify_sha1(std::path::Path::new("/no/such/file"), "00").is_err());
    }

    #[test]
    fn verify_sha1_hashes_beyond_one_read_buffer() {
        // `io::copy` reads in 8 KiB chunks. 100 KB forces a second iteration,
        // which the 11-byte fixtures above never do.
        let dir = tempdir().unwrap();
        let p = dir.path().join("big");
        std::fs::write(&p, "a".repeat(100_000).as_bytes()).unwrap();
        assert!(verify_sha1(&p, "c4d4b30851182fc4eb8675494d42fd7f17e29c93").unwrap());
    }

    #[test]
    fn verify_sha1_compare_ignores_digest_case() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("f");
        std::fs::write(&p, b"hello world").unwrap();
        assert!(verify_sha1(&p, "2AAE6C35C94FCFB415DBE95F408B9CE91EE846ED").unwrap());
    }

    #[test]
    fn supported_variants_constants() {
        assert_eq!(SUPPORTED_VARIANTS, &["base", "small", "medium"]);
        assert_eq!(DEFAULT_VARIANT.as_str(), "base");
        assert_eq!(MODELS_SUBDIR, "models");
    }

    #[test]
    fn parse_variant_accepts_allow_list() {
        assert_eq!(parse_variant("base").unwrap().as_str(), "base");
        assert_eq!(parse_variant("small").unwrap().as_str(), "small");
        assert_eq!(parse_variant("medium").unwrap().as_str(), "medium");
    }

    #[test]
    fn parse_variant_rejects_empty_and_unknown() {
        assert_eq!(parse_variant(""), None);
        assert_eq!(parse_variant("BASE"), None, "case-sensitive");
        assert_eq!(parse_variant("tiny"), None);
        assert_eq!(parse_variant("base "), None, "trailing space");
    }

    #[test]
    fn parse_variant_rejects_path_traversal_inputs() {
        assert_eq!(parse_variant("../base"), None);
        assert_eq!(parse_variant("base/"), None);
        assert_eq!(parse_variant("/base"), None);
        assert_eq!(parse_variant("../../etc/passwd"), None);
    }

    #[test]
    fn model_path_joins_base_models_filename() {
        let base = Path::new("/tmp/chronicle");
        assert_eq!(
            model_path(base, variant("base")),
            Path::new("/tmp/chronicle/models/ggml-base.bin"),
        );
        assert_eq!(
            model_path(base, variant("medium")),
            Path::new("/tmp/chronicle/models/ggml-medium.bin"),
        );
    }

    #[test]
    fn model_present_false_when_file_missing() {
        let dir = tempdir().unwrap();
        assert!(!model_present(dir.path(), variant("base")));
    }

    #[test]
    fn model_present_true_when_file_exists() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(MODELS_SUBDIR)).unwrap();
        std::fs::write(
            dir.path().join(MODELS_SUBDIR).join("ggml-base.bin"),
            b"fake model bytes",
        )
        .unwrap();
        assert!(model_present(dir.path(), variant("base")));
        // Different variant at the same location is still missing.
        assert!(!model_present(dir.path(), variant("small")));
    }

    #[test]
    fn concat_segment_text_joins_and_trims() {
        let segs = ["  hello ", "world  "];
        assert_eq!(concat_segment_text(segs.iter().copied()), "hello world");
    }

    #[test]
    fn concat_segment_text_empty_for_no_segments() {
        let segs: [&str; 0] = [];
        assert_eq!(concat_segment_text(segs.iter().copied()), "");
    }

    #[test]
    fn concat_segment_text_empty_for_whitespace_only() {
        let segs = ["   ", "\n\t"];
        assert_eq!(concat_segment_text(segs.iter().copied()), "");
    }

    #[test]
    fn concat_segment_text_drops_blank_audio_marker() {
        let segs = ["[BLANK_AUDIO]"];
        assert_eq!(concat_segment_text(segs.iter().copied()), "");
    }

    #[test]
    fn concat_segment_text_drops_marker_but_keeps_speech() {
        let segs = ["hello ", "[BLANK_AUDIO]", " world"];
        assert_eq!(concat_segment_text(segs.iter().copied()), "hello  world");
    }

    #[test]
    fn concat_segment_text_drops_other_bracketed_markers() {
        // Both seen in live rows. The rule is structural, not an allow-list.
        let segs = ["[Motor]", "[_BEG_]"];
        assert_eq!(concat_segment_text(segs.iter().copied()), "");
    }

    #[test]
    fn concat_segment_text_keeps_brackets_inside_prose() {
        // Single segment: that is what makes this pass. The split-segment
        // test below is the case where it does not.
        let segs = ["the [sic] answer"];
        assert_eq!(
            concat_segment_text(segs.iter().copied()),
            "the [sic] answer"
        );
    }

    /// Pins a KNOWN LOSS, not a guarantee. If this ever returns
    /// `"the [sic] answer"`, that is an improvement: update the test and close
    /// HEU-622. But the expected value has a DOUBLE space, so a change to how
    /// the join handles whitespace also fails this test. Check which moved.
    #[test]
    fn concat_segment_text_drops_bracketed_token_split_into_own_segment() {
        let segs = ["the ", "[sic]", " answer"];
        assert_eq!(
            concat_segment_text(segs.iter().copied()),
            "the  answer",
            "a bracketed token in its own segment is dropped — known limitation, HEU-622"
        );
    }

    /// The other side of the same mechanism: a marker whisper splits out is
    /// removed while the speech beside it survives. Shape taken from live rows.
    #[test]
    fn concat_segment_text_drops_split_marker_but_keeps_neighbouring_speech() {
        let segs = ["[BLANK_AUDIO]", " >> and the second thing we tried."];
        assert_eq!(
            concat_segment_text(segs.iter().copied()),
            ">> and the second thing we tried."
        );
    }

    #[test]
    fn concat_segment_text_keeps_multiword_bracketed_text() {
        // Isolates the whitespace condition: only that check can spare this.
        let segs = ["[hello world]"];
        assert_eq!(concat_segment_text(segs.iter().copied()), "[hello world]");
    }

    #[test]
    fn concat_segment_text_keeps_bracketed_text_in_spaceless_scripts() {
        // Isolates the ASCII condition. CJK and Thai have no inter-word spaces,
        // so only that check can spare these.
        for s in ["[这是一个完整的句子]", "[こんにちは世界]", "[สวัสดีชาวโลก]"]
        {
            let segs = [s];
            assert_eq!(
                concat_segment_text(segs.iter().copied()),
                s,
                "spaceless-script speech must survive the marker filter"
            );
        }
    }

    #[test]
    fn concat_segment_text_keeps_unclosed_bracket() {
        // Isolates the closing-bracket condition: nothing else in the test set
        // fails without it.
        let segs = ["[unclosed"];
        assert_eq!(concat_segment_text(segs.iter().copied()), "[unclosed");
    }

    #[test]
    fn concat_segment_text_keeps_empty_brackets() {
        // Isolates the non-empty condition. `[]` carries no marker name, so it
        // is punctuation, not a whisper emission.
        let segs = ["[]"];
        assert_eq!(concat_segment_text(segs.iter().copied()), "[]");
    }

    #[test]
    fn concat_segment_text_keeps_trailing_bracket_alone() {
        // Isolates the opening-bracket condition. Without this, dropping
        // `strip_prefix('[')` changes nothing in the suite.
        let segs = ["sic]"];
        assert_eq!(concat_segment_text(segs.iter().copied()), "sic]");
    }

    #[test]
    fn concat_segment_text_drops_a_marker_carrying_a_leading_space() {
        // Pins the `trim()` inside `is_whisper_marker`. whisper emits a leading
        // space per segment, so the marker arrives as `" [BLANK_AUDIO]"`.
        // Delete that trim and this is the only assertion that fails.
        let segs = ["hello", " [BLANK_AUDIO]", " world"];
        assert_eq!(concat_segment_text(segs.iter().copied()), "hello world");
    }

    #[test]
    fn decode_round_trips_to_16k_mono() {
        use chronicle_audio::OggOpusEncoder;
        let dir = tempdir().unwrap();
        let path = dir.path().join("seg.opus");

        // 1 second of 48 kHz mono (a quiet 220 Hz tone).
        let samples: Vec<f32> = (0..48_000)
            .map(|i| (i as f32 * 220.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.2)
            .collect();
        OggOpusEncoder::new(1, 24_000, opus::Application::Voip)
            .encode_to_file(&samples, &path)
            .unwrap();

        let pcm = decode_opus_16k_mono(&path).unwrap();
        // ~16 000 samples (1 s @ 16 kHz), within one 20 ms frame of slack.
        assert!(
            (15_600..=16_400).contains(&pcm.len()),
            "expected ~16000 samples, got {}",
            pcm.len()
        );
    }

    #[test]
    fn decode_rejects_non_opus_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("garbage.opus");
        std::fs::write(&path, b"not an ogg opus file at all").unwrap();
        assert!(matches!(
            decode_opus_16k_mono(&path),
            Err(TranscriptionError::Decode(_))
        ));
    }

    #[test]
    fn decode_empty_audio_returns_empty_pcm() {
        use chronicle_audio::OggOpusEncoder;
        let dir = tempdir().unwrap();
        let path = dir.path().join("empty.opus");
        // Zero samples → a headers-only stream (OpusHead + OpusTags, no audio).
        OggOpusEncoder::new(1, 24_000, opus::Application::Voip)
            .encode_to_file(&[], &path)
            .unwrap();
        let pcm = decode_opus_16k_mono(&path).unwrap();
        assert!(
            pcm.is_empty(),
            "expected empty PCM, got {} samples",
            pcm.len()
        );
    }

    #[test]
    fn engine_load_fails_on_garbage_model() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(MODELS_SUBDIR)).unwrap();
        std::fs::write(
            dir.path().join(MODELS_SUBDIR).join("ggml-base.bin"),
            b"not a real ggml model",
        )
        .unwrap();
        let v = parse_variant("base").unwrap();
        assert!(matches!(
            TranscriptionEngine::load(dir.path(), v),
            Err(TranscriptionError::ModelLoad(_))
        ));
    }

    /// `say` the phrase, encode with the production encoder, decode back to
    /// 16 kHz mono PCM — the same path the pipeline takes. Test-only.
    #[track_caller]
    fn synthesize_test_phrase(phrase: &str) -> Vec<f32> {
        use chronicle_audio::OggOpusEncoder;

        // Generate speech with macOS `say` as 48 kHz mono f32 WAV.
        let dir = tempdir().unwrap();
        let wav = dir.path().join("tts.wav");
        let ok = std::process::Command::new("say")
            .args([
                "-o",
                wav.to_str().unwrap(),
                "--data-format=LEF32@48000",
                phrase,
            ])
            .status()
            .expect("failed to spawn `say` — is it on PATH?")
            .success();
        assert!(ok, "say failed");

        // Read the WAV's f32 samples (find the `data` chunk; rest is f32le).
        let bytes = std::fs::read(&wav).expect("failed to read `say` output");
        let data_pos = bytes
            .windows(4)
            .position(|w| w == b"data")
            .expect("no `data` chunk in `say` output")
            + 8; // skip the "data" tag + size
        let samples: Vec<f32> = bytes[data_pos..]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes(*c))
            .collect();

        // Encode to Ogg/Opus with the production encoder at the production
        // bitrate (AudioConfig::default().bitrate), then decode like the
        // pipeline does.
        let opus = dir.path().join("tts.opus");
        OggOpusEncoder::new(1, 64_000, opus::Application::Voip)
            .encode_to_file(&samples, &opus)
            .unwrap();
        let pcm = decode_opus_16k_mono(&opus).unwrap();

        // If `say` or the WAV parse yielded no audio, an empty transcript would
        // be blamed on the detect_language bug instead of the fixture.
        assert!(
            pcm.len() > DECODE_RATE,
            "TTS produced under 1s of audio ({} samples) — fix the fixture, not the engine",
            pcm.len()
        );
        pcm
    }

    /// Chronicle *base* dir — the one holding `models/` — overridable for a
    /// non-default data dir. Shared by the tests that need a real model, so
    /// they cannot drift apart on where they look.
    #[track_caller]
    fn test_base_dir(variant: ModelVariant) -> PathBuf {
        let base = std::env::var("CHRONICLE_TEST_BASE_DIR").unwrap_or_else(|_| {
            format!(
                "{}/Library/Application Support/Chronicle",
                std::env::var("HOME").expect("HOME environment variable must be set")
            )
        });
        let base = PathBuf::from(base);
        // A missing model is a failure, not a skip. Reporting `ok` without
        // running whisper is how the detect_language bug went unnoticed.
        assert!(
            model_present(&base, variant),
            "no `{variant}` model at {} — provision one or set CHRONICLE_TEST_BASE_DIR",
            base.display()
        );
        base
    }

    /// What one state's create+free cycle costs: the work HEU-664 stops doing
    /// per call. The pre-fix path built and freed a state per `transcribe`, so
    /// the cycle is exactly the removed work. Do not quote it as a creation
    /// cost or as a transcription latency delta.
    #[test]
    #[ignore = "timing measurement for HEU-664; needs a provisioned model; run manually"]
    fn measure_state_create_free_cost() {
        // The figure is per-variant, and HEU-664's baseline run used `small`
        // while the code default is `base`.
        let variant = std::env::var("CHRONICLE_TEST_VARIANT")
            .ok()
            .map(|v| {
                parse_variant(&v).unwrap_or_else(|| {
                    panic!(
                        "CHRONICLE_TEST_VARIANT must be one of {SUPPORTED_VARIANTS:?}, got {v:?}"
                    )
                })
            })
            .unwrap_or(DEFAULT_VARIANT);
        let base = test_base_dir(variant);
        let engine = TranscriptionEngine::load(&base, variant).expect("model must be present");

        // Timed separately. This figure swings roughly 6x across runs with how
        // warm Metal is in the session, so it is not a stable number.
        let first_start = std::time::Instant::now();
        let first = engine.ctx.create_state().expect("first state");
        let first_elapsed = first_start.elapsed();
        drop(first);

        // Min is the most robust single figure; a scheduler hiccup skews a mean.
        let runs = 10;
        let mut samples: Vec<std::time::Duration> = Vec::with_capacity(runs);
        for _ in 0..runs {
            let iter_start = std::time::Instant::now();
            let state = engine.ctx.create_state().expect("state");
            drop(state);
            samples.push(iter_start.elapsed());
        }
        let elapsed: std::time::Duration = samples.iter().sum();
        samples.sort();
        let min = samples[0];
        // True median for an even `runs`: the mean of the two middles.
        let median = (samples[runs / 2 - 1] + samples[runs / 2]) / 2;

        // `whisper-rs-sys` builds whisper.cpp with a hardcoded CMake `Release`,
        // so a debug run measures the same native code. The label says so.
        println!(
            "state create+free ({}, rustc {}, whisper.cpp Release always): \
             first-after-load {:?} (NOT reproducible -- Metal warmth), \
             then {} runs: min {:?}, median {:?}, mean {:?}, total {:?}",
            variant.as_str(),
            if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            },
            first_elapsed,
            runs,
            min,
            median,
            elapsed / runs as u32,
            elapsed
        );
    }

    #[test]
    #[ignore = "needs a provisioned whisper model + macOS `say`; run manually"]
    fn engine_transcribes_real_speech() {
        let base = test_base_dir(DEFAULT_VARIANT);

        let pcm = synthesize_test_phrase("the quick brown fox jumps over the lazy dog");
        let engine = TranscriptionEngine::load(&base, DEFAULT_VARIANT).unwrap();

        // 1) Cold: the short-PCM guard must reject before the slot is ever
        // populated, or a too-short first segment pays a full backend build.
        assert!(
            matches!(
                engine.transcribe(&[0.0; 1]),
                Err(TranscriptionError::Whisper(ref m))
                    if m == "input sample buffer too short: got 1, need at least 41 samples"
            ),
            "a non-empty too-short buffer must be rejected on its own terms"
        );
        assert_eq!(
            engine.state.creation_count(),
            0,
            "a rejected too-short buffer must not have built a state"
        );

        // 2) Transcribe with the real engine and assert it produced text.
        let t = engine.transcribe(&pcm).unwrap();
        assert!(
            !t.text.is_empty(),
            "expected non-empty transcript, got empty (the detect_language bug)"
        );
        let lower = t.text.to_lowercase();
        assert!(
            lower.contains("fox") || lower.contains("quick") || lower.contains("brown"),
            "transcript should contain a spoken word, got: {:?}",
            t.text
        );
        // `set_language(None)` must auto-detect as well as transcribe.
        assert_eq!(
            t.language.as_deref(),
            Some("en"),
            "expected auto-detected language `en`"
        );

        // 3) Second call on the SAME engine — this is the HEU-664 guarantee.
        // No words in common with phrase 1, so a stale first result cannot pass.
        let second_pcm = synthesize_test_phrase("pack my box with five dozen liquor jugs");
        let second = engine.transcribe(&second_pcm).expect("second transcribe");
        let lower = second.text.to_lowercase();
        assert!(
            lower.contains("box") || lower.contains("dozen") || lower.contains("jugs"),
            "second transcription on a reused state returned: {:?}",
            second.text
        );

        // The point of HEU-664: two transcriptions, ONE state.
        assert_eq!(
            engine.state.creation_count(),
            1,
            "two transcriptions must share one whisper state"
        );

        // 4) Warm: the same guard with the slot populated. Without it the `Err`
        // from `full()` reaches `StateSlot::with` and discards a healthy state.
        assert!(
            matches!(
                engine.transcribe(&[]),
                Err(TranscriptionError::Whisper(ref m)) if m == "Input sample buffer was empty."
            ),
            "empty PCM must be rejected with whisper-rs's own wording"
        );

        // The rebuild moves the counter, not the discard, so the warm empty
        // call must be followed by a real one or the guard is invisible.
        let third = engine
            .transcribe(&second_pcm)
            .expect("third transcribe after an empty buffer");
        let lower = third.text.to_lowercase();
        assert!(
            lower.contains("box") || lower.contains("dozen") || lower.contains("jugs"),
            "third transcription after a rejected empty buffer returned: {:?}",
            third.text
        );
        assert_eq!(
            engine.state.creation_count(),
            1,
            "a rejected empty buffer must not have discarded the state"
        );
    }

    #[test]
    fn decode_handles_partial_final_frame() {
        use chronicle_audio::OggOpusEncoder;
        let dir = tempdir().unwrap();
        let path = dir.path().join("partial.opus");
        // Not a whole-frame multiple → exercises the padded final frame + granule trim.
        let samples: Vec<f32> = (0..24_137)
            .map(|i| (i as f32 * 200.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.2)
            .collect();
        OggOpusEncoder::new(1, 24_000, opus::Application::Voip)
            .encode_to_file(&samples, &path)
            .unwrap();
        let pcm = decode_opus_16k_mono(&path).unwrap();
        // ~24137/3 ≈ 8046 samples @ 16 kHz, within one 20 ms frame (320) of slack.
        assert!(
            (7_700..=8_400).contains(&pcm.len()),
            "expected ~8046 samples, got {}",
            pcm.len()
        );
    }

    #[test]
    fn manifest_covers_every_supported_variant() {
        for v in SUPPORTED_VARIANTS {
            let variant = parse_variant(v).unwrap();
            let info = model_info(variant);
            assert_eq!(info.variant, variant);
            assert!(info.url.starts_with("https://huggingface.co/"));
            assert!(
                info.url.ends_with(&format!("ggml-{v}.bin")),
                "{v}: url must point at its own variant's file"
            );
            assert_eq!(info.sha1.len(), 40, "sha1 must be 40 hex chars");
            assert!(info.sha1.chars().all(|c| c.is_ascii_hexdigit()));
            assert!(info.size_bytes > 100_000_000, "all variants exceed 100 MB");
        }
    }

    #[test]
    fn manifest_pins_match_fetch_script_values() {
        // Sizes have no in-repo counterpart, so exact pins catch edits.
        let script = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../scripts/fetch-whisper-model.sh"),
        )
        .expect("fetch script readable");
        for v in SUPPORTED_VARIANTS {
            let info = model_info(parse_variant(v).unwrap());
            // Anchor to the variant's own case branch, not the whole file —
            // an unanchored contains() passes when two branches swap values.
            let label = format!("{v})");
            let start = script
                .find(&label)
                .unwrap_or_else(|| panic!("{v}: case branch missing from fetch script"));
            let end = script[start..]
                .find(";;")
                .map_or(script.len(), |e| start + e);
            let branch = &script[start..end];
            assert!(
                branch.contains(info.sha1),
                "{v}: sha1 drifted from its fetch-script branch"
            );
            assert!(
                branch.contains(info.url),
                "{v}: url drifted from its fetch-script branch"
            );
        }
        assert_eq!(
            model_info(parse_variant("base").unwrap()).size_bytes,
            148_000_000
        );
        assert_eq!(
            model_info(parse_variant("small").unwrap()).size_bytes,
            488_000_000
        );
        assert_eq!(
            model_info(parse_variant("medium").unwrap()).size_bytes,
            1_534_000_000
        );
    }
}
