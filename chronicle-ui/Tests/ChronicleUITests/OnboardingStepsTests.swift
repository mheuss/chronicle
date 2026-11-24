import Testing
@testable import ChronicleUI

@Suite(.watchdog)
struct OnboardingStepsTests {
    // Design §3.2.
    @Test func screenRecordingStep() {
        #expect(ScreenRecordingStepState.from(read: .authorized, captureRunning: false) == .done(captureStarted: false))
        #expect(ScreenRecordingStepState.from(read: .authorized, captureRunning: true) == .done(captureStarted: true))
        #expect(ScreenRecordingStepState.from(read: .unavailable, captureRunning: false) == .unverified)
        #expect(ScreenRecordingStepState.from(read: nil, captureRunning: false) == .unverified)
    }

    @Test(arguments: [HelperPermission.denied, .notDetermined, .restricted])
    func everyNonGrantReadIsNeeded(read: HelperPermission) {
        #expect(ScreenRecordingStepState.from(read: read, captureRunning: false) == .needed)
    }

    private static func inputs(
        srDone: Bool = true, permission: HelperPermission? = .authorized, micState: MicState? = .off,
        skipped: Bool = false, turnOnSucceeded: Bool = false, busy: Bool = false,
        connected: Bool = true, backgroundEnabled: Bool = true
    ) -> MicStepState.Inputs {
        .init(screenRecordingDone: srDone, permission: permission, micState: micState, skipped: skipped,
              turnOnSucceeded: turnOnSucceeded, busy: busy, connected: connected,
              backgroundEnabled: backgroundEnabled)
    }

    // Design §3.3, one row each, in order.
    @Test func micStepRows() {
        #expect(MicStepState.from(Self.inputs(srDone: false, micState: .on, skipped: true)) == .locked)
        #expect(MicStepState.from(Self.inputs(micState: .on)) == .alreadyOn)
        #expect(MicStepState.from(Self.inputs(skipped: true)) == .skipped)
        #expect(MicStepState.from(Self.inputs(permission: .denied)) == .noAccess(restricted: false))
        #expect(MicStepState.from(Self.inputs(permission: .restricted)) == .noAccess(restricted: true))
        #expect(MicStepState.from(Self.inputs(turnOnSucceeded: true)) == .savedOn)
        #expect(MicStepState.from(Self.inputs(busy: true)) == .working)
        #expect(MicStepState.from(Self.inputs(connected: false))
                == .noDaemon(caption: CaptureAvailabilityCopy.notRunning))
        #expect(MicStepState.from(Self.inputs(connected: false, backgroundEnabled: false))
                == .noDaemon(caption: CaptureAvailabilityCopy.popoverBackgroundOff))
        #expect(MicStepState.from(Self.inputs()) == .ready)
    }

    // Each check gives inputs that match two rows, and expects the earlier one.
    @Test func firstMatchWins() {
        #expect(MicStepState.from(Self.inputs(micState: .on, skipped: true)) == .alreadyOn)
        #expect(MicStepState.from(Self.inputs(permission: .denied, skipped: true)) == .skipped)
        #expect(MicStepState.from(Self.inputs(permission: .restricted, skipped: true)) == .skipped)
        #expect(MicStepState.from(Self.inputs(skipped: true, turnOnSucceeded: true)) == .skipped)
        #expect(MicStepState.from(Self.inputs(permission: .denied, busy: true)) == .noAccess(restricted: false))
        #expect(MicStepState.from(Self.inputs(permission: .restricted, busy: true)) == .noAccess(restricted: true))
        #expect(MicStepState.from(Self.inputs(turnOnSucceeded: true, busy: true)) == .savedOn)
        #expect(MicStepState.from(Self.inputs(busy: true, connected: false)) == .working)
    }

    @Test func alreadyOnSettlesWithNoButtonPressed() {
        let step = MicStepState.from(Self.inputs(micState: .on))
        #expect(step.isSettled)
    }

    @Test func aSucceededTurnOnWithoutAuthorizationIsNotSettled() {
        #expect(MicStepState.from(Self.inputs(permission: .notDetermined, turnOnSucceeded: true)) == .ready)
    }

    @Test(arguments: [MicStepState.locked, .working, .noDaemon(caption: "x"), .ready])
    func unsettled(step: MicStepState) { #expect(!step.isSettled) }

    @Test(arguments: [MicStepState.alreadyOn, .skipped, .noAccess(restricted: false), .noAccess(restricted: true), .savedOn])
    func settled(step: MicStepState) { #expect(step.isSettled) }

    // BR11: 5 reads × settled/unsettled. Only one row enables Done.
    @Test(arguments: [HelperPermission.authorized, .denied, .notDetermined, .restricted, .unavailable],
          [MicStepState.skipped, .ready])
    func canFinish(read: HelperPermission, mic: MicStepState) {
        let expected = read == .authorized && mic == .skipped
        #expect(OnboardingProgress.canFinish(screenRecording: read, mic: mic) == expected)
    }

    @Test func firstUnsettledStep() {
        #expect(OnboardingStep.firstUnsettled(screenRecordingDone: false) == .screenRecording)
        #expect(OnboardingStep.firstUnsettled(screenRecordingDone: true) == .microphone)
    }
}
