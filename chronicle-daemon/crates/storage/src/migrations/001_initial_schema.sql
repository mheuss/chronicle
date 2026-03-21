-- Chronicle storage schema v1
-- Tables, FTS5 indexes, sync triggers, indexes, and default config.

-- Main tables

CREATE TABLE IF NOT EXISTS screenshots (
    id            INTEGER PRIMARY KEY,
    timestamp     INTEGER NOT NULL,
    display_id    TEXT    NOT NULL,
    app_name      TEXT,
    app_bundle_id TEXT,
    window_title  TEXT,
    image_path    TEXT    NOT NULL,
    ocr_text      TEXT,
    phash         BLOB,
    resolution    TEXT,
    created_at    INTEGER NOT NULL DEFAULT (unixepoch('subsec') * 1000)
);

CREATE TABLE IF NOT EXISTS audio_segments (
    id              INTEGER PRIMARY KEY,
    start_timestamp INTEGER NOT NULL,
    end_timestamp   INTEGER NOT NULL,
    source          TEXT    NOT NULL CHECK (source IN ('mic', 'system')),
    audio_path      TEXT    NOT NULL,
    transcript      TEXT,
    whisper_model   TEXT,
    language        TEXT,
    created_at      INTEGER NOT NULL DEFAULT (unixepoch('subsec') * 1000)
);

CREATE TABLE IF NOT EXISTS config (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- FTS5 virtual tables (external content)

CREATE VIRTUAL TABLE IF NOT EXISTS screenshots_fts USING fts5(
    ocr_text, app_name, window_title,
    content=screenshots, content_rowid=id
);

CREATE VIRTUAL TABLE IF NOT EXISTS audio_fts USING fts5(
    transcript,
    content=audio_segments, content_rowid=id
);

-- FTS sync triggers: screenshots

CREATE TRIGGER IF NOT EXISTS screenshots_ai AFTER INSERT ON screenshots BEGIN
    INSERT INTO screenshots_fts(rowid, ocr_text, app_name, window_title)
    VALUES (new.id, new.ocr_text, new.app_name, new.window_title);
END;

CREATE TRIGGER IF NOT EXISTS screenshots_ad AFTER DELETE ON screenshots BEGIN
    INSERT INTO screenshots_fts(screenshots_fts, rowid, ocr_text, app_name, window_title)
    VALUES ('delete', old.id, old.ocr_text, old.app_name, old.window_title);
END;

CREATE TRIGGER IF NOT EXISTS screenshots_au AFTER UPDATE ON screenshots BEGIN
    INSERT INTO screenshots_fts(screenshots_fts, rowid, ocr_text, app_name, window_title)
    VALUES ('delete', old.id, old.ocr_text, old.app_name, old.window_title);
    INSERT INTO screenshots_fts(rowid, ocr_text, app_name, window_title)
    VALUES (new.id, new.ocr_text, new.app_name, new.window_title);
END;

-- FTS sync triggers: audio

CREATE TRIGGER IF NOT EXISTS audio_ai AFTER INSERT ON audio_segments BEGIN
    INSERT INTO audio_fts(rowid, transcript) VALUES (new.id, new.transcript);
END;

CREATE TRIGGER IF NOT EXISTS audio_ad AFTER DELETE ON audio_segments BEGIN
    INSERT INTO audio_fts(audio_fts, rowid, transcript) VALUES ('delete', old.id, old.transcript);
END;

CREATE TRIGGER IF NOT EXISTS audio_au AFTER UPDATE ON audio_segments BEGIN
    INSERT INTO audio_fts(audio_fts, rowid, transcript) VALUES ('delete', old.id, old.transcript);
    INSERT INTO audio_fts(rowid, transcript) VALUES (new.id, new.transcript);
END;

-- Indexes

CREATE INDEX IF NOT EXISTS idx_screenshots_timestamp ON screenshots(timestamp);
CREATE INDEX IF NOT EXISTS idx_screenshots_app ON screenshots(app_bundle_id, timestamp);
CREATE INDEX IF NOT EXISTS idx_audio_timestamp ON audio_segments(start_timestamp);
CREATE INDEX IF NOT EXISTS idx_audio_source ON audio_segments(source, start_timestamp);

-- Default configuration

INSERT OR IGNORE INTO config (key, value) VALUES ('retention_days', '30');
INSERT OR IGNORE INTO config (key, value) VALUES ('capture_interval_ms', '2000');
