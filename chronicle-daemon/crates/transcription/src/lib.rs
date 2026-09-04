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
/// Listed smallest-to-largest by file size (base ≈ 150 MB, medium
/// ≈ 1.5 GB). Lookup uses linear scan, which is fine at this size.
pub const SUPPORTED_VARIANTS: &[&str] = &["base", "small", "medium"];

/// An allow-listed whisper.cpp model variant.
///
/// The wrapped string is always one of [`SUPPORTED_VARIANTS`]. The field is
/// private and [`parse_variant`] is the only public constructor, so a
/// `ModelVariant` can never hold an arbitrary string — including one with
/// path-traversal characters. That is what makes [`model_path`] safe: it
/// cannot be reached with anything outside the allow-list.
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

/// Pinned download source for one model variant. URLs 302-redirect to the
/// Hugging Face CDN; SHA1s are upstream's published digests (integrity —
/// TLS provides authenticity). `size_bytes` is the advertised size, used
/// for the disk precheck and UI labels before a file exists locally.
/// Keep in lockstep with scripts/fetch-whisper-model.sh (AD-2).
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
        // Upstream Content-Length 487,601,967, rounded UP to decimal MB.
        // Advertised sizes feed the disk precheck, so rounding down would
        // fail in the unsafe direction (precheck passes, download ENOSPCs).
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

/// Pinned manifest entry for a variant. Total (`ModelVariant` is
/// allow-list-only and the manifest covers the allow-list, enforced by test).
pub fn model_info(variant: ModelVariant) -> &'static ModelInfo {
    MANIFEST
        .iter()
        .find(|m| m.variant == variant)
        .expect("manifest covers every allow-listed variant")
}

/// Parse a raw string into one of the supported variants.
///
/// Returns a [`ModelVariant`] only when `s` exactly matches an entry in
/// [`SUPPORTED_VARIANTS`]. This is the sole public constructor of
/// `ModelVariant`, so anything not in the allow-list — including
/// path-traversal attempts — returns `None`.
pub fn parse_variant(s: &str) -> Option<ModelVariant> {
    SUPPORTED_VARIANTS
        .iter()
        .copied()
        .find(|v| *v == s)
        .map(ModelVariant)
}

/// Path to the ggml model file for a given variant.
///
/// `variant` is a [`ModelVariant`], so it is guaranteed to be one of the
/// allow-listed names — there is no way to reach this function with an
/// arbitrary string. Existence is NOT checked — see [`model_present`].
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

/// Stream-SHA1 a file and compare against `expected`. The digest is produced
/// as lowercase hex; the comparison ignores case, so `expected` may be either.
/// Used by the daemon's downloader (NFR-1) — digest guards integrity, TLS
/// guards authenticity (AD-6).
///
/// **Blocking and CPU-bound — call it under `tokio::task::spawn_blocking`.**
/// The `sha1` crate has no aarch64 hardware backend, so on Apple Silicon this
/// is software SHA1: seconds of un-yielding work on the 1.5 GB `medium` model.
/// Called directly from an async path it stalls a runtime worker and starves
/// the 1 Hz IPC status poll — the same poll that renders the `Verifying` state
/// to the user.
///
/// Streams via [`std::io::copy`] rather than reading the file in, so a 1.5 GB
/// model never lands in memory. Returns `Err` only for I/O failures; a file
/// that reads fine but hashes differently is `Ok(false)`, which is a rejected
/// download rather than a broken one.
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

/// Rejection message for a buffer under [`MIN_DECODABLE_SAMPLES`]. Matches
/// whisper-rs's own `WhisperError::NoSamples` wording, so the empty case reads
/// to a caller exactly as it did before HEU-664.
const TOO_SHORT_MSG: &str = "Input sample buffer was empty.";

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

