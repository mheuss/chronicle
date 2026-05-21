//! Daemon settings persisted across restarts.
//!
//! A minimal `key=value` text file at `<base_dir>/settings`. Holds the
//! microphone preference (`mic_enabled`), the capture pause state
//! (`capture_paused`), and the selected whisper-model variant
//! (`whisper_model`). Deliberately not JSON — see the HEU-330 design's
//! Data Architecture and anti-scope.

use chronicle_transcription::{DEFAULT_VARIANT, parse_variant};
use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

const SETTINGS_FILE: &str = "settings";
const MIC_KEY: &str = "mic_enabled";
// Wired into the daemon's IPC + capture pipeline in later HEU-242 tasks; the
// accessors below are public-but-unused at the T5 checkpoint.
#[allow(dead_code)]
const PAUSED_KEY: &str = "capture_paused";
const WHISPER_MODEL_KEY: &str = "whisper_model";

/// Read all key=value lines into a BTreeMap. A missing file or read failure
/// returns an empty map; malformed lines (no `=`) are silently dropped.
fn read_all(base_dir: &Path) -> BTreeMap<String, String> {
    let Ok(contents) = std::fs::read_to_string(base_dir.join(SETTINGS_FILE)) else {
        return BTreeMap::new();
    };
    let mut map = BTreeMap::new();
    for line in contents.lines() {
        if let Some((key, value)) = line.split_once('=') {
            map.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    map
}

/// Persist the full key=value map atomically. Removes any stale `.tmp` from a
/// previous crash, creates a fresh `0600` temp file, then renames it over the
/// destination — so the settings file is never group- or world-readable, even
/// briefly. Best-effort: write failures are logged, not returned.
fn write_all(base_dir: &Path, map: &BTreeMap<String, String>) {
    let path = base_dir.join(SETTINGS_FILE);
    let tmp = base_dir.join(format!("{SETTINGS_FILE}.tmp"));

    // Remove any temp file a crashed run left behind so `create_new` below
    // always makes a fresh, owner-only file rather than reusing a stale mode.
    let _ = std::fs::remove_file(&tmp);

    let mut body = String::new();
    for (k, v) in map {
        body.push_str(k);
        body.push('=');
        body.push_str(v);
        body.push('\n');
    }

    // Create the temp file `0600` up front. Setting the mode at creation means
    // there is no window where the file is group/world-readable, and no
    // failure path that renames a wrongly-permissioned file into place.
    let write_result = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| f.write_all(body.as_bytes()));
    if let Err(e) = write_result {
        log::warn!("failed to write settings temp file: {e}");
        let _ = std::fs::remove_file(&tmp);
        return;
    }

    if let Err(e) = std::fs::rename(&tmp, &path) {
        log::warn!("failed to persist settings file: {e}");
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Read the persisted microphone setting. A missing file, an unreadable file,
/// a missing key, or an unrecognized value all return `false` (microphone off).
pub fn read_mic_setting(base_dir: &Path) -> bool {
    read_all(base_dir)
        .get(MIC_KEY)
        .map(|v| v == "true")
        .unwrap_or(false)
}

/// Persist the microphone setting. Best-effort: a write failure is logged, not
/// returned — the live toggle has already taken effect. Preserves other keys
/// in the settings file (e.g. `capture_paused`).
pub fn write_mic_setting(base_dir: &Path, on: bool) {
    let mut map = read_all(base_dir);
    map.insert(MIC_KEY.into(), on.to_string());
    write_all(base_dir, &map);
}

/// Read the persisted capture-paused setting. A missing file, an unreadable
/// file, a missing key, or an unrecognized value all return `false` (not paused).
pub fn read_capture_paused(base_dir: &Path) -> bool {
    read_all(base_dir)
        .get(PAUSED_KEY)
        .map(|v| v == "true")
        .unwrap_or(false)
}

/// Persist the capture-paused setting. Best-effort: a write failure is logged,
/// not returned — the live pause has already taken effect. Preserves other
/// keys in the settings file (e.g. `mic_enabled`).
pub fn write_capture_paused(base_dir: &Path, on: bool) {
    let mut map = read_all(base_dir);
    map.insert(PAUSED_KEY.into(), on.to_string());
    write_all(base_dir, &map);
}

/// Read the persisted whisper-model variant and validate it against the
/// allow-list in `chronicle-transcription`. Anything not in
/// `SUPPORTED_VARIANTS` — including a missing file, missing key, empty
/// string, or unrecognized value — returns `DEFAULT_VARIANT` ("base").
///
/// The returned `&'static str` is a slice from the allow-list itself,
/// so it is safe to interpolate into a model filename without further
/// sanitization.
pub fn read_whisper_model(base_dir: &Path) -> &'static str {
    let raw = read_all(base_dir)
        .get(WHISPER_MODEL_KEY)
        .cloned()
        .unwrap_or_default();
    parse_variant(&raw).unwrap_or(DEFAULT_VARIANT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    #[test]
    fn read_missing_file_is_false() {
        let dir = tempdir().unwrap();
        assert!(!read_mic_setting(dir.path()));
    }

    #[test]
    fn write_then_read_round_trips() {
        let dir = tempdir().unwrap();
        write_mic_setting(dir.path(), true);
        assert!(read_mic_setting(dir.path()));

        write_mic_setting(dir.path(), false);
        assert!(!read_mic_setting(dir.path()));
    }

    #[test]
    fn read_ignores_unknown_keys_and_garbage() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("settings"), "other=1\njunk\n").unwrap();
        assert!(!read_mic_setting(dir.path()));
    }

    #[test]
    fn written_file_is_owner_only() {
        let dir = tempdir().unwrap();
        write_mic_setting(dir.path(), true);
        let meta = std::fs::metadata(dir.path().join("settings")).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "settings file should be 0600, got {mode:o}");
    }

    #[test]
    fn write_replaces_stale_temp_file() {
        // A crashed run can leave a `settings.tmp` behind. write_mic_setting
        // must clear it and still produce a correct, owner-only settings file.
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("settings.tmp"), "stale garbage").unwrap();

        write_mic_setting(dir.path(), true);

        assert!(read_mic_setting(dir.path()));
        let meta = std::fs::metadata(dir.path().join("settings")).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "settings file should be 0600, got {mode:o}");
    }

    #[test]
    fn writing_capture_paused_preserves_mic_enabled() {
        let dir = tempdir().unwrap();
        write_mic_setting(dir.path(), true);
        write_capture_paused(dir.path(), true);
        assert!(read_mic_setting(dir.path()));
        assert!(read_capture_paused(dir.path()));
    }

    #[test]
    fn writing_mic_enabled_preserves_capture_paused() {
        let dir = tempdir().unwrap();
        write_capture_paused(dir.path(), true);
        write_mic_setting(dir.path(), true);
        assert!(read_capture_paused(dir.path()));
        assert!(read_mic_setting(dir.path()));
    }

    #[test]
    fn both_keys_round_trip_independently() {
        let dir = tempdir().unwrap();
        write_mic_setting(dir.path(), true);
        write_capture_paused(dir.path(), false);
        write_mic_setting(dir.path(), false);
        write_capture_paused(dir.path(), true);
        assert!(!read_mic_setting(dir.path()));
        assert!(read_capture_paused(dir.path()));
    }

    #[test]
    fn capture_paused_missing_returns_false() {
        let dir = tempdir().unwrap();
        write_mic_setting(dir.path(), true);
        assert!(!read_capture_paused(dir.path()));
    }

    #[test]
    fn settings_file_remains_0600_after_multi_key_write() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        write_mic_setting(dir.path(), true);
        write_capture_paused(dir.path(), true);
        let meta = std::fs::metadata(dir.path().join("settings")).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "settings file should be 0600, got {mode:o}");
    }

    #[test]
    fn write_multi_key_replaces_stale_temp_file() {
        // A crashed run can leave a `settings.tmp` behind. The multi-key writer
        // must clear it (just like the single-key writer used to) and still
        // produce a correct, owner-only settings file with both keys preserved.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("settings.tmp"), "stale garbage").unwrap();
        write_mic_setting(dir.path(), true);
        write_capture_paused(dir.path(), true);
        assert!(read_mic_setting(dir.path()));
        assert!(read_capture_paused(dir.path()));
        let meta = std::fs::metadata(dir.path().join("settings")).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "settings file should be 0600 after stale-tmp recovery, got {mode:o}"
        );
    }

    #[test]
    fn read_ignores_malformed_lines_and_unknown_keys() {
        // Lines with no `=` separator and unknown keys must be silently
        // ignored — readers fall back to the per-key default (false).
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("settings"),
            "this-line-has-no-equals\nunknown_key=true\nmic_enabled=true\n",
        )
        .unwrap();
        assert!(read_mic_setting(dir.path()), "known key still readable");
        assert!(
            !read_capture_paused(dir.path()),
            "missing key reads as false"
        );
    }

    #[test]
    fn read_whisper_model_missing_file_returns_default() {
        let dir = tempdir().unwrap();
        assert_eq!(read_whisper_model(dir.path()), "base");
    }

    #[test]
    fn read_whisper_model_round_trips_allow_listed_value() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("settings"), "whisper_model=small\n").unwrap();
        assert_eq!(read_whisper_model(dir.path()), "small");
    }

    #[test]
    fn read_whisper_model_empty_value_returns_default() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("settings"), "whisper_model=\n").unwrap();
        assert_eq!(read_whisper_model(dir.path()), "base");
    }

    #[test]
    fn read_whisper_model_unknown_value_returns_default() {
        // Settings file edited by hand with a typo — fall back to default
        // rather than carry an unknown variant through to the path layer.
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("settings"), "whisper_model=tiny\n").unwrap();
        assert_eq!(read_whisper_model(dir.path()), "base");
    }

    #[test]
    fn read_whisper_model_traversal_attempt_returns_default() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("settings"),
            "whisper_model=../../etc/passwd\n",
        )
        .unwrap();
        assert_eq!(read_whisper_model(dir.path()), "base");
    }

    #[test]
    fn read_whisper_model_preserves_other_keys() {
        // mic_enabled and capture_paused must still parse normally when
        // whisper_model coexists.
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("settings"),
            "mic_enabled=true\nwhisper_model=medium\ncapture_paused=false\n",
        )
        .unwrap();
        assert!(read_mic_setting(dir.path()));
        assert_eq!(read_whisper_model(dir.path()), "medium");
        assert!(!read_capture_paused(dir.path()));
    }
}
