import SwiftUI
import AppKit

@main
struct ChronicleApp: App {
    @State private var connection = DaemonConnection()
    @State private var startupAlert = StartupAlertState()
    @State private var transcriptionAlert = TranscriptionAlertState()
    @State private var searchFilter = SearchFilterState()
    @State private var background = BackgroundService()

    var body: some Scene {
        MenuBarExtra {
            SearchPopoverView()
                .environment(connection)
                .environment(startupAlert)
                .environment(transcriptionAlert)
                .environment(searchFilter)
        } label: {
            MenuBarIcon(connection: connection)
                .task {
                    connection.connect()
                    await background.reconcileOnLaunch()
                }
                .onReceive(NotificationCenter.default.publisher(
                    for: NSApplication.didBecomeActiveNotification)) { _ in
                    Task { await background.reconcileOnLaunch() }
                }
        }
        .menuBarExtraStyle(.window)

        Settings {
            SettingsView()
                .environment(connection)
                .environment(background)
                // Settings is a provision entry point too, so it needs to be
                // able to spend a stale dismissal — see
                // `TranscriptionAlertState.provisionRequested()`.
                .environment(transcriptionAlert)
                .frame(minWidth: 480, minHeight: 320)
        }

        WindowGroup(id: "screenshot", for: Int64.self) { $id in
            if let id {
                ScreenshotDetailView(id: id)
                    .environment(connection)
                    .frame(minWidth: 640, minHeight: 480)
            }
        }
        .windowResizability(.contentMinSize)
    }
}
