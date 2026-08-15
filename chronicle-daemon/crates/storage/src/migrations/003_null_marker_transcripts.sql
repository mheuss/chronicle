-- HEU-620: clear whisper's own markers from persisted transcripts.
--
-- whisper.cpp emits [BLANK_AUDIO] (and [Motor], [_BEG_], ...) as ordinary
-- segment text. Those passed the empty-text guard, were stored as transcripts,
-- and were tokenized into audio_fts as searchable words -- on the database
-- this migration was written against, `audio` matched 174 rows and `blank`
-- matched 171, almost all of them silence.
--
-- The predicate follows `is_whisper_marker` in the transcription crate: the
-- ENTIRE trimmed transcript must be one bracketed ASCII token with no internal
-- whitespace. Note SQLite's LIKE treats '[' as an ordinary character, so '[%]'
-- means "starts with [ and ends with ]".
--
-- It is not an exact mirror, and the difference is deliberate. The Rust rule
-- rejects any `char::is_whitespace`; SQL has no such predicate, so this names
-- the four whitespace characters whisper actually emits (space, tab, CR, LF).
-- Vertical tab and form feed are therefore treated as ordinary characters
-- here. A transcript that is one bracketed ASCII token whose ONLY internal
-- whitespace is a form feed would be cleared here and spared by the Rust
-- rule. No such row exists in the live database and whisper has no path to
-- producing one, so the divergence is recorded rather than closed.
--
-- Each clause rules out a different way of destroying real speech:
--
--   length >= 3        '[]' is punctuation, not a marker.
--   no space/tab/nl    A bracketed aside in prose survives. This is what
--                      spares the live row (id 1501) that begins
--                      "[ Background noise ] Kind of a family for some
--                      time. ..." -- markers wrapped around genuine
--                      speech. That row is 115 characters and repeats
--                      "[ Background noise ]" four times in all; the
--                      speech between them is what must not be lost.
--   char len = byte    THE ASCII TEST, and the one that is easy to omit.
--   len                Chinese, Japanese and Thai have no inter-word spaces,
--                      so a bracketed phrase in those scripts is a single
--                      whitespace-free token and the clause above cannot
--                      tell it from a marker. In UTF-8, character count
--                      equals byte count only when every character is ASCII,
--                      so this clause keeps '[这是一个完整的句子]' while
--                      still dropping '[BLANK_AUDIO]'. Without it this
--                      migration irreversibly deletes transcript text, and
--                      only for non-English users.
--
-- whisper_model is deliberately left in place. It is what marks the row as
-- "transcription ran and found no speech" rather than "never transcribed".
--
-- language IS cleared, alongside the transcript. The pipeline stopped writing
-- a language on no-speech rows because whisper's detection there is made on
-- noise (live silent rows carry 'nn', Nynorsk) and search.rs reads the column
-- back out to callers. Leaving it populated here would make a migrated legacy
-- row disagree with a freshly written one about the same state, which is
-- exactly the ambiguity this ticket exists to remove.
--
-- The audio_au trigger fires per row and reindexes audio_fts via the FTS5
-- 'delete' command with the OLD value. Never touch audio_fts directly here --
-- it is an external-content table and will corrupt if the two disagree.
UPDATE audio_segments
SET transcript = NULL,
    language   = NULL
WHERE transcript IS NOT NULL
  AND length(trim(transcript)) >= 3
  AND trim(transcript) LIKE '[%]'
  AND instr(trim(transcript), ' ') = 0
  AND instr(trim(transcript), char(9)) = 0
  AND instr(trim(transcript), char(10)) = 0
  AND instr(trim(transcript), char(13)) = 0
  AND length(trim(transcript)) = length(CAST(trim(transcript) AS BLOB));
