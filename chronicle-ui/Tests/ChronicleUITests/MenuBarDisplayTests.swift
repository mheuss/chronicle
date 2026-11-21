import Testing
@testable import ChronicleUI

@Suite("Menu bar display")
struct MenuBarDisplayTests {
    @Test(arguments: CaptureAvailability.allCases)
    func displayComesFromAvailability(a: CaptureAvailability) {
        let d = MenuBarDisplay.from(a)
        #expect(d.symbol == a.menuBarSymbol)
        #expect(d.tint == a.tint)
        #expect(d.label == CaptureAvailabilityCopy.menuBarLabel(a))
    }
}
