use rusqlite::{Connection, Row, params};

use crate::error::Result;
use crate::models::{Screenshot, ScreenshotMetadata};

fn row_to_screenshot(row: &Row<'_>) -> rusqlite::Result<Screenshot> {
    Ok(Screenshot {
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
    })
}

pub(crate) fn insert(conn: &Connection, meta: &ScreenshotMetadata) -> Result<i64> {
    conn.execute(
        "INSERT INTO screenshots (timestamp, display_id, app_name, app_bundle_id, window_title, image_path, ocr_text, phash, resolution)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            meta.timestamp,
            meta.display_id,
            meta.app_name,
            meta.app_bundle_id,
            meta.window_title,
            meta.image_path,
            meta.ocr_text,
            meta.phash,
            meta.resolution,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub(crate) fn get(conn: &Connection, id: i64) -> Result<Screenshot> {
    let screenshot = conn.query_row(
        "SELECT id, timestamp, display_id, app_name, app_bundle_id, window_title, image_path, ocr_text, phash, resolution, created_at
         FROM screenshots WHERE id = ?1",
        params![id],
        row_to_screenshot,
    )?;
    Ok(screenshot)
}

pub(crate) fn get_timeline(
    conn: &Connection,
    start: i64,
    end: i64,
    display_id: Option<&str>,
) -> Result<Vec<Screenshot>> {
    let results = match display_id {
        Some(did) => {
            let mut stmt = conn.prepare(
                "SELECT id, timestamp, display_id, app_name, app_bundle_id, window_title, image_path, ocr_text, phash, resolution, created_at
                 FROM screenshots
                 WHERE timestamp >= ?1 AND timestamp <= ?2 AND display_id = ?3
                 ORDER BY timestamp ASC",
            )?;
            let rows = stmt.query_map(params![start, end, did], row_to_screenshot)?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        }
        None => {
            let mut stmt = conn.prepare(
                "SELECT id, timestamp, display_id, app_name, app_bundle_id, window_title, image_path, ocr_text, phash, resolution, created_at
                 FROM screenshots
                 WHERE timestamp >= ?1 AND timestamp <= ?2
                 ORDER BY timestamp ASC",
            )?;
            let rows = stmt.query_map(params![start, end], row_to_screenshot)?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        }
    };
    Ok(results)
}

pub(crate) fn update_ocr_text(conn: &Connection, id: i64, ocr_text: &str) -> Result<()> {
    let rows_affected = conn.execute(
        "UPDATE screenshots SET ocr_text = ?1 WHERE id = ?2",
        params![ocr_text, id],
    )?;
    if rows_affected == 0 {
        return Err(crate::error::StorageError::Other(format!(
            "not found: id {}",
            id
        )));
    }
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

    fn sample_meta() -> ScreenshotMetadata {
        ScreenshotMetadata {
            timestamp: 1_700_000_000_000,
            display_id: "display1".into(),
            app_name: Some("Safari".into()),
            app_bundle_id: Some("com.apple.Safari".into()),
            window_title: Some("Google".into()),
            image_path: "/data/screenshots/shot.heif".into(),
            ocr_text: Some("Hello world login button".into()),
            phash: None,
            resolution: Some("2560x1440".into()),
        }
    }

    #[test]
    fn insert_and_get_screenshot() {
        let conn = setup_db();
        let meta = sample_meta();
        let id = insert(&conn, &meta).unwrap();
        assert!(id > 0);

        let screenshot = get(&conn, id).unwrap();
        assert_eq!(screenshot.id, id);
        assert_eq!(screenshot.timestamp, meta.timestamp);
        assert_eq!(screenshot.display_id, meta.display_id);
        assert_eq!(screenshot.app_name, meta.app_name);
        assert_eq!(screenshot.app_bundle_id, meta.app_bundle_id);
        assert_eq!(screenshot.window_title, meta.window_title);
        assert_eq!(screenshot.image_path, meta.image_path);
        assert_eq!(screenshot.ocr_text, meta.ocr_text);
        assert_eq!(screenshot.resolution, meta.resolution);
        assert!(screenshot.created_at > 0);
    }

    #[test]
    fn get_timeline_filters_by_range_and_display() {
        let conn = setup_db();

        // Insert 3 screenshots: 2 in range on display1, 1 out of range
        let mut m1 = sample_meta();
        m1.timestamp = 1000;
        m1.display_id = "display1".into();
        insert(&conn, &m1).unwrap();

        let mut m2 = sample_meta();
        m2.timestamp = 2000;
        m2.display_id = "display1".into();
        insert(&conn, &m2).unwrap();

        let mut m3 = sample_meta();
        m3.timestamp = 3000;
        m3.display_id = "display2".into();
        insert(&conn, &m3).unwrap();

        // All in range, no display filter
        let all = get_timeline(&conn, 500, 3500, None).unwrap();
        assert_eq!(all.len(), 3);

        // Range filter: only 1000..2500
        let ranged = get_timeline(&conn, 500, 2500, None).unwrap();
        assert_eq!(ranged.len(), 2);

        // Display filter
        let filtered = get_timeline(&conn, 500, 3500, Some("display1")).unwrap();
        assert_eq!(filtered.len(), 2);

        let filtered2 = get_timeline(&conn, 500, 3500, Some("display2")).unwrap();
        assert_eq!(filtered2.len(), 1);
    }

    #[test]
    fn update_ocr_text_updates_existing_row() {
        let conn = setup_db();
        let meta = sample_meta();
        let id = insert(&conn, &meta).unwrap();

        update_ocr_text(&conn, id, "Updated OCR content").unwrap();

        let screenshot = get(&conn, id).unwrap();
        assert_eq!(screenshot.ocr_text.as_deref(), Some("Updated OCR content"));
    }

    #[test]
    fn insert_triggers_fts_index() {
        let conn = setup_db();
        let meta = sample_meta(); // ocr_text = "Hello world login button"
        insert(&conn, &meta).unwrap();

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM screenshots_fts WHERE screenshots_fts MATCH 'login'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn get_nonexistent_id_returns_error() {
        let conn = setup_db();
        let result = get(&conn, 9999);
        assert!(result.is_err());
    }

    #[test]
    fn update_ocr_text_nonexistent_id_returns_error() {
        let conn = setup_db();
        let result = update_ocr_text(&conn, 9999, "some text");
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("not found: id 9999"));
    }

    #[test]
    fn update_ocr_triggers_fts_reindex() {
        let conn = setup_db();
        let meta = sample_meta(); // ocr_text = "Hello world login button"
        let id = insert(&conn, &meta).unwrap();

        update_ocr_text(&conn, id, "Dashboard settings panel").unwrap();

        // Old term should be gone
        let old_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM screenshots_fts WHERE screenshots_fts MATCH 'login'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(old_count, 0);

        // New term should be present
        let new_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM screenshots_fts WHERE screenshots_fts MATCH 'dashboard'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(new_count, 1);
    }
}
