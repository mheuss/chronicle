//! The transcription attempt cap.

/// Attempts after which a row is given up on. Shared by every path that
/// transcribes, so a row is tried at most this many times in total.
pub const MAX_TRANSCRIBE_ATTEMPTS: u32 = 3;
