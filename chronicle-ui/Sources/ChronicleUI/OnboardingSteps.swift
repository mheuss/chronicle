enum OnboardingStep: Equatable, Sendable {
    case screenRecording, microphone

    static func firstUnsettled(screenRecordingDone: Bool) -> OnboardingStep {
        screenRecordingDone ? .microphone : .screenRecording
    }
}

/// What the Screen Recording step shows (CHR-94 design §3.2). The helper
/// prints only authorized or denied, so the other words read as needed.
enum ScreenRecordingStepState: Equatable, Sendable {
    case needed, unverified
    case done(captureStarted: Bool)

    static func from(read: HelperPermission?, captureRunning: Bool) -> ScreenRecordingStepState {
        switch read {
        case .authorized: .done(captureStarted: captureRunning)
        case .denied, .notDetermined, .restricted: .needed
        case .unavailable, nil: .unverified
        }
    }

    var isDone: Bool {
        if case .done = self { true } else { false }
    }
}

/// The microphone step's row in CHR-94 design §3.3. The first match wins.
enum MicStepState: Equatable, Sendable {
    case locked
    case alreadyOn, skipped, savedOn
    case noAccess(restricted: Bool)
    case working
    case noDaemon(caption: String)
    case ready

    struct Inputs: Equatable, Sendable {
        var screenRecordingDone: Bool
        var permission: HelperPermission?
        var micState: MicState?
        var skipped: Bool
        var turnOnSucceeded: Bool
        var busy: Bool
        var connected: Bool
        var backgroundEnabled: Bool
    }

    static func from(_ i: Inputs) -> MicStepState {
        guard i.screenRecordingDone else { return .locked }
        if i.permission == .authorized && i.micState == .on { return .alreadyOn }
        if i.skipped { return .skipped }
        if i.permission == .denied { return .noAccess(restricted: false) }
        if i.permission == .restricted { return .noAccess(restricted: true) }
        if i.turnOnSucceeded && i.permission == .authorized { return .savedOn }
        if i.busy { return .working }
        if !i.connected {
            return .noDaemon(caption: i.backgroundEnabled
                ? CaptureAvailabilityCopy.notRunning
                : OnboardingCopy.micBackgroundOff)
        }
        return .ready
    }

    var isSettled: Bool {
        switch self {
        case .alreadyOn, .skipped, .savedOn, .noAccess: true
        case .locked, .working, .noDaemon, .ready: false
        }
    }
}

enum OnboardingProgress {
    /// Done needs Screen Recording and a settled microphone step (BR11).
    static func canFinish(screenRecording: HelperPermission?, mic: MicStepState) -> Bool {
        screenRecording == .authorized && mic.isSettled
    }
}