/// True when `s` is entirely one bracketed whisper marker — `[BLANK_AUDIO]`,
/// `[Motor]`, `[_BEG_]`. whisper.cpp emits these as ordinary segment text, so
/// without this check they are indistinguishable from speech to every layer
/// above and land in `audio_fts` as searchable words (FTS5 tokenizes
/// `[BLANK_AUDIO]` into `blank` and `audio`).
///
/// **This is a heuristic biased toward keeping text, not a definition of what a
/// marker is.** It is structural rather than an allow-list because whisper's
/// marker vocabulary is not stable across models — but it does not follow that
/// every marker fits this shape, and rows this pipeline itself wrote prove
/// otherwise: `[ Inaudible ]`, `[sad music]`, `[Distant by the wind]`,
/// `[報告 ]`. Every one is a single segment of pure marker text that this
/// function deliberately keeps.
///
/// (The live database also holds `[silence] [silence]` and `[Music] [Music]
/// [Music]`. Those are weaker evidence, not stronger: whether whisper ever hands
/// this function a whole multi-marker string as ONE segment is not recoverable
/// from a stored transcript. Either way the outcome is the same — split across
/// segments each `[silence]` is dropped individually, and packed into one
/// segment the internal space spares it here. The four rows above need no such
/// assumption. See `003_null_marker_transcripts.sql`, GRANULARITY.)
///
/// It errs in both directions, knowingly:
///
/// - **Misses** markers with internal whitespace or non-ASCII content (the rows
///   above). They survive into `transcript` and `audio_fts`. Widening the rule
///   to catch them would put real speech at risk — live row 1501 is
///   `[ Background noise ]` wrapped around ordinary conversation, and any rule
///   loose enough to catch `[silence] [silence]` also catches that.
/// - **Over-matches** a bracketed ASCII token that is genuinely speech, when
///   whisper isolates it in its own segment. `["the ", "[sic]", " answer"]`
///   yields `"the  answer"` — pinned by
///   `concat_segment_text_drops_bracketed_token_split_into_own_segment`, which
///   asserts the loss rather than hiding it. Nothing structural distinguishes
///   `[sic]` from `[Motor]`; see HEU-622.
///
/// Given that, each inner condition rules out a different false positive:
///
/// - **non-empty** — `[]` is not a marker.
/// - **ASCII** — this is the one that keeps the guard honest in spaceless
///   scripts. `set_language(None)` means any language can come back, and a
///   bracketed Chinese, Japanese, or Thai phrase is a single whitespace-free
///   token, so whitespace alone cannot tell it from a marker. Requiring ASCII
///   inside the brackets keeps `[这是一个完整的句子]` as speech while still
///   dropping `[BLANK_AUDIO]`. Without it this function silently deletes real
///   transcript text, and only for non-English users.
/// - **whitespace-free** — a bracketed aside survives *when whisper keeps it in
///   one segment with its surrounding words*. That caveat is load-bearing: the
///   filter runs per segment, so this condition cannot protect a token whisper
///   split out on its own.
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
/// markers, so a segment whisper filled with `[BLANK_AUDIO]` reads as empty
/// here. An empty result means "no usable speech"; what the caller persists for
/// that is the caller's business — see `pipeline::transcribe_loop`.
///
/// whisper-rs 0.14.4 exposes no per-segment `no_speech_prob`, so probability
/// filtering of music/noise is deferred (HEU-472); the `suppress_blank` whisper
/// param, this marker filter, and the empty check are the combined guard.
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
/// The decoder is created for 16 kHz mono output, and libopus resamples and
/// downmixes to that format internally — a mono-initialized decoder downmixes
/// stereo packets per the Opus API — so no separate resampler is needed and
/// the OpusHead channel count needs no checking (Chronicle encodes mono
/// anyway). The first two Ogg packets are the OpusHead
/// and OpusTags headers (RFC 7845); the rest are audio. The encoder's pre-skip
/// (lookahead priming) is dropped using the OpusHead pre_skip, and the final
/// granule position drives a defensive end clamp (see below) so the decoded
/// duration matches the captured segment.
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
    // PacketReader::read_packet returns Result<Option<Packet>, ogg::OggReadError>.
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
        last_granule = packet.absgp_page(); // public method; the field is private
    }

    // Drop encoder pre-skip (scale 48 kHz → 16 kHz).
    let skip16 = pre_skip * DECODE_RATE / ENCODE_RATE;
    if skip16 >= pcm.len() {
        pcm.clear();
    } else {
        pcm.drain(..skip16);
    }

    // Defensive clamp to the granule-reported length: take the final granule
    // (48 kHz), subtract pre_skip, and scale to 16 kHz. Our encoder writes the
    // *padded* final-frame length into the granule (not an RFC 7845 end-trimmed
    // value), so for our own segments total16 equals the decoded length and the
    // truncate never fires — up to ~one 20 ms frame of trailing near-silence is
    // left in by design (whisper tolerates it; over-trimming would be worse).
    // The clamp still protects against a future producer that writes a true
    // end-trimmed granule position.
    // Divide, never multiply. `ENCODE_RATE` is an exact multiple of `DECODE_RATE`
    // (48k/16k = 3), so this is bit-identical to `* DECODE_RATE / ENCODE_RATE` while
    // being unable to overflow at all — scaling by multiply first would overflow at
    // ~1.15e15 in u64 just as it does in usize, since usize is 64-bit here.
    // That threshold is reachable: `last_granule` is a raw u64 off the Ogg page
    // header, and a page on which no packet completes carries granulepos -1,
    // i.e. `u64::MAX`.
    let total16 = (last_granule.saturating_sub(pre_skip as u64)
        / (ENCODE_RATE / DECODE_RATE) as u64) as usize;
    if total16 > 0 && pcm.len() > total16 {
        pcm.truncate(total16);
    }

    Ok(pcm)
}

