use rusqlite::Connection;

use crate::error::Result;

const MIGRATIONS: &[&str] = &[
    include_str!("migrations/001_initial_schema.sql"),
    include_str!("migrations/002_path_indexes.sql"),
    include_str!("migrations/003_null_marker_transcripts.sql"),
];

/// Configure connection-level PRAGMAs. Call on every new connection.
pub(crate) fn setup_connection(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    Ok(())
}

/// Run pending migrations. Uses PRAGMA user_version to track progress.
pub(crate) fn migrate(conn: &Connection) -> Result<()> {
    let current_version: u32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;

    for (i, migration) in MIGRATIONS.iter().enumerate() {
        let version = (i + 1) as u32;
        if version > current_version {
            // The migration and its version stamp must land together. As two
            // separate autocommit statements, a crash between them leaves the
            // schema at version N stamped N-1, and the replay re-runs the whole
            // migration. That is harmless today, but by two different
            // mechanisms: 001 and 002 are entirely `IF NOT EXISTS` /
            // `INSERT OR IGNORE`, while 003 is a data UPDATE whose own
            // `transcript IS NOT NULL` guard excludes the rows it already
            // cleared (pinned by `migration_003_nulls_marker_transcripts_only`).
            // Do not assume `IF NOT EXISTS` is the only pattern in play — the
            // next migration need not be idempotent at all, and
            // `ALTER TABLE ADD COLUMN` has no idempotent form.
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(migration)?;
            tx.pragma_update(None, "user_version", version)?;
            tx.commit()?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    /// Migration 003 clears whisper's own markers from persisted transcripts.
    /// Stops at version 2 first, so `migrate` below runs only 003.
    ///
    /// The fixture is built so that removing any single element of the
    /// predicate changes the outcome for exactly one row — see
    /// `docs/development/storage.md`, "Testing a guard?": when two guards can
    /// each independently spare a fixture, a test targeting one of them proves
    /// nothing. Verified by mutation: dropping `trim()` spares row 12, and
    /// dropping the length, bracket, space, tab, newline, CR, or ASCII clause
    /// pulls in row 8, 7, 5, 9, 10, 11, or 6 respectively.
    ///
    /// `transcript IS NOT NULL` is the one element that cannot be pinned by
    /// outcome — NULL never satisfies the other clauses either — so row 13
    /// guards it indirectly, via the FTS integrity-check.
    #[test]
    fn migration_003_nulls_marker_transcripts_only() {
        let conn = Connection::open_in_memory().unwrap();
        setup_connection(&conn).unwrap();
        conn.execute_batch(MIGRATIONS[0]).unwrap();
        conn.execute_batch(MIGRATIONS[1]).unwrap();
        conn.pragma_update(None, "user_version", 2u32).unwrap();

        // Seed a language on every row. Without it the "language is cleared"
        // assertion below passes vacuously, since the column would be NULL
        // already.
        let insert = |id: i64, transcript: &str| {
            conn.execute(
                "INSERT INTO audio_segments
                   (id, start_timestamp, end_timestamp, source, audio_path, transcript, whisper_model, language)
                 VALUES (?1, 1, 2, 'mic', '/tmp/x.opus', ?2, 'base', 'nn')",
                rusqlite::params![id, transcript],
            )
            .unwrap();
        };
        insert(1, "[BLANK_AUDIO]");
        insert(2, "[Motor]");
        insert(3, "the quick brown fox");
        insert(4, "the [sic] answer");
        insert(5, "[hello world]");
        // Spaceless script: no internal whitespace, so ONLY the ASCII clause
        // can spare it. Deleting this row would be irreversible loss of real
        // speech, and only for non-English users.
        insert(6, "[这是一个完整的句子]");
        // Only the closing-bracket clause spares this one.
        insert(7, "[unclosed");
        // Only the length clause spares this one.
        insert(8, "[]");
        // The Rust rule rejects ANY whitespace; SQL has to name each kind, so
        // these two pin the tab and newline clauses. Without them those clauses
        // could be deleted and every assertion would still pass — the exact
        // trap docs/development/storage.md warns about.
        insert(9, "[tab\there]");
        insert(10, "[nl\nhere]");
        insert(11, "[cr\rhere]");
        // Pins `trim()` itself. Every other fixture row is already
        // whitespace-clean, so all six trim() calls are no-ops across them —
        // delete `trim(` throughout the migration and they all still pass.
        // This row is a marker ONLY after trimming, so it is cleared only if
        // trim() is doing its job.
        insert(12, "  [BLANK_AUDIO]  ");
        // A row that was never transcribed. `transcript IS NOT NULL` must skip
        // it, so the audio_au trigger never hands a NULL OLD.transcript to the
        // FTS5 'delete' command — which is what the integrity-check below would
        // catch.
        conn.execute(
            "INSERT INTO audio_segments
               (id, start_timestamp, end_timestamp, source, audio_path, transcript, whisper_model, language)
             VALUES (13, 1, 2, 'mic', '/tmp/x.opus', NULL, NULL, NULL)",
            [],
        )
        .unwrap();

        migrate(&conn).unwrap();

        let col = |id: i64, c: &str| -> Option<String> {
            conn.query_row(
                &format!("SELECT {c} FROM audio_segments WHERE id = ?1"),
                [id],
                |r| r.get(0),
            )
            .unwrap()
        };

        assert_eq!(col(1, "transcript"), None, "[BLANK_AUDIO] is cleared");
        assert_eq!(
            col(2, "transcript"),
            None,
            "any bracketed ASCII token is cleared"
        );

        // language is cleared with the transcript, so a migrated legacy row
        // matches what the pipeline now writes for the same state (HEU-620).
        assert_eq!(
            col(1, "language"),
            None,
            "a cleared row keeps no detected language"
        );
        // whisper_model survives — it is what still marks the row as attempted.
        assert_eq!(
            col(1, "whisper_model").as_deref(),
            Some("base"),
            "whisper_model survives — the row still reads as attempted, not untranscribed"
        );

        assert_eq!(
            col(3, "transcript").as_deref(),
            Some("the quick brown fox"),
            "speech survives"
        );
        assert_eq!(
            col(4, "transcript").as_deref(),
            Some("the [sic] answer"),
            "bracketed aside in prose survives (whitespace clause)"
        );
        assert_eq!(
            col(5, "transcript").as_deref(),
            Some("[hello world]"),
            "multi-word bracket survives (whitespace clause)"
        );
        assert_eq!(
            col(6, "transcript").as_deref(),
            Some("[这是一个完整的句子]"),
            "spaceless-script speech survives — only the ASCII clause spares this"
        );
        assert_eq!(
            col(7, "transcript").as_deref(),
            Some("[unclosed"),
            "unclosed bracket survives (closing-bracket clause)"
        );
        assert_eq!(
            col(8, "transcript").as_deref(),
            Some("[]"),
            "empty brackets survive (length clause)"
        );
        assert_eq!(
            col(9, "transcript").as_deref(),
            Some("[tab\there]"),
            "an internal tab survives (tab clause)"
        );
        assert_eq!(
            col(10, "transcript").as_deref(),
            Some("[nl\nhere]"),
            "an internal newline survives (newline clause)"
        );
        assert_eq!(
            col(11, "transcript").as_deref(),
            Some("[cr\rhere]"),
            "an internal carriage return survives (CR clause)"
        );
        assert_eq!(
            col(12, "transcript"),
            None,
            "a marker padded with whitespace is still a marker (trim clause)"
        );
        assert_eq!(
            col(13, "transcript"),
            None,
            "an untranscribed row is untouched — it was already NULL"
        );
        assert_eq!(
            col(13, "whisper_model"),
            None,
            "and it stays untranscribed: the migration must not stamp it"
        );
        // A spared row keeps every column, not just its text.
        assert_eq!(
            col(3, "language").as_deref(),
            Some("nn"),
            "rows the migration does not touch are left entirely alone"
        );

        // The whole point of clearing them: the trigger reindexes audio_fts,
        // so the marker's tokens stop matching. Without this the migration
        // could pass while leaving the search pollution in place.
        let hits: i64 = conn
            .query_row(
                "SELECT count(*) FROM audio_fts WHERE audio_fts MATCH 'blank'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            hits, 0,
            "clearing the transcript must clear the FTS index too"
        );

        // External-content FTS5 desyncs silently; ask SQLite directly.
        conn.execute_batch("INSERT INTO audio_fts(audio_fts) VALUES('integrity-check')")
            .expect("audio_fts must stay consistent with audio_segments");

        // Replay safety. `migrate` is version-gated, so running it again is a
        // no-op and proves nothing — apply the migration body directly instead.
        // A crash between `execute_batch` and the `user_version` stamp replays
        // exactly this, and `transcript IS NOT NULL` is what makes it harmless.
        conn.execute_batch(MIGRATIONS[2]).unwrap();
        let changed: i64 = conn
            .query_row("SELECT changes()", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            changed, 0,
            "replaying 003 must clear nothing further — already-cleared rows are \
             excluded by `transcript IS NOT NULL`"
        );
        conn.execute_batch("INSERT INTO audio_fts(audio_fts) VALUES('integrity-check')")
            .expect("audio_fts must survive a replay too");
    }

    #[test]
    fn migration_creates_all_tables() {
        let conn = Connection::open_in_memory().unwrap();
        setup_connection(&conn).unwrap();
        migrate(&conn).unwrap();

        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();

        assert!(tables.contains(&"screenshots".to_string()));
        assert!(tables.contains(&"audio_segments".to_string()));
        assert!(tables.contains(&"config".to_string()));
        assert!(tables.contains(&"screenshots_fts".to_string()));
        assert!(tables.contains(&"audio_fts".to_string()));
    }

    #[test]
    fn migration_seeds_default_config() {
        let conn = Connection::open_in_memory().unwrap();
        setup_connection(&conn).unwrap();
        migrate(&conn).unwrap();

        let retention: String = conn
            .query_row(
                "SELECT value FROM config WHERE key = 'retention_days'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retention, "30");
    }

    #[test]
    fn migration_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        setup_connection(&conn).unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap(); // second run should not error
    }

    fn index_names(conn: &Connection) -> Vec<String> {
        conn.prepare("SELECT name FROM sqlite_master WHERE type='index' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap()
    }

    #[test]
    fn migration_creates_path_indexes() {
        let conn = Connection::open_in_memory().unwrap();
        setup_connection(&conn).unwrap();
        migrate(&conn).unwrap();

        let indexes = index_names(&conn);
        assert!(
            indexes.contains(&"idx_screenshots_image_path".to_string()),
            "missing screenshots path index, got: {indexes:?}"
        );
        assert!(
            indexes.contains(&"idx_audio_segments_audio_path".to_string()),
            "missing audio path index, got: {indexes:?}"
        );
    }

    #[test]
    fn migration_upgrades_from_v1() {
        let conn = Connection::open_in_memory().unwrap();
        setup_connection(&conn).unwrap();

        // Simulate a database created before 002 existed: apply only migration 001
        // and stamp user_version to match.
        conn.execute_batch(MIGRATIONS[0]).unwrap();
        conn.pragma_update(None, "user_version", 1u32).unwrap();

        migrate(&conn).unwrap();

        let version: u32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(
            version,
            MIGRATIONS.len() as u32,
            "migrate() must advance user_version to the latest migration"
        );
        assert!(index_names(&conn).contains(&"idx_screenshots_image_path".to_string()));
    }

    /// The same upgrade, but on a real file-backed WAL database — the path that
    /// actually ships to every existing install.
    ///
    /// `migration_upgrades_from_v1` runs in memory, where there is no WAL, no
    /// journal, and no reopen. The on-disk case was verified once by hand against
    /// a 128 MB production database, but that evidence does not live in the repo
    /// and would not protect migration 003. This does.
    #[test]
    fn migration_upgrades_from_v1_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("chronicle.db");

        // Build a v1 database and close it, so the upgrade runs against a file
        // this process is opening fresh rather than a connection it already holds.
        {
            let conn = Connection::open(&db_path).unwrap();
            setup_connection(&conn).unwrap();
            conn.execute_batch(MIGRATIONS[0]).unwrap();
            conn.pragma_update(None, "user_version", 1u32).unwrap();
        }

        let conn = Connection::open(&db_path).unwrap();
        setup_connection(&conn).unwrap();
        let before: u32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(before, 1, "fixture must start at v1 or this proves nothing");

        migrate(&conn).unwrap();

        let after: u32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(after, MIGRATIONS.len() as u32);

        let indexes = index_names(&conn);
        assert!(
            indexes.contains(&"idx_screenshots_image_path".to_string()),
            "missing screenshots path index after on-disk upgrade, got: {indexes:?}"
        );
        assert!(
            indexes.contains(&"idx_audio_segments_audio_path".to_string()),
            "missing audio path index after on-disk upgrade, got: {indexes:?}"
        );

        // The v1 data must still be there — an upgrade that drops user captures
        // would be catastrophic and is exactly what a file-backed test can catch.
        let retention: String = conn
            .query_row(
                "SELECT value FROM config WHERE key = 'retention_days'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            retention, "30",
            "migration 002 must not disturb seeded config"
        );
    }

    #[test]
    fn path_indexes_cover_the_sweep_query() {
        let conn = Connection::open_in_memory().unwrap();
        setup_connection(&conn).unwrap();
        migrate(&conn).unwrap();

        // Asserting the index exists in sqlite_master proves nothing about
        // whether the planner uses it, and "the planner uses it" is the entire
        // point of migration 002. Without this, dropping the index or changing
        // the sweep's query shape puts the daemon back to a multi-minute
        // startup stall with every test still green. See HEU-547.
        for (sql, index) in [
            (
                "SELECT image_path FROM screenshots",
                "idx_screenshots_image_path",
            ),
            (
                "SELECT audio_path FROM audio_segments",
                "idx_audio_segments_audio_path",
            ),
        ] {
            let plan: String = conn
                .query_row(&format!("EXPLAIN QUERY PLAN {sql}"), [], |row| row.get(3))
                .unwrap();
            assert!(
                plan.contains(index),
                "planner ignored {index} for `{sql}`: {plan}"
            );
        }
    }

    #[test]
    fn setup_connection_enables_wal() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let conn = Connection::open(&db_path).unwrap();
        setup_connection(&conn).unwrap();

        let journal_mode: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(journal_mode.to_lowercase(), "wal");
    }
}
