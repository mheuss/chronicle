import Testing
@testable import ChronicleUI

@Suite("Menu bar display", .watchdog)
struct MenuBarDisplayTests {
    @Test(arguments: CaptureAvailability.allCases)
    func displayComesFromAvailability(a: CaptureAvailability) {
        let d = MenuBarDisplay.from(a)
        #expect(d.symbol == a.menuBarSymbol)
        #expect(d.tint == a.tint)
        #expect(d.label == CaptureAvailabilityCopy.menuBarLabel(a))
    }

    @Test func everyStateHasItsOwnShapeAndSpokenLabel() {
        let all = CaptureAvailability.allCases
        #expect(Set(all.map(\.menuBarSymbol)).count == all.count)
        #expect(Set(all.map(\.badgeSymbol)).count == all.count)
        #expect(Set(all.map(CaptureAvailabilityCopy.menuBarLabel)).count == all.count)
    }

    @Test func tintsMatchTheReadmeTable() {
        let expected: [CaptureAvailability: MenuBarDisplay.Tint] = [
            .backgroundOff: .orange, .paused: .orange,
            .starting: .yellow, .permissionUnverified: .yellow,
            .notRunning: .red, .needsScreenRecording: .red, .notCapturing: .red,
            .capturing: .green,
        ]
        for a in CaptureAvailability.allCases {
            #expect(a.tint == expected[a], "\(a)")
        }
    }
}
