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
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

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
pub struct ModelInfo {
    pub variant: ModelVariant,
    pub url: &'static str,
    pub sha1: &'static str,
    pub size_bytes: u64,
}

const MANIFEST: [ModelInfo; 3] = [
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
        size_bytes: 466_000_000,
    },
    ModelInfo {
        variant: ModelVariant("medium"),
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-medium.bin",
        sha1: "fd9727b6e1217c2f614f9b698455c4ffd82463b4",
        size_bytes: 1_530_000_000,
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

/// Concatenate whisper sub-segment texts and trim. An empty result means "no
/// usable speech" and the caller must skip the DB write so blank rows never reach
/// `audio_fts`. whisper-rs 0.14.4 exposes no per-segment `no_speech_prob`, so
/// probability filtering of music/noise is deferred (see the design §4); the
/// `suppress_blank` whisper param plus this empty check is the 472 guard.
pub fn concat_segment_text<'a>(segments: impl IntoIterator<Item = &'a str>) -> String {
    let mut out = String::new();
    for text in segments {
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
pub trait Transcriber: Send + Sync {
    fn transcribe(&self, pcm_16k_mono: &[f32]) -> Result<Transcript, TranscriptionError>;
    /// The resolved model variant, written to the transcript row.
    fn variant(&self) -> &str;
}

/// whisper.cpp engine. The `WhisperContext` (the loaded model) is created once
/// and shared via `Arc`; each `transcribe` call creates its own `WhisperState`.
pub struct TranscriptionEngine {
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
            ctx,
            variant: variant.as_str().to_string(),
        })
    }
}

impl Transcriber for TranscriptionEngine {
    fn transcribe(&self, pcm_16k_mono: &[f32]) -> Result<Transcript, TranscriptionError> {
        let mut state = self
            .ctx
            .create_state()
            .map_err(|e| TranscriptionError::Whisper(e.to_string()))?;

        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_translate(false);
        // language = None means "auto-detect AND transcribe". (set_detect_language(true)
        // is detect-ONLY — it returns 0 segments and no text. HEU-472 T11.)
        params.set_language(None);
        params.set_suppress_blank(true);
        params.set_n_threads(4); // Metal does the work; keep CPU threads modest

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

        // Empty-text guard (whisper-rs 0.14.4 has no per-segment no_speech_prob).
        let text = concat_segment_text(texts.iter().map(String::as_str));
        let language = state
            .full_lang_id_from_state()
            .ok()
            .and_then(whisper_rs::get_lang_str)
            .map(|s| s.to_string());

        Ok(Transcript { text, language })
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

    #[test]
    #[ignore = "needs a provisioned whisper model + macOS `say`; run manually"]
    fn engine_transcribes_real_speech() {
        use chronicle_audio::OggOpusEncoder;

        // Chronicle *base* dir — the one holding `models/` — overridable for a
        // non-default data dir. `#[ignore]` already keeps this off CI, so anyone
        // reaching this line asked for the test on purpose: a missing model is a
        // failure, not a skip. Reporting `ok` without running whisper is exactly
        // how the bug this test guards went unnoticed.
        let base = std::env::var("CHRONICLE_TEST_BASE_DIR").unwrap_or_else(|_| {
            format!(
                "{}/Library/Application Support/Chronicle",
                std::env::var("HOME").expect("HOME environment variable must be set")
            )
        });
        let base = PathBuf::from(base);
        assert!(
            model_present(&base, DEFAULT_VARIANT),
            "no whisper model at {} — provision one or set CHRONICLE_TEST_BASE_DIR",
            base.display()
        );

        // 1) Generate speech with macOS `say` as 48 kHz mono f32 WAV.
        let dir = tempdir().unwrap();
        let wav = dir.path().join("tts.wav");
        let phrase = "the quick brown fox jumps over the lazy dog";
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

        // 2) Read the WAV's f32 samples (find the `data` chunk; rest is f32le).
        let bytes = std::fs::read(&wav).expect("failed to read `say` output");
        let data_pos = bytes
            .windows(4)
            .position(|w| w == b"data")
            .expect("no `data` chunk in `say` output")
            + 8; // skip the "data" tag + size
        let samples: Vec<f32> = bytes[data_pos..]
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();

        // 3) Encode to Ogg/Opus with the production encoder at the production
        //    bitrate (AudioConfig::default().bitrate), then decode like the
        //    pipeline does.
        let opus = dir.path().join("tts.opus");
        OggOpusEncoder::new(1, 64_000, opus::Application::Voip)
            .encode_to_file(&samples, &opus)
            .unwrap();
        let pcm = decode_opus_16k_mono(&opus).unwrap();

        // Guard the transcript assert below: if `say` or the WAV parse yielded no
        // audio, an empty transcript would be blamed on the detect_language bug
        // and send the next maintainer after an already-correct parameter.
        assert!(
            pcm.len() > DECODE_RATE,
            "TTS produced under 1s of audio ({} samples) — fix the fixture, not the engine",
            pcm.len()
        );

        // 4) Transcribe with the real engine and assert it produced text.
        let engine = TranscriptionEngine::load(&base, DEFAULT_VARIANT).unwrap();
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
            assert_eq!(info.sha1.len(), 40, "sha1 must be 40 hex chars");
            assert!(info.sha1.chars().all(|c| c.is_ascii_hexdigit()));
            assert!(info.size_bytes > 100_000_000, "all variants exceed 100 MB");
        }
    }

    #[test]
    fn manifest_pins_match_fetch_script_values() {
        // Duplicated knowingly with scripts/fetch-whisper-model.sh (AD-2).
        assert_eq!(
            model_info(parse_variant("base").unwrap()).sha1,
            "465707469ff3a37a2b9b8d8f89f2f99de7299dac"
        );
        assert_eq!(
            model_info(parse_variant("small").unwrap()).sha1,
            "55356645c2b361a969dfd0ef2c5a50d530afd8d5"
        );
        assert_eq!(
            model_info(parse_variant("medium").unwrap()).sha1,
            "fd9727b6e1217c2f614f9b698455c4ffd82463b4"
        );
    }
}
