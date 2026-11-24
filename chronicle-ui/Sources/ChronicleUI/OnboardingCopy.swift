/// Every string in the onboarding window (CHR-94 design §5).
enum OnboardingCopy {
    static let windowTitle = "Set Up Chronicle"

    static let screenRecordingTitle = "Screen Recording"
    static let screenRecordingWhat = "Chronicle saves what's on your screen so you can search it later."
    static let screenRecordingHow = """
        Turn on Chronicle in System Settings › Privacy & Security › Screen & System Audio Recording. \
        If Chronicle isn't in the list, click +, then choose Chronicle.
        """
    /// Shown with or without the How line, so it names no earlier step.
    static let screenRecordingSheets = """
        macOS may ask whether dev.heuss.chronicle.daemon may bypass the private window picker \
        to access your screen and audio: click Allow. If it offers Quit & Reopen, click Later.
        """
    /// The prompt a repeat Open System Settings press can bring up before the
    /// grant (V11), so it shows only while the step isn't done.
    static let screenRecordingRequestPrompt = """
        If macOS says Chronicle would like to record this computer's screen and audio, \
        click Open System Settings.
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

    /// What VoiceOver reads for each step's heading: its title and state
    /// (NFR1). VoiceOver on macOS 27 didn't read this heading's value (V12),
    /// so the state is in the label.
    static func stepLabel(_ title: String, done: Bool) -> String {
        "\(title), \(done ? "Done" : "Not yet")"
    }

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
        case .working(let turningOn): turningOn ? "Turning on the microphone…" : "Checking the microphone…"
        case .noDaemon(let backgroundOff): backgroundOff ? micBackgroundOff : CaptureAvailabilityCopy.notRunning
        case .ready: nil
        }
    }
}
