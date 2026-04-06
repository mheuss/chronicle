use rusqlite::{Connection, params};

use crate::error::{Result, StorageError};
use crate::models::{AudioSegment, Screenshot, SearchFilter, SearchResult, SearchSource};

pub(crate) fn search(
    conn: &Connection,
    query: &str,
    filter: &SearchFilter,
    limit: usize,
    offset: usize,
) -> Result<Vec<SearchResult>> {
    let mut results: Vec<SearchResult> = Vec::new();

    // When filter is All, we fetch up to (limit + offset) rows from each source,
    // merge and sort in memory, then apply limit/offset. A UNION ALL query would
    // let SQLite handle this, but complicates the row-mapping logic. For typical
    // usage (small limit, small offset), memory overhead is negligible.
    let sub_limit = (limit + offset) as i64;

    if *filter == SearchFilter::All || *filter == SearchFilter::ScreenOnly {
        let mut stmt = conn.prepare(
            "SELECT s.id, s.timestamp, s.display_id, s.app_name, s.app_bundle_id,
                    s.window_title, s.image_path, s.ocr_text, s.phash, s.resolution,
                    s.created_at,
                    snippet(screenshots_fts, -1, '<b>', '</b>', '...', 32) AS snip,
                    rank
             FROM screenshots_fts
             JOIN screenshots s ON s.id = screenshots_fts.rowid
             WHERE screenshots_fts MATCH ?1
             ORDER BY rank
             LIMIT ?2",
        )?;

        let rows = stmt
            .query_map(params![query, sub_limit], |row| {
                let screenshot = Screenshot {
                    id: row.get(0)?,
                    timestamp: row.get(1)?,
                    display_id: row.get(2)?,
                    app_name: row.get(3)?,
                    app_bundle_id: row.get(4)?,
                    window_title: row.get(5)?,
                    image_path: row.get(6)?,
                    ocr_text: row.get(7)?,
                    phash: row.get(8)?,
                    resolution: row.get(9)?,
                    created_at: row.get(10)?,
                };
                let snippet: String = row.get(11)?;
                let rank: f64 = row.get(12)?;
                Ok(SearchResult {
                    source: SearchSource::Screen(screenshot),
                    snippet,
                    rank,
                })
            })
            .map_err(|e| map_fts5_error(e, query))?;

        for row in rows {
            results.push(row?);
        }
    }

    if *filter == SearchFilter::All || *filter == SearchFilter::AudioOnly {
        let mut stmt = conn.prepare(
            "SELECT a.id, a.start_timestamp, a.end_timestamp, a.source, a.audio_path,
                    a.transcript, a.whisper_model, a.language, a.created_at,
                    snippet(audio_fts, -1, '<b>', '</b>', '...', 32) AS snip,
                    rank
             FROM audio_fts
             JOIN audio_segments a ON a.id = audio_fts.rowid
             WHERE audio_fts MATCH ?1
             ORDER BY rank
             LIMIT ?2",
        )?;

        let rows = stmt
            .query_map(params![query, sub_limit], |row| {
                let segment = AudioSegment {
                    id: row.get(0)?,
                    start_timestamp: row.get(1)?,
                    end_timestamp: row.get(2)?,
                    source: row.get(3)?,
                    audio_path: row.get(4)?,
                    transcript: row.get(5)?,
                    whisper_model: row.get(6)?,
                    language: row.get(7)?,
                    created_at: row.get(8)?,
                };
                let snippet: String = row.get(9)?;
                let rank: f64 = row.get(10)?;
                Ok(SearchResult {
                    source: SearchSource::Audio(segment),
                    snippet,
                    rank,
                })
            })
            .map_err(|e| map_fts5_error(e, query))?;

        for row in rows {
            results.push(row?);
        }
    }

    // Sort combined results by rank (lower is better in FTS5)
    results.sort_by(|a, b| {
        a.rank
            .partial_cmp(&b.rank)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Apply limit and offset on the combined sorted results
    let results = results.into_iter().skip(offset).take(limit).collect();

    Ok(results)
}

/// Map FTS5 syntax errors to a more descriptive `StorageError`.
fn map_fts5_error(err: rusqlite::Error, query: &str) -> StorageError {
    let msg = err.to_string();
    if msg.contains("fts5: syntax error") {
        StorageError::Other(format!("invalid search query: {}", query))
    } else {
        StorageError::Database(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AudioSegmentMetadata, ScreenshotMetadata};
    use crate::schema;
    use crate::{audio, screenshots};

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::setup_connection(&conn).unwrap();
        schema::migrate(&conn).unwrap();
        conn
    }

    fn insert_test_screenshot(conn: &Connection) -> i64 {
        let meta = ScreenshotMetadata {
            timestamp: 1_700_000_000_000,
            display_id: "display1".into(),
            app_name: Some("Terminal".into()),
            app_bundle_id: Some("com.apple.Terminal".into()),
            window_title: Some("kubectl".into()),
            image_path: "/data/screenshots/shot.heif".into(),
            ocr_text: Some("deployment pipeline kubernetes cluster".into()),
            phash: None,
            resolution: Some("2560x1440".into()),
        };
        screenshots::insert(conn, &meta).unwrap()
    }

    fn insert_test_audio(conn: &Connection) -> i64 {
        let meta = AudioSegmentMetadata {
            start_timestamp: 1_700_000_000_000,
            end_timestamp: 1_700_000_030_000,
            source: "mic".into(),
            audio_path: "/data/audio/segment.opus".into(),
            transcript: Some("discussing the kubernetes deployment strategy".into()),
            whisper_model: Some("base".into()),
            language: Some("en".into()),
        };
        audio::insert(conn, &meta).unwrap()
    }

    #[test]
    fn search_finds_screenshot_by_ocr_text() {
        let conn = setup_db();
        insert_test_screenshot(&conn);

        let results = search(&conn, "pipeline", &SearchFilter::All, 10, 0).unwrap();
        assert_eq!(results.len(), 1);
        assert!(matches!(results[0].source, SearchSource::Screen(_)));
    }

    #[test]
    fn search_finds_audio_by_transcript() {
        let conn = setup_db();
        insert_test_audio(&conn);

        let results = search(&conn, "strategy", &SearchFilter::All, 10, 0).unwrap();
        assert_eq!(results.len(), 1);
        assert!(matches!(results[0].source, SearchSource::Audio(_)));
    }

    #[test]
    fn search_finds_both_with_shared_term() {
        let conn = setup_db();
        insert_test_screenshot(&conn);
        insert_test_audio(&conn);

        let results = search(&conn, "kubernetes", &SearchFilter::All, 10, 0).unwrap();
        assert_eq!(results.len(), 2);

        let has_screen = results
            .iter()
            .any(|r| matches!(r.source, SearchSource::Screen(_)));
        let has_audio = results
            .iter()
            .any(|r| matches!(r.source, SearchSource::Audio(_)));
        assert!(has_screen);
        assert!(has_audio);
    }

    #[test]
    fn search_screen_only_filter() {
        let conn = setup_db();
        insert_test_screenshot(&conn);
        insert_test_audio(&conn);

        let results = search(&conn, "kubernetes", &SearchFilter::ScreenOnly, 10, 0).unwrap();
        assert_eq!(results.len(), 1);
        assert!(matches!(results[0].source, SearchSource::Screen(_)));
    }

    #[test]
    fn search_audio_only_filter() {
        let conn = setup_db();
        insert_test_screenshot(&conn);
        insert_test_audio(&conn);

        let results = search(&conn, "kubernetes", &SearchFilter::AudioOnly, 10, 0).unwrap();
        assert_eq!(results.len(), 1);
        assert!(matches!(results[0].source, SearchSource::Audio(_)));
    }

    #[test]
    fn search_respects_limit_and_offset() {
        let conn = setup_db();
        insert_test_screenshot(&conn);
        insert_test_audio(&conn);

        // Both match "kubernetes", limit to 1
        let results = search(&conn, "kubernetes", &SearchFilter::All, 1, 0).unwrap();
        assert_eq!(results.len(), 1);

        // Offset past first result
        let results = search(&conn, "kubernetes", &SearchFilter::All, 10, 1).unwrap();
        assert_eq!(results.len(), 1);

        // Offset past all results
        let results = search(&conn, "kubernetes", &SearchFilter::All, 10, 2).unwrap();
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn search_returns_empty_for_no_match() {
        let conn = setup_db();
        insert_test_screenshot(&conn);
        insert_test_audio(&conn);

        let results = search(&conn, "nonexistentterm", &SearchFilter::All, 10, 0).unwrap();
        assert_eq!(results.len(), 0);
    }
}
