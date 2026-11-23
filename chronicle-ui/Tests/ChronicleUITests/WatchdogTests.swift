import Foundation
import Synchronization
import Testing

@Suite("Watchdog", .watchdog)
struct WatchdogTests {
    // MARK: Setting

    @Test func unsetIsTheDefault() {
        #expect(WatchdogSetting.parse(nil) == .default)
        #expect(WatchdogSetting.default.armedSeconds == 120)
        #expect(WatchdogSetting.default.notice == nil)
    }

    @Test func zeroTurnsTheWatchdogOff() {
        #expect(WatchdogSetting.parse("0") == .off)
        #expect(WatchdogSetting.off.armedSeconds == nil)
        #expect(WatchdogSetting.off.notice == "CHRONICLE TEST WATCHDOG: off (CHRONICLE_TEST_WATCHDOG_SECONDS=0)\n")
    }

    @Test func aWholeNumberReplacesN() {
        #expect(WatchdogSetting.parse("5") == .seconds(5))
        #expect(WatchdogSetting.parse("86400") == .seconds(86_400))
        #expect(WatchdogSetting.seconds(5).armedSeconds == 5)
        #expect(WatchdogSetting.seconds(5).notice == "CHRONICLE TEST WATCHDOG: N is 5 s (CHRONICLE_TEST_WATCHDOG_SECONDS)\n")
    }

    @Test(arguments: ["", "abc", "-5", "+5", "1.5", " 5", "86401", "99999999999999999999"])
    func anythingElseIsInvalid(raw: String) {
        #expect(WatchdogSetting.parse(raw) == .invalid(raw))
    }

    @Test func anInvalidValueKeepsTheDefaultAndWarns() {
        #expect(WatchdogSetting.invalid("abc").armedSeconds == 120)
        #expect(WatchdogSetting.invalid("abc").notice == "CHRONICLE TEST WATCHDOG: ignoring CHRONICLE_TEST_WATCHDOG_SECONDS=\"abc\"; N is 120 s\n")
    }

    @Test func anInvalidValueWithANewlineStaysOnOneLine() {
        #expect(WatchdogSetting.invalid("a\nb").notice == "CHRONICLE TEST WATCHDOG: ignoring CHRONICLE_TEST_WATCHDOG_SECONDS=\"a\\nb\"; N is 120 s\n")
    }

    // MARK: Message

    @Test func aLabelNamesTheTestAndItsSourceLocation() {
        #expect(WatchdogMessage.label(name: "t()", fileID: "M/F.swift", line: 3) == "t() (M/F.swift:3)")
    }

