import Foundation

/// Is capture working, and if not, why (design §6.1). The menu bar, Settings,
/// the popover and the dialog are meant to share this one classifier (AD11).
enum CaptureAvailability: Equatable, Hashable, CaseIterable {
    case backgroundOff, starting, notRunning, needsScreenRecording, permissionUnverified
    case paused, capturing, notCapturing

    /// `status` must be `DaemonConnection.currentStatus`: nil until this
    /// connection's first Status. `startingWindowOpen` is
    /// `BackgroundService.startingWindowOpen`. `replyOverdue` is
    /// `DaemonConnection.replyOverdue`: a connected daemon that stopped
    /// answering reads as not running.
    static func classify(
        connection: DaemonConnection.ConnectionState,
        status: StatusResponse?,
        backgroundEnabled: Bool,
        startingWindowOpen: Bool,
        replyOverdue: Bool
    ) -> CaptureAvailability {
        guard connection == .connected else {
            guard backgroundEnabled else { return .backgroundOff }
            return startingWindowOpen ? .starting : .notRunning
        }
        guard let status else { return replyOverdue ? .notRunning : .starting }
        guard let capture = status.data.capture else { return .notCapturing }
        if capture.paused { return .paused }
        switch capture.state {
        case "running":
            return .capturing
        case "permission_denied":
            return status.data.permissions?.screenRecording == .unavailable
                ? .permissionUnverified : .needsScreenRecording
        default:
            return .notCapturing
        }
    }

    /// Symbol for the menu bar. Each state has its own shape.
    var menuBarSymbol: String {
        switch self {
        case .backgroundOff: "power.circle"
        case .starting: "ellipsis.circle"
        case .notRunning: "xmark.circle"
        case .needsScreenRecording: "lock.circle"
        case .permissionUnverified: "questionmark.circle"
        case .paused: "pause.circle.fill"
        case .capturing: "record.circle.fill"
        case .notCapturing: "exclamationmark.circle"
        }
    }

    /// Filled symbol for the Settings badge. Each state has its own shape.
    var badgeSymbol: String {
        switch self {
        case .backgroundOff: "power.circle.fill"
        case .starting: "ellipsis.circle.fill"
        case .notRunning: "xmark.circle.fill"
        case .needsScreenRecording: "lock.circle.fill"
        case .permissionUnverified: "questionmark.circle.fill"
        case .paused: "pause.circle.fill"
        case .capturing: "record.circle.fill"
        case .notCapturing: "exclamationmark.circle.fill"
        }
    }

    var tint: MenuBarDisplay.Tint {
        switch self {
        case .backgroundOff, .paused: .orange
        case .starting, .permissionUnverified: .yellow
        case .notRunning, .needsScreenRecording, .notCapturing: .red
        case .capturing: .green
        }
    }
}

extension DaemonConnection {
    /// The availability for this connection and the background setting.
    func availability(background: BackgroundService) -> CaptureAvailability {
        CaptureAvailability.classify(
            connection: state, status: currentStatus,
            backgroundEnabled: background.isEnabled,
            startingWindowOpen: background.startingWindowOpen,
            replyOverdue: replyOverdue)
    }
}
