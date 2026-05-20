import SwiftUI
import AppKit

// Sets the activation policy to .accessory so the app is a true
// menu-bar-only background app — no Dock icon, no App Switcher entry,
// but the MenuBarExtra icon renders correctly. NSApplicationDelegateAdaptor
// is the SwiftUI-idiomatic place to host this kind of NSApplication
// lifecycle setup.
final class AppDelegate: NSObject, NSApplicationDelegate {
    // Use applicationWillFinishLaunching (not Did) so the policy is set
    // BEFORE SwiftUI constructs scenes and MenuBarExtra creates its
    // NSStatusItem. Setting the policy after status item creation
    // invalidates the item on macOS.
    func applicationWillFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.accessory)
    }
}

@main
struct ChronicleApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
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
