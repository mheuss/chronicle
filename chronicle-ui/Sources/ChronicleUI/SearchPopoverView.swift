import AppKit
import SwiftUI

struct SearchPopoverView: View {
    @Environment(DaemonConnection.self) private var connection
    @Environment(StartupAlertState.self) private var startupAlert
    @Environment(TranscriptionAlertState.self) private var transcriptionAlert
    @Environment(\.openWindow) private var openWindow
    @Environment(\.openSettings) private var openSettings

    @State private var query: String = ""
    @State private var results: [SearchHit] = []
    @State private var isLoading: Bool = false
    @State private var requestID: UInt64 = 0
    @State private var searchError: Bool = false
    /// Why the daemon turned down the last model request, if it did. Cleared
    /// on the next attempt; a rejection changes no daemon state, so nothing
    /// else would ever tell the user the click did nothing.
    @State private var provisionRejection: String?

    /// Which message the search area should render. A pure function of the
    /// inputs so it can be unit-tested without driving the SwiftUI view — the
    /// HEU-478 error-vs-no-match disambiguation lives here.
    enum SearchContent: Equatable {
        case disconnected
        case prompt
        case searchFailed
        case noMatches
        case results
    }

    // `nonisolated` for the same reason SettingsView's formatters are: `View`
    // conformance makes the type `@MainActor`, and this is a pure function of
    // its arguments called from a nonisolated test. It doesn't trap today
    // only because it contains no closure — which is exactly the "trap
    // waiting for the next edit" the note in SettingsView describes.
    nonisolated static func searchContent(
        connected: Bool,
        queryEmpty: Bool,
        isLoading: Bool,
        searchError: Bool,
        hasResults: Bool
    ) -> SearchContent {
        if !connected { return .disconnected }
        if queryEmpty { return .prompt }
        if !isLoading && searchError { return .searchFailed }
        if !isLoading && !hasResults { return .noMatches }
        return .results
    }

    var body: some View {
        VStack(spacing: 0) {
            if showPausedBanner {
                PausedBanner(
                    onResume: resumeCapture,
                    onDismiss: { startupAlert.dismissed = true }
                )
                Divider()
            }

            if transcriptionAlert.shouldShow {
                TranscriptionBanner(
                    transcription: connection.lastStatus?.data.transcription,
                    rejection: provisionRejection,
                    onDismiss: { transcriptionAlert.dismissed = true },
                    onProvision: provisionModel
                )
                Divider()
            }

            connectionRowOrSearchField

            Divider()

            content

            Divider()

            // Popover footer. `MenuBarExtra(.window)` does NOT auto-add
            // a Quit item, so we provide an explicit one. Settings is
            // also exposed here for discoverability; macOS users can
            // still use Cmd+, when the popover is focused.
            HStack(spacing: 8) {
                Button {
                    NSApp.activate(ignoringOtherApps: true)
                    openSettings()
                } label: {
                    Label("Settings…", systemImage: "gear")
                }
                .keyboardShortcut(",")
                .buttonStyle(.borderless)
                Spacer()
                Button {
                    connection.disconnect()
                    NSApp.terminate(nil)
                } label: {
                    Label("Quit", systemImage: "power")
                }
                .keyboardShortcut("q")
                .buttonStyle(.borderless)
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 6)
            .font(.callout)
        }
        .frame(width: 420, height: 520)
        .task {
            connection.connect()
        }
        .task(id: query) {
            await runSearch()
        }
        .task(id: connection.lastStatus?.data.capture?.paused) {
            if let status = connection.lastStatus {
                startupAlert.evaluate(status: status)
            }
        }
        // Separate from the paused task rather than sharing one id: `task(id:)`
        // takes a single Equatable, and Swift tuples aren't Equatable. Keying
        // each alert on its own signal is also the correct behaviour — a
        // transcription change must be observed even when `paused` never moves.
        .task(id: connection.lastStatus?.data.transcription?.state) {
            if let status = connection.lastStatus {
                transcriptionAlert.evaluate(status: status)
            }
            // A rejection describes one attempt, not a state, so it goes stale
            // the moment the state moves. Safe to clear here: a rejection
            // changes no daemon state, so it cannot clear itself.
            provisionRejection = nil
        }
        // 1 Hz status poll while a provision runs and the popover is visible.
        // View-owned by design (§4.1): closing the popover cancels this task
        // and the daemon's download carries on regardless — the poll drives
        // the progress bar, not the work. Keyed on the state so the loop
        // restarts across transitions and exits once the state settles.
        //
        // Deliberately a SECOND `.task` on the same id as the one above, not
        // a consolidation of it. Merging them would tie `evaluate` — which
        // must run once per transition — to the lifetime of a loop that sits
        // sleeping for a second at a time, so a cancellation mid-sleep would
        // take the alert evaluation with it.
        .task(id: connection.lastStatus?.data.transcription?.state) {
            while !Task.isCancelled, isProvisioningActive {
                try? await Task.sleep(for: .seconds(1))
                if Task.isCancelled { return }
                _ = try? await connection.requestStatus()
            }
        }
    }

