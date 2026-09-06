//! Feature-gated microphone characterization (HEU-650).
//!
//! Records what the input-node tap receives and what `AVAudioConverter`
//! produces from it, as two 32-bit float WAV files, for offline analysis. The
//! audio thread only fills a [`CharacterizationFrame`] and `try_send`s it; a
//! plain thread writes the files (ADR-013). Compiled only with
//! `--features characterize`.

use crate::microphone::ConversionOutcome;

/// One tap callback's worth of data: both sides of the conversion for the same
/// input buffer, so the writer never has to pair two streams.
#[derive(Debug, Clone, PartialEq)]
pub struct CharacterizationFrame {
    /// Monotonic per-session counter, assigned in the callback. A gap means a
    /// frame was dropped between the tap and the writer.
    pub seq: u64,
    /// Native input, one `Vec` per channel, already stride-corrected. Every
    /// channel has the same length.
    ///
    /// Empty means the callback got a buffer with frames it could not read as
    /// f32 planes. The `seq` was still consumed. Treat such a frame as
    /// malformed, never as silence: it is a hole in the recording.
    pub native: Vec<Vec<f32>>,
    /// The converter's result for this same buffer.
    pub outcome: ConversionOutcome,
}
