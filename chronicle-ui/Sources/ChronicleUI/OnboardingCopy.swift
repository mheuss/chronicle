/// Every string in the onboarding window (CHR-94 design §5).
enum OnboardingCopy {
    static let windowTitle = "Set Up Chronicle"

    static let screenRecordingTitle = "Screen Recording"
    static let screenRecordingWhat = "Chronicle saves what's on your screen so you can search it later."
    static let screenRecordingHow = """
        Turn on Chronicle in System Settings › Privacy & Security › Screen & System Audio Recording. \
        If Chronicle isn't in the list, click +, then choose Chronicle.
        """
    static let screenRecordingSheets = """
        macOS may then ask whether "dev.heuss.chronicle.daemon" may record your screen. Click Allow. \
        If it offers Quit & Reopen, click Later. Chronicle captures in the background and starts \
        without a restart.
        """
    static let openSettingsButton = "Open System Settings"

    static let micTitle = "Microphone (optional)"
    static let micBody =
        "Chronicle can also record and transcribe your microphone. You can change this later in Settings."
    static let turnOnButton = "Turn on microphone"
    static let skipButton = "Skip"
    static let micBackgroundOff = "Background capture is off. Turn it on in Settings to use the microphone."

    static let doneButton = "Done"
    static let closeButton = "Close"
    static let finishSetupRow = "Finish setting up Chronicle…"

    /// What VoiceOver reads as each step's state (NFR1).
    static func stepValue(done: Bool) -> String { done ? "Done" : "Not yet" }

    static func screenRecordingStatus(_ step: ScreenRecordingStepState) -> String? {
        switch step {
        case .needed: nil
        case .unverified: CaptureAvailabilityCopy.settingsText(.permissionUnverified)
        case .done(let started): started ? "Screen Recording is on. Capture started." : "Screen Recording is on."
        }
    }

    static func micStatus(_ step: MicStepState) -> String? {
        switch step {
        case .locked: "Turn on Screen Recording first."
        case .alreadyOn: "The microphone is on."
        case .skipped: "Skipped. You can turn the microphone on later in Settings."
        case .savedOn: "Microphone on. It records once capture is running."
        case .noAccess(let restricted): restricted ? MicRowState.restrictedCaption : MicRowState.deniedCaption
        case .working: "Turning on the microphone…"
        case .noDaemon(let caption): caption
        case .ready: nil
        }
    }
}
