use rusqlite::Connection;

use crate::error::Result;

const MIGRATIONS: &[&str] = &[
    include_str!("migrations/001_initial_schema.sql"),
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
    let current_version: u32 =
        conn.pragma_query_value(None, "user_version", |row| row.get(0))?;

    for (i, migration) in MIGRATIONS.iter().enumerate() {
        let version = (i + 1) as u32;
        if version > current_version {
            conn.execute_batch(migration)?;
            conn.pragma_update(None, "user_version", version)?;
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
