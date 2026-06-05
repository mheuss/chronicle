import AppKit
import SwiftUI

struct SearchPopoverView: View {
    @Environment(DaemonConnection.self) private var connection
    @Environment(StartupAlertState.self) private var startupAlert
    @Environment(\.openWindow) private var openWindow
    @Environment(\.openSettings) private var openSettings

    @State private var query: String = ""
    @State private var results: [SearchHit] = []
    @State private var isLoading: Bool = false
    @State private var requestID: UInt64 = 0
    @State private var searchError: Bool = false

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

    static func searchContent(
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
