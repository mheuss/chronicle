import SwiftUI

/// What a result row's source badge says. Split from the view so tests can
/// reach it.
struct SourceBadge: Equatable {
    let label: String
    let systemImage: String

    static func of(_ hit: SearchHit) -> SourceBadge {
        switch hit.source {
        case .screen:
            return SourceBadge(label: "Screen", systemImage: "display")
        case .audio:
            switch hit.audioSource {
            case "mic": return SourceBadge(label: "Mic", systemImage: "waveform")
            case "system": return SourceBadge(label: "System", systemImage: "waveform")
            default: return SourceBadge(label: "Audio", systemImage: "waveform")
            }
        case .unknown:
            return SourceBadge(label: "Other", systemImage: "questionmark.square")
        }
    }
}

struct ResultRow: View {
    let hit: SearchHit

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 6) {
                sourceBadge
                if showsAppName {
                    Text(hit.appName ?? "Unknown app")
                        .font(.callout).bold()
                    Text("·").foregroundStyle(.secondary)
                }
                Text(relativeTime(hit.timestampMs))
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
            Text(snippetAttributedString(hit.snippet))
                .font(.body)
                .lineLimit(2)
                .foregroundStyle(.primary)
        }
        .padding(.vertical, 4)
    }

    private var showsAppName: Bool {
        switch hit.source {
        case .screen: true
        case .audio, .unknown: false
        }
    }

    private var sourceBadge: some View {
        let badge = SourceBadge.of(hit)
        return HStack(spacing: 3) {
            Image(systemName: badge.systemImage).imageScale(.small)
            Text(badge.label).font(.caption2.weight(.semibold))
        }
        .foregroundStyle(.secondary)
        .padding(.horizontal, 6).padding(.vertical, 2)
        .background(
            RoundedRectangle(cornerRadius: 4).fill(Color.secondary.opacity(0.12))
        )
    }

    private func relativeTime(_ ms: Int64) -> String {
        let date = Date(timeIntervalSince1970: TimeInterval(ms) / 1000.0)
        let formatter = RelativeDateTimeFormatter()
        formatter.unitsStyle = .full
        return formatter.localizedString(for: date, relativeTo: Date())
    }
}