    /// Start (or retry) a model download. Deliberately does not pre-dismiss
    /// the banner: `setWhisperModel` refreshes status on success, which moves
    /// the banner into its progress branch — dismissing here would hide the
    /// progress the user just asked for.
    ///
    /// A rejection changes no daemon state, so discarding the reply would
    /// make the click a silent no-op — the banner would keep saying
    /// "Download the base model…" with nothing at all having happened. The
    /// commonest cause is a double-click landing while the first request is
    /// still in flight.
    private func provisionModel(_ variant: String) {
        Task {
            provisionRejection = nil
            // Redundant from this button — it lives on a banner that is by
            // definition visible — but the clear belongs to the ACT of
            // requesting a provision, not to whichever view requested it.
            // Settings makes the same call for the case that is not redundant.
            transcriptionAlert.provisionRequested()
            do {
                let response = try await connection.setWhisperModel(variant)
                if !response.ok {
                    provisionRejection = TranscriptionBannerCopy.rejection(response.status)
                }
            } catch {
                provisionRejection = TranscriptionBannerCopy.unreachable
            }
        }
    }

    /// True while the daemon reports an operation in flight worth polling for.
    ///
    /// Delegates rather than re-deriving: this is the same question the banner
    /// asks to decide whether to draw a bar, and a `default:` arm here would
    /// silently classify a future state as "not in flight" — the exact hazard
    /// the exhaustive switches in `TranscriptionBannerCopy` guard against.
    private var isProvisioningActive: Bool {
        TranscriptionBannerCopy(connection.lastStatus?.data.transcription).showsProgress
    }

    @ViewBuilder
    private var connectionRowOrSearchField: some View {
        switch connection.state {
        case .connected:
            HStack {
                Image(systemName: "magnifyingglass").foregroundStyle(.secondary)
                TextField("Search what you've seen", text: $query)
                    .textFieldStyle(.plain)
                if isLoading {
                    ProgressView().controlSize(.small)
                }
            }
            .padding(.horizontal, 12).padding(.vertical, 8)
        case .connecting:
            Label("Connecting to Chronicle daemon…", systemImage: "ellipsis")
                .foregroundStyle(.secondary)
                .padding()
        case .disconnected:
            Label("Chronicle daemon is not running.", systemImage: "exclamationmark.triangle")
                .foregroundStyle(.red)
                .padding()
        }
    }

    @ViewBuilder
    private var content: some View {
        switch Self.searchContent(
            connected: connection.state == .connected,
            queryEmpty: query.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
            isLoading: isLoading,
            searchError: searchError,
            hasResults: !results.isEmpty
        ) {
        case .disconnected:
            EmptyView()
        case .prompt:
            Text("Start typing to search your history")
                .foregroundStyle(.secondary)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        case .searchFailed:
            Text("Couldn't complete the search — try again.")
                .foregroundStyle(.secondary)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        case .noMatches:
            Text("No matches")
                .foregroundStyle(.secondary)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        case .results:
            // [A11y] Wrap each row in a Button so the result is keyboard-
            // accessible (Tab/Shift-Tab to focus, Return/Space to open).
            List(results) { hit in
                Button {
                    NSApp.activate(ignoringOtherApps: true)
                    openWindow(id: "screenshot", value: hit.id)
                } label: {
                    ResultRow(hit: hit)
                }
                .buttonStyle(.plain)
            }
            .listStyle(.plain)
        }
    }

    private var showPausedBanner: Bool {
        startupAlert.pausedOnBoot && !startupAlert.dismissed
    }

    private func runSearch() async {
        let myID = requestID &+ 1
        requestID = myID
        // Clear any prior error the instant the user edits, before the debounce
        // sleep — otherwise the failure message lingers for ~200ms (HEU-478).
        searchError = false
        let trimmed = query.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else {
            results = []
            isLoading = false
            return
        }
        // Enter the loading state before the debounce so the pending window
        // doesn't transiently render `.noMatches` while a search is queued
        // (HEU-478). The requestID guard in the defer ensures only the latest
        // request clears it.
        isLoading = true
        defer { if requestID == myID { isLoading = false } }
        try? await Task.sleep(for: .milliseconds(200))
        if Task.isCancelled || requestID != myID { return }
        do {
            let hits = try await connection.search(trimmed, limit: 50, offset: 0)
            if requestID == myID {
                results = hits
            }
        } catch is CancellationError {
            // ignore — task cancelled before the request landed
        } catch {
            // A non-cancellation error means the daemon or IPC failed. Surface
            // it instead of rendering an empty result set as a misleading
            // "No matches". See HEU-478.
            if requestID == myID {
                results = []
                searchError = true
            }
        }
    }

    private func resumeCapture() {
        // Do NOT pre-dismiss the banner. `connection.resumeCapture` refreshes
        // status on success, which calls `StartupAlertState.evaluate(status:)`
        // and auto-dismisses via the resume edge. Pre-dismissing here would
        // hide the warning even when the daemon failed to resume.
        Task { _ = try? await connection.resumeCapture() }
    }
}

