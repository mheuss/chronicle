//! Transcription pipeline for Chronicle.
//!
//! The whisper.cpp engine wiring lives in HEU-472. This crate currently
//! owns the model-path convention and the allow-list of supported
//! variants so HEU-472 (and the daemon startup check) agree on what is
//! valid and where to look.

use std::path::{Path, PathBuf};

/// Subdirectory under the Chronicle base dir where ggml model files live.
pub const MODELS_SUBDIR: &str = "models";

/// Allow-list of whisper.cpp model variants the daemon understands.
/// Listed smallest-to-largest by file size (base ≈ 150 MB, medium
/// ≈ 1.5 GB). Lookup uses linear scan, which is fine at this size.
pub const SUPPORTED_VARIANTS: &[&str] = &["base", "small", "medium"];

/// Default variant used when settings is missing, empty, or invalid.
pub const DEFAULT_VARIANT: &str = "base";

/// Parse a raw string into one of the supported variants.
///
/// Returns the matching entry from [`SUPPORTED_VARIANTS`] — a `'static`
/// string slice — so callers can safely embed the result in a path
/// without worrying about path-traversal characters. Returns `None` for
/// anything not in the allow-list.
pub fn parse_variant(s: &str) -> Option<&'static str> {
    SUPPORTED_VARIANTS.iter().copied().find(|v| *v == s)
}

/// Path to the ggml model file for a given variant.
///
/// The variant must come from [`parse_variant`] / [`SUPPORTED_VARIANTS`]
/// — passing arbitrary user input here is a footgun because it becomes
/// part of the filename. Existence is NOT checked — see [`model_present`].
pub fn model_path(base_dir: &Path, variant: &'static str) -> PathBuf {
    base_dir
        .join(MODELS_SUBDIR)
        .join(format!("ggml-{variant}.bin"))
}

/// Quick existence check used by daemon startup to decide whether to log
/// a "model missing, transcription idle" warning.
pub fn model_present(base_dir: &Path, variant: &'static str) -> bool {
    model_path(base_dir, variant).is_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::tempdir;

    #[test]
    fn supported_variants_constants() {
        assert_eq!(SUPPORTED_VARIANTS, &["base", "small", "medium"]);
        assert_eq!(DEFAULT_VARIANT, "base");
        assert_eq!(MODELS_SUBDIR, "models");
    }

    #[test]
    fn parse_variant_accepts_allow_list() {
        assert_eq!(parse_variant("base"), Some("base"));
        assert_eq!(parse_variant("small"), Some("small"));
        assert_eq!(parse_variant("medium"), Some("medium"));
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
            model_path(base, "base"),
            Path::new("/tmp/chronicle/models/ggml-base.bin"),
        );
        assert_eq!(
            model_path(base, "medium"),
            Path::new("/tmp/chronicle/models/ggml-medium.bin"),
        );
    }

    #[test]
    fn model_present_false_when_file_missing() {
        let dir = tempdir().unwrap();
        assert!(!model_present(dir.path(), "base"));
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
        assert!(model_present(dir.path(), "base"));
        // Different variant at the same location is still missing.
        assert!(!model_present(dir.path(), "small"));
    }
}