/// A type that can turn 16 kHz mono PCM into a [`Transcript`]. The trait makes
/// the transcribe loop testable with a fake — the real impl needs a model file.
///
/// # Contract
///
/// Implementations return **speech text only**, with engine-specific markers
/// already removed — `TranscriptionEngine` does this via
/// [`concat_segment_text`], which drops whisper.cpp's `[BLANK_AUDIO]` and
/// friends. Content-level filtering is the implementation's job, not the
/// caller's: the rule for what counts as a marker is engine-specific, and a
/// caller applying one engine's rule to every implementation would corrupt
/// output from the others.
///
/// An empty or whitespace-only result means **no usable speech was found**.
/// That is a meaningful answer rather than a failure, and callers act on it:
/// `pipeline::transcribe_loop` records it as an attempt with a NULL transcript
/// (HEU-620). Return `Err` only when transcription could not be performed.
///
/// `language` on such a result is **not** required to be `None`, and
/// `TranscriptionEngine`'s is not — whisper reports a detection even for
/// silence, derived from noise. Implementations need not suppress it, because
/// `Storage::update_transcript_full` clears `language` whenever the transcript
/// collapses to nothing, so a language fabricated on THAT result cannot reach
/// the database.
///
/// Scoped deliberately: it is not a storage-wide invariant (see that method's
/// own docs). A row whose text does not collapse keeps whatever language it was
/// given — which is why the six pure-marker rows migration 003 knowingly spares
/// still carry a noise-derived one. Stated here because this trait doc is where
/// a second implementor would look for the rule.
pub trait Transcriber: Send + Sync {
    fn transcribe(&self, pcm_16k_mono: &[f32]) -> Result<Transcript, TranscriptionError>;
    /// The resolved model variant, written to the transcript row.
    fn variant(&self) -> &str;
}

