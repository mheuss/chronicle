//! Transcription pipeline for Chronicle.
//!
//! The whisper.cpp engine wiring lives in HEU-472. This crate currently
//! owns the model-path convention and the allow-list of supported
//! variants so HEU-472 (and the daemon startup check) agree on what is
//! valid and where to look.

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
/// libopus resamples 48 kHz → 16 kHz and downmixes to mono internally, so no
/// separate resampler is needed. The first two Ogg packets are the OpusHead
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
    let total16 = (last_granule as usize).saturating_sub(pre_skip) * DECODE_RATE / ENCODE_RATE;
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
    /// Load the ggml model for `variant` with the Metal backend. Fallible — the
    /// caller logs and stays idle on `Err` (graceful degradation, BR-5).
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
        params.set_detect_language(true); // auto-detect
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
            texts.push(
                state
                    .full_get_segment_text(i)
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
}
