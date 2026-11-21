import Foundation

/// The outcome of the last microphone operation, shown until the next toggle
/// or a re-check that finds the mic state or the permission read changed
/// (design §2.3).
enum MicOperationMark: Equatable, Sendable {
    case none, rejected, unconfirmed
}

/// The Settings Microphone row. Rows of design §2.2 are checked top to
/// bottom, and the first match wins.
struct MicRowState: Equatable {
    let isOn: Bool
    let isEnabled: Bool
    let caption: String?
    let showsSettingsButton: Bool

    static let settingsURL = URL(
        string: "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone")!

    static func notConnectedCaption() -> String {
        "Chronicle isn't running."
    }

    static func from(permission: MicPermission?, status: StatusResponse?,
                     connected: Bool, mark: MicOperationMark) -> MicRowState {
        func disabled(_ caption: String, button: Bool = false) -> MicRowState {
            MicRowState(isOn: false, isEnabled: false, caption: caption, showsSettingsButton: button)
        }
        func enabled(_ isOn: Bool, _ caption: String?) -> MicRowState {
            MicRowState(isOn: isOn, isEnabled: true, caption: caption, showsSettingsButton: false)
        }

        guard connected else { return disabled(notConnectedCaption()) }
        guard let permission, let micState = status?.data.audio?.micState else {
            return disabled("Checking the microphone…")
        }
        switch permission {
        case .unavailable:
            return disabled("Chronicle couldn't check microphone access. Close and reopen Settings to check again.")
        case .denied:
            return disabled(
                "Chronicle doesn't have microphone access. Turn it on in System Settings → Privacy & Security → Microphone.",
                button: true)
        case .restricted:
            return disabled(
                "Microphone access is restricted on this Mac, for example by a device management profile.")
        case .authorized, .notDetermined:
            break
        }
        if status?.data.capture?.paused == true {
            return disabled("Capture is paused. Resume capture to change the microphone.")
        }
        let errorCaption: String?
        switch micState {
        case .unknown:
            return disabled("This version of Chronicle can't read the microphone state. Update Chronicle.")
        case .error:
            errorCaption = "The microphone couldn't start."
        case .on, .off, .permissionDenied:
            errorCaption = nil
        }
        let isOn = permission == .authorized && micState == .on
        switch mark {
        case .rejected: return enabled(isOn, "Couldn't change the microphone. Try again.")
        case .unconfirmed: return enabled(isOn, "Chronicle hasn't confirmed the microphone change yet.")
        case .none: break
        }
        if permission == .notDetermined {
            return enabled(false, "Chronicle asks for microphone access when you turn this on.")
        }
        return enabled(isOn, errorCaption)
    }
}