/// Holds one long-lived `T` behind a `Mutex`, created on first use.
///
/// Generic so the lifecycle — lazy creation, invalidation, poison recovery —
/// is testable with a fake `T` and no whisper model; those three paths are
/// otherwise reachable only from an ignored real-model test. It is still a
/// whisper state slot: its log lines name transcription, because they are what
/// HEU-664's live verification greps for.
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
                // LOAD-BEARING: the slot must be emptied before the `is_none`
                // check below. A panic leaves the mutex poisoned with the
                // corrupted state still in it. Keep that state and `is_none`
                // is false, so `make()` never runs and `use_state` is handed
                // the corrupted state on this very call. Emptying makes the
                // worst case an empty slot, which any later call just refills.
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
            // Matches the per-call behaviour this type replaces: a state that
            // errored is never reused. Upstream documents no reuse guarantee
            // after an error, so do not invent one. Which of a caller's error
            // paths can actually reach here is version-pinned, so it lives
            // with the other whisper-rs findings rather than in this type.
            //
            // Emptied before the log so a panicking logger cannot poison the
            // mutex with the errored state still in it. The count reads the
            // same either way — a discard never decrements it.
            *slot = None;
            // The count is what verifies the fix: one state per engine means
            // one backend bring-up, and each discard adds at most one more —
            // the rebuild only happens if another transcription follows, so a
            // discard on the last one adds a line but no allocation
            // (HEU-664). Deliberately whisper-specific wording in an otherwise
            // domain-agnostic type: this string is the anchor HEU-664's live
            // verification greps for.
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
/// for the life of the engine — creating one allocates the whole GGML compute
/// backend, so doing it per call rebuilt Metal on every segment (HEU-664).
///
/// The slot holds its mutex across the whole decode, so concurrent
/// `transcribe` calls serialize. That costs nothing today: `transcribe_loop`
/// drains its channel one job at a time with a single `spawn_blocking` in
/// flight. It is a deliberate trade — a shared state cannot be used from two
/// threads at once, and per-worker states would reintroduce the per-state
/// backend allocation this exists to remove.
///
/// The trade is memory: between segments the engine now holds the GGML compute
/// backend, the KV caches, the mel buffer, and the last decode's segments, and
/// keeps holding them while the daemon is idle. That is the point — it is what
/// the rebuild was paying for. Expect resident size to sit higher than before,
/// though HEU-664 did not measure by how much.
pub struct TranscriptionEngine {
    // Declared before `ctx` so it drops first, which is correct regardless of
    // how `WhisperState` holds its context. Do not reorder.
    state: StateSlot<WhisperState>,
    ctx: WhisperContext,
    variant: String,
}

