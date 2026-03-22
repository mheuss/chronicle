//! Capture engine managing per-display SCStreams.
//!
//! `CaptureEngine` enumerates all connected displays, creates one `SCStream`
//! per display, and delivers frames over a bounded mpsc channel.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use screencapturekit::prelude::*;
use tokio::sync::mpsc;

use crate::error::{CaptureError, Result};
use crate::handler::FrameHandler;
use crate::{CaptureConfig, CaptureStatus, CapturedFrame};

/// Multi-display capture engine.
///
/// Call [`CaptureEngine::start`] to enumerate displays, create one `SCStream`
/// per display, and begin delivering `CapturedFrame`s over the returned
/// receiver. Use [`stop`](CaptureEngine::stop) to tear down all streams and
/// [`status`](CaptureEngine::status) to read health counters.
pub struct CaptureEngine {
    streams: Vec<SCStream>,
    _sender: mpsc::Sender<CapturedFrame>,
    frames_captured: Arc<AtomicU64>,
    frames_dropped: Arc<AtomicU64>,
}

impl CaptureEngine {
    /// Enumerate displays and start one capture stream per display.
    ///
    /// Returns the engine and a receiver that delivers `CapturedFrame`s.
    /// The channel has a bounded buffer of `config.channel_buffer_size`.
    ///
    /// # Errors
    ///
    /// * `CaptureError::NoDisplays` -- no displays found
    /// * `CaptureError::ScreenCaptureKit` -- SCK returned an error
    pub fn start(config: CaptureConfig) -> Result<(Self, mpsc::Receiver<CapturedFrame>)> {
        if config.frame_interval_secs <= 0.0 {
            return Err(CaptureError::ScreenCaptureKit(
                "frame_interval_secs must be positive".into(),
            ));
        }
        if config.channel_buffer_size == 0 {
            return Err(CaptureError::ScreenCaptureKit(
                "channel_buffer_size must be at least 1".into(),
            ));
        }

        let (sender, receiver) = mpsc::channel(config.channel_buffer_size);

        let content = SCShareableContent::get().map_err(|e| {
            CaptureError::ScreenCaptureKit(format!("failed to get shareable content: {e}"))
        })?;

        let displays = content.displays();
        if displays.is_empty() {
            return Err(CaptureError::NoDisplays);
        }

        let frames_captured = Arc::new(AtomicU64::new(0));
        let frames_dropped = Arc::new(AtomicU64::new(0));

        // Build a CMTime for the minimum frame interval.
        // Uses millisecond precision: e.g. 2.0 s → value=2000, timescale=1000.
        let frame_interval = seconds_to_cmtime(config.frame_interval_secs);

        let mut streams: Vec<SCStream> = Vec::with_capacity(displays.len());

        for display in &displays {
            let display_id = display.display_id();
            let width = display.width();
            let height = display.height();

            // Build the content filter once per display and reuse it for both
            // the scale-factor detection and the stream itself.
            let filter = SCContentFilter::create()
                .with_display(display)
                .with_excluding_windows(&[])
                .build();

            // Detect retina scale via SCShareableContentInfo (macOS 14.0+).
            let scale_factor = {
                let info =
                    screencapturekit::shareable_content::SCShareableContentInfo::for_filter(&filter);
                match info {
                    Some(info) => f64::from(info.point_pixel_scale()),
                    None => {
                        // Heuristic: if the pixel width is at least 2x the
                        // display point width reported by the frame, assume 2x.
                        // Since we cannot know for sure without the info object,
                        // default to 2.0 on modern Macs.
                        2.0
                    }
                }
            };

            // Capture at the display's native pixel dimensions.
            let stream_config = SCStreamConfiguration::new()
                .with_width(width)
                .with_height(height)
                .with_pixel_format(PixelFormat::BGRA)
                .with_shows_cursor(true)
                .with_minimum_frame_interval(&frame_interval);

            let handler = FrameHandler::new(
                sender.clone(),
                display_id,
                scale_factor,
                width,
                height,
                Arc::clone(&frames_captured),
                Arc::clone(&frames_dropped),
            );

            let mut stream = SCStream::new(&filter, &stream_config);
            stream.add_output_handler(handler, SCStreamOutputType::Screen);
            if let Err(e) = stream.start_capture() {
                // Stop all previously-started streams before propagating.
                for started in &streams {
                    let _ = started.stop_capture();
                }
                return Err(CaptureError::ScreenCaptureKit(format!(
                    "failed to start capture on display {display_id}: {e}"
                )));
            }

            log::info!(
                "Started capture on display {display_id} ({width}x{height}, scale {scale_factor})"
            );
            streams.push(stream);
        }

        let engine = Self {
            streams,
            _sender: sender,
            frames_captured,
            frames_dropped,
        };

        Ok((engine, receiver))
    }

    /// Stop all active capture streams.
    ///
    /// After calling this the receiver will eventually drain and close.
    pub fn stop(&mut self) -> Result<()> {
        for stream in &self.streams {
            if let Err(e) = stream.stop_capture() {
                log::warn!("Error stopping stream: {e}");
            }
        }
        self.streams.clear();
        Ok(())
    }

    /// Return a point-in-time health snapshot.
    pub fn status(&self) -> CaptureStatus {
        CaptureStatus {
            active_displays: self.streams.len(),
            total_frames_captured: self.frames_captured.load(Ordering::Relaxed),
            total_frames_dropped: self.frames_dropped.load(Ordering::Relaxed),
        }
    }
}

impl Drop for CaptureEngine {
    fn drop(&mut self) {
        for stream in &self.streams {
            if let Err(e) = stream.stop_capture() {
                log::warn!("Failed to stop stream on drop: {e}");
            }
        }
        self.streams.clear();
    }
}

/// Convert a fractional-seconds interval into a `CMTime`.
///
/// Uses millisecond precision (timescale = 1000) so values like 0.5 s or 2.0 s
/// are represented accurately.
fn seconds_to_cmtime(secs: f64) -> CMTime {
    let millis = (secs * 1000.0).round() as i64;
    CMTime::new(millis, 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seconds_to_cmtime_converts_correctly() {
        let time = seconds_to_cmtime(2.0);
        // 2.0 seconds = 2000 value with timescale 1000
        assert_eq!(time.value, 2000);
        assert_eq!(time.timescale, 1000);
    }

    #[test]
    fn seconds_to_cmtime_fractional() {
        let time = seconds_to_cmtime(0.5);
        assert_eq!(time.value, 500);
        assert_eq!(time.timescale, 1000);
    }
}
