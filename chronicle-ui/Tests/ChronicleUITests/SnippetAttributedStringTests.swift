import Testing
import Foundation
@testable import ChronicleUI

@Suite("Snippet → AttributedString")
struct SnippetAttributedStringTests {

    @Test("Plain text returns unchanged")
    func plain() throws {
        let s = snippetAttributedString("kubectl get pods")
        #expect(String(s.characters) == "kubectl get pods")
    }

    @Test("Single bold region is marked bold")
    func singleBold() throws {
        let s = snippetAttributedString("find <b>this</b> here")
        let plain = String(s.characters)
        #expect(plain == "find this here")
        let range = s.range(of: "this")!
        #expect(s[range].inlinePresentationIntent == .stronglyEmphasized)
    }

    @Test("Multiple bold regions")
    func multipleBold() throws {
        let s = snippetAttributedString("<b>one</b> two <b>three</b>")
        let plain = String(s.characters)
        #expect(plain == "one two three")
    }

    @Test("Unbalanced tags fall back to plain string")
    func unbalanced() throws {
        let s = snippetAttributedString("hello <b>world")
        #expect(String(s.characters) == "hello <b>world")
    }

    @Test("Empty input returns empty AttributedString")
    func emptyInput() throws {
        let s = snippetAttributedString("")
        #expect(String(s.characters) == "")
    }
}
