import SwiftUI

struct ResultRow: View {
    let hit: SearchHit

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 6) {
                sourceBadge
                Text(hit.appName ?? "Unknown app")
                    .font(.callout).bold()
                Text("·").foregroundStyle(.secondary)
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

    private var sourceBadge: some View {
        HStack(spacing: 3) {
            Image(systemName: "display").imageScale(.small)
            Text("Screen").font(.caption2.weight(.semibold))
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
