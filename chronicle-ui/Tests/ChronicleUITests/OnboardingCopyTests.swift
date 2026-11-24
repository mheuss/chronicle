import Testing
@testable import ChronicleUI

@Suite(.watchdog)
struct OnboardingCopyTests {
    @Test func howNamesThePaneAndThePlusHint() {
        #expect(OnboardingCopy.screenRecordingHow.contains("Screen & System Audio Recording"))
        #expect(OnboardingCopy.screenRecordingHow.contains(
            "If Chronicle isn't in the list, click +, then choose Chronicle."))
    }

    @Test func sheetsLineNamesTheDaemonAllowAndLater() {
        #expect(OnboardingCopy.screenRecordingSheets == """
            macOS may ask whether dev.heuss.chronicle.daemon may bypass the private window picker \
            to access your screen and audio: click Allow. If it offers Quit & Reopen, click Later.
            """)
    }

    @Test func requestPromptLineNamesTheMacOSPromptAndItsButton() {
        #expect(OnboardingCopy.screenRecordingRequestPrompt == """
            If macOS says Chronicle would like to record this computer's screen and audio, \
            click Open System Settings.
            """)
    }

    @Test func sheetsLineStandsWithoutTheHowLine() {
        #expect(!OnboardingCopy.screenRecordingSheets.contains("then"))
    }

    @Test func stepLabelCarriesTheState() {
        #expect(OnboardingCopy.stepLabel("Screen Recording", done: false) == "Screen Recording, Not yet")
        #expect(OnboardingCopy.stepLabel("Microphone (optional)", done: true) == "Microphone (optional), Done")
    }

    @Test func screenRecordingStatus() {
        #expect(OnboardingCopy.screenRecordingStatus(.needed) == nil)
        #expect(OnboardingCopy.screenRecordingStatus(.checking) == nil)
        #expect(OnboardingCopy.screenRecordingStatus(.unverified)
                == CaptureAvailabilityCopy.settingsText(.permissionUnverified))
        #expect(OnboardingCopy.screenRecordingStatus(.done(captureStarted: false)) == "Screen Recording is on.")
        #expect(OnboardingCopy.screenRecordingStatus(.done(captureStarted: true))
                == "Screen Recording is on. Capture started.")
    }

    @Test func micStatusReusesTheMicRowCaptions() {
        #expect(OnboardingCopy.micStatus(.noAccess(restricted: false)) == MicRowState.deniedCaption)
        #expect(OnboardingCopy.micStatus(.noAccess(restricted: true)) == MicRowState.restrictedCaption)
        #expect(OnboardingCopy.micStatus(.noDaemon(backgroundOff: false)) == CaptureAvailabilityCopy.notRunning)
        #expect(OnboardingCopy.micStatus(.noDaemon(backgroundOff: true)) == OnboardingCopy.micBackgroundOff)
        #expect(OnboardingCopy.micStatus(.working(turningOn: true)) == "Turning on the microphone…")
        #expect(OnboardingCopy.micStatus(.working(turningOn: false)) == "Checking the microphone…")
        #expect(OnboardingCopy.micStatus(.ready) == nil)
        #expect(OnboardingCopy.micStatus(.savedOn) == "Microphone on. It records once capture is running.")
        #expect(OnboardingCopy.micStatus(.alreadyOn) == "The microphone is on.")
    }

    @Test func micRowCaptionsKeepTheirOpening() {
        #expect(MicRowState.deniedCaption.hasPrefix("Chronicle doesn't have microphone access."))
        #expect(MicRowState.restrictedCaption.hasPrefix("Microphone access is restricted on this Mac"))
    }
}
