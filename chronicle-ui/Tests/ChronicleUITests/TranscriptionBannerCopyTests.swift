import Testing
import Foundation
@testable import ChronicleUI

@Suite("Transcription banner copy")
struct TranscriptionBannerCopyTests {

    private func stats(
        _ state: TranscriptionState,
        variant: String = "base",
        loaded: String? = nil,
        error: String? = nil,
        models: [ModelEntry] = [ModelEntry(variant: "base", downloaded: false, sizeBytes: 148_000_000)]
    ) -> TranscriptionStats {
        TranscriptionStats(
            state: state, variant: variant, loadedVariant: loaded, error: error,
            downloadBytes: nil, downloadTotalBytes: nil, models: models)
    }

    @Test("missing offers a download with the advertised size")
    func missingOffersDownload() {
        let copy = TranscriptionBannerCopy(stats(.missing))
        #expect(copy.headline.contains("Download the base model"))
        #expect(copy.headline.contains("148"), "the size comes from the daemon's models list")
        #expect(copy.action == .init(title: "Download", variant: "base"))
        #expect(!copy.showsProgress)
    }

    // The button and the sentence must act on the same variant — a Download
    // button that fetches something other than what the copy named would be
    // worse than no button.
    @Test("the action targets the variant the copy names")
    func actionMatchesCopy() {
        let copy = TranscriptionBannerCopy(stats(.missing, variant: "small"))
        #expect(copy.headline.contains("small model"))
        #expect(copy.action?.variant == "small")
    }

    @Test("an unknown variant degrades instead of lying about the size")
    func missingSizeDegrades() {
        let copy = TranscriptionBannerCopy(stats(.missing, variant: "medium", models: []))
        #expect(copy.headline.contains("size unknown"))
    }

    @Test("in-flight states show progress and offer no action")
    func inFlightShowsProgress() {
        for state in [TranscriptionState.downloading, .verifying, .loading] {
            let copy = TranscriptionBannerCopy(stats(state))
            #expect(copy.showsProgress, "\(state) is in flight")
            #expect(copy.action == nil, "\(state) has nothing to retry yet")
            #expect(!copy.isFailure)
        }
    }

    // A failed SWITCH still has an engine serving. Saying "transcription is
    // off" there would be false, and it is the case where the user most needs
    // to know they did not lose what they had.
    @Test("a failed switch names the engine still serving")
    func failedSwitchNamesServingEngine() {
        let copy = TranscriptionBannerCopy(
            stats(.error, variant: "small", loaded: "base", error: "download failed: timed out"))
        #expect(copy.headline == "Switch to small failed — still using base.")
        #expect(copy.detail == "download failed: timed out")
        #expect(copy.action == .init(title: "Retry", variant: "small"))
    }

    // The recorded decision (plan Decisions, 2026-08-01). A corrupt-but-present
    // model at boot reaches `.error` with `loadedVariant` nil having downloaded
    // NOTHING, so the design's "Download failed" copy would be a lie. Pins the
    // neutral wording, and that the daemon's own reason still gets through.
    @Test("a corrupt model at boot does not claim a download failed")
    func corruptModelDoesNotBlameADownload() {
        let copy = TranscriptionBannerCopy(
            stats(.error, error: "model load failed: invalid ggml header"))
        #expect(!copy.headline.lowercased().contains("download"))
        #expect(copy.headline == "Audio transcription is off.")
        #expect(copy.detail == "model load failed: invalid ggml header")
        #expect(copy.action == .init(title: "Retry", variant: "base"))
    }

    @Test("detail is dropped when the daemon sends no reason")
    func detailAbsentWithoutError() {
        #expect(TranscriptionBannerCopy(stats(.error)).detail == nil)
        #expect(TranscriptionBannerCopy(stats(.error, error: "")).detail == nil)
        #expect(TranscriptionBannerCopy(stats(.missing, error: "stale")).detail == nil,
                "a leftover error string must not leak into a non-error state")
    }

    @Test("ready and unknown say transcription is on and offer nothing")
    func resolvedStatesAreQuiet() {
        for state in [TranscriptionState.ready, .unknown] {
            let copy = TranscriptionBannerCopy(stats(state))
            #expect(copy.action == nil)
            #expect(!copy.showsProgress)
            #expect(!copy.isFailure)
            #expect(copy.headline == "Audio transcription is on.")
            // The banner can render one frame of this copy before `evaluate`
            // hides it, so the icon must not contradict the sentence.
            #expect(copy.icon == "waveform", "the OFF icon would be a lie here")
        }
    }

    // The banner is hidden for a block-less daemon, so this only guards
    // against a crash if that ever changes.
    @Test("a nil block falls back without trapping")
    func nilBlockFallsBack() {
        let copy = TranscriptionBannerCopy(nil)
        #expect(copy.variant == "base")
        #expect(copy.action != nil)
    }
}
