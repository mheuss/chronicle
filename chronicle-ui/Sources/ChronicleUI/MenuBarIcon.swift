import SwiftUI

/// Menu bar status icon. Active = `record.circle` filled green;
/// paused = `pause.circle` orange; disconnected = `xmark.circle` red.
struct MenuBarIcon: View {
    var connection: DaemonConnection

    var body: some View {
        Image(systemName: symbolName)
            .foregroundStyle(tint)
            .accessibilityLabel(label)
    }

    private var symbolName: String {
        switch connection.state {
        case .disconnected, .connecting:
            return "xmark.circle"
        case .connected:
            if connection.lastStatus?.data.capture?.paused == true {
                return "pause.circle.fill"
            }
            return "record.circle.fill"
        }
    }

    private var tint: Color {
        switch connection.state {
        case .disconnected: return .red
        case .connecting: return .yellow
        case .connected:
            return connection.lastStatus?.data.capture?.paused == true ? .orange : .green
        }
    }

    private var label: String {
        switch connection.state {
        case .disconnected: return "Chronicle daemon disconnected"
        case .connecting: return "Chronicle daemon connecting"
        case .connected:
            return connection.lastStatus?.data.capture?.paused == true
                ? "Chronicle capture paused" : "Chronicle capture active"
        }
    }
}
