import Testing
import Foundation
@testable import ChronicleUI

@Suite("Capture availability")
struct CaptureAvailabilityTests {
    let now = Date(timeIntervalSince1970: 1_000)

    func classify(_ c: DaemonConnection.ConnectionState, _ s: StatusResponse?,
                  background: Bool = true, sinceSecondsAgo: Double? = 5,
                  overdue: Bool = false) -> CaptureAvailability {
        CaptureAvailability.classify(
            connection: c, status: s, backgroundEnabled: background,
            startingSince: sinceSecondsAgo.map { now.addingTimeInterval(-$0) },
            replyOverdue: overdue, now: now)
    }

    @Test func notConnectedStates() {
        #expect(classify(.disconnected, nil, background: false) == .backgroundOff)
        #expect(classify(.connecting, nil, sinceSecondsAgo: 5) == .starting)
        #expect(classify(.disconnected, nil, sinceSecondsAgo: 61) == .notRunning)
        #expect(classify(.disconnected, nil, sinceSecondsAgo: nil) == .notRunning)
    }

    @Test func connectedWithNoCurrentStatusIsStartingUntilRepliesGoOverdue() {
        #expect(classify(.connected, nil, sinceSecondsAgo: 600) == .starting)
        #expect(classify(.connected, nil, sinceSecondsAgo: 600, overdue: true) == .notRunning)
        #expect(classify(.connected, nil, sinceSecondsAgo: 5, overdue: true) == .notRunning,
                "a stalled daemon reads as not running inside the starting window too")
    }

    @Test func connectedStates() {
        #expect(classify(.connected, statusWithCapture(state: "running", paused: true)) == .paused)
        #expect(classify(.connected, statusWithCapture(state: "running", paused: false)) == .capturing)
        #expect(classify(.connected, statusDenied(screenRecording: .denied)) == .needsScreenRecording)
        #expect(classify(.connected, statusDenied(screenRecording: .unavailable)) == .permissionUnverified)
        #expect(classify(.connected, statusWithoutCapture()) == .notCapturing)
        #expect(classify(.connected, statusWithCapture(state: "permission_denied", paused: true)) == .paused)
    }

    @Test(arguments: ["unknown", "idle", "stopping", "poisoned", "frobnicating"])
    func anyOtherStateIsNotCapturing(state: String) {
        #expect(classify(.connected, statusWithCapture(state: state, paused: false)) == .notCapturing)
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
