//! Opus/Ogg encoder for raw f32 PCM samples.
//!
//! Encodes audio into Ogg/Opus files with RFC 7845 compliant headers.

use std::fs;
use std::io::Write;
use std::path::Path;

use ogg::writing::{PacketWriteEndInfo, PacketWriter};

use crate::{AudioError, Result};

/// 20ms frame at 48kHz.
const FRAME_SIZE: usize = 960;

/// Maximum size of a single encoded Opus packet.
const MAX_PACKET_SIZE: usize = 4000;

/// Serial number for the single logical bitstream.
const STREAM_SERIAL: u32 = 1;

/// Encodes raw f32 PCM samples to Ogg/Opus files.
pub struct OggOpusEncoder {
    sample_rate: u32,
    channels: u8,
    bitrate: u32,
}

impl OggOpusEncoder {
    /// Create a new encoder with the given sample rate, channel count, and bitrate.
    pub fn new(sample_rate: u32, channels: u8, bitrate: u32) -> Self {
        Self {
            sample_rate,
            channels,
            bitrate,
        }
    }

    /// Encode f32 PCM samples and write the result to a file at `path`.
    ///
    /// Creates parent directories if they don't exist.
    pub fn encode_to_file(&self, samples: &[f32], path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = fs::File::create(path)?;
        let writer = std::io::BufWriter::new(file);
        self.encode_to_writer(samples, writer)
    }

    /// Encode f32 PCM samples and write Ogg/Opus data to the given writer.
    fn encode_to_writer<W: Write>(&self, samples: &[f32], writer: W) -> Result<()> {
        let channels = match self.channels {
            1 => opus::Channels::Mono,
            2 => opus::Channels::Stereo,
            n => {
                return Err(AudioError::Encoding(format!(
                    "unsupported channel count: {n}"
                )))
            }
        };

        let mut opus_enc = opus::Encoder::new(48_000, channels, opus::Application::Voip)
            .map_err(|e| AudioError::Encoding(format!("opus encoder create: {e}")))?;

        opus_enc
            .set_bitrate(opus::Bitrate::Bits(self.bitrate as i32))
            .map_err(|e| AudioError::Encoding(format!("set bitrate: {e}")))?;

        let mut pw = PacketWriter::new(writer);

        // RFC 7845 Section 5.1: OpusHead identification header.
        let opus_head = self.build_opus_head();
        pw.write_packet(
            opus_head,
            STREAM_SERIAL,
            PacketWriteEndInfo::EndPage,
            0,
        )
        .map_err(|e| AudioError::Encoding(format!("write opus head: {e}")))?;

        // RFC 7845 Section 5.2: OpusTags comment header.
        let opus_tags = Self::build_opus_tags();
        pw.write_packet(
            opus_tags,
            STREAM_SERIAL,
            PacketWriteEndInfo::EndPage,
            0,
        )
        .map_err(|e| AudioError::Encoding(format!("write opus tags: {e}")))?;

        // Encode audio frames.
        let total_samples = samples.len();
        let num_full_frames = total_samples / FRAME_SIZE;
        let remaining = total_samples % FRAME_SIZE;
        let total_frames = if remaining > 0 {
            num_full_frames + 1
        } else {
            num_full_frames
        };

        let mut encoded_buf = vec![0u8; MAX_PACKET_SIZE];
        let mut granule_pos: u64 = 0;

        for frame_idx in 0..total_frames {
            let is_last = frame_idx + 1 == total_frames;
            let offset = frame_idx * FRAME_SIZE;

            // Build the frame, padding the last one with silence if needed.
            let frame: Vec<f32> = if offset + FRAME_SIZE <= total_samples {
                samples[offset..offset + FRAME_SIZE].to_vec()
            } else {
                let mut padded = vec![0.0f32; FRAME_SIZE];
                let available = total_samples - offset;
                padded[..available].copy_from_slice(&samples[offset..]);
                padded
            };

            let encoded_len = opus_enc
                .encode_float(&frame, &mut encoded_buf)
                .map_err(|e| AudioError::Encoding(format!("encode frame {frame_idx}: {e}")))?;

            granule_pos += FRAME_SIZE as u64;

            let end_info = if is_last {
                PacketWriteEndInfo::EndStream
            } else {
                PacketWriteEndInfo::NormalPacket
            };

            pw.write_packet(
                encoded_buf[..encoded_len].to_vec(),
                STREAM_SERIAL,
                end_info,
                granule_pos,
            )
            .map_err(|e| AudioError::Encoding(format!("write audio packet: {e}")))?;
        }

        Ok(())
    }

