import Foundation

/// A text scan of the test sources for `@Test` functions that `.watchdog`
/// does not reach. It relies on the repo's formatting: free functions and
/// type declarations start at column 0, and members are indented. It is a
/// scan, not a proof. Known limits:
/// - A one-line type whose `{}` wraps onto an indented line can lend its trait
///   to a following type the scan does not recognize, such as a `nonisolated` one.
/// - `@Test` inside a block comment, a multi-line string, or a raw string with an
///   inner quote reads as a test, and fails loudly.
/// - `@Testing.Suite` is not read as a suite.
/// - A one-line type holding a test reports as a free function.
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

    private nonisolated(unsafe) static let watchdogPattern = /\.watchdog(?![A-Za-z0-9_])/

    private nonisolated(unsafe) static let testPattern = /@(?:Testing\.)?Test(?![A-Za-z0-9_])/

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
            func checkMember(_ lineNumber: Int) {
                guard let current else {
                    failures.append(Failure(file: file.name, line: lineNumber, reason: "@Test outside any type"))
                    return
                }
                if current.isExtension {
                    extensionTests.append((file.name, lineNumber, current.name))
                } else if !current.isWatchdogSuite {
                    failures.append(Failure(file: file.name, line: lineNumber, reason: "@Test in \(current.name), which has no @Suite(... .watchdog ...)"))
                }
            }
            let lines = file.text.split(omittingEmptySubsequences: false, whereSeparator: \.isNewline)
            for (index, line) in lines.enumerated() {
                let lineNumber = index + 1
                let text = String(line)
                let code = codeOnly(text)
                if openParens > 0 {
                    openParens += parenDelta(code)
                    attributes += code
                    if code.firstMatch(of: testPattern) != nil {
                        failures.append(Failure(file: file.name, line: lineNumber, reason: "free @Test function; no suite can carry .watchdog"))
                    }
                    continue
                }
                let isTestLine = code.firstMatch(of: testPattern) != nil
                guard !text.isEmpty, text.first?.isWhitespace == false else {
                    if isTestLine { checkMember(lineNumber) }
                    continue
                }
                if isTestLine {
                    failures.append(Failure(file: file.name, line: lineNumber, reason: "free @Test function; no suite can carry .watchdog"))
                    continue
                }
                var declaration = Substring(code)
                if code.hasPrefix("@") {
                    guard let split = leadingAttributes(code) else {
                        attributes += code
                        openParens = parenDelta(code)
                        continue
                    }
                    attributes += split.attributes
                    guard !split.rest.isEmpty else { continue }
                    declaration = split.rest
                }
                if let match = declaration.firstMatch(of: declarationPattern) {
                    let isExtension = match.output.1 == "extension"
                    let isWatchdogSuite = suiteArguments(in: attributes)?.firstMatch(of: watchdogPattern) != nil
                    current = Declaration(name: String(match.output.2), isExtension: isExtension, isWatchdogSuite: isWatchdogSuite)
                    if isWatchdogSuite { watchdogSuites.insert(String(match.output.2)) }
                    if code.contains("{"), braceDelta(code) == 0 { current = nil }
                } else if code.hasPrefix("}") {
                    current = nil
                }
                attributes = ""
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

    /// The text inside the first `@Suite(...)`, or nil when there is none.
    private static func suiteArguments(in attributes: String) -> Substring? {
        guard let open = attributes.range(of: "@Suite(") else { return nil }
        var depth = 1
        var index = open.upperBound
        while index < attributes.endIndex {
            switch attributes[index] {
            case "(": depth += 1
            case ")":
                depth -= 1
                if depth == 0 { return attributes[open.upperBound..<index] }
            default: break
            }
            index = attributes.index(after: index)
        }
        return attributes[open.upperBound...]
    }

    /// Splits off the attributes at the start of a column-0 line, or returns
    /// nil when one of them is still open at the end of the line.
    private static func leadingAttributes(_ code: String) -> (attributes: String, rest: Substring)? {
        var index = code.startIndex
        while true {
            while index < code.endIndex, code[index].isWhitespace { index = code.index(after: index) }
            guard index < code.endIndex, code[index] == "@" else { break }
            index = code.index(after: index)
            while index < code.endIndex, code[index].isLetter || code[index].isNumber || code[index] == "_" {
                index = code.index(after: index)
            }
            guard index < code.endIndex, code[index] == "(" else { continue }
            var depth = 0
            repeat {
                if code[index] == "(" { depth += 1 } else if code[index] == ")" { depth -= 1 }
                index = code.index(after: index)
            } while depth > 0 && index < code.endIndex
            if depth > 0 { return nil }
        }
        return (String(code[..<index]), code[index...].drop(while: { $0.isWhitespace }))
    }

    private static func braceDelta(_ code: String) -> Int {
        code.reduce(0) { $0 + ($1 == "{" ? 1 : $1 == "}" ? -1 : 0) }
    }

    private static func parenDelta(_ code: String) -> Int {
        code.reduce(0) { $0 + ($1 == "(" ? 1 : $1 == ")" ? -1 : 0) }
    }
}
