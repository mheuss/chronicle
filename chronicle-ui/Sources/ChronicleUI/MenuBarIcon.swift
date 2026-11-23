import SwiftUI

/// What the menu bar item shows. Each availability has its own symbol shape,
/// so it reads without relying on tint.
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

    /// The icon for one availability (design AD11).
    static func from(_ a: CaptureAvailability) -> MenuBarDisplay {
        MenuBarDisplay(symbol: a.menuBarSymbol, tint: a.tint,
                       label: CaptureAvailabilityCopy.menuBarLabel(a))
    }
}

/// Menu bar status icon, drawn from `CaptureAvailability`.
struct MenuBarIcon: View {
    var connection: DaemonConnection
    var background: BackgroundService

    var body: some View {
        let display = MenuBarDisplay.from(connection.availability(background: background))
        Image(systemName: display.symbol)
            .foregroundStyle(display.tint.color)
            .accessibilityLabel(display.label)
    }
}
