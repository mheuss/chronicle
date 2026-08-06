use rusqlite::Connection;

use crate::error::Result;

const MIGRATIONS: &[&str] = &[
    include_str!("migrations/001_initial_schema.sql"),
    include_str!("migrations/002_path_indexes.sql"),
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
            // migration. That is harmless today because 001 and 002 are entirely
            // `IF NOT EXISTS` / `INSERT OR IGNORE`, but the next migration need
            // not be — `ALTER TABLE ADD COLUMN` has no idempotent form.
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
