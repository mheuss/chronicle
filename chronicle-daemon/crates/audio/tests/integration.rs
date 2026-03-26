//! Integration tests for chronicle-audio.
//!
//! These tests require:
//! - A real macOS display (no headless/CI)
//! - Screen Recording + Microphone permission granted to the test runner
//! - macOS 12.3+
//!
//! Run manually: cargo test -p chronicle-audio --test integration -- --ignored

use std::time::Duration;

use chronicle_audio::{AudioConfig, AudioEngine};

#[test]
#[ignore]
fn captures_system_audio() {
    let output_dir = std::env::temp_dir().join("chronicle-audio-integration");
    let _ = std::fs::remove_dir_all(&output_dir);
    std::fs::create_dir_all(&output_dir).expect("failed to create temp output dir");

    let config = AudioConfig {
        segment_duration_secs: 2,
        output_dir: output_dir.clone(),
        ..AudioConfig::default()
    };

    let mut engine = AudioEngine::new(config)
        .expect("AudioEngine::new failed");

    let rx = engine
        .start()
        .expect("failed to start audio capture — is Screen Recording permission granted?");

    // Wait long enough for at least one full segment (2s duration + buffer).
    std::thread::sleep(Duration::from_secs(3));

    engine.stop().expect("failed to stop audio capture");

    // Collect all segments delivered over the channel.
    let mut segments = Vec::new();
    while let Ok(seg) = rx.try_recv() {
        segments.push(seg);
    }

    assert!(
        !segments.is_empty(),
        "expected at least one completed segment"
    );

    let first = &segments[0];
    assert!(first.path.exists(), "segment file should exist on disk");
    assert!(
        first.start_timestamp > 0,
        "start_timestamp should be positive"
    );
    assert!(
        first.end_timestamp > first.start_timestamp,
        "end_timestamp ({}) should be greater than start_timestamp ({})",
        first.end_timestamp,
        first.start_timestamp
    );

    // Verify the file starts with the Ogg magic bytes.
    let bytes = std::fs::read(&first.path).expect("failed to read segment file");
    assert!(
        bytes.len() >= 4 && &bytes[..4] == b"OggS",
        "segment file should be a valid Ogg file (expected OggS header)"
    );

    // Clean up.
    let _ = std::fs::remove_dir_all(&output_dir);
}

#[test]
#[ignore]
fn engine_stop_flushes_partial_segment() {
    let output_dir = std::env::temp_dir().join("chronicle-audio-integration-flush");
    let _ = std::fs::remove_dir_all(&output_dir);
    std::fs::create_dir_all(&output_dir).expect("failed to create temp output dir");

    let config = AudioConfig {
        // Long segment duration so we never complete a full segment naturally.
        segment_duration_secs: 60,
        output_dir: output_dir.clone(),
        ..AudioConfig::default()
    };

    let mut engine = AudioEngine::new(config)
        .expect("AudioEngine::new failed");

    let rx = engine
        .start()
        .expect("failed to start audio capture — is Screen Recording permission granted?");

    // Capture for a short time. With a 60s segment duration, no segment
    // completes naturally. The stop() call should flush the partial.
    std::thread::sleep(Duration::from_secs(2));

    engine.stop().expect("failed to stop audio capture");

    // Collect all segments delivered over the channel.
    let mut segments = Vec::new();
    while let Ok(seg) = rx.try_recv() {
        segments.push(seg);
    }

    assert!(
        !segments.is_empty(),
        "expected at least one segment from flush of partial data"
    );

    // Clean up.
    let _ = std::fs::remove_dir_all(&output_dir);
}
