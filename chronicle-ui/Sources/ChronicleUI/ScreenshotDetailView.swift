import SwiftUI
import AppKit

struct ScreenshotDetailView: View {
    let id: Int64
    @Environment(DaemonConnection.self) private var connection
    @State private var state: LoadState = .loading

    enum LoadState {
        case loading
        case loaded(SearchHit, NSImage)
        case notFound
        case fileMissing(SearchHit)
        case error(String)
    }

    var body: some View {
        Group {
            switch state {
            case .loading:
                ProgressView("Loading…")
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            case .loaded(let hit, let img):
                VStack(spacing: 0) {
                    // Native size inside a scroll view (design BR-3 says
                    // "the HEIF at native size"). User scrolls or resizes
                    // the window to fit. Zoom controls and fit-to-window
                    // are deferred to a follow-up polish ticket.
                    ScrollView([.horizontal, .vertical]) {
                        Image(nsImage: img)
                            .accessibilityLabel(accessibilityLabel(for: hit))
                    }
                    Divider()
                    metadataStrip(hit)
                }
            case .notFound:
                ContentUnavailableView(
                    "Screenshot removed",
                    systemImage: "trash",
                    description: Text("This screenshot has been removed (retention cleanup).")
                )
            case .fileMissing:
                ContentUnavailableView(
                    "Image file missing",
                    systemImage: "questionmark.folder",
                    description: Text("The metadata is present but the image file was not found on disk.")
                )
            case .error(let msg):
                ContentUnavailableView(
                    "Failed to load",
                    systemImage: "exclamationmark.triangle",
                    description: Text(msg)
                )
            }
        }
        .task(id: id) {
            await load()
        }
    }

    private func metadataStrip(_ hit: SearchHit) -> some View {
        HStack {
            VStack(alignment: .leading, spacing: 2) {
                Text(hit.appName ?? "Unknown app").bold()
                if let title = hit.windowTitle { Text(title).font(.caption) }
                Text(formattedTime(hit.timestampMs)).font(.caption).foregroundStyle(.secondary)
            }
            Spacer()
        }
        .padding(8)
        .background(.thinMaterial)
    }

    private func accessibilityLabel(for hit: SearchHit) -> String {
        let parts = [
            hit.appName,
            hit.windowTitle,
            formattedTime(hit.timestampMs),
        ].compactMap { $0 }
        return "Screenshot from \(parts.joined(separator: " — "))"
    }

    private func load() async {
        do {
            let hit = try await connection.getScreenshot(id: id)
            guard let hit else {
                state = .notFound
                return
            }
            if let img = NSImage(contentsOfFile: hit.imagePath) {
                state = .loaded(hit, img)
            } else {
                state = .fileMissing(hit)
            }
        } catch {
            state = .error(error.localizedDescription)
        }
    }

    private func formattedTime(_ ms: Int64) -> String {
        let date = Date(timeIntervalSince1970: TimeInterval(ms) / 1000.0)
        return date.formatted(date: .abbreviated, time: .standard)
    }
}
