//! Segment accumulator — buffers PCM samples and triggers encoding at segment boundaries.

use std::path::Path;
use std::sync::mpsc;

use crate::encoder::OggOpusEncoder;
use crate::{segment_path, AudioSource, CompletedSegment, Result};

/// Buffers incoming PCM samples for a single audio source and encodes them
/// to Opus/Ogg files when a segment boundary is reached.
pub struct SegmentAccumulator {
    source: AudioSource,
    buffer: Vec<f32>,
    samples_per_segment: usize,
    sample_rate: u32,
    encoder: OggOpusEncoder,
    output_dir: Box<Path>,
    segment_start_ms: Option<i64>,
    sender: mpsc::Sender<CompletedSegment>,
}

impl SegmentAccumulator {
    /// Create a new accumulator for the given source.
    ///
    /// `segment_duration_secs` controls how many seconds of audio accumulate
    /// before a segment is flushed to disk and sent over the channel.
    pub fn new(
        source: AudioSource,
        sample_rate: u32,
        segment_duration_secs: u32,
        bitrate: u32,
        output_dir: &Path,
        sender: mpsc::Sender<CompletedSegment>,
    ) -> Self {
        let samples_per_segment = sample_rate as usize * segment_duration_secs as usize;
        let encoder = OggOpusEncoder::new(sample_rate, 1, bitrate);
        Self {
            source,
            buffer: Vec::new(),
            samples_per_segment,
            sample_rate,
            encoder,
            output_dir: output_dir.into(),
            segment_start_ms: None,
            sender,
        }
    }

    /// Push new PCM samples into the buffer.
    ///
    /// `timestamp_ms` is the wall-clock timestamp of the first sample in this
    /// push. On the first push of a segment it becomes the segment start time.
    ///
    /// When enough samples have accumulated, the segment is encoded and sent.
    /// If a single push contains more than one segment worth of data, multiple
    /// segments are flushed.
    pub fn push(&mut self, samples: &[f32], timestamp_ms: i64) -> Result<()> {
        if self.segment_start_ms.is_none() {
            self.segment_start_ms = Some(timestamp_ms);
        }

        self.buffer.extend_from_slice(samples);

        while self.buffer.len() >= self.samples_per_segment {
            let segment_samples: Vec<f32> =
                self.buffer.drain(..self.samples_per_segment).collect();
            self.flush_segment(&segment_samples)?;
            // Next segment starts right after the previous one ended.
            let duration_ms =
                (self.samples_per_segment as f64 / self.sample_rate as f64 * 1000.0) as i64;
            self.segment_start_ms = Some(self.segment_start_ms.unwrap() + duration_ms);
        }

        Ok(())
    }

