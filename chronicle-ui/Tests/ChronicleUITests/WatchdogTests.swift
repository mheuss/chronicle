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

            """)
    }

    @Test func theFiredMessageSaysSoWhenNoTestIsInFlight() {
        #expect(WatchdogMessage.fired(seconds: 120, inFlight: []) == """
            CHRONICLE TEST WATCHDOG FIRED: the test run passed its 120 s deadline.
              no test in flight; the hang is outside a test body

            """)
    }

    // MARK: Registry

    @Test("a running test is in flight exactly once")
    func aRunningTestIsInFlight() {
        let mine = TestWatchdog.shared.inFlight().filter { $0.hasPrefix("a running test is in flight exactly once (") }
        #expect(mine.count == 1, "\(TestWatchdog.shared.inFlight())")
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

    @Suite(.watchdog)
    struct Nested {
        @Test("a test under two watchdog suites is in flight once")
        func aNestedTestIsInFlightOnce() {
            let mine = TestWatchdog.shared.inFlight().filter { $0.hasPrefix("a test under two watchdog suites is in flight once (") }
            #expect(mine.count == 1, "\(TestWatchdog.shared.inFlight())")
        }
    }
}
