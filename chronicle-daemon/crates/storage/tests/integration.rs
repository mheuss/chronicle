use chronicle_storage::{
    AudioSegmentMetadata, CleanupStats, ScreenshotMetadata, SearchFilter, SearchSource,
    Storage, StorageConfig,
};
use chrono::{Datelike, Utc};
use tempfile::tempdir;

/// Full workflow integration test exercising the complete async API end-to-end.
#[tokio::test]
async fn full_workflow() {
    // 1. Open storage with a temp directory
    let dir = tempdir().unwrap();
    let config = StorageConfig {
        base_dir: dir.path().to_path_buf(),
        pool_size: 2,
    };
    let storage = Storage::open(config).await.unwrap();

    // 2. Allocate screenshot and audio paths; verify parent dirs and date structure
    let now = Utc::now();
    let ts: i64 = now.timestamp_millis();
    let expected_date = format!("{}/{:02}/{:02}", now.year(), now.month(), now.day());

    let screenshot_path = storage
        .allocate_screenshot_path(ts, "display1")
        .await
        .unwrap();
    assert!(
        screenshot_path.parent().unwrap().exists(),
        "screenshot parent dir should be created"
    );
    let screenshot_path_str = screenshot_path.to_string_lossy();
    assert!(
        screenshot_path_str.contains(&expected_date),
        "screenshot path should contain date structure, got: {screenshot_path_str}"
    );

    let audio_path = storage
        .allocate_audio_path(ts, "mic")
        .await
        .unwrap();
    assert!(
        audio_path.parent().unwrap().exists(),
        "audio parent dir should be created"
    );
    let audio_path_str = audio_path.to_string_lossy();
    assert!(
        audio_path_str.contains(&expected_date),
        "audio path should contain date structure, got: {audio_path_str}"
    );

    // 3. Write fake files to those paths
    std::fs::write(&screenshot_path, b"fake screenshot data").unwrap();
    std::fs::write(&audio_path, b"fake audio data").unwrap();

    // 4. Insert screenshot metadata with OCR text
    let screenshot_id = storage
        .insert_screenshot(ScreenshotMetadata {
            timestamp: ts,
            display_id: "display1".into(),
            app_name: Some("Terminal".into()),
            app_bundle_id: Some("com.apple.Terminal".into()),
            window_title: Some("kubectl".into()),
            image_path: screenshot_path_str.to_string(),
            ocr_text: Some("kubernetes deployment pipeline".into()),
            phash: None,
            resolution: Some("2560x1440".into()),
        })
        .await
        .unwrap();
    assert!(screenshot_id > 0);

    // 5. Insert audio segment metadata with transcript
    let audio_id = storage
        .insert_audio_segment(AudioSegmentMetadata {
            start_timestamp: ts,
            end_timestamp: ts + 30_000,
            source: "mic".into(),
            audio_path: audio_path_str.to_string(),
            transcript: Some("discussing the kubernetes migration plan".into()),
            whisper_model: Some("base".into()),
            language: Some("en".into()),
        })
        .await
        .unwrap();
    assert!(audio_id > 0);

    // 6. Search for "kubernetes" with All filter -- expect 2 results
    let results = storage
        .search("kubernetes", SearchFilter::All, 10, 0)
        .await
        .unwrap();
    assert_eq!(results.len(), 2, "All filter should find 2 results for 'kubernetes'");

    // 7. Search with ScreenOnly filter -- expect 1 result
    let results = storage
        .search("kubernetes", SearchFilter::ScreenOnly, 10, 0)
        .await
        .unwrap();
    assert_eq!(results.len(), 1, "ScreenOnly filter should find 1 result");
    assert!(matches!(results[0].source, SearchSource::Screen(_)));

    // 8. Update OCR text
    storage
        .update_ocr_text(screenshot_id, "updated content grafana dashboard".into())
        .await
        .unwrap();

    // 9. Verify old term "pipeline" not found in ScreenOnly, new term "grafana" found
    let results = storage
        .search("pipeline", SearchFilter::ScreenOnly, 10, 0)
        .await
        .unwrap();
    assert_eq!(
        results.len(),
        0,
        "old term 'pipeline' should no longer match in ScreenOnly"
    );

    let results = storage
        .search("grafana", SearchFilter::ScreenOnly, 10, 0)
        .await
        .unwrap();
    assert_eq!(results.len(), 1, "'grafana' should match in ScreenOnly after OCR update");

    // 10. Update transcript
    storage
        .update_transcript(audio_id, "new transcript about CI pipeline".into())
        .await
        .unwrap();

    // 11. Verify "pipeline" found in AudioOnly
    let results = storage
        .search("pipeline", SearchFilter::AudioOnly, 10, 0)
        .await
        .unwrap();
    assert_eq!(
        results.len(),
        1,
        "'pipeline' should match in AudioOnly after transcript update"
    );
    assert!(matches!(results[0].source, SearchSource::Audio(_)));

    // 12. Get timeline with range filter
    let timeline = storage
        .get_timeline(ts - 1000, ts + 1000, None)
        .await
        .unwrap();
    assert_eq!(timeline.len(), 1, "timeline should contain the screenshot");
    assert_eq!(timeline[0].id, screenshot_id);

    // Also verify display_id filter
    let timeline_filtered = storage
        .get_timeline(ts - 1000, ts + 1000, Some("display1".into()))
        .await
        .unwrap();
    assert_eq!(timeline_filtered.len(), 1);

    let timeline_empty = storage
        .get_timeline(ts - 1000, ts + 1000, Some("nonexistent".into()))
        .await
        .unwrap();
    assert_eq!(timeline_empty.len(), 0);

    // 13. Get and set config (verify default retention_days = "30", set to "7", verify)
    let retention = storage.get_config("retention_days").await.unwrap();
    assert_eq!(retention, Some("30".to_string()), "default retention_days should be 30");

    storage.set_config("retention_days", "7").await.unwrap();
    let retention = storage.get_config("retention_days").await.unwrap();
    assert_eq!(retention, Some("7".to_string()), "retention_days should be updated to 7");

    // 14. Get status (verify counts, db_size > 0)
    let status = storage.status().await.unwrap();
    assert_eq!(status.screenshot_count, 1);
    assert_eq!(status.audio_segment_count, 1);
    assert!(status.db_size_bytes > 0, "db_size_bytes should be > 0");
    assert!(status.total_disk_usage_bytes > 0, "total_disk_usage_bytes should be > 0");
    assert_eq!(status.oldest_entry, Some(ts));

    // 15. Run cleanup (nothing expired with fresh data and 7-day retention)
    let cleanup: CleanupStats = storage.run_cleanup().await.unwrap();
    assert_eq!(cleanup.screenshots_deleted, 0, "no screenshots should be deleted");
    assert_eq!(cleanup.audio_segments_deleted, 0, "no audio segments should be deleted");

    // Verify data is still intact after cleanup
    let status_after = storage.status().await.unwrap();
    assert_eq!(status_after.screenshot_count, 1);
    assert_eq!(status_after.audio_segment_count, 1);
}
