//! Counters for buffers the real-time audio callbacks discarded.

use std::sync::atomic::{AtomicU64, Ordering};

/// Buffers the real-time callbacks discarded.
///
/// Incremented on the audio thread with a single relaxed `fetch_add` —
/// ADR-013 allows counting there and nothing else. All reporting happens
/// off-thread in the daemon's drop reporter.
///
/// Owned by `AudioPipeline`, which is constructed once and never rebuilt, so
/// these counts are monotonic for the life of the process. That is what lets
/// the reporter compute deltas by plain subtraction with no restart case.
#[derive(Default)]
pub struct AudioDropCounters {
    /// Mic buffer dropped: encoding channel full (downstream slow).
    pub mic_full: AtomicU64,
    /// Mic buffer dropped: encoding channel closed (downstream gone).
    pub mic_closed: AtomicU64,
    /// Mic buffer dropped: `AVAudioConverter` returned `Error`.
    pub mic_convert_failed: AtomicU64,
    /// System audio buffer dropped: encoding channel full.
    pub system_full: AtomicU64,
    /// System audio buffer dropped: encoding channel closed.
    pub system_closed: AtomicU64,
}

/// Point-in-time copy, mirroring `CountersSnapshot` in the daemon's
/// `pipeline/counters.rs`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AudioDropSnapshot {
    pub mic_full: u64,
    pub mic_closed: u64,
    pub mic_convert_failed: u64,
    pub system_full: u64,
    pub system_closed: u64,
}

impl AudioDropCounters {
    pub fn snapshot(&self) -> AudioDropSnapshot {
        AudioDropSnapshot {
            mic_full: self.mic_full.load(Ordering::Relaxed),
            mic_closed: self.mic_closed.load(Ordering::Relaxed),
            mic_convert_failed: self.mic_convert_failed.load(Ordering::Relaxed),
            system_full: self.system_full.load(Ordering::Relaxed),
            system_closed: self.system_closed.load(Ordering::Relaxed),
        }
    }
}

/// Which of the two audio producers a dropped buffer came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AudioSourceKind {
    /// The SCK system-audio delivery callback.
    System,
    /// The `AVAudioEngine` microphone tap.
    Microphone,
}

/// Send one audio buffer and account for the outcome.
///
/// The whole "try_send, then classify, then count" path in one place, so a test
/// can drive it with a real channel. Both callback bodies reduce to a call,
/// which is the point: a counter increment written *inside* an ObjC delivery
/// callback or an `RcBlock` tap is invisible to every test, because invoking
/// those needs a real `CMSampleBuffer` or real audio hardware. Here it is not.
pub(crate) fn send_audio<T>(
    sender: &std::sync::mpsc::SyncSender<T>,
    buffer: T,
    counters: &AudioDropCounters,
    from: AudioSourceKind,
) {
    let Err(e) = sender.try_send(buffer) else {
        return;
    };
    let full = matches!(e, std::sync::mpsc::TrySendError::Full(_));
    match (from, full) {
        (AudioSourceKind::System, true) => counters.system_full.fetch_add(1, Ordering::Relaxed),
        (AudioSourceKind::System, false) => counters.system_closed.fetch_add(1, Ordering::Relaxed),
        (AudioSourceKind::Microphone, true) => counters.mic_full.fetch_add(1, Ordering::Relaxed),
        (AudioSourceKind::Microphone, false) => counters.mic_closed.fetch_add(1, Ordering::Relaxed),
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_reads_every_field() {
        let c = AudioDropCounters::default();
        // Every value distinct: a swapped pair inside snapshot() must fail
        // this test, and two fields sharing a value would hide exactly that.
        c.mic_full.fetch_add(3, Ordering::Relaxed);
        c.mic_closed.fetch_add(1, Ordering::Relaxed);
        c.mic_convert_failed.fetch_add(4, Ordering::Relaxed);
        c.system_full.fetch_add(2, Ordering::Relaxed);
        c.system_closed.fetch_add(5, Ordering::Relaxed);

        let s = c.snapshot();

        assert_eq!(s.mic_full, 3);
        assert_eq!(s.mic_closed, 1);
        assert_eq!(s.mic_convert_failed, 4);
        assert_eq!(s.system_full, 2);
        assert_eq!(s.system_closed, 5);
    }

    #[test]
    fn send_audio_counts_each_source_and_cause_separately() {
        let c = AudioDropCounters::default();

        // Success touches nothing.
        let (tx, rx) = std::sync::mpsc::sync_channel::<u8>(1);
        send_audio(&tx, 1, &c, AudioSourceKind::System);
        assert_eq!(c.snapshot(), AudioDropSnapshot::default());

        // Full, from each source.
        send_audio(&tx, 2, &c, AudioSourceKind::System);
        assert_eq!(c.snapshot().system_full, 1);
        send_audio(&tx, 3, &c, AudioSourceKind::Microphone);
        assert_eq!(c.snapshot().mic_full, 1);
        assert_eq!(c.snapshot().system_full, 1, "mic must not bump system");

        // Closed, from each source.
        drop(rx);
        send_audio(&tx, 4, &c, AudioSourceKind::System);
        assert_eq!(c.snapshot().system_closed, 1);
        send_audio(&tx, 5, &c, AudioSourceKind::Microphone);
        assert_eq!(c.snapshot().mic_closed, 1);

        // Nothing bled across the four fields.
        let s = c.snapshot();
        assert_eq!(
            (s.system_full, s.system_closed, s.mic_full, s.mic_closed),
            (1, 1, 1, 1)
        );
        assert_eq!(s.mic_convert_failed, 0);
    }

    #[test]
    fn snapshot_of_fresh_counters_is_all_zero() {
        assert_eq!(
            AudioDropCounters::default().snapshot(),
            AudioDropSnapshot::default()
        );
    }
}
