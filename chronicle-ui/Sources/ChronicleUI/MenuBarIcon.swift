import SwiftUI

/// What the menu bar item shows. Each state has its own symbol shape, so it
/// reads without relying on tint. The one exception is connecting and waiting
/// for status, which share `ellipsis.circle` because both mean no status yet.
struct MenuBarDisplay: Equatable {
    enum Tint: Equatable {
        case red, yellow, orange, green

        var color: Color {
            switch self {
            case .red: return .red
            case .yellow: return .yellow
            case .orange: return .orange
            case .green: return .green
            }
        }
    }

    let symbol: String
    let tint: Tint
    let label: String

    /// The icon for a connection state and the latest status.
    static func from(
        connectionState: DaemonConnection.ConnectionState,
        status: StatusResponse?
    ) -> MenuBarDisplay {
        switch connectionState {
        case .disconnected:
            return MenuBarDisplay(symbol: "xmark.circle", tint: .red,
                                  label: "Chronicle daemon disconnected")
        case .connecting:
            return MenuBarDisplay(symbol: "ellipsis.circle", tint: .yellow,
                                  label: "Chronicle daemon connecting")
        case .connected:
            switch CaptureHealth.classify(status) {
            case .noStatus:
                return MenuBarDisplay(symbol: "ellipsis.circle", tint: .yellow,
                                      label: "Chronicle waiting for status")
            case .paused:
                return MenuBarDisplay(symbol: "pause.circle.fill", tint: .orange,
                                      label: "Chronicle capture paused")
            case .running:
                return MenuBarDisplay(symbol: "record.circle.fill", tint: .green,
                                      label: "Chronicle capture active")
            case .notRunning:
                return MenuBarDisplay(symbol: "exclamationmark.circle", tint: .red,
                                      label: "Chronicle capture not running")
            }
        }
    }
}

/// Menu bar status icon, drawn from `MenuBarDisplay`.
struct MenuBarIcon: View {
    var connection: DaemonConnection

    var body: some View {
        let display = MenuBarDisplay.from(
            connectionState: connection.state, status: connection.lastStatus)
        Image(systemName: display.symbol)
            .foregroundStyle(display.tint.color)
            .accessibilityLabel(display.label)
    }
}
