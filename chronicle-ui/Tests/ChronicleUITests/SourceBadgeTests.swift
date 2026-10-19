import Testing

@testable import ChronicleUI

@Suite("Search result source badge")
struct SourceBadgeTests {
    private func hit(_ source: SearchHitSource, audioSource: String? = nil) -> SearchHit {
        SearchHit(
            id: 1, source: source, timestampMs: 0, appName: nil, appBundleId: nil,
            windowTitle: nil, imagePath: nil, endTimestampMs: nil,
            audioSource: audioSource, snippet: "", rank: 0
        )
    }

    @Test("each source gets its own badge")
    func badges() {
        #expect(SourceBadge.of(hit(.screen)) == SourceBadge(label: "Screen", systemImage: "display"))
        #expect(SourceBadge.of(hit(.audio, audioSource: "mic")) == SourceBadge(label: "Mic", systemImage: "waveform"))
        #expect(SourceBadge.of(hit(.audio, audioSource: "system")) == SourceBadge(label: "System", systemImage: "waveform"))
        #expect(SourceBadge.of(hit(.audio)) == SourceBadge(label: "Audio", systemImage: "waveform"))
        #expect(SourceBadge.of(hit(.unknown)) == SourceBadge(label: "Other", systemImage: "questionmark.square"))
    }
}
