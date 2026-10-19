use std::path::{Path, PathBuf};

use rusqlite::{Connection, Row, params};

use crate::error::Result;
use crate::models::{AudioSegment, AudioSegmentMetadata, PendingTranscription};

fn row_to_audio_segment(row: &Row<'_>) -> rusqlite::Result<AudioSegment> {
    Ok(AudioSegment {
        id: row.get(0)?,
        start_timestamp: row.get(1)?,
        end_timestamp: row.get(2)?,
        source: row.get(3)?,
        audio_path: row.get(4)?,
        transcript: row.get(5)?,
        whisper_model: row.get(6)?,
        language: row.get(7)?,
        created_at: row.get(8)?,
    })
}

pub(crate) fn insert(conn: &Connection, meta: &AudioSegmentMetadata) -> Result<i64> {
    conn.execute(
        "INSERT INTO audio_segments (start_timestamp, end_timestamp, source, audio_path, transcript, whisper_model, language)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            meta.start_timestamp,
            meta.end_timestamp,
            meta.source,
            meta.audio_path,
            meta.transcript,
            meta.whisper_model,
            meta.language,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub(crate) fn get(conn: &Connection, id: i64) -> Result<AudioSegment> {
    let segment = conn.query_row(
        "SELECT id, start_timestamp, end_timestamp, source, audio_path, transcript, whisper_model, language, created_at
         FROM audio_segments WHERE id = ?1",
        params![id],
        row_to_audio_segment,
    )?;
    Ok(segment)
}

pub(crate) fn update_transcript(conn: &Connection, id: i64, transcript: &str) -> Result<()> {
    let rows_affected = conn.execute(
        "UPDATE audio_segments SET transcript = ?1 WHERE id = ?2",
        params![transcript, id],
    )?;
    if rows_affected == 0 {
        return Err(crate::error::StorageError::Other(format!(
            "not found: id {}",
            id
        )));
    }
    Ok(())
}

/// `transcript: None` records that transcription ran and found no speech — a
/// different state from an untranscribed row, which has no `whisper_model`
/// (CHR-38). Passing `None` also clears any previous transcript from
/// `audio_fts`, because the `audio_au` trigger reindexes on every update.
///
/// **This function does no normalization.** It will write `Some("")` — a
/// blank shape — exactly as given. Trimming, the blank-to-`None` collapse, and
/// the pairing that clears `language` alongside all live one level up in
/// [`crate::Storage::update_transcript_full`]. Go through the wrapper unless you
/// have a specific reason not to.
pub(crate) fn update_transcript_full(
    conn: &Connection,
    id: i64,
    audio_path: &Path,
    transcript: Option<&str>,
    whisper_model: &str,
    language: Option<&str>,
) -> Result<()> {
    let rows_affected = conn.execute(
        "UPDATE audio_segments SET transcript = ?1, whisper_model = ?2, language = ?3
         WHERE id = ?4 AND audio_path = ?5",
        params![
            transcript,
            whisper_model,
            language,
            id,
            audio_path.to_string_lossy()
        ],
    )?;
    if rows_affected == 0 {
        return Err(crate::error::StorageError::Other(format!(
            "not found: id {}",
            id
        )));
    }
    Ok(())
}

/// A row still owed a transcription attempt: never stamped, no text, and
/// under the cap. `?2` is the cap in every query that uses it.
const PENDING: &str = "whisper_model IS NULL AND transcript IS NULL AND transcribe_attempts < ?2";

pub(crate) fn record_transcription_failure(
    conn: &Connection,
    id: i64,
    audio_path: &Path,
) -> Result<()> {
    conn.execute(
        "UPDATE audio_segments SET transcribe_attempts = transcribe_attempts + 1
         WHERE id = ?1 AND audio_path = ?2",
        params![id, audio_path.to_string_lossy()],
    )?;
    Ok(())
}

pub(crate) fn pending_transcriptions(
    conn: &Connection,
    after_id: i64,
    max_attempts: u32,
    limit: u32,
) -> Result<Vec<PendingTranscription>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT id, audio_path FROM audio_segments
         WHERE {PENDING} AND id > ?1 ORDER BY id LIMIT ?3"
    ))?;
    let rows = stmt.query_map(params![after_id, max_attempts, limit], |row| {
        Ok(PendingTranscription {
            id: row.get(0)?,
            audio_path: PathBuf::from(row.get::<_, String>(1)?),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub(crate) fn needs_transcription(
    conn: &Connection,
    id: i64,
    audio_path: &Path,
    max_attempts: u32,
) -> Result<bool> {
    Ok(conn.query_row(
        &format!(
            "SELECT EXISTS(SELECT 1 FROM audio_segments
             WHERE id = ?1 AND audio_path = ?3 AND {PENDING})"
        ),
        params![id, max_attempts, audio_path.to_string_lossy()],
        |row| row.get(0),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema;

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::setup_connection(&conn).unwrap();
        schema::migrate(&conn).unwrap();
        conn
    }

    fn sample_meta() -> AudioSegmentMetadata {
        AudioSegmentMetadata {
            start_timestamp: 1_700_000_000_000,
            end_timestamp: 1_700_000_030_000,
            source: "mic".into(),
            audio_path: "/data/audio/segment.opus".into(),
            transcript: Some("meeting notes about project timeline".into()),
            whisper_model: Some("base".into()),
            language: Some("en".into()),
        }
    }

    #[test]
    fn insert_and_get_audio_segment() {
        let conn = setup_db();
        let meta = sample_meta();
        let id = insert(&conn, &meta).unwrap();
        assert!(id > 0);

        let segment = get(&conn, id).unwrap();
        assert_eq!(segment.id, id);
        assert_eq!(segment.start_timestamp, meta.start_timestamp);
        assert_eq!(segment.end_timestamp, meta.end_timestamp);
        assert_eq!(segment.source, meta.source);
        assert_eq!(segment.audio_path, meta.audio_path);
        assert_eq!(segment.transcript, meta.transcript);
        assert_eq!(segment.whisper_model, meta.whisper_model);
        assert_eq!(segment.language, meta.language);
        assert!(segment.created_at > 0);
    }

    #[test]
    fn insert_rejects_invalid_source() {
        let conn = setup_db();
        let mut meta = sample_meta();
        meta.source = "bluetooth".into();

        let result = insert(&conn, &meta);
        assert!(result.is_err());
    }

    #[test]
    fn update_transcript_updates_existing_row() {
        let conn = setup_db();
        let meta = sample_meta();
        let id = insert(&conn, &meta).unwrap();

        update_transcript(&conn, id, "Updated transcript content").unwrap();

        let segment = get(&conn, id).unwrap();
        assert_eq!(
            segment.transcript.as_deref(),
            Some("Updated transcript content")
        );
    }

    #[test]
    fn get_nonexistent_id_returns_error() {
        let conn = setup_db();
        let result = get(&conn, 9999);
        assert!(result.is_err());
    }

    #[test]
    fn update_transcript_nonexistent_id_returns_error() {
        let conn = setup_db();
        let result = update_transcript(&conn, 9999, "some text");
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("not found: id 9999"));
    }

    #[test]
    fn insert_triggers_fts_index() {
        let conn = setup_db();
        let meta = sample_meta(); // transcript = "meeting notes about project timeline"
        insert(&conn, &meta).unwrap();

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM audio_fts WHERE audio_fts MATCH 'timeline'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn update_transcript_triggers_fts_reindex() {
        let conn = setup_db();
        let meta = sample_meta(); // transcript = "meeting notes about project timeline"
        let id = insert(&conn, &meta).unwrap();

        update_transcript(&conn, id, "quarterly budget review spreadsheet").unwrap();

        // Old term should be gone
        let old_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM audio_fts WHERE audio_fts MATCH 'timeline'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(old_count, 0);

        // New term should be present
        let new_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM audio_fts WHERE audio_fts MATCH 'budget'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(new_count, 1);
    }

    #[test]
    fn update_transcript_full_sets_all_columns_and_reindexes() {
        let conn = setup_db();
        let id = insert(&conn, &sample_meta()).unwrap();

        update_transcript_full(
            &conn,
            id,
            Path::new("/data/audio/segment.opus"),
            Some("quarterly budget review"),
            "base",
            Some("en"),
        )
        .unwrap();

        let seg = get(&conn, id).unwrap();
        assert_eq!(seg.transcript.as_deref(), Some("quarterly budget review"));
        assert_eq!(seg.whisper_model.as_deref(), Some("base"));
        assert_eq!(seg.language.as_deref(), Some("en"));

        // FTS reindex fired (audio_au trigger): old term gone, new term present.
        let old: i64 = conn
            .query_row(
                "SELECT count(*) FROM audio_fts WHERE audio_fts MATCH 'timeline'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(old, 0);
        let new: i64 = conn
            .query_row(
                "SELECT count(*) FROM audio_fts WHERE audio_fts MATCH 'budget'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(new, 1);
    }

    #[test]
    fn update_transcript_full_nonexistent_id_errors() {
        let conn = setup_db();
        assert!(
            update_transcript_full(&conn, 999, Path::new("/any.opus"), Some("x"), "base", None)
                .is_err()
        );
    }

    #[test]
    fn update_transcript_full_records_attempt_with_null_transcript() {
        let conn = setup_db();
        // sample_meta() arrives WITH a transcript, so this also proves the
        // update clears text that was already there.
        let id = insert(&conn, &sample_meta()).unwrap();

        update_transcript_full(
            &conn,
            id,
            Path::new("/data/audio/segment.opus"),
            None,
            "small",
            Some("en"),
        )
        .unwrap();

        let seg = get(&conn, id).unwrap();
        assert!(
            seg.transcript.is_none(),
            "a silent segment stores no transcript text, and any prior text is cleared"
        );
        assert_eq!(
            seg.whisper_model.as_deref(),
            Some("small"),
            "the attempt is recorded, which is what distinguishes it from untranscribed \
             (deliberately not sample_meta's \"base\" — that would pass without the write)"
        );
        // NULL transcript + a language IS the shape migration 003 exists to
        // remove — asserted here deliberately, because THIS function does no
        // normalization and must be shown not to. The pairing that prevents it
        // lives in `Storage::update_transcript_full`; see its docs. Do not read
        // this assertion as the sanctioned row shape.
        assert_eq!(seg.language.as_deref(), Some("en"));
    }

    #[test]
    fn update_transcript_full_null_transcript_clears_fts() {
        let conn = setup_db();
        let id = insert(&conn, &sample_meta()).unwrap();
        // sample_meta's transcript is "meeting notes about project timeline".
        let indexed: i64 = conn
            .query_row(
                "SELECT count(*) FROM audio_fts WHERE audio_fts MATCH 'timeline'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            indexed, 1,
            "precondition: the row is indexed before we clear"
        );

        update_transcript_full(
            &conn,
            id,
            Path::new("/data/audio/segment.opus"),
            None,
            "base",
            None,
        )
        .unwrap();

        // The audio_au trigger issues the FTS5 'delete' with the OLD value,
        // so nulling the transcript must remove its terms from the index.
        // This is what makes migration 003 clear search pollution as a side
        // effect rather than needing to touch audio_fts directly.
        let after: i64 = conn
            .query_row(
                "SELECT count(*) FROM audio_fts WHERE audio_fts MATCH 'timeline'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(after, 0, "clearing the transcript must clear the FTS index");
    }

    fn insert_shaped(
        conn: &Connection,
        path: &str,
        transcript: Option<&str>,
        whisper_model: Option<&str>,
    ) -> i64 {
        insert(
            conn,
            &AudioSegmentMetadata {
                audio_path: path.into(),
                transcript: transcript.map(Into::into),
                whisper_model: whisper_model.map(Into::into),
                language: None,
                ..sample_meta()
            },
        )
        .unwrap()
    }

    fn attempts(conn: &Connection, id: i64) -> i64 {
        conn.query_row(
            "SELECT transcribe_attempts FROM audio_segments WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn record_transcription_failure_counts_up_and_ignores_missing_rows() {
        let conn = setup_db();
        let id = insert_shaped(&conn, "/a.opus", None, None);
        record_transcription_failure(&conn, id, Path::new("/a.opus")).unwrap();
        record_transcription_failure(&conn, id, Path::new("/a.opus")).unwrap();
        assert_eq!(attempts(&conn, id), 2);
        record_transcription_failure(&conn, id + 1_000, Path::new("/a.opus")).unwrap();
    }

    /// Replace the row at `id` with a different recording at `new_path`, as
    /// SQLite does when it reuses a deleted row's id.
    fn replace_row(conn: &Connection, id: i64, new_path: &str) {
        conn.execute("DELETE FROM audio_segments WHERE id = ?1", params![id])
            .unwrap();
        conn.execute(
            "INSERT INTO audio_segments (id, start_timestamp, end_timestamp, source, audio_path)
             VALUES (?1, 1, 2, 'mic', ?2)",
            params![id, new_path],
        )
        .unwrap();
    }

    #[test]
    fn row_methods_ignore_a_different_recording_at_the_same_id() {
        let conn = setup_db();
        let id = insert_shaped(&conn, "/old.opus", None, None);
        replace_row(&conn, id, "/new.opus");
        let old = Path::new("/old.opus");

        assert!(
            update_transcript_full(&conn, id, old, Some("stale"), "base", None).is_err(),
            "the old recording's row is gone"
        );
        record_transcription_failure(&conn, id, old).unwrap();
        assert!(!needs_transcription(&conn, id, old, 3).unwrap());

        let row = get(&conn, id).unwrap();
        assert!(row.transcript.is_none() && row.whisper_model.is_none());
        assert_eq!(attempts(&conn, id), 0, "the new recording is untouched");
        assert!(needs_transcription(&conn, id, Path::new("/new.opus"), 3).unwrap());
    }

    #[test]
    fn pending_transcriptions_selects_only_pending_rows() {
        let conn = setup_db();
        let first = insert_shaped(&conn, "/first.opus", None, None);
        let silent = insert_shaped(&conn, "/silent.opus", None, Some("base"));
        let speech = insert_shaped(&conn, "/speech.opus", Some("hello"), Some("base"));
        let unstamped_text = insert_shaped(&conn, "/text.opus", Some("hello"), None);
        let capped = insert_shaped(&conn, "/capped.opus", None, None);
        for _ in 0..3 {
            record_transcription_failure(&conn, capped, Path::new("/capped.opus")).unwrap();
        }
        let last = insert_shaped(&conn, "/last.opus", None, None);

        let all = pending_transcriptions(&conn, 0, 3, 10).unwrap();
        assert_eq!(
            all,
            vec![
                PendingTranscription {
                    id: first,
                    audio_path: "/first.opus".into(),
                },
                PendingTranscription {
                    id: last,
                    audio_path: "/last.opus".into(),
                },
            ]
        );
        assert_eq!(
            pending_transcriptions(&conn, first, 3, 10).unwrap(),
            vec![PendingTranscription {
                id: last,
                audio_path: "/last.opus".into(),
            }],
            "the cursor is exclusive"
        );
        assert_eq!(pending_transcriptions(&conn, 0, 3, 1).unwrap().len(), 1);
        assert_eq!(
            pending_transcriptions(&conn, 0, 4, 10).unwrap().len(),
            3,
            "the cap is the caller's"
        );

        for (id, path, expected) in [
            (first, "/first.opus", true),
            (silent, "/silent.opus", false),
            (speech, "/speech.opus", false),
            (unstamped_text, "/text.opus", false),
            (capped, "/capped.opus", false),
            (last, "/last.opus", true),
            (last + 1_000, "/last.opus", false),
        ] {
            assert_eq!(
                needs_transcription(&conn, id, Path::new(path), 3).unwrap(),
                expected,
                "row {id}"
            );
        }
    }
}