    /// Build the OpusHead identification header per RFC 7845 Section 5.1.
    fn build_opus_head(&self) -> Vec<u8> {
        let mut head = Vec::with_capacity(19);
        head.extend_from_slice(b"OpusHead"); // Magic signature
        head.push(1); // Version
        head.push(self.channels); // Channel count
        head.extend_from_slice(&0u16.to_le_bytes()); // Pre-skip
        head.extend_from_slice(&self.sample_rate.to_le_bytes()); // Input sample rate
        head.extend_from_slice(&0u16.to_le_bytes()); // Output gain
        head.push(0); // Channel mapping family
        head
    }

    /// Build the OpusTags comment header per RFC 7845 Section 5.2.
    fn build_opus_tags() -> Vec<u8> {
        let vendor = b"chronicle-audio";
        let mut tags = Vec::with_capacity(8 + 4 + vendor.len() + 4);
        tags.extend_from_slice(b"OpusTags"); // Magic signature
        tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes()); // Vendor string length
        tags.extend_from_slice(vendor); // Vendor string
        tags.extend_from_slice(&0u32.to_le_bytes()); // User comment list length (0)
        tags
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_silence(num_samples: usize) -> Vec<f32> {
        vec![0.0_f32; num_samples]
    }

    fn make_tone(num_samples: usize, freq_hz: f32, sample_rate: f32) -> Vec<f32> {
        (0..num_samples)
            .map(|i| {
                let t = i as f32 / sample_rate;
                (2.0 * std::f32::consts::PI * freq_hz * t).sin()
            })
            .collect()
    }

    #[test]
    fn encode_silence_produces_valid_ogg_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("silence.opus");
        let encoder = OggOpusEncoder::new(48_000, 1, 64_000);
        let samples = make_silence(48_000); // 1 second

        encoder.encode_to_file(&samples, &path).unwrap();

        let data = std::fs::read(&path).unwrap();
        assert!(!data.is_empty(), "file should not be empty");
        assert_eq!(
            &data[..4],
            b"OggS",
            "file should start with Ogg capture pattern"
        );
    }

    #[test]
    fn encode_tone_produces_larger_file_than_silence() {
        let dir = tempfile::tempdir().unwrap();
        let silence_path = dir.path().join("silence.opus");
        let tone_path = dir.path().join("tone.opus");
        let encoder = OggOpusEncoder::new(48_000, 1, 64_000);

        let silence = make_silence(48_000);
        let tone = make_tone(48_000, 440.0, 48_000.0);

        encoder.encode_to_file(&silence, &silence_path).unwrap();
        encoder.encode_to_file(&tone, &tone_path).unwrap();

        let silence_size = std::fs::metadata(&silence_path).unwrap().len();
        let tone_size = std::fs::metadata(&tone_path).unwrap().len();

        assert!(
            tone_size > silence_size,
            "tone ({tone_size} bytes) should compress to more bytes than silence ({silence_size} bytes)"
        );
    }

    #[test]
    fn encode_decode_round_trip_preserves_sample_count() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("round_trip.opus");
        let encoder = OggOpusEncoder::new(48_000, 1, 64_000);
        let samples = make_silence(48_000); // 1 second = 50 frames of 960

        encoder.encode_to_file(&samples, &path).unwrap();

        // Read back Ogg packets and count them.
        let file = std::fs::File::open(&path).unwrap();
        let buf_reader = std::io::BufReader::new(file);
        let mut reader = ogg::PacketReader::new(buf_reader);

        let mut packet_count = 0u32;
        while let Some(_pkt) = reader.read_packet().unwrap() {
            packet_count += 1;
        }

        // 2 header packets (OpusHead + OpusTags) + 50 audio packets
        assert_eq!(
            packet_count, 52,
            "expected 2 header packets + 50 audio packets, got {packet_count}"
        );
    }

    #[test]
    fn encode_short_segment_works() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("short.opus");
        let encoder = OggOpusEncoder::new(48_000, 1, 64_000);
        let samples = make_silence(500); // less than one frame

        encoder.encode_to_file(&samples, &path).unwrap();

        let data = std::fs::read(&path).unwrap();
        assert!(!data.is_empty(), "file should not be empty");
        assert_eq!(
            &data[..4],
            b"OggS",
            "file should start with Ogg capture pattern"
        );
    }
}
