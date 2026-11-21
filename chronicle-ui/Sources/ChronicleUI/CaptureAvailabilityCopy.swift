/// Every capture-state string (design §6.2, §7.2), so views need not write
/// their own. Switches are exhaustive with no `default:`.
enum CaptureAvailabilityCopy {
    /// Meant to be shared with the mic row's caption while not connected (BR14).
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
        case .notCapturing: "Chronicle capture not running"
        }
    }

    static let fixButton = "Fix…"
    static let popoverRow = "Screen Recording is off. Fix…"
    static let dialogTitle = "Chronicle needs Screen Recording"
    static let dialogBody = """
        Chronicle saves what's on your screen so you can search it later. macOS needs \
        your OK first. Turn on Chronicle in System Settings › Privacy & Security › \
        Screen & System Audio Recording. Capture starts a few seconds after you turn it \
        on, with no restart. Search keeps working for everything already saved.

        If Chronicle isn't in the list, click +, then choose Chronicle.
        """
    static let dialogOpenButton = "Open System Settings"
    static let dialogNotNowButton = "Not Now"
}
