import Testing
import Foundation
import Observation
@testable import ChronicleUI

@Suite("Capture availability")
struct CaptureAvailabilityTests {
    func classify(_ c: DaemonConnection.ConnectionState, _ s: StatusResponse?,
                  background: Bool = true, windowOpen: Bool = true,
                  overdue: Bool = false) -> CaptureAvailability {
        CaptureAvailability.classify(
            connection: c, status: s, backgroundEnabled: background,
            startingWindowOpen: windowOpen, replyOverdue: overdue)
    }

    @Test func notConnectedStates() {
        #expect(classify(.disconnected, nil, background: false) == .backgroundOff)
        #expect(classify(.connecting, nil, windowOpen: true) == .starting)
        #expect(classify(.disconnected, nil, windowOpen: false) == .notRunning)
    }

    @Test func connectedWithNoCurrentStatusIsStartingUntilRepliesGoOverdue() {
        #expect(classify(.connected, nil, windowOpen: false) == .starting)
        #expect(classify(.connected, nil, windowOpen: false, overdue: true) == .notRunning)
        #expect(classify(.connected, nil, windowOpen: true, overdue: true) == .notRunning,
                "a stalled daemon reads as not running inside the starting window too")
    }

    @Test func connectedStates() {
        #expect(classify(.connected, statusWithCapture(state: "running", paused: true)) == .paused)
        #expect(classify(.connected, statusWithCapture(state: "running", paused: false)) == .capturing)
        #expect(classify(.connected, statusDenied(screenRecording: .denied)) == .needsScreenRecording)
        #expect(classify(.connected, statusDenied(screenRecording: .unavailable)) == .permissionUnverified)
        // Only `unavailable` is unverified. Every other read, including one
        // this UI doesn't know, still needs Screen Recording.
        for state in [PermissionState.unknown, .notDetermined, .restricted, .authorized] {
            #expect(classify(.connected, statusDenied(screenRecording: state)) == .needsScreenRecording,
                    "\(state)")
        }
        #expect(classify(.connected, statusWithCapture(state: "permission_denied", paused: false))
                == .needsScreenRecording, "no permissions block")
        #expect(classify(.connected, statusWithoutCapture()) == .notCapturing)
        #expect(classify(.connected, statusWithCapture(state: "permission_denied", paused: true)) == .paused)
    }

    @Test(arguments: ["unknown", "idle", "stopping", "poisoned", "frobnicating"])
    func anyOtherStateIsNotCapturing(state: String) {
        #expect(classify(.connected, statusWithCapture(state: state, paused: false)) == .notCapturing)
    }

    @Test("a disconnected daemon reads as not running once the window ends, with no other change")
    @MainActor
    func theWindowEndingRedrawsAsNotRunning() async {
        let defaults = UserDefaults(suiteName: "dev.heuss.chronicle.tests.CaptureAvailability")!
        defaults.removePersistentDomain(forName: "dev.heuss.chronicle.tests.CaptureAvailability")
        let background = BackgroundService(
            agent: FakeLoginService("agent", log: CallLog()),
            mainApp: FakeLoginService("app", log: CallLog()),
            defaults: defaults, fingerprint: "fp", isInApplicationsFolder: true,
            startingWindow: .milliseconds(200))
        let conn = DaemonConnection(connectionFactory: {
            throw IPCError.connectionFailed(errno: ECONNREFUSED)
        })
        #expect(conn.state == .disconnected)
        final class Flag: @unchecked Sendable { var fired = false }
        let redraw = Flag()
        let before = withObservationTracking {
            conn.availability(background: background)
        } onChange: {
            redraw.fired = true
        }
        #expect(before == .starting)
        await waitUntil(ticks: 100) { !background.startingWindowOpen }
        #expect(redraw.fired, "closing the window notifies a view that reads availability")
        #expect(conn.availability(background: background) == .notRunning)
    }

    @Test(arguments: CaptureAvailability.allCases.filter { $0 != .backgroundOff })
    func popoverNotConnectedTextIsTheSettingsText(a: CaptureAvailability) {
        #expect(SearchPopoverView.notConnectedText(a) == CaptureAvailabilityCopy.settingsText(a))
    }

    @Test func popoverSaysWhatBackgroundOffBlocks() {
        // Settings' bare "Off" sits in a labeled row. The popover has no label,
        // and with no daemon it has no search either.
        #expect(SearchPopoverView.notConnectedText(.backgroundOff)
            == "Background capture is off. Turn it on in Settings to search.")
    }

    @Test func pauseButtonNeedsACurrentStatus() {
        #expect(SettingsView.pauseButtonDisabled(inFlight: false, connected: true, hasCurrentStatus: false),
                "with no current Status the button can't know whether capture is paused")
        #expect(!SettingsView.pauseButtonDisabled(inFlight: false, connected: true, hasCurrentStatus: true))
        #expect(SettingsView.pauseButtonDisabled(inFlight: true, connected: true, hasCurrentStatus: true))
        #expect(SettingsView.pauseButtonDisabled(inFlight: false, connected: false, hasCurrentStatus: true))
    }

    @Test func notConnectedCaptionIsTheSharedCopy() {
        #expect(MicRowState.notConnectedCaption() == CaptureAvailabilityCopy.notRunning)
    }

    @Test func transcriptionFallbackText() {
        #expect(TranscriptionRowFallback.text(hasCurrentStatus: true, availability: .capturing)
            == "Requires daemon update")
        #expect(TranscriptionRowFallback.text(hasCurrentStatus: false, availability: .starting)
            == "Starting Chronicle… this can take a few seconds.")
        #expect(TranscriptionRowFallback.text(hasCurrentStatus: false, availability: .notRunning)
            == "Chronicle isn't running.")
    }

    @Test func settingsTextTable() {
        for a in CaptureAvailability.allCases {
            let expected: String
            switch a {
            case .backgroundOff: expected = "Off"
            case .starting: expected = "Starting Chronicle… this can take a few seconds."
            case .notRunning: expected = "Chronicle isn't running."
            case .needsScreenRecording: expected = "Needs Screen Recording"
            case .permissionUnverified: expected = "Chronicle can't check Screen Recording right now."
            case .paused: expected = "Paused"
            case .capturing: expected = "Active"
            case .notCapturing: expected = "Not capturing"
            }
            #expect(CaptureAvailabilityCopy.settingsText(a) == expected)
        }
    }

    @Test func everyStateHasItsOwnSymbolShape() {
        let menu = Set(CaptureAvailability.allCases.map(\.menuBarSymbol))
        let badge = Set(CaptureAvailability.allCases.map(\.badgeSymbol))
        #expect(menu.count == CaptureAvailability.allCases.count)
        #expect(badge.count == CaptureAvailability.allCases.count)
    }

    @Test func dialogCopyNamesThePaneAndThePlusHint() {
        #expect(CaptureAvailabilityCopy.dialogBody.contains("Screen & System Audio Recording"))
        #expect(CaptureAvailabilityCopy.dialogBody.contains(
            "If Chronicle isn't in the list, click +, then choose Chronicle."))
    }
}
