//! Transcription pipeline for Chronicle.
//!
//! The whisper.cpp engine wiring lives in HEU-472. This crate currently
//! owns the model-path convention and the allow-list of supported
//! variants so HEU-472 (and the daemon startup check) agree on what is
//! valid and where to look.

use std::fmt;
use std::path::{Path, PathBuf};

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
}