impl TranscriptionEngine {
    /// Load the ggml model for `variant` — Metal when the `metal` feature is on,
    /// CPU otherwise. Fallible — the caller logs and stays idle on `Err`
    /// (graceful degradation, BR-5).
    pub fn load(base_dir: &Path, variant: ModelVariant) -> Result<Self, TranscriptionError> {
        let path = model_path(base_dir, variant);
        let path_str = path
            .to_str()
            .ok_or_else(|| TranscriptionError::ModelLoad("model path is not UTF-8".into()))?;

        // GPU (Metal) when the `metal` feature is on; CPU otherwise — makes
        // `--no-default-features --features cpu` an honest CPU build.
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
        // `StateSlot::with` would discard a state that never ran — and the next
        // segment would pay a full backend rebuild for nothing.
        //
        // The threshold covers the whole class, not just the empty case:
        // `full()` returns early on an empty slice, and a buffer below
        // MIN_DECODABLE_SAMPLES fails a step later in language auto-detect,
        // also before touching the state.
        //
        // Nothing the daemon writes lands in that band today — the Opus
        // encoder pads to whole frames, so the shortest decodable segment is
        // 216 samples. That floor belongs to another crate and nothing pins
        // it, which is why this guards the range rather than the one member
        // of it that is reachable right now.
        if pcm_16k_mono.len() < MIN_DECODABLE_SAMPLES {
            return Err(TranscriptionError::Whisper(TOO_SHORT_MSG.into()));
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
                // Pinned, not inherited. The state is shared across segments now,
                // so this stops one segment's *prompt history* priming the next.
                // It is already the whisper.cpp default, but whisper-rs documents
                // it as `false` (whisper_params.rs:119-121) and that is wrong.
                //
                // It does NOT make segments fully independent, and nothing can:
                // the state's decoder RNG is seeded once and never reset, so a
                // segment that hits temperature fallback shifts later ones, and
                // identical audio at a different queue position can decode
                // differently. Output is no longer byte-identical across a
                // queue; whether quality differs has not been measured.
                // Accepted — whisper.cpp exposes no reseed, and the
                // alternatives are disabling fallback or per-call states, the
                // latter being what HEU-664 removes. Mechanism, against
                // whisper-rs-sys 0.13.1's bundled whisper.cpp: the RNG is
                // seeded once in whisper_init_state (:3346) and the per-call
                // re-init loop starts at j = 1 (:5426), so decoder 0 is never
                // reseeded; it is drawn from only on temperature fallback
                // (:5199), which is live because temperature_inc defaults to
                // 0.2 (:4686).
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

                // Marker filter + empty-text guard (whisper-rs 0.14.4 has no
                // per-segment no_speech_prob).
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
    fn too_short_msg_matches_whisper_rs() {
        assert_eq!(
            TOO_SHORT_MSG,
            whisper_rs::WhisperError::NoSamples.to_string()
        );
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
        // `io::copy` reads in 8 KiB chunks, so the 11-byte fixtures above never
        // drive a second iteration — an implementation that hashed only the
        // first buffer would pass every other test here. 100 KB forces the loop
        // and is what actually pins the streaming property.
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
        // The whole point of the allow-list is that an attacker (or a
        // typo) cannot turn the settings value into a path component.
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
        // `[Motor]` appears 3 times in the live database; `[_BEG_]` is another
        // whisper.cpp emission. The rule is structural, not an allow-list.
        let segs = ["[Motor]", "[_BEG_]"];
        assert_eq!(concat_segment_text(segs.iter().copied()), "");
    }

    #[test]
    fn concat_segment_text_keeps_brackets_inside_prose() {
        // A bracketed aside in real speech keeps its spaces, so the whitespace
        // check leaves it alone. Losing this would silently truncate transcripts.
        // NOTE the single segment: that is what makes this pass. See the
        // split-segment test below for the case where it does not.
        let segs = ["the [sic] answer"];
        assert_eq!(
            concat_segment_text(segs.iter().copied()),
            "the [sic] answer"
        );
    }

    /// Pins a KNOWN LOSS rather than a guarantee.
    ///
    /// `is_whisper_marker` runs per segment, and whisper's boundaries are
    /// timing-dependent. When it isolates a bracketed ASCII token, that token is
    /// dropped whether it is a marker or genuine speech — nothing structural
    /// separates `[sic]` from `[Motor]`. The sibling test above passes only
    /// because it puts the whole sentence in one segment, which was giving false
    /// confidence in a case it never exercised.
    ///
    /// Asserted so the limitation is visible in the suite. If a future change
    /// makes this return `"the [sic] answer"`, that is an improvement — update
    /// the test and close HEU-622.
    ///
    /// One caveat before drawing that conclusion: the expected `"the  answer"`
    /// has a DOUBLE space, an artifact of both neighbours keeping their own
    /// spacing across the dropped segment. A change to how the join handles
    /// whitespace runs would also fail this test, and that has nothing to do
    /// with HEU-622. Check which of the two moved before deciding.
    #[test]
    fn concat_segment_text_drops_bracketed_token_split_into_own_segment() {
        let segs = ["the ", "[sic]", " answer"];
        assert_eq!(
            concat_segment_text(segs.iter().copied()),
            "the  answer",
            "a bracketed token in its own segment is dropped — known limitation, HEU-622"
        );
    }

    /// The other side of the same mechanism, and the reason it is worth having:
    /// a marker whisper splits out is removed cleanly while the speech beside it
    /// survives. This is the common real-world shape — live rows 1082 and 1396,
    /// whose text is captured audio and so is not reproduced here. The fixture
    /// below mirrors their shape (leading marker, then `>>`-prefixed speech)
    /// with invented words; nothing in the assertion depends on which words.
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
        // Isolates the whitespace condition: this string satisfies both the
        // bracket-shape and ASCII checks, so only the whitespace check can
        // spare it. A fixture that two conditions can each independently spare
        // proves nothing about either.
        let segs = ["[hello world]"];
        assert_eq!(concat_segment_text(segs.iter().copied()), "[hello world]");
    }

    #[test]
    fn concat_segment_text_keeps_bracketed_text_in_spaceless_scripts() {
        // Isolates the ASCII condition. CJK and Thai have no inter-word
        // spaces, so a bracketed phrase is a single whitespace-free token and
        // the whitespace check cannot spare it — only the ASCII check can.
        // Dropping these would be silent transcript loss for exactly the
        // users the auto-detect path exists to serve.
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
        // Isolates the OPENING-bracket condition, the mirror of
        // `concat_segment_text_keeps_unclosed_bracket`. Without this, relaxing
        // `strip_prefix('[')` away changes nothing in the suite and the leading
        // bracket ships untested. The migration fixture pins the SQL half of the
        // same guard with row 14; this is the Rust half.
        let segs = ["sic]"];
        assert_eq!(concat_segment_text(segs.iter().copied()), "sic]");
    }

    #[test]
    fn concat_segment_text_drops_a_marker_carrying_a_leading_space() {
        // Pins the `trim()` INSIDE `is_whisper_marker`, which nothing else does.
        // whisper emits a leading space per segment, so the marker arrives as
        // `" [BLANK_AUDIO]"` — without the trim it is not recognised, survives
        // as text, and lands in the transcript. Delete that trim and this is the
        // only assertion in the suite that fails. The migration fixture pins the
        // SQL side's `trim()` with row 12; this is the Rust side.
        //
        // It also shows the words staying apart across the dropped segment, but
        // that alone is already covered three times over — by `_joins_and_trims`,
        // `_drops_marker_but_keeps_speech`, and
        // `_drops_bracketed_token_split_into_own_segment`. Each puts a word on
        // either side of a boundary and asserts the spaces survive.
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
    ///
    /// Extracted so a second phrase costs one line rather than a duplicated
    /// fixture; the whole body was previously inlined in the caller.
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

        // Guard the transcript asserts in the caller: if `say` or the WAV parse
        // yielded no audio, an empty transcript would be blamed on the
        // detect_language bug and send the next maintainer after an
        // already-correct parameter.
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
        // `#[ignore]` already keeps these off CI, so anyone reaching this line
        // asked for the test on purpose: a missing model is a failure, not a
        // skip. Reporting `ok` without running whisper is exactly how the bug
        // `engine_transcribes_real_speech` guards went unnoticed.
        assert!(
            model_present(&base, variant),
            "no `{variant}` model at {} — provision one or set CHRONICLE_TEST_BASE_DIR",
            base.display()
        );
        base
    }

    /// What one state's create+free cycle costs — the work HEU-664 stops doing
    /// per call.
    ///
    /// **Create AND free, not creation alone.** Each iteration builds a state
    /// and drops it inside the timer. That is deliberate: the pre-HEU-664 path
    /// created a state per `transcribe` call and freed it at the end of that
    /// call, so the cycle is exactly the removed work. Do not quote this as a
    /// creation cost.
    ///
    /// Not a before/after latency benchmark either: it measures the removed
    /// work directly, which needs no base-revision build or fixed PCM fixture.
    /// Never report it as a transcription latency delta.
    #[test]
    #[ignore = "timing measurement for HEU-664; needs a provisioned model; run manually"]
    fn measure_state_create_free_cost() {
        // Overridable because the figure is per-variant and the variant is part
        // of the label: HEU-664's baseline churn run used `small`, while the
        // code default is `base`. Comparing a `base` cost against a `small`
        // run is the mistake this exists to prevent.
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

        // Timed separately, but NOT because "the first state pays one-time Metal
        // setup" — measurement says otherwise. The Metal device and metallib are
        // a process-level singleton that `load` above already touched, outside
        // both timers. Observed swinging by roughly 6x across runs on one
        // machine while the steady state held. So this figure is "how warm was
        // Metal in this session", is not reproducible, and must not be quoted
        // as a stable number — the run that produced HEU-664's figures is on
        // the ticket.
        let first_start = std::time::Instant::now();
        let first = engine.ctx.create_state().expect("first state");
        let first_elapsed = first_start.elapsed();
        // Drop before the loop: holding it would keep a second backend resident
        // throughout, which is not the lifecycle being measured.
        drop(first);

        // Per-iteration samples, not just a mean: the spread is a few percent,
        // so one scheduler hiccup skews a number that gets quoted
        // indefinitely. Min is the most robust single figure here.
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
        // True median: for an even `runs`, the mean of the two middles. Indexing
        // `runs / 2` alone would report the upper middle, which is off by half
        // an interval from what "median" is quoted to mean.
        let median = (samples[runs / 2 - 1] + samples[runs / 2]) / 2;

        // whisper.cpp/ggml is built by `whisper-rs-sys` with a hardcoded CMake
        // `Release` (its build.rs), so `cargo test --release` measures the same
        // native code as a debug run. Only the thin Rust wrapper changes. The
        // label says so, or the reader infers this is a pessimistic debug
        // figure that production beats -- it is not.
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

        // 1) Cold: the empty-PCM guard must reject before the slot is ever
        // populated. Without it, `with` finds an empty slot, builds the whole
        // GGML backend, hands it to `full()`, and discards it on the `Err` —
        // so an empty first segment pays a full build for nothing. Checked
        // cold as well as warm because `load` leaves the slot empty and
        // nothing exempts the first segment from being short.
        assert!(
            matches!(
                engine.transcribe(&[]),
                Err(TranscriptionError::Whisper(ref m)) if m == "Input sample buffer was empty."
            ),
            "empty PCM must be rejected with the guard's documented literal"
        );
        assert_eq!(
            engine.state.creation_count(),
            0,
            "a rejected empty buffer must not have built a state"
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
        // `set_language(None)` must auto-detect *as well as* transcribe — the
        // plan expects a non-NULL language on the persisted row.
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

        // The actual point of HEU-664: two transcriptions, ONE state.
        // Without this the test passes identically against per-call creation.
        assert_eq!(
            engine.state.creation_count(),
            1,
            "two transcriptions must share one whisper state"
        );

        // 4) Warm: the same guard with the slot populated. Without it the `Err`
        // from `full()` reaches `StateSlot::with`, which discards a healthy
        // state — turning a run of short segments back into the per-segment
        // rebuild HEU-664 removes.
        assert!(
            matches!(
                engine.transcribe(&[]),
                Err(TranscriptionError::Whisper(ref m)) if m == "Input sample buffer was empty."
            ),
            "empty PCM must be rejected with the guard's documented literal"
        );

        // The rebuild moves the counter, not the discard — emptying the slot
        // builds nothing. So the warm empty call must be followed by a real one
        // or the assertion below passes with the guard deleted. Confirmed by
        // mutation: without this third transcription it is invisible.
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
        // Real cross-check against the other copy of these pins (AD-2):
        // drift in either the manifest or the script fails here. Sizes have
        // no in-repo counterpart, so exact pins catch accidental edits.
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
