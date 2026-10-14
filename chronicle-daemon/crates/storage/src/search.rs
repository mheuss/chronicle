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
    let mut screen: Vec<SearchResult> = Vec::new();
    let mut audio: Vec<SearchResult> = Vec::new();

    // Each source is fetched in its own rank order, up to (limit + offset)
    // rows, then interleaved in memory before limit/offset are applied.
    let sub_limit = (limit + offset) as i64;

    // FTS5 parses the MATCH argument as query syntax, so raw user input with
    // punctuation (parens, quotes, `*`, ...) triggers a syntax error that would
    // otherwise surface to the user as a misleading "No matches". Convert the
    // input into quoted prefix terms via `sanitize_fts5_query` so arbitrary
    // input is safe. A query with no alphanumeric characters has no search
    // terms, so return empty rather than building a degenerate MATCH
    // expression. See CHR-68 / CHR-63.
    if !query.chars().any(char::is_alphanumeric) {
        return Ok(Vec::new());
    }
    let match_query = sanitize_fts5_query(query);

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
            .query_map(params![match_query, sub_limit], |row| {
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
            .map_err(|e| map_fts5_error(e, query.chars().count()))?;

        for row in rows {
            screen.push(row?);
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
            .query_map(params![match_query, sub_limit], |row| {
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
            .map_err(|e| map_fts5_error(e, query.chars().count()))?;

        for row in rows {
            audio.push(row?);
        }
    }

    let results = interleave(screen, audio)
        .into_iter()
        .skip(offset)
        .take(limit)
        .collect();

    Ok(results)
}

/// Alternate screen and audio hits, screen first, each already in its own
/// rank order. bm25 scores from the two FTS tables are not comparable, so
/// they are never sorted against each other.
fn interleave(screen: Vec<SearchResult>, audio: Vec<SearchResult>) -> Vec<SearchResult> {
    let mut out = Vec::with_capacity(screen.len() + audio.len());
    let mut s = screen.into_iter();
    let mut a = audio.into_iter();
    loop {
        match (s.next(), a.next()) {
            (None, None) => break,
            (x, y) => out.extend(x.into_iter().chain(y)),
        }
    }
    out
}

/// Convert raw user input into a safe FTS5 MATCH expression.
///
/// Each whitespace-delimited token is wrapped as a quoted literal phrase with a
/// trailing prefix wildcard (`tok` -> `"tok"*`). Quoting makes FTS5-significant
/// punctuation literal so arbitrary input cannot produce a syntax error;
/// space-joining preserves the default AND semantics across tokens; and the
/// wildcard enables prefix matching. Internal double quotes are escaped by
/// doubling, per FTS5 string rules. Non-alphanumeric tokens survive as quoted
/// literals (e.g. `()` -> `"()"*`); `search()` short-circuits input with no
/// alphanumeric characters before calling this, since FTS5 rejects an empty
/// MATCH expression. Punctuation *inside* a token (e.g. `foo.bar` -> `"foo.bar"*`)
/// tokenizes to an adjacent-phrase prefix — it matches `foo` next to `bar`, not
/// the two as independent AND terms.
fn sanitize_fts5_query(query: &str) -> String {
    query
        .split_whitespace()
        .map(|token| format!("\"{}\"*", token.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Map an FTS5 `MATCH` query failure to a generic, PII-safe `StorageError`.
///
/// Takes the query *length*, not the query, so it cannot leak user content. FTS5
/// MATCH parse failures — syntax errors, `"no such column: <token>"` from a
/// `tok:` / `-1` column-filter form, unterminated strings — all surface as
/// SQLite's generic error code (`ErrorCode::Unknown` == SQLITE_ERROR), and their
/// messages can echo the user's query token. Any such error is scrubbed to a
/// fixed message, because the returned text flows to the UI as `Response::Error`
/// and `StorageError::Database`'s `Display` would otherwise carry that token.
/// Genuine infrastructure failures (I/O, corruption, locking — distinct codes)
/// pass through unchanged. Only the length is logged. See CHR-67. (After
/// `sanitize_fts5_query`, user input is valid FTS5, so this is defense-in-depth.)
fn map_fts5_error(err: rusqlite::Error, query_len: usize) -> StorageError {
    if matches!(
        &err,
        rusqlite::Error::SqliteFailure(e, _) if e.code == rusqlite::ErrorCode::Unknown
    ) {
        log::debug!("rejected invalid FTS5 query (q_len={query_len})");
        return StorageError::Other("invalid search query syntax".into());
    }
    StorageError::Database(err)
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

    fn insert_screen_text(conn: &Connection, ocr: &str) -> i64 {
        let meta = ScreenshotMetadata {
            timestamp: 1_700_000_000_000,
            display_id: "display1".into(),
            app_name: None,
            app_bundle_id: None,
            window_title: None,
            image_path: "/data/screenshots/shot.heif".into(),
            ocr_text: Some(ocr.into()),
            phash: None,
            resolution: None,
        };
        screenshots::insert(conn, &meta).unwrap()
    }

    fn insert_audio_text(conn: &Connection, transcript: &str) -> i64 {
        let meta = AudioSegmentMetadata {
            start_timestamp: 1_700_000_000_000,
            end_timestamp: 1_700_000_030_000,
            source: "mic".into(),
            audio_path: "/data/audio/segment.opus".into(),
            transcript: Some(transcript.into()),
            whisper_model: Some("base".into()),
            language: Some("en".into()),
        };
        audio::insert(conn, &meta).unwrap()
    }

    /// Every screen row matches "kubernetes", so its bm25 weight clamps to
    /// near zero. Six non-matching audio rows make the term rare in
    /// `audio_fts`, so every audio match outranks every screen match.
    fn mixed_fixture(conn: &Connection, screens: usize, audios: usize) {
        for _ in 0..screens {
            insert_screen_text(conn, "kubernetes cluster notes");
        }
        for _ in 0..audios {
            insert_audio_text(conn, "we talked about kubernetes");
        }
        for _ in 0..6 {
            insert_audio_text(conn, "lunch plans for friday");
        }
    }

    fn kinds(results: &[SearchResult]) -> Vec<&'static str> {
        results
            .iter()
            .map(|r| match r.source {
                SearchSource::Screen(_) => "screen",
                SearchSource::Audio(_) => "audio",
            })
            .collect()
    }

    fn hit_keys(results: &[SearchResult]) -> Vec<(&'static str, i64)> {
        results
            .iter()
            .map(|r| match &r.source {
                SearchSource::Screen(s) => ("screen", s.id),
                SearchSource::Audio(a) => ("audio", a.id),
            })
            .collect()
    }

    fn screen_ids(results: &[SearchResult]) -> Vec<i64> {
        results
            .iter()
            .filter_map(|r| match &r.source {
                SearchSource::Screen(s) => Some(s.id),
                SearchSource::Audio(_) => None,
            })
            .collect()
    }

    #[test]
    fn all_mode_alternates_screen_and_audio() {
        let conn = setup_db();
        mixed_fixture(&conn, 2, 2);
        let results = search(&conn, "kubernetes", &SearchFilter::All, 10, 0).unwrap();
        assert_eq!(kinds(&results), ["screen", "audio", "screen", "audio"]);
    }

    #[test]
    fn all_mode_starts_with_screen_even_when_audio_ranks_better() {
        let conn = setup_db();
        mixed_fixture(&conn, 1, 1);
        let audio_only = search(&conn, "kubernetes", &SearchFilter::AudioOnly, 10, 0).unwrap();
        let screen_only = search(&conn, "kubernetes", &SearchFilter::ScreenOnly, 10, 0).unwrap();
        assert!(
            audio_only[0].rank < screen_only[0].rank,
            "fixture premise: the audio hit must outrank the screen hit"
        );
        let results = search(&conn, "kubernetes", &SearchFilter::All, 10, 0).unwrap();
        assert_eq!(kinds(&results), ["screen", "audio"]);
    }

    #[test]
    fn all_mode_appends_the_longer_source() {
        let conn = setup_db();
        mixed_fixture(&conn, 3, 1);
        let results = search(&conn, "kubernetes", &SearchFilter::All, 10, 0).unwrap();
        assert_eq!(kinds(&results), ["screen", "audio", "screen", "screen"]);
    }

    #[test]
    fn all_mode_appends_the_longer_audio_source_in_audio_order() {
        let conn = setup_db();
        insert_screen_text(&conn, "kubernetes cluster notes");
        insert_audio_text(&conn, "kubernetes kubernetes kubernetes");
        insert_audio_text(
            &conn,
            "kubernetes came up once in a long call about many other unrelated things",
        );
        insert_audio_text(&conn, "kubernetes again");
        for _ in 0..6 {
            insert_audio_text(&conn, "lunch plans for friday");
        }
        let audio_only = search(&conn, "kubernetes", &SearchFilter::AudioOnly, 10, 0).unwrap();
        let all = search(&conn, "kubernetes", &SearchFilter::All, 10, 0).unwrap();
        assert_eq!(kinds(&all), ["screen", "audio", "audio", "audio"]);
        let audio_ids = |rs: &[SearchResult]| -> Vec<i64> {
            rs.iter()
                .filter_map(|r| match &r.source {
                    SearchSource::Audio(a) => Some(a.id),
                    SearchSource::Screen(_) => None,
                })
                .collect()
        };
        assert_eq!(audio_ids(&all), audio_ids(&audio_only));
    }

    #[test]
    fn all_mode_pages_through_the_interleaved_sequence() {
        let conn = setup_db();
        mixed_fixture(&conn, 2, 2);
        let full = search(&conn, "kubernetes", &SearchFilter::All, 10, 0).unwrap();
        assert_eq!(full.len(), 4, "fixture premise: four matches");
        let even = search(&conn, "kubernetes", &SearchFilter::All, 2, 2).unwrap();
        assert_eq!(kinds(&even), ["screen", "audio"]);
        assert_eq!(hit_keys(&even), hit_keys(&full[2..4]));
        let odd = search(&conn, "kubernetes", &SearchFilter::All, 2, 1).unwrap();
        assert_eq!(hit_keys(&odd), hit_keys(&full[1..3]));
    }

    #[test]
    fn all_mode_keeps_screen_mode_order_for_screen_hits() {
        let conn = setup_db();
        insert_screen_text(&conn, "kubernetes kubernetes kubernetes");
        insert_screen_text(
            &conn,
            "kubernetes and a much longer line of unrelated words about other things",
        );
        insert_screen_text(&conn, "kubernetes notes");
        for _ in 0..4 {
            insert_screen_text(&conn, "nothing relevant here");
        }
        insert_audio_text(&conn, "kubernetes on the call");
        insert_audio_text(&conn, "more kubernetes talk");
        let screen_only = search(&conn, "kubernetes", &SearchFilter::ScreenOnly, 10, 0).unwrap();
        let all = search(&conn, "kubernetes", &SearchFilter::All, 10, 0).unwrap();
        assert_eq!(
            screen_ids(&screen_only).len(),
            3,
            "fixture must match 3 screens"
        );
        assert_eq!(screen_ids(&all), screen_ids(&screen_only));
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

    #[test]
    fn search_with_punctuation_does_not_error() {
        // FTS5-significant punctuation (parens, quotes, `*`) must be treated as
        // literal text, not query syntax — otherwise a syntax error surfaces as
        // a misleading "No matches". Regression: CHR-68.
        let conn = setup_db();
        insert_test_screenshot(&conn); // ocr: "deployment pipeline kubernetes cluster"

        let results = search(&conn, "pipeline)", &SearchFilter::All, 10, 0)
            .expect("punctuation query must not be an FTS5 syntax error");
        assert_eq!(results.len(), 1);
        assert!(matches!(results[0].source, SearchSource::Screen(_)));
    }

    #[test]
    fn search_multi_word_uses_and_not_phrase() {
        // Multi-word queries must match documents containing all the words in
        // any order/position (AND), not only as an adjacent phrase. Guards
        // against a phrase-quoting fix that would silently regress search.
        let conn = setup_db();
        insert_test_screenshot(&conn); // "deployment pipeline kubernetes cluster"

        // Both words are present but reversed and non-adjacent in the doc.
        let results = search(&conn, "kubernetes deployment", &SearchFilter::All, 10, 0).unwrap();
        assert_eq!(results.len(), 1);
        assert!(matches!(results[0].source, SearchSource::Screen(_)));
    }

    #[test]
    fn search_supports_prefix_match() {
        // A partial word should match longer terms (deploy -> deployment).
        let conn = setup_db();
        insert_test_screenshot(&conn); // "deployment pipeline kubernetes cluster"

        let results = search(&conn, "deploy", &SearchFilter::All, 10, 0).unwrap();
        assert_eq!(results.len(), 1);
        assert!(matches!(results[0].source, SearchSource::Screen(_)));
    }

    #[test]
    fn search_punctuation_only_returns_empty() {
        let conn = setup_db();
        insert_test_screenshot(&conn);
        // A query with no actual search terms must return empty, not error.
        let results = search(&conn, "()", &SearchFilter::All, 10, 0).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn sanitize_fts5_query_quotes_and_prefixes_tokens() {
        assert_eq!(sanitize_fts5_query("foo bar"), "\"foo\"* \"bar\"*");
        // FTS5-significant punctuation stays inside the quoted literal.
        assert_eq!(sanitize_fts5_query("foo(bar)"), "\"foo(bar)\"*");
        // Internal double quotes are escaped by doubling.
        assert_eq!(
            sanitize_fts5_query("say \"hi\""),
            "\"say\"* \"\"\"hi\"\"\"*"
        );
    }

    #[test]
    fn map_fts5_error_does_not_leak_query() {
        // CHR-67: a syntax error must not echo the raw query back to the UI.
        let conn = setup_db();
        let secret = "leakyterm)";
        let mut stmt = conn
            .prepare("SELECT rowid FROM screenshots_fts WHERE screenshots_fts MATCH ?1")
            .unwrap();
        let err = stmt
            .query_map(params![secret], |_| -> rusqlite::Result<()> { Ok(()) })
            .and_then(|rows| rows.collect::<rusqlite::Result<Vec<()>>>())
            .expect_err("raw punctuation must trigger an FTS5 syntax error");
        let msg = map_fts5_error(err, secret.chars().count()).to_string();
        assert!(!msg.contains("leakyterm"), "must not leak the query: {msg}");
    }

    #[test]
    fn map_fts5_error_scrubs_non_syntax_parse_errors() {
        // A column-filter form ("tok:") yields "no such column: <tok>", which is
        // NOT "fts5: syntax error" but still echoes the user's token, so it must
        // be scrubbed too. Regression for the PR #27 review (CHR-67).
        let conn = setup_db();
        let secret = "topsecret:x";
        let mut stmt = conn
            .prepare("SELECT rowid FROM screenshots_fts WHERE screenshots_fts MATCH ?1")
            .unwrap();
        let err = stmt
            .query_map(params![secret], |_| -> rusqlite::Result<()> { Ok(()) })
            .and_then(|rows| rows.collect::<rusqlite::Result<Vec<()>>>())
            .expect_err("column-filter query should be an FTS5 parse error");
        assert!(
            err.to_string().contains("no such column"),
            "test premise: expected a non-syntax FTS5 parse error, got: {err}"
        );
        let msg = map_fts5_error(err, secret.chars().count()).to_string();
        assert!(
            !msg.contains("topsecret"),
            "must not leak the query token: {msg}"
        );
    }

    #[test]
    fn search_with_punctuation_matches_audio() {
        // The same sanitization must protect the audio FTS branch, not just screens.
        let conn = setup_db();
        insert_test_audio(&conn); // transcript: "discussing the kubernetes deployment strategy"

        let results = search(&conn, "deployment)", &SearchFilter::All, 10, 0)
            .expect("punctuation query must not be an FTS5 syntax error");
        assert_eq!(results.len(), 1);
        assert!(matches!(results[0].source, SearchSource::Audio(_)));
    }

    #[test]
    fn search_whitespace_only_returns_empty() {
        // No alphanumeric characters -> no search terms -> empty, not an error.
        let conn = setup_db();
        insert_test_screenshot(&conn);

        let results = search(&conn, "   \t  ", &SearchFilter::All, 10, 0).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn search_intra_token_punctuation_is_adjacent_phrase() {
        // A token with internal punctuation sanitizes to a quoted phrase, so it
        // matches the words ADJACENT (finds "kubernetes deployment" together)
        // rather than as independent AND terms anywhere in the document.
        let conn = setup_db();
        insert_test_screenshot(&conn); // "deployment pipeline kubernetes cluster" (not adjacent)
        insert_test_audio(&conn); // "...the kubernetes deployment strategy" (adjacent)

        let results = search(&conn, "kubernetes.deployment", &SearchFilter::All, 10, 0).unwrap();
        assert_eq!(results.len(), 1);
        assert!(matches!(results[0].source, SearchSource::Audio(_)));
    }
}
