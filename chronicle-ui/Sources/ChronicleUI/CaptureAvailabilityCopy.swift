/// Every capture-state string (design §6.2, §7.2), so views need not write
/// their own. Switches are exhaustive with no `default:`.
enum CaptureAvailabilityCopy {
    /// Shared with the mic row's caption while not connected (BR14).
    static let notRunning = "Chronicle isn't running."

    static func settingsText(_ a: CaptureAvailability) -> String {
        switch a {
        case .backgroundOff: "Off"
        case .starting: "Starting Chronicle… this can take a few seconds."
        case .notRunning: notRunning
        case .needsScreenRecording: "Needs Screen Recording"
        case .permissionUnverified: "Chronicle can't check Screen Recording right now."
        case .paused: "Paused"
        case .capturing: "Active"
        case .notCapturing: "Not capturing"
        }
    }

    static func menuBarLabel(_ a: CaptureAvailability) -> String {
        switch a {
        case .backgroundOff: "Chronicle background capture off"
        case .starting: "Chronicle starting"
        case .notRunning: "Chronicle isn't running"
        case .needsScreenRecording: "Chronicle needs Screen Recording"
        case .permissionUnverified: "Chronicle can't check Screen Recording"
        case .paused: "Chronicle capture paused"
        case .capturing: "Chronicle capture active"
        case .notCapturing: "Chronicle capture stopped"
        }
    }

    /// The popover's text with background capture off. Settings shows "Off" in
    /// a labeled row. The popover has no row label, and with background off
    /// there is no daemon to search.
    static let popoverBackgroundOff = "Background capture is off. Turn it on in Settings to search."

    static let fixButton = "Fix…"
    /// What VoiceOver says for the Fix… button, which reads alone out of context.
    static let fixButtonAccessibilityLabel = "Fix Screen Recording"
    static let popoverRow = "Screen Recording is off. Fix…"
}

/// The transcription row's text when it has no transcription block to render
/// (design §6.3).
enum TranscriptionRowFallback {
    /// Version skew either way: a daemon too old to send the block, or one too
    /// new for this UI to understand its state.
    static let requiresDaemonUpdate = "Requires daemon update"

    static func text(hasCurrentStatus: Bool, availability: CaptureAvailability) -> String {
        hasCurrentStatus ? requiresDaemonUpdate : CaptureAvailabilityCopy.settingsText(availability)
    }
}
