import SwiftUI

struct SettingsView: View {
    @Environment(DaemonConnection.self) private var connection
    @State private var isPauseInFlight: Bool = false

    var body: some View {
        TabView {
            generalTab.tabItem { Label("General", systemImage: "gear") }
        }
        .padding(20)
        .task {
            connection.connect()
            if connection.lastStatus == nil {
                _ = try? await connection.requestStatus()
            }
        }
    }

    private var generalTab: some View {
        VStack(alignment: .leading, spacing: 16) {
            row("Status") { statusBadge }
            row("Disk usage") { Text(diskUsageText) }
            row("Retention") { Text(retentionText) }
            row("Oldest record") { Text(oldestText) }

            Divider()

            HStack {
                Spacer()
                Button(isPaused ? "Resume Capture" : "Pause Capture") {
                    togglePause()
                }
                .controlSize(.large)
                .keyboardShortcut(.defaultAction)
                .disabled(isPauseInFlight || connection.state != .connected)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private func row<Content: View>(_ label: String, @ViewBuilder content: () -> Content) -> some View {
        HStack(alignment: .firstTextBaseline) {
            Text(label).foregroundStyle(.secondary).frame(width: 120, alignment: .trailing)
            content()
            Spacer()
        }
    }

    /// Three-way capture status. We don't show "Active" until we have both a
    /// live connection AND a status snapshot that says we're not paused; a
    /// missing snapshot or a disconnected daemon now shows a distinct badge
    /// instead of falsely flashing green.
    private enum CaptureStatus {
        case disconnected
        case paused
        case active
    }

    private var captureStatus: CaptureStatus {
        guard connection.state == .connected,
              let paused = connection.lastStatus?.data.capture?.paused else {
            return .disconnected
        }
        return paused ? .paused : .active
    }

    private var statusBadge: some View {
        HStack(spacing: 6) {
            switch captureStatus {
            case .disconnected:
                Image(systemName: "xmark.circle.fill").foregroundStyle(.secondary)
                Text("Disconnected")
            case .paused:
                Image(systemName: "pause.circle.fill").foregroundStyle(.orange)
                Text("Paused")
            case .active:
                Image(systemName: "record.circle.fill").foregroundStyle(.green)
                Text("Active")
            }
        }
    }

    private var isPaused: Bool {
        captureStatus == .paused
    }

    private var diskUsageText: String {
        guard let storage = connection.lastStatus?.data.storage else { return "—" }
        let bcf = ByteCountFormatter()
        let total = bcf.string(fromByteCount: Int64(storage.totalDiskUsageBytes))
        return "\(total) (\(storage.screenshotCount) screenshots, \(storage.audioSegmentCount) audio segments)"
    }

    private var retentionText: String {
        guard let storage = connection.lastStatus?.data.storage else { return "—" }
        return "\(storage.retentionDays) days"
    }

    private var oldestText: String {
        guard let storage = connection.lastStatus?.data.storage,
              let oldest = storage.oldestEntryMs else { return "—" }
        return Date(timeIntervalSince1970: TimeInterval(oldest) / 1000).formatted(date: .abbreviated, time: .omitted)
    }

    private func togglePause() {
        let wantPause = !isPaused
        isPauseInFlight = true
        Task {
            defer { isPauseInFlight = false }
            do {
                _ = wantPause
                    ? try await connection.pauseCapture()
                    : try await connection.resumeCapture()
                _ = try? await connection.requestStatus()
            } catch {
                // Errors surface in logs; UI shows last-known state.
            }
        }
    }
}
