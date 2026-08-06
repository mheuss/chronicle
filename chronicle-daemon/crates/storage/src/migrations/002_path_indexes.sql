-- Chronicle storage schema v2
-- Covering indexes for the orphan sweep's path lookup (HEU-547).
--
-- The sweep reads every tracked path with `SELECT <path_col> FROM <table>`.
-- With these indexes SQLite satisfies that from the index alone (a covering
-- index scan) instead of scanning the full table.

CREATE INDEX IF NOT EXISTS idx_screenshots_image_path
    ON screenshots(image_path);

CREATE INDEX IF NOT EXISTS idx_audio_segments_audio_path
    ON audio_segments(audio_path);
