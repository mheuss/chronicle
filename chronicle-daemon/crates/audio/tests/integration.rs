//! Integration tests for chronicle-audio.
//!
//! The AudioPipeline no longer manages its own SCStream. Full end-to-end
//! audio capture tests live at the daemon level where the capture crate
//! registers the audio handler on the primary display's stream.
//!
//! These tests verify the pipeline creates and shuts down cleanly.

use chronicle_audio::{AudioConfig, AudioPipeline};

#[test]
fn pipeline_creates_and_stops_cleanly() {
    let output_dir = std::env::temp_dir().join("chronicle-audio-integration-pipeline");
    let _ = std::fs::remove_dir_all(&output_dir);
    std::fs::create_dir_all(&output_dir).expect("failed to create temp output dir");

    let config = AudioConfig {
        segment_duration_secs: 2,
        output_dir: output_dir.clone(),
        ..AudioConfig::default()
    };

    let (mut pipeline, _rx) = AudioPipeline::create(config).expect("AudioPipeline::create failed");

    // Handler and queue should be available for external registration.
    // Verify, then drop the references before stopping.
    {
        let _handler = pipeline.handler().expect("handler should be Some before stop");
        let _queue = pipeline.queue();
    }

    // Stop should complete without error.
    pipeline.stop().expect("failed to stop pipeline");

    // Clean up.
    let _ = std::fs::remove_dir_all(&output_dir);
}
