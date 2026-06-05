import Testing

@testable import ChronicleUI

@Suite("SearchPopoverView content state")
struct SearchContentStateTests {
    // The core of HEU-478: a daemon/IPC error must read as a failure, never as
    // a clean "No matches".
    @Test("daemon error surfaces the failure message, not no-matches")
    func errorBeatsNoMatches() {
        let state = SearchPopoverView.searchContent(
            connected: true, queryEmpty: false, isLoading: false,
            searchError: true, hasResults: false
        )
        #expect(state == .searchFailed)
    }

    @Test("empty results without an error reads as no-matches")
    func emptyResultsAreNoMatches() {
        let state = SearchPopoverView.searchContent(
            connected: true, queryEmpty: false, isLoading: false,
            searchError: false, hasResults: false
        )
        #expect(state == .noMatches)
    }

    @Test("while loading, neither error nor no-match is shown")
    func loadingSuppressesMessages() {
        let state = SearchPopoverView.searchContent(
            connected: true, queryEmpty: false, isLoading: true,
            searchError: true, hasResults: false
        )
        #expect(state == .results)
    }

    @Test("an empty query shows the prompt even with a stale error")
    func emptyQueryShowsPrompt() {
        let state = SearchPopoverView.searchContent(
            connected: true, queryEmpty: true, isLoading: false,
            searchError: true, hasResults: false
        )
        #expect(state == .prompt)
    }

    @Test("a disconnected daemon shows nothing")
    func disconnectedShowsNothing() {
        let state = SearchPopoverView.searchContent(
            connected: false, queryEmpty: false, isLoading: false,
            searchError: true, hasResults: true
        )
        #expect(state == .disconnected)
    }

    @Test("results present render the list")
    func resultsRenderList() {
        let state = SearchPopoverView.searchContent(
            connected: true, queryEmpty: false, isLoading: false,
            searchError: false, hasResults: true
        )
        #expect(state == .results)
    }
}
