import SwiftUI
import AppKit

@main
struct ChronicleApp: App {
    @State private var connection: DaemonConnection
    @State private var mic: MicController
    @State private var startupAlert = StartupAlertState()
    @State private var transcriptionAlert = TranscriptionAlertState()
    @State private var searchFilter = SearchFilterState()
    @State private var background = BackgroundService()

    init() {
        let connection = DaemonConnection()
        _connection = State(initialValue: connection)
        _mic = State(initialValue: MicController(daemon: connection))
    }

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
                    background.onEnabled = { [connection] in connection.reconnectNow() }
                    connection.connect()
                    await background.reconcileOnLaunch()
                }
                .onReceive(NotificationCenter.default.publisher(
                    for: NSApplication.didBecomeActiveNotification)) { _ in
                    Task { await background.reconcileOnLaunch() }
                    Task { await mic.recheck() }
                }
        }
        .menuBarExtraStyle(.window)

        Settings {
            SettingsView()
                .environment(connection)
                .environment(background)
                .environment(mic)
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
