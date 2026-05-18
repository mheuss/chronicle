//! Daemon settings persisted across restarts.
//!
//! A minimal `key=value` text file at `<base_dir>/settings`. One value today
//! (`mic_enabled`). Deliberately not JSON and not a settings subsystem — see
//! the HEU-330 design's Data Architecture and anti-scope.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

const SETTINGS_FILE: &str = "settings";
const MIC_KEY: &str = "mic_enabled";

/// Read the persisted microphone setting. A missing file, an unreadable file,
/// or an unrecognized value all return `false` (microphone off).
// HEU-330: called by main() once the control plane is wired (Task 9).
#[allow(dead_code)]
pub fn read_mic_setting(base_dir: &Path) -> bool {
    let Ok(contents) = std::fs::read_to_string(base_dir.join(SETTINGS_FILE)) else {
        return false;
    };
    for line in contents.lines() {
        if let Some((key, value)) = line.split_once('=')
            && key.trim() == MIC_KEY
        {
            return value.trim() == "true";
        }
    }
    false
}

/// Persist the microphone setting. Best-effort: a write failure is logged, not
/// returned — the live toggle has already taken effect. Written atomically
/// (temp file + rename) at `0600`.
// HEU-330: called by main() once the control plane is wired (Task 9).
#[allow(dead_code)]
pub fn write_mic_setting(base_dir: &Path, on: bool) {
    let path = base_dir.join(SETTINGS_FILE);
    let tmp = base_dir.join(format!("{SETTINGS_FILE}.tmp"));
    if let Err(e) = std::fs::write(&tmp, format!("{MIC_KEY}={on}\n")) {
        log::warn!("failed to write settings temp file: {e}");
        return;
    }
    if let Err(e) = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)) {
        log::warn!("failed to set settings file permissions: {e}");
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        log::warn!("failed to persist settings file: {e}");
    }
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
}
