/// Sanitize a user-supplied identifier (display_id, source) so it cannot
/// escape the intended directory. Replaces `/`, `\`, `..`, and null bytes
/// with `_`.
#[cfg(test)]
fn sanitize_id(input: &str) -> String {
    input.replace("..", "_").replace(['/', '\\', '\0'], "_")
}

#[cfg(test)]
fn date_parts(timestamp_millis: i64) -> (i32, u32, u32) {
    use chrono::{DateTime, Datelike, Utc};
    let dt = DateTime::<Utc>::from_timestamp_millis(timestamp_millis)
        .unwrap_or_else(|| DateTime::<Utc>::from_timestamp(0, 0).unwrap());
    (dt.year(), dt.month(), dt.day())
}

/// Build a non-canonical screenshot path. Only used in tests to verify
/// the date-partitioned structure without hitting the filesystem.
#[cfg(test)]
fn screenshot_path(
    base_dir: &std::path::Path,
    timestamp: i64,
    display_id: &str,
) -> std::path::PathBuf {
    let safe_id = sanitize_id(display_id);
    let (year, month, day) = date_parts(timestamp);
    base_dir
        .join("screenshots")
        .join(format!("{}/{:02}/{:02}", year, month, day))
        .join(format!("{}_{}.heif", timestamp, safe_id))
}

/// Build a non-canonical audio path. Only used in tests.
#[cfg(test)]
fn audio_path(base_dir: &std::path::Path, timestamp: i64, source: &str) -> std::path::PathBuf {
    let safe_source = sanitize_id(source);
    let (year, month, day) = date_parts(timestamp);
    base_dir
        .join("audio")
        .join(format!("{}/{:02}/{:02}", year, month, day))
        .join(format!("{}_{}.opus", timestamp, safe_source))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn screenshot_path_has_correct_structure() {
        let base = PathBuf::from("/data");
        // 2026-03-21 12:00:00.000 UTC = 1774094400000 ms
        let ts: i64 = 1774094400000;
        let path = screenshot_path(&base, ts, "display1");

        assert_eq!(
            path,
            PathBuf::from("/data/screenshots/2026/03/21/1774094400000_display1.heif")
        );
    }

    #[test]
    fn audio_path_has_correct_structure() {
        let base = PathBuf::from("/data");
        let ts: i64 = 1774094400000;
        let path = audio_path(&base, ts, "mic");

        assert_eq!(
            path,
            PathBuf::from("/data/audio/2026/03/21/1774094400000_mic.opus")
        );
    }

    #[test]
    fn sanitize_id_replaces_path_separators() {
        assert_eq!(sanitize_id("../etc/passwd"), "__etc_passwd");
        assert_eq!(sanitize_id("display/1"), "display_1");
        assert_eq!(sanitize_id("foo\\bar"), "foo_bar");
        assert_eq!(sanitize_id("ok\0bad"), "ok_bad");
    }

    #[test]
    fn screenshot_path_sanitizes_display_id() {
        let base = PathBuf::from("/data");
        let ts: i64 = 1774094400000;
        let path = screenshot_path(&base, ts, "../evil");
        // Should not escape the screenshots directory
        assert!(!path.to_string_lossy().contains(".."));
    }
}
