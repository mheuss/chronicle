import SwiftUI
import AppKit

@main
struct ChronicleApp: App {
    @State private var connection: DaemonConnection
    @State private var mic: MicController
    @State private var startupAlert = StartupAlertState()
    @State private var transcriptionAlert = TranscriptionAlertState()
    @State private var searchFilter = SearchFilterState()
    @State private var background: BackgroundService
    @State private var onboarding: OnboardingWindowController

    init() {
        let connection = DaemonConnection()
        let mic = MicController(daemon: connection)
        let background = BackgroundService()
        let reader = ScreenRecordingPermission()
        let model = OnboardingModel(mic: mic, reader: reader, daemon: connection,
                                    backgroundEnabled: { background.isEnabled })
        _connection = State(initialValue: connection)
        _mic = State(initialValue: mic)
        _background = State(initialValue: background)
        _onboarding = State(initialValue: OnboardingWindowController(
            model: model, reader: reader, presenter: OnboardingWindowPresenter()))
    }

    var body: some Scene {
        MenuBarExtra {
            SearchPopoverView()
                .environment(connection)
                .environment(startupAlert)
                .environment(transcriptionAlert)
                .environment(searchFilter)
                .environment(background)
                .environment(onboarding)
        } label: {
            MenuBarIcon(connection: connection, background: background)
                .task {
                    background.onAgentRegistered = { [connection] in connection.reconnectNow() }
                    Task { await onboarding.launchCheck() }
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
                .environment(onboarding)
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
