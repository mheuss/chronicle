import Foundation

/// A text scan of the test sources for `@Test` functions that `.watchdog`
/// does not reach. It relies on the repo's formatting: free functions and
/// type declarations start at column 0, and members are indented. It is a
/// scan, not a proof.
enum WatchdogScan {
    struct Failure: Equatable, CustomStringConvertible {
        let file: String
        let line: Int
        let reason: String
        var description: String { "\(file):\(line): \(reason)" }
    }

    private struct Declaration {
        let name: String
        let isExtension: Bool
        let isWatchdogSuite: Bool
    }

    private nonisolated(unsafe) static let declarationPattern =
        /^(?:(?:public|internal|fileprivate|private|final)\s+)*(struct|class|enum|actor|extension)\s+([A-Za-z_][A-Za-z0-9_]*)/

    static func scan(_ files: [(name: String, text: String)]) -> [Failure] {
        var failures: [Failure] = []
        var watchdogSuites: Set<String> = []
        var extensionTests: [(file: String, line: Int, type: String)] = []

        for file in files {
            var attributes = ""
            var openParens = 0
            var current: Declaration?
            for (index, line) in file.text.split(separator: "\n", omittingEmptySubsequences: false).enumerated() {
                let lineNumber = index + 1
                let text = String(line)
                if openParens > 0 {
                    let code = codeOnly(text)
                    attributes += code
                    openParens += parenDelta(code)
                    continue
                }
                if text.hasPrefix("@Test") {
                    failures.append(Failure(file: file.name, line: lineNumber, reason: "free @Test function; no suite can carry .watchdog"))
                    continue
                }
                if text.hasPrefix("@") {
                    let code = codeOnly(text)
                    attributes += code
                    openParens = parenDelta(code)
                    continue
                }
                if let match = text.firstMatch(of: declarationPattern) {
                    let isExtension = match.output.1 == "extension"
                    let isWatchdogSuite = attributes.contains("@Suite(") && attributes.contains(".watchdog")
                    current = Declaration(name: String(match.output.2), isExtension: isExtension, isWatchdogSuite: isWatchdogSuite)
                    if isWatchdogSuite { watchdogSuites.insert(String(match.output.2)) }
                    attributes = ""
                    continue
                }
                if text.hasPrefix("}") {
                    current = nil
                }
                if !text.isEmpty, text.first?.isWhitespace == false {
                    attributes = ""
                }
                guard text.first?.isWhitespace == true,
                      text.drop(while: { $0.isWhitespace }).hasPrefix("@Test")
                else { continue }
                guard let current else {
                    failures.append(Failure(file: file.name, line: lineNumber, reason: "@Test outside any type"))
                    continue
                }
                if current.isExtension {
                    extensionTests.append((file.name, lineNumber, current.name))
                } else if !current.isWatchdogSuite {
                    failures.append(Failure(file: file.name, line: lineNumber, reason: "@Test in \(current.name), which has no @Suite(... .watchdog ...)"))
                }
            }
        }
        for test in extensionTests where !watchdogSuites.contains(test.type) {
            failures.append(Failure(file: test.file, line: test.line, reason: "@Test in an extension of \(test.type), which is not a watchdog suite"))
        }
        return failures
    }

    /// The line with string-literal contents emptied and any `//` comment
    /// removed, so neither can supply a `.watchdog` or a parenthesis.
    private static func codeOnly(_ text: String) -> String {
        var code = ""
        var inString = false
        var escaped = false
        var previous: Character?
        for character in text {
            if inString {
                if escaped { escaped = false }
                else if character == "\\" { escaped = true }
                else if character == "\"" { inString = false; code.append(character) }
            } else if character == "\"" {
                inString = true
                code.append(character)
            } else if character == "/", previous == "/" {
                code.removeLast()
                break
            } else {
                code.append(character)
            }
            previous = character
        }
        return code
    }

    private static func parenDelta(_ code: String) -> Int {
        code.reduce(0) { $0 + ($1 == "(" ? 1 : $1 == ")" ? -1 : 0) }
    }
}