private struct PausedBanner: View {
    let onResume: () -> Void
    let onDismiss: () -> Void

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: "pause.circle.fill").foregroundStyle(.orange)
            Text("Chronicle is paused. You're not being captured right now.")
                .font(.callout)
            Spacer()
            Button("Resume", action: onResume).buttonStyle(.borderedProminent)
            Button(action: onDismiss) {
                Image(systemName: "xmark").foregroundStyle(.secondary)
            }
            .buttonStyle(.plain)
        }
        .padding(.horizontal, 12).padding(.vertical, 8)
        .background(Color.orange.opacity(0.12))
    }
}

/// State-driven: it offers a Download when no model is present, a live bar
/// while one is being fetched or prepared, and a Retry with the daemon's own
/// reason when something failed. What it SAYS lives in
/// `TranscriptionBannerCopy`, which is testable; this type only renders.
private struct TranscriptionBanner: View {
    /// nil only when the daemon is too old to send the block; the banner is
    /// not shown in that case, so the fallback copy is belt-and-braces.
    let transcription: TranscriptionStats?
    /// Set when the daemon REJECTED the last request. `ok: false` changes no
    /// state, so without this the click is a silent no-op.
    let rejection: String?
    let onDismiss: () -> Void
    let onProvision: (String) -> Void

    var body: some View {
        let copy = TranscriptionBannerCopy(transcription)
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            VStack(alignment: .leading, spacing: 4) {
                // [A11y] Every branch states the condition in words. The icon,
                // the tint and the bar only reinforce what the sentence says.
                Label(copy.headline, systemImage: copy.icon)
                    .font(.callout)
                    .foregroundStyle(copy.isFailure ? Color.red : Color.primary)
                // The daemon's own error wins when there is one: a rejection
                // is vaguer and would bury the diagnosis. The rejection only
                // gets the line when there is nothing better to say.
                if let line = copy.detail ?? rejection {
                    // Capped: this is the daemon's own error text, which can
                    // carry two 64-char digests, and the popover is a fixed
                    // 520pt that cannot grow. Full text on hover.
                    Text(line)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(3)
                        // Complementary, not redundant: lineLimit caps growth,
                        // fixedSize stops the wrapped lines being compressed
                        // away inside this fixed-height popover.
                        .fixedSize(horizontal: false, vertical: true)
                        .help(line)
                }
                if copy.showsProgress {
                    progressRow
                }
            }
            Spacer(minLength: 8)
            if let action = copy.action {
                // [A11y] A labeled button, never a bare icon.
                Button(action.title) { onProvision(action.variant) }
                    .buttonStyle(.borderedProminent)
                    .controlSize(.small)
            }
            Button(action: onDismiss) {
                Image(systemName: "xmark").foregroundStyle(.secondary)
            }
            .buttonStyle(.plain)
            .accessibilityLabel("Dismiss transcription notice")
        }
        .padding(.horizontal, 12).padding(.vertical, 8)
        .background(Color.secondary.opacity(0.12))
    }

    // MARK: - Progress

    @ViewBuilder
    private var progressRow: some View {
        // `downloadTotalBytes` is nil until Content-Length lands, and is
        // never `Some(0)` — so an unknown TOTAL means indeterminate, never a
        // divide by zero. That guarantee is about the total only:
        // `downloadBytes` is published from zero and the branch below has to
        // handle it, which is not a contradiction of this line.
        let state = transcription?.state ?? .missing
        if state == .downloading, let total = transcription?.downloadTotalBytes, total > 0 {
            let done = transcription?.downloadBytes ?? 0
            ProgressView(value: Double(done), total: Double(total))
                .progressViewStyle(.linear)
            // [A11y] Numeric text beside the bar — progress is never conveyed
            // by the bar's length alone.
            Text("\(byteLabel(done)) of \(byteLabel(total))")
                .font(.caption.monospacedDigit())
                .foregroundStyle(.secondary)
        } else if state == .downloading, let done = transcription?.downloadBytes, done > 0 {
            // Indeterminate bar, but the byte count is still live — the daemon
            // publishes `download_bytes` throughout and only withholds the
            // TOTAL until Content-Length arrives. Hiding a moving number
            // behind a barber pole is exactly what the [A11y] obligation is
            // aimed at.
            ProgressView().progressViewStyle(.linear)
            Text("\(byteLabel(done)) downloaded")
                .font(.caption.monospacedDigit())
                .foregroundStyle(.secondary)
        } else {
            // `.verifying` / `.loading` genuinely have no number to show; the
            // "Preparing…" headline carries them. Also the first instant of a
            // download, where the daemon publishes `Some(0)` — "Zero KB
            // downloaded" is worse than no label.
            ProgressView().progressViewStyle(.linear)
        }
    }

    private func byteLabel(_ bytes: UInt64) -> String {
        // clamping, not the trapping initializer: a wrong label beats a crash
        // if a future field ever carries a sentinel above Int64.max.
        ByteCountFormatter.string(fromByteCount: Int64(clamping: bytes), countStyle: .file)
    }
}