    /// Flush remaining buffered samples as a partial segment.
    ///
    /// Call this when audio capture stops. No-op if the buffer is empty.
    pub fn flush(&mut self) -> Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }

        let samples: Vec<f32> = self.buffer.drain(..).collect();
        self.flush_segment(&samples)?;
        self.segment_start_ms = None;
        Ok(())
    }

    /// Encode samples to a file and send a CompletedSegment over the channel.
    fn flush_segment(&mut self, samples: &[f32]) -> Result<()> {
        let start_ms = self.segment_start_ms.unwrap_or(0);
        let duration_ms = (samples.len() as f64 / self.sample_rate as f64 * 1000.0) as i64;
        let end_ms = start_ms + duration_ms;

        let path = segment_path(&self.output_dir, start_ms, self.source);
        self.encoder.encode_to_file(samples, &path)?;

        let segment = CompletedSegment {
            source: self.source,
            path,
            start_timestamp: start_ms,
            end_timestamp: end_ms,
        };

        // Best-effort send — ignore channel errors (receiver may have dropped).
        let _ = self.sender.send(segment);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    const SAMPLE_RATE: u32 = 48_000;
    const SEGMENT_SECS: u32 = 1;
    const BITRATE: u32 = 64_000;
    const SAMPLES_PER_SEGMENT: usize = SAMPLE_RATE as usize * SEGMENT_SECS as usize;

    fn make_accumulator(
        dir: &Path,
        sender: mpsc::Sender<CompletedSegment>,
    ) -> SegmentAccumulator {
        SegmentAccumulator::new(
            AudioSource::Microphone,
            SAMPLE_RATE,
            SEGMENT_SECS,
            BITRATE,
            dir,
            sender,
        )
    }

    fn make_silence(num_samples: usize) -> Vec<f32> {
        vec![0.0_f32; num_samples]
    }

    #[test]
    fn accumulator_flushes_at_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::channel();
        let mut acc = make_accumulator(dir.path(), tx);

        let timestamp_ms = 1_700_000_000_000_i64;
        let samples = make_silence(SAMPLES_PER_SEGMENT); // exactly 1 second

        acc.push(&samples, timestamp_ms).unwrap();

        let segment = rx.try_recv().expect("should have received one segment");
        assert_eq!(segment.source, AudioSource::Microphone);
        assert_eq!(segment.start_timestamp, timestamp_ms);
        let expected_end = timestamp_ms + 1000; // 1 second = 1000ms
        assert_eq!(segment.end_timestamp, expected_end);
        assert!(segment.path.exists(), "encoded file should exist on disk");

        // No more segments.
        assert!(rx.try_recv().is_err(), "should have no more segments");
    }

    #[test]
    fn accumulator_partial_flush_on_stop() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::channel();
        let mut acc = make_accumulator(dir.path(), tx);

        let timestamp_ms = 1_700_000_000_000_i64;
        let samples = make_silence(SAMPLES_PER_SEGMENT / 2); // 0.5 seconds

        acc.push(&samples, timestamp_ms).unwrap();

        // No segment yet — we're below the boundary.
        assert!(
            rx.try_recv().is_err(),
            "should not produce a segment before boundary"
        );

        // Flush remaining on stop.
        acc.flush().unwrap();

        let segment = rx.try_recv().expect("flush should produce a partial segment");
        assert_eq!(segment.source, AudioSource::Microphone);
        assert_eq!(segment.start_timestamp, timestamp_ms);
        let expected_end = timestamp_ms + 500; // 0.5 seconds = 500ms
        assert_eq!(segment.end_timestamp, expected_end);
        assert!(segment.path.exists(), "encoded file should exist on disk");
    }

    #[test]
    fn accumulator_handles_multiple_segments() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::channel();
        let mut acc = make_accumulator(dir.path(), tx);

        let timestamp_ms = 1_700_000_000_000_i64;
        // 2.5 seconds = 2 full segments + 0.5 partial
        let samples = make_silence(SAMPLES_PER_SEGMENT * 2 + SAMPLES_PER_SEGMENT / 2);

        acc.push(&samples, timestamp_ms).unwrap();

        // Should have produced 2 full segments.
        let seg1 = rx.try_recv().expect("should have first segment");
        assert_eq!(seg1.start_timestamp, timestamp_ms);
        assert_eq!(seg1.end_timestamp, timestamp_ms + 1000);
        assert!(seg1.path.exists());

        let seg2 = rx.try_recv().expect("should have second segment");
        assert_eq!(seg2.start_timestamp, timestamp_ms + 1000);
        assert_eq!(seg2.end_timestamp, timestamp_ms + 2000);
        assert!(seg2.path.exists());

        // No third segment yet.
        assert!(rx.try_recv().is_err(), "no third segment before flush");

        // Flush remaining 0.5s.
        acc.flush().unwrap();

        let seg3 = rx.try_recv().expect("flush should produce third partial segment");
        assert_eq!(seg3.start_timestamp, timestamp_ms + 2000);
        assert_eq!(seg3.end_timestamp, timestamp_ms + 2500);
        assert!(seg3.path.exists());
    }
}
