import Testing
@testable import ChronicleUI

struct MicRowStateTests {
    private func row(_ permission: MicPermission?, _ status: StatusResponse?,
                     connected: Bool = true, mark: MicOperationMark = .none) -> MicRowState {
        MicRowState.from(permission: permission, status: status, connected: connected, mark: mark)
    }

    private func disabled(_ caption: String, button: Bool = false) -> MicRowState {
        MicRowState(isOn: false, isEnabled: false, caption: caption, showsSettingsButton: button)
    }

    @Test func notConnected() {
        #expect(row(.authorized, statusWithMic(.on), connected: false)
            == disabled(MicRowState.notConnectedCaption()))
    }

    @Test func loadingWhenPermissionStatusOrAudioIsMissing() {
        let checking = disabled("Checking the microphone…")
        #expect(row(nil, statusWithMic(.off)) == checking)
        #expect(row(.authorized, nil) == checking)
        #expect(row(.authorized, statusWithCapture(state: "running", paused: false)) == checking)
    }

    @Test func unavailable() {
        #expect(row(.unavailable, statusWithMic(.on))
            == disabled("Chronicle couldn't check microphone access. It will try again."))
    }

    @Test func denied() {
        #expect(row(.denied, statusWithMic(.on)) == disabled(
            "Chronicle doesn't have microphone access. Turn it on in System Settings → Privacy & Security → Microphone.",
            button: true))
    }

    @Test func restricted() {
        #expect(row(.restricted, statusWithMic(.off)) == disabled(
            "Microphone access is restricted on this Mac, for example by a device management profile."))
    }

    @Test func paused() {
        #expect(row(.authorized, statusWithMic(.off, paused: true))
            == disabled("Capture is paused. Resume capture to change the microphone."))
    }

    @Test func unknownMicState() {
        #expect(row(.authorized, statusWithMic(.unknown))
            == disabled("This version of Chronicle can't read the microphone state. Update Chronicle."))
    }

    @Test func rejectedOperation() {
        #expect(row(.authorized, statusWithMic(.off), mark: .rejected) == MicRowState(
            isOn: false, isEnabled: true, caption: "Couldn't change the microphone. Try again.",
            showsSettingsButton: false))
    }

    @Test func unconfirmedOperation() {
        #expect(row(.authorized, statusWithMic(.on), mark: .unconfirmed) == MicRowState(
            isOn: true, isEnabled: true,
            caption: "Chronicle hasn't confirmed the microphone change yet.",
            showsSettingsButton: false))
    }

    @Test func notDetermined() {
        #expect(row(.notDetermined, statusWithMic(.off)) == MicRowState(
            isOn: false, isEnabled: true,
            caption: "Chronicle asks for microphone access when you turn this on.",
            showsSettingsButton: false))
    }

    @Test(arguments: [
        (MicState.on, true, nil as String?),
        (.off, false, nil),
        (.error, false, "The microphone couldn't start."),
        (.permissionDenied, false, nil),
    ])
    func authorized(micState: MicState, isOn: Bool, caption: String?) {
        #expect(row(.authorized, statusWithMic(micState)) == MicRowState(
            isOn: isOn, isEnabled: true, caption: caption, showsSettingsButton: false))
    }

    // Each input matches two rows, so the earlier row of design §2.2 must win.
    @Test func firstMatchingRowWins() {
        let rejected = MicRowState(
            isOn: false, isEnabled: true, caption: "Couldn't change the microphone. Try again.",
            showsSettingsButton: false)
        #expect(row(nil, nil, connected: false) == disabled(MicRowState.notConnectedCaption()))
        #expect(row(.denied, statusWithMic(.on, paused: true)) == disabled(
            "Chronicle doesn't have microphone access. Turn it on in System Settings → Privacy & Security → Microphone.",
            button: true))
        #expect(row(.authorized, statusWithMic(.unknown, paused: true))
            == disabled("Capture is paused. Resume capture to change the microphone."))
        #expect(row(.authorized, statusWithMic(.unknown), mark: .rejected)
            == disabled("This version of Chronicle can't read the microphone state. Update Chronicle."))
        #expect(row(.notDetermined, statusWithMic(.off), mark: .rejected) == rejected)
        #expect(row(.authorized, statusWithMic(.error), mark: .rejected) == rejected)
    }

    @Test func onlyAuthorizedPlusOnRendersOn() {
        for permission in [MicPermission.denied, .restricted, .notDetermined, .unavailable] {
            #expect(!row(permission, statusWithMic(.on)).isOn)
        }
    }
}
