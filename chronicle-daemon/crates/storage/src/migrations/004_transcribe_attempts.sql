-- Transcription attempts per audio row (CHR-15). Existing rows start at 0.
ALTER TABLE audio_segments ADD COLUMN transcribe_attempts INTEGER NOT NULL DEFAULT 0;
