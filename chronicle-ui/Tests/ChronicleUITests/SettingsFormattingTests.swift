import Testing

@testable import ChronicleUI

/// The pure formatting helpers behind the Settings transcription group. Same
/// shape as `SearchContentStateTests`: static functions on the view, so the
/// wording can be pinned without driving SwiftUI.
///
/// Sizes are asserted by their digits, not by the whole formatted string.
/// `ByteCountFormatter` output is locale- and OS-dependent ("148 MB" today),
/// and pinning it exactly would fail somewhere that has nothing to do with
/// this code. The separators and the "not downloaded" wording ARE ours, so
/// those are pinned exactly.
@Suite("Settings formatting")
struct SettingsFormattingTests {
    private func model(_ variant: String, downloaded: Bool, _ size: UInt64) -> ModelEntry {
        ModelEntry(variant: variant, downloaded: downloaded, sizeBytes: size)
    }

    @Test("a downloaded model reads as variant and size, with no caveat")
    func downloadedPickerLabel() {
        let label = SettingsView.pickerLabel(model("base", downloaded: true, 148_000_000))
        #expect(label.hasPrefix("base — "))
        #expect(label.contains("148"))
        #expect(!label.contains("not downloaded"))
    }

    // The size the user is about to spend disk and bandwidth on, and the fact
    // that picking it starts a download, are the two things this label exists
    // to say. 488, not the design's 466 — see the design addendum; the number
    // comes from the daemon's manifest, so the test carries a real one.
    @Test("a model not on disk says so in words")
    func undownloadedPickerLabel() {
        let label = SettingsView.pickerLabel(model("small", downloaded: false, 488_000_000))
        #expect(label.hasPrefix("small — "))
        #expect(label.contains("488"))
        #expect(label.hasSuffix(", not downloaded"))
    }

    @Test("the summary ticks what is on disk and dashes what is not")
    func summaryMarksDownloads() {
        let summary = SettingsView.downloadedSummary([
            model("base", downloaded: true, 148_000_000),
            model("small", downloaded: false, 488_000_000),
            model("medium", downloaded: false, 1_534_000_000),
        ])
        #expect(summary.hasPrefix("base ✓ "))
        #expect(summary.contains("148"))
        #expect(summary.contains(" · small —"))
        #expect(summary.hasSuffix(" · medium —"))
        // An absent model has no size worth quoting here — the picker is where
        // the user goes to find out what a download would cost.
        #expect(!summary.contains("488"))
    }

    @Test("a models list the daemon did not send degrades to a dash")
    func summaryWithNoModels() {
        #expect(SettingsView.downloadedSummary([]) == "—")
    }

    // The picker must not accept a change the daemon is in no position to act
    // on. Delegating to `TranscriptionBannerCopy` rather than re-listing the
    // in-flight states keeps that exhaustive switch the single place a new
    // state has to be classified.
    @Test("the picker is disabled while an operation is in flight")
    func pickerDisabledWhileProvisioning() {
        for state in [TranscriptionState.downloading, .verifying, .loading] {
            #expect(!SettingsView.pickerEnabled(connected: true, transcription: stats(state)),
                    "\(state) is in flight")
        }
    }

    // `.error` is the case that matters here: a failed switch is exactly when
    // the user wants to pick something else, so the picker stays live even
    // though an action button is also on screen. Same for `.missing`, which
    // carries a Download button beside an enabled picker.
    @Test("the picker is enabled once the state settles")
    func pickerEnabledWhenSettled() {
        for state in [TranscriptionState.ready, .missing, .error, .unknown] {
            #expect(SettingsView.pickerEnabled(connected: true, transcription: stats(state)),
                    "\(state) can accept a change")
        }
    }

    @Test("a disconnected daemon or a missing block disables the picker")
    func pickerDisabledWithoutADaemon() {
        #expect(!SettingsView.pickerEnabled(connected: false, transcription: stats(.ready)))
        #expect(!SettingsView.pickerEnabled(connected: true, transcription: nil))
    }

    private func stats(_ state: TranscriptionState) -> TranscriptionStats {
        TranscriptionStats(
            state: state, variant: "base", loadedVariant: nil, error: nil,
            downloadBytes: nil, downloadTotalBytes: nil, models: [])
    }
}
