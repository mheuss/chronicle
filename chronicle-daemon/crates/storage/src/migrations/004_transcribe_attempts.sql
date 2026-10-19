-- Failed transcription attempts per audio row (CHR-15). A row with
-- whisper_model NULL, transcript NULL and fewer than the daemon's cap is
-- pending; at or above the cap it has been given up on.
ALTER TABLE audio_segments ADD COLUMN transcribe_attempts INTEGER NOT NULL DEFAULT 0;
