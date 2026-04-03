//! Integration tests for chronicle-capture.
//!
//! These tests require:
//! - A real macOS display (no headless/CI)
//! - Screen Recording permission granted to the test runner
//! - macOS 12.3+
//!
//! Run manually: cargo test -p chronicle-capture --test integration -- --ignored

use chronicle_capture::{CaptureConfig, CaptureEngine, encode_heif};

#[ignore]
#[tokio::test]
async fn capture_engine_delivers_frames() {
    let config = CaptureConfig {
        frame_interval_secs: 0.5,
        channel_buffer_size: 16,
        audio: None,
    };

    let (mut engine, mut receiver) = CaptureEngine::start(config)
        .expect("Failed to start capture — is Screen Recording permission granted?");

    // Wait for at least one frame
    let frame = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        receiver.recv(),
    )
    .await
    .expect("Timed out waiting for frame")
    .expect("Channel closed before receiving a frame");

    assert!(frame.display_id > 0, "display_id should be non-zero");
    assert!(frame.width > 0, "width should be non-zero");
    assert!(frame.height > 0, "height should be non-zero");
    assert!(frame.timestamp > 0, "timestamp should be non-zero");
    assert!(frame.scale_factor >= 1.0, "scale_factor should be >= 1.0");

    let status = engine.status();
    assert!(status.active_displays > 0);
    assert!(status.total_frames_captured >= 1);

    engine.stop().expect("Failed to stop capture");

    // Drop engine to close all senders
    drop(engine);
    // Channel should now be closed
    let result = receiver.recv().await;
    assert!(result.is_none(), "Channel should close after engine is dropped");
}

#[ignore]
#[tokio::test]
async fn capture_engine_finds_displays() {
    let config = CaptureConfig::default();
    let (mut engine, _receiver) = CaptureEngine::start(config)
        .expect("Failed to start — no displays or no permission");

    let status = engine.status();
    assert!(status.active_displays >= 1, "Should find at least one display");

    engine.stop().expect("Failed to stop");
}

#[ignore]
#[tokio::test]
async fn encode_captured_frame_as_heif() {
    let config = CaptureConfig {
        frame_interval_secs: 0.5,
        channel_buffer_size: 16,
        audio: None,
    };
    let (engine, mut receiver) = CaptureEngine::start(config)
        .expect("failed to start capture engine");

    let frame = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        receiver.recv(),
    )
    .await
    .expect("timed out waiting for frame")
    .expect("channel closed without delivering a frame");

    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let path = dir.path().join("captured.heif");

    encode_heif(frame.sample_buffer.as_ref(), &path, 0.65)
        .expect("HEIF encoding failed");

    assert!(path.exists(), "HEIF file was not created");
    let metadata = std::fs::metadata(&path).unwrap();
    assert!(metadata.len() > 1000, "HEIF file suspiciously small: {} bytes", metadata.len());

    drop(engine);
}
