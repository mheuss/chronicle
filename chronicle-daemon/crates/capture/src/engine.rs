//! Capture engine managing per-display SCStreams.
//!
//! `CaptureEngine` enumerates all connected displays, creates one `SCStream`
//! per display, and delivers frames over a bounded mpsc channel.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::{autoreleasepool, Retained};
use objc2::AnyThread;
use objc2_core_media::CMTime;
use objc2_foundation::{NSArray, NSError};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamOutputType,
};
use tokio::sync::mpsc;

use crate::error::{CaptureError, Result};
use crate::handler::CaptureOutputHandler;
use crate::{CaptureConfig, CaptureStatus, CapturedFrame};

// ---------------------------------------------------------------------------
// CoreGraphics FFI for primary display detection
// ---------------------------------------------------------------------------

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGMainDisplayID() -> u32;
}

/// Multi-display capture engine.
///
/// Call [`CaptureEngine::start`] to enumerate displays, create one `SCStream`
/// per display, and begin delivering `CapturedFrame`s over the returned
/// receiver. Use [`stop`](CaptureEngine::stop) to tear down all streams and
/// [`status`](CaptureEngine::status) to read health counters.
pub struct CaptureEngine {
    streams: Vec<Retained<SCStream>>,
    handlers: Vec<Retained<CaptureOutputHandler>>,
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

        let displays = enumerate_displays()?;
        if displays.is_empty() {
            return Err(CaptureError::NoDisplays);
        }

        let frames_captured = Arc::new(AtomicU64::new(0));
        let frames_dropped = Arc::new(AtomicU64::new(0));

        // Build a CMTime for the minimum frame interval.
        // Uses millisecond precision: e.g. 2.0 s -> value=2000, timescale=1000.
        let frame_interval = seconds_to_cmtime(config.frame_interval_secs);

        // Identify the primary display.
        let primary_display_id = unsafe { CGMainDisplayID() };

        let mut streams: Vec<Retained<SCStream>> = Vec::with_capacity(displays.len());
        let mut handlers: Vec<Retained<CaptureOutputHandler>> = Vec::with_capacity(displays.len());

        for display in &displays {
            let display_id = unsafe { display.displayID() };
            let width = unsafe { display.width() } as u32;
            let height = unsafe { display.height() } as u32;

            // TODO: Detect actual retina scale factor per display. External
            // non-retina monitors (common with Mac mini/Studio/Pro) use 1.0x.
            // The screencapturekit wrapper used SCShareableContentInfo::for_filter
            // for this. We need an objc2 equivalent or a CoreGraphics query
            // (CGDisplayScreenSize + pixel dimensions). For now, default to 2.0
            // which is correct for built-in Apple Silicon displays.
            let scale_factor = 2.0;
            let is_primary = display_id == primary_display_id;

            // Build the content filter and stream configuration.
            let (filter, stream_config) = autoreleasepool(|_| {
                let empty_windows: Retained<NSArray<_>> = NSArray::new();
                let filter = unsafe {
                    SCContentFilter::initWithDisplay_excludingWindows(
                        SCContentFilter::alloc(),
                        display,
                        &empty_windows,
                    )
                };

                let sc_config = unsafe { SCStreamConfiguration::new() };
                unsafe {
                    sc_config.setWidth(width as usize);
                    sc_config.setHeight(height as usize);
                    sc_config.setShowsCursor(true);
                    sc_config.setMinimumFrameInterval(frame_interval);
                    // BGRA = 'BGRA' as FourCC
                    sc_config.setPixelFormat(u32::from_be_bytes(*b"BGRA"));
                }

                // Configure audio capture on the primary display.
                if is_primary && let Some(ref audio) = config.audio {
                    unsafe {
                        sc_config.setCapturesAudio(true);
                        sc_config.setSampleRate(audio.sample_rate as isize);
                        sc_config.setChannelCount(audio.channel_count as isize);
                        sc_config.setExcludesCurrentProcessAudio(true);
                        if audio.capture_microphone {
                            sc_config.setCaptureMicrophone(true);
                        }
                    }
                }

                (filter, sc_config)
            });

            // Create the handler wired to our frame channel.
            let handler = CaptureOutputHandler::new(
                sender.clone(),
                display_id,
                scale_factor,
                width,
                height,
                Arc::clone(&frames_captured),
                Arc::clone(&frames_dropped),
            );

            // Create the SCStream.
            let stream = autoreleasepool(|_| unsafe {
                SCStream::initWithFilter_configuration_delegate(
                    SCStream::alloc(),
                    &filter,
                    &stream_config,
                    None,
                )
            });

            // Register the output handler.
            let queue = DispatchQueue::new(
                &format!("com.chronicle.capture.display-{display_id}"),
                None,
            );

            autoreleasepool(|_| unsafe {
                stream
                    .addStreamOutput_type_sampleHandlerQueue_error(
                        handler.as_protocol_object(),
                        SCStreamOutputType::Screen,
                        Some(&queue),
                    )
                    .map_err(|e| {
                        CaptureError::ScreenCaptureKit(format!(
                            "failed to add output handler for display {display_id}: {}",
                            e.localizedDescription()
                        ))
                    })
            })?;

            // Register audio handler on the primary display's stream.
            if is_primary && let Some(ref audio) = config.audio {
                // System audio — hard error. Core functionality.
                autoreleasepool(|_| unsafe {
                    stream
                        .addStreamOutput_type_sampleHandlerQueue_error(
                            &audio.handler,
                            SCStreamOutputType::Audio,
                            Some(&audio.queue),
                        )
                        .map_err(|e| {
                            CaptureError::ScreenCaptureKit(format!(
                                "failed to register audio handler on primary display: {}",
                                e.localizedDescription()
                            ))
                        })
                })?;

                // Microphone — soft error. Permission may be denied or
                // hardware may be absent. Log and continue.
                if audio.capture_microphone {
                    let mic_result = autoreleasepool(|_| unsafe {
                        stream.addStreamOutput_type_sampleHandlerQueue_error(
                            &audio.handler,
                            SCStreamOutputType::Microphone,
                            Some(&audio.queue),
                        )
                    });
                    if let Err(e) = mic_result {
                        log::warn!(
                            "Microphone registration failed (permission denied?): {}",
                            e.localizedDescription()
                        );
                    }
                }

                log::info!("Registered audio handler on primary display {display_id}");
            }

            // Start capture.
            if let Err(e) = start_stream(&stream) {
                // Stop all previously-started streams before propagating.
                for started in &streams {
                    let _ = stop_stream(started);
                }
                return Err(CaptureError::ScreenCaptureKit(format!(
                    "failed to start capture on display {display_id}: {e}"
                )));
            }

            log::info!(
                "Started capture on display {display_id} ({width}x{height}, scale {scale_factor}{})",
                if is_primary { ", primary" } else { "" }
            );
            streams.push(stream);
            handlers.push(handler);
        }

