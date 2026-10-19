import Testing

@testable import ChronicleUI

@Suite("Search filter state")
struct SearchFilterStateTests {
    @Test("a new app session starts on All")
    func startsOnAll() {
        #expect(SearchFilterState().filter == .all)
    }
}
