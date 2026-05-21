import SwiftUI

@main
struct ChronicleApp: App {
    @State private var connection = DaemonConnection()
    @State private var startupAlert = StartupAlertState()

    var body: some Scene {
        MenuBarExtra {
            SearchPopoverView()
                .environment(connection)
                .environment(startupAlert)
        } label: {
            MenuBarIcon(connection: connection)
        }
        .menuBarExtraStyle(.window)

        Settings {
            SettingsView()
                .environment(connection)
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
