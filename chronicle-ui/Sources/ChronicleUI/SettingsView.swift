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
            row("Transcription") { transcriptionBadge }
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

    /// Transcription state, straight from the daemon's status block. Phase 1
    /// only reports; the variant picker and download button arrive in Phase 2.
    private var transcriptionBadge: some View {
        // .firstTextBaseline, unlike the other badges: the error branch is the
        // only badge whose text can wrap, and under default centre alignment
        // the icon floats to the middle of the paragraph while the row's
        // "Transcription" label stays on line 1.
        HStack(alignment: .firstTextBaseline, spacing: 6) {
            if let transcription = connection.lastStatus?.data.transcription {
                transcriptionBadgeBody(transcription)
            } else {
                // No block at all = a daemon older than this UI. Say so plainly
                // rather than implying transcription is broken.
                Text("Requires daemon update").foregroundStyle(.secondary)
            }
        }
    }

    /// [A11y] Every branch pairs its icon and colour with words that carry the
    /// state on their own (docs/standards/accessibility.md "Text and Color").
    ///
    /// Exhaustive, with no `default:` arm — a state added to
    /// `TranscriptionState` later must fail to compile here and get classified
    /// deliberately instead of inheriting whichever branch happened to catch it.
    @ViewBuilder
    private func transcriptionBadgeBody(_ transcription: TranscriptionStats) -> some View {
        switch transcription.state {
        case .ready, .unknown:
            // `.unknown` rides with `.ready` by decision (plan Decisions,
            // 2026-08-02) — a state this UI doesn't recognize is not surfaced
            // as a problem.
            Image(systemName: "waveform").foregroundStyle(.green)
            Text(transcription.loadedVariant.map { "Active (\($0))" } ?? "Active")
        case .missing:
            Image(systemName: "waveform.slash").foregroundStyle(.secondary)
            Text("Off — model not downloaded")
        case .error:
            Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(.red)
            // Neutral framing: this state is also reached by a corrupt model at
            // boot, where nothing was ever downloaded (plan Decisions,
            // 2026-08-01 — Task 15 copy nuance).
            //
            // This is the only unbounded daemon string in Settings, and the
            // Settings scene sizes to content ideal width. Uncapped, this row's
            // ideal width just tracks the error length with no ceiling —
            // measured 579pt at 70 characters, 1439pt at 210. Those are row
            // widths; the window adds scene padding and tab chrome on top, so
            // it opens far past the 480pt minimum at ChronicleApp.swift:23.
            // The 300pt cap pins the row at 458pt flat no matter how long the
            // error is, which puts the window at ~500pt — essentially on its
            // floor. The fixedSize then keeps the wrapped lines from being
            // compressed away if this row ever lands in a height-constrained
            // container. Text already wraps by default — neither modifier is
            // undoing a truncation, so don't "restore" a lineLimit here.
            Text(transcription.error ?? "Transcription is off")
                .frame(maxWidth: 300, alignment: .leading)
                .fixedSize(horizontal: false, vertical: true)
        case .downloading, .verifying, .loading:
            ProgressView().controlSize(.small)
            Text(transcription.state.rawValue.capitalized)
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
