use rusqlite::{params, Connection, Row};

use crate::error::Result;
use crate::models::{AudioSegment, AudioSegmentMetadata};

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
    conn.execute(
        "UPDATE audio_segments SET transcript = ?1 WHERE id = ?2",
        params![transcript, id],
    )?;
    Ok(())
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
        assert_eq!(segment.transcript.as_deref(), Some("Updated transcript content"));
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
}