    @Test func theFiredMessageNamesEachTestInFlight() {
        let message = WatchdogMessage.fired(seconds: 120, inFlight: ["b() (M/B.swift:2)", "a() (M/A.swift:1)"])
        #expect(message == """
            CHRONICLE TEST WATCHDOG FIRED: the test run passed its 120 s deadline. In flight:
              a() (M/A.swift:1)
              b() (M/B.swift:2)
            The deadline covers the whole process. For --repeat-until or --maximum-repetitions runs, raise CHRONICLE_TEST_WATCHDOG_SECONDS or set it to 0.

            """)
    }

    @Test func theFiredMessageSaysSoWhenNoTestIsInFlight() {
        #expect(WatchdogMessage.fired(seconds: 120, inFlight: []) == """
            CHRONICLE TEST WATCHDOG FIRED: the test run passed its 120 s deadline.
              no test in flight; the hang is outside a test body
            The deadline covers the whole process. For --repeat-until or --maximum-repetitions runs, raise CHRONICLE_TEST_WATCHDOG_SECONDS or set it to 0.

            """)
    }

    // MARK: Registry

    @Test("a running test is in flight exactly once")
    func aRunningTestIsInFlight() {
        #expect(Self.count("a running test is in flight exactly once (") == 1, "\(TestWatchdog.shared.inFlight())")
    }

    @Test("a suite is never in flight")
    func aSuiteIsNeverInFlight() {
        #expect(!TestWatchdog.shared.inFlight().contains { $0.hasPrefix("Watchdog (") }, "\(TestWatchdog.shared.inFlight())")
    }

    @Test func endRemovesTheEntry() {
        let token = TestWatchdog.shared.begin("round trip (X/Y.swift:1)")
        #expect(TestWatchdog.shared.inFlight().contains("round trip (X/Y.swift:1)"))
        TestWatchdog.shared.end(token)
        #expect(!TestWatchdog.shared.inFlight().contains("round trip (X/Y.swift:1)"))
    }

    // These call the trait directly from a detached task, which drops the
    // task-local that marks the running test as already registered.

    private static func count(_ prefix: String) -> Int {
        TestWatchdog.shared.inFlight().filter { $0.hasPrefix(prefix) }.count
    }

    @Test("execute registers a test function until its body returns")
    func executeRegistersUntilTheBodyReturns() async throws {
        let test = try #require(Test.current)
        let prefix = "execute registers a test function until its body returns ("
        let before = Self.count(prefix)
        let during = try await Task.detached {
            let seen = Mutex(0)
            try await WatchdogTrait().execute({ seen.withLock { $0 = Self.count(prefix) } }, for: test, testCase: nil)
            return seen.withLock { $0 }
        }.value
        #expect(during == before + 1)
        #expect(Self.count(prefix) == before)
    }

    @Test("execute unregisters a test function whose body throws")
    func executeUnregistersWhenTheBodyThrows() async throws {
        struct Thrown: Error {}
        let test = try #require(Test.current)
        let prefix = "execute unregisters a test function whose body throws ("
        let before = Self.count(prefix)
        await #expect(throws: Thrown.self) {
            try await Task.detached {
                try await WatchdogTrait().execute({ throw Thrown() }, for: test, testCase: nil)
            }.value
        }
        #expect(Self.count(prefix) == before)
    }

    @Test("execute does not register a per-case step")
    func executeSkipsAPerCaseStep() async throws {
        let test = try #require(Test.current)
        let testCase = try #require(Test.Case.current)
        let prefix = "execute does not register a per-case step ("
        let before = Self.count(prefix)
        let during = try await Task.detached {
            let seen = Mutex(0)
            try await WatchdogTrait().execute({ seen.withLock { $0 = Self.count(prefix) } }, for: test, testCase: testCase)
            return seen.withLock { $0 }
        }.value
        #expect(during == before)
    }

    // MARK: Scan

    private func scan(_ text: String) -> [String] {
        WatchdogScan.scan([(name: "F.swift", text: text)]).map(\.description)
    }

    @Test func aWatchdogSuitePasses() {
        #expect(scan("@Suite(\"S\", .watchdog)\n@MainActor\nstruct S {\n    @Test func t() {}\n}\n") == [])
    }

    @Test func aWatchdogAfterNestedParenthesesPasses() {
        #expect(scan("@Suite(\"S\", .timeLimit(.minutes(1)), .watchdog)\nstruct S {\n    @Test func t() {}\n}\n") == [])
    }

    @Test func aMultiLineSuiteAttributePasses() {
        #expect(scan("@Suite(\"S\",\n       .watchdog,\n       .enabled(if: f(\"(\")))\nstruct S {\n    @Test func t() {}\n}\n") == [])
    }

    @Test func aSuiteWithoutTheWatchdogFails() {
        #expect(scan("@Suite(\"S\", .timeLimit(.minutes(1)))\nstruct S {\n    @Test func t() {}\n}\n")
            == ["F.swift:3: @Test in S, which has no @Suite(... .watchdog ...)"])
    }

    @Test func aTypeWithNoSuiteAttributeFails() {
        #expect(scan("@MainActor\nstruct S {\n    @Test func t() {}\n}\n")
            == ["F.swift:3: @Test in S, which has no @Suite(... .watchdog ...)"])
    }

    @Test func aFreeTestBeforeASuiteFails() {
        #expect(scan("@Test func free() {}\n@Suite(.watchdog)\nstruct S {\n    @Test func t() {}\n}\n")
            == ["F.swift:1: free @Test function; no suite can carry .watchdog"])
    }

    @Test func aFreeTestAfterASuiteFails() {
        #expect(scan("@Suite(.watchdog)\nstruct S {\n    @Test func t() {}\n}\n\n@Test(\"free\") func free() {}\n")
            == ["F.swift:6: free @Test function; no suite can carry .watchdog"])
    }

    @Test func aWatchdogOnlyInACommentFails() {
        #expect(scan("@Suite(\"S\",\n       // .watchdog,\n       .serialized)\nstruct S {\n    @Test func t() {}\n}\n")
            == ["F.swift:5: @Test in S, which has no @Suite(... .watchdog ...)"])
    }

    @Test func aWatchdogOnlyInAStringFails() {
        #expect(scan("@Suite(\"checking .watchdog\")\nstruct S {\n    @Test func t() {}\n}\n")
            == ["F.swift:3: @Test in S, which has no @Suite(... .watchdog ...)"])
    }

    @Test func aTestAfterAClosedSuiteUnderAnUnknownDeclarationFails() {
        #expect(scan("@Suite(.watchdog)\nstruct S {\n}\nnonisolated struct T {\n    @Test func t() {}\n}\n")
            == ["F.swift:5: @Test outside any type"])
    }

    @Test(arguments: [
        "@Suite(.enabled(if: Flags.watchdog))",
        "@Suite(.enabled(if: Config.mode == .watchdog))",
        "@Suite(.tags(.watchdog))",
        "@Suite(\"S\", .tags(.fast, .watchdog))",
        "@Suite(.enabled(if: on(a, .watchdog, b)))",
    ])
    func aWatchdogInsideAnotherArgumentIsNotTheTrait(attribute: String) {
        #expect(scan("\(attribute)\nstruct S {\n    @Test func t() {}\n}\n")
            == ["F.swift:3: @Test in S, which has no @Suite(... .watchdog ...)"])
    }

    @Test func aWatchdogOutsideTheSuiteAttributeFails() {
        #expect(scan("@Suite(\"S\")\n@Foo(.watchdog)\nstruct S {\n    @Test func t() {}\n}\n")
            == ["F.swift:4: @Test in S, which has no @Suite(... .watchdog ...)"])
    }

    @Test func aLongerTraitNameIsNotTheWatchdog() {
        #expect(scan("@Suite(.watchdogDisabled)\nstruct S {\n    @Test func t() {}\n}\n")
            == ["F.swift:3: @Test in S, which has no @Suite(... .watchdog ...)"])
    }

    @Test func anAttributeSeparatedFromItsTypeDoesNotCarryOver() {
        #expect(scan("@Suite(.watchdog)\nfunc helper() {}\nstruct S {\n    @Test func t() {}\n}\n")
            == ["F.swift:4: @Test in S, which has no @Suite(... .watchdog ...)"])
    }

    @Test func crlfLineEndingsAreRead() {
        #expect(scan("struct S {\r\n    @Test func t() {}\r\n}\r\n")
            == ["F.swift:2: @Test in S, which has no @Suite(... .watchdog ...)"])
    }

    @Test func aFreeTestAfterAnotherAttributeFails() {
        #expect(scan("@MainActor @Test func free() async {}\n")
            == ["F.swift:1: free @Test function; no suite can carry .watchdog"])
    }

    @Test func aMemberTestAfterAnotherAttributeIsChecked() {
        #expect(scan("struct S {\n    @MainActor @Test func t() async {}\n}\n")
            == ["F.swift:2: @Test in S, which has no @Suite(... .watchdog ...)"])
        #expect(scan("@Suite(.watchdog)\nstruct S {\n    @MainActor @Test func t() async {}\n}\n") == [])
    }

    @Test func anAttributeOnTheDeclarationLineCounts() {
        #expect(scan("@Suite(.watchdog) struct S {\n    @Test func t() {}\n}\n") == [])
        #expect(scan("@Suite(.watchdog) struct A {}\n@MainActor struct B {\n    @Test func t() {}\n}\n")
            == ["F.swift:3: @Test in B, which has no @Suite(... .watchdog ...)"])
    }

    @Test func aOneLineSuiteDoesNotLendItsTraitToTheNextScope() {
        #expect(scan("@Suite(.watchdog) struct A {}\nnonisolated struct B {\n    @Test func t() {}\n}\n")
            == ["F.swift:3: @Test outside any type"])
    }

    @Test func aGluedOrQualifiedTestAttributeIsSeen() {
        #expect(scan("@MainActor@Test func free() {}\n")
            == ["F.swift:1: free @Test function; no suite can carry .watchdog"])
        #expect(scan("struct S {\n    @MainActor@Test func t() {}\n}\n")
            == ["F.swift:2: @Test in S, which has no @Suite(... .watchdog ...)"])
        #expect(scan("@Testing.Test func free() {}\n")
            == ["F.swift:1: free @Test function; no suite can carry .watchdog"])
        #expect(scan("struct S {\n    @Testing.Test func t() {}\n}\n")
            == ["F.swift:2: @Test in S, which has no @Suite(... .watchdog ...)"])
    }

    @Test func aTestAfterAMultiLineAttributeIsSeen() {
        #expect(scan("@available(macOS 15,\n           *) @Test func free() {}\n")
            == ["F.swift:2: free @Test function; no suite can carry .watchdog"])
        #expect(scan("struct S {\n    @available(macOS 15,\n               *) @Test func t() {}\n}\n")
            == ["F.swift:3: @Test in S, which has no @Suite(... .watchdog ...)"])
    }

    @Test func aDeclarationWithItsBraceOnTheNextLinePasses() {
        #expect(scan("@Suite(.watchdog)\nstruct S\n    : Sendable {\n    @Test func t() {}\n}\n") == [])
    }

    @Test func aTestInsideAOneLineTypeFails() {
        #expect(scan("struct S { @Test func t() {} }\n")
            == ["F.swift:1: free @Test function; no suite can carry .watchdog"])
    }

    @Test func anExtensionOfAWatchdogSuitePasses() {
        #expect(scan("@Suite(.watchdog)\nstruct S {}\n\nextension S {\n    @Test func t() {}\n}\n") == [])
    }

    @Test func anExtensionOfAnotherTypeFails() {
        #expect(scan("struct S {}\n\nextension S {\n    @Test func t() {}\n}\n")
            == ["F.swift:4: @Test in an extension of S, which is not a watchdog suite"])
    }

    @Test func everySuiteCarriesTheWatchdog() throws {
        let directory = URL(fileURLWithPath: #filePath).deletingLastPathComponent()
        let paths = (FileManager.default.subpaths(atPath: directory.path) ?? [])
            .filter { $0.hasSuffix(".swift") }
            .sorted()
        #expect(paths.contains("WatchdogTests.swift"))
        let files = try paths.map { path in
            (name: path, text: try String(contentsOf: directory.appendingPathComponent(path), encoding: .utf8))
        }
        let failures = WatchdogScan.scan(files)
        #expect(failures.isEmpty, "\(failures.map(\.description).joined(separator: "\n"))")
    }

    @Suite(.watchdog)
    struct Nested {
        @Test("a test under two watchdog suites is in flight once")
        func aNestedTestIsInFlightOnce() {
            #expect(WatchdogTests.count("a test under two watchdog suites is in flight once (") == 1, "\(TestWatchdog.shared.inFlight())")
        }
    }
}