        let engine = Self {
            streams,
            handlers,
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
            if let Err(e) = stop_stream(stream) {
                log::warn!("Error stopping stream: {e}");
            }
        }
        self.streams.clear();
        self.handlers.clear();
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
            if let Err(e) = stop_stream(stream) {
                log::warn!("Failed to stop stream on drop: {e}");
            }
        }
        self.streams.clear();
        self.handlers.clear();
    }
}

// ---------------------------------------------------------------------------
// SCK helpers
// ---------------------------------------------------------------------------

/// Enumerate all connected displays via SCShareableContent.
///
/// Uses a synchronous channel + block2 callback to bridge the async SCK API.
fn enumerate_displays() -> Result<Vec<Retained<SCDisplay>>> {
    let (tx, rx) =
        std::sync::mpsc::sync_channel::<std::result::Result<Vec<Retained<SCDisplay>>, String>>(1);

    let block = RcBlock::new(move |content: *mut SCShareableContent, error: *mut NSError| {
        if !error.is_null() {
            let desc = unsafe { (*error).localizedDescription() };
            let _ = tx.send(Err(desc.to_string()));
            return;
        }

        if content.is_null() {
            let _ = tx.send(Err("SCShareableContent was null".into()));
            return;
        }

        let content = unsafe { &*content };
        let displays = unsafe { content.displays() };
        let mut result = Vec::with_capacity(displays.len());
        for i in 0..displays.len() {
            result.push(displays.objectAtIndex(i));
        }
        let _ = tx.send(Ok(result));
    });

    unsafe {
        SCShareableContent::getShareableContentWithCompletionHandler(&block);
    }

    rx.recv()
        .map_err(|_| {
            CaptureError::ScreenCaptureKit("display enumeration channel closed".into())
        })?
        .map_err(CaptureError::ScreenCaptureKit)
}

/// Start the SCStream capture, blocking until the completion handler fires.
fn start_stream(stream: &SCStream) -> std::result::Result<(), String> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<Option<String>>(1);

    let block = RcBlock::new(move |error: *mut NSError| {
        if error.is_null() {
            let _ = tx.send(None);
        } else {
            let desc = unsafe { (*error).localizedDescription() };
            let _ = tx.send(Some(desc.to_string()));
        }
    });

    unsafe {
        stream.startCaptureWithCompletionHandler(Some(&block));
    }

    match rx.recv() {
        Ok(None) => Ok(()),
        Ok(Some(err)) => Err(err),
        Err(_) => Err("start capture completion handler channel closed".into()),
    }
}

/// Stop the SCStream capture, blocking until the completion handler fires.
fn stop_stream(stream: &SCStream) -> std::result::Result<(), String> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<Option<String>>(1);

    let block = RcBlock::new(move |error: *mut NSError| {
        if error.is_null() {
            let _ = tx.send(None);
        } else {
            let desc = unsafe { (*error).localizedDescription() };
            let _ = tx.send(Some(desc.to_string()));
        }
    });

    unsafe {
        stream.stopCaptureWithCompletionHandler(Some(&block));
    }

    match rx.recv() {
        Ok(None) => Ok(()),
        Ok(Some(err)) => Err(err),
        Err(_) => Err("stop capture completion handler channel closed".into()),
    }
}

/// Convert a fractional-seconds interval into a `CMTime`.
///
/// Uses millisecond precision (timescale = 1000) so values like 0.5 s or 2.0 s
/// are represented accurately.
fn seconds_to_cmtime(secs: f64) -> CMTime {
    let millis = (secs * 1000.0).round() as i64;
    unsafe { CMTime::new(millis, 1000) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seconds_to_cmtime_converts_correctly() {
        let time = seconds_to_cmtime(2.0);
        // CMTime is packed — copy fields to locals to avoid unaligned access.
        let value = { time.value };
        let timescale = { time.timescale };
        assert_eq!(value, 2000);
        assert_eq!(timescale, 1000);
    }

    #[test]
    fn seconds_to_cmtime_fractional() {
        let time = seconds_to_cmtime(0.5);
        let value = { time.value };
        let timescale = { time.timescale };
        assert_eq!(value, 500);
        assert_eq!(timescale, 1000);
    }
}
