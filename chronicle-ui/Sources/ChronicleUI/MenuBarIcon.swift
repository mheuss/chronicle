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

    /// Green only when the daemon reports capture `running`. Paused is checked
    /// first because the daemon reports "paused" whenever the pause flag is set.
    /// Any other state, including one this build does not know, is not running.
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
            guard let capture = status?.data.capture else {
                return MenuBarDisplay(symbol: "ellipsis.circle", tint: .yellow,
                                      label: "Chronicle waiting for status")
            }
            if capture.paused {
                return MenuBarDisplay(symbol: "pause.circle.fill", tint: .orange,
                                      label: "Chronicle capture paused")
            }
            if capture.state == "running" {
                return MenuBarDisplay(symbol: "record.circle.fill", tint: .green,
                                      label: "Chronicle capture active")
            }
            return MenuBarDisplay(symbol: "exclamationmark.circle", tint: .red,
                                  label: "Chronicle capture not running")
        }
    }
}

/// Menu bar status icon, drawn from `MenuBarDisplay`. Six states:
/// disconnected = `xmark.circle` red; connecting = `ellipsis.circle` yellow;
/// connected with no status yet = `ellipsis.circle` yellow; paused =
/// `pause.circle.fill` orange; running = `record.circle.fill` green; any other
/// state = `exclamationmark.circle` red.
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
