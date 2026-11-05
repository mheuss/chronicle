import Foundation
import ServiceManagement
import Testing
@testable import ChronicleUI

@MainActor
final class CallLog {
    var entries: [String] = []
}

@MainActor
final class FakeLoginService: LoginService {
    let name: String
    let log: CallLog
    var status: SMAppService.Status
    var registerError: Error?
    var unregisterError: Error?
    var holdUnregister = false
    private var gate: CheckedContinuation<Void, Never>?

    init(_ name: String, status: SMAppService.Status = .notRegistered, log: CallLog) {
        self.name = name
        self.status = status
        self.log = log
    }

    func register() throws {
        log.entries.append("\(name).register")
        if let registerError { throw registerError }
        status = .enabled
    }

    func unregister() async throws {
        log.entries.append("\(name).unregister.start")
        // Only the first held call parks. A second one while it is parked means
        // the queue failed to serialize; it runs through so the test fails on
        // an assertion instead of hanging on a continuation nobody resumes.
        if holdUnregister && gate == nil { await withCheckedContinuation { gate = $0 } }
        log.entries.append("\(name).unregister.end")
        if let unregisterError { throw unregisterError }
        status = .notRegistered
    }

    func release() {
        gate?.resume()
        gate = nil
    }
}

struct Boom: Error {}

@MainActor
@Suite("BackgroundService", .timeLimit(.minutes(1)))
final class BackgroundServiceTests {
    let log = CallLog()
    let defaults: UserDefaults
    let suite = "BackgroundServiceTests.\(UUID().uuidString)"

    init() {
        defaults = UserDefaults(suiteName: suite)!
    }

    deinit {
        UserDefaults().removePersistentDomain(forName: suite)
    }

    func make(agent: FakeLoginService, mainApp: FakeLoginService,
              fingerprint: String = "fp-1", inApplications: Bool = true) -> BackgroundService {
        BackgroundService(agent: agent, mainApp: mainApp, defaults: defaults,
                          fingerprint: fingerprint, isInApplicationsFolder: inApplications)
    }

    @Test func launchRegistersEachUnregisteredService() async {
        let agent = FakeLoginService("agent", log: log)
        let app = FakeLoginService("app", log: log)
        await make(agent: agent, mainApp: app).reconcileOnLaunch()
        #expect(log.entries == ["agent.register", "app.register"])
        #expect(defaults.string(forKey: BackgroundService.fingerprintKey) == "fp-1")
    }

    @Test func launchOutsideApplicationsRegistersNothing() async {
        let agent = FakeLoginService("agent", log: log)
        let app = FakeLoginService("app", log: log)
        await make(agent: agent, mainApp: app, inApplications: false).reconcileOnLaunch()
        #expect(log.entries.isEmpty)
    }

    @Test func changedFingerprintReplacesTheAgentInOrder() async {
        defaults.set("fp-old", forKey: BackgroundService.fingerprintKey)
        let agent = FakeLoginService("agent", status: .enabled, log: log)
        let app = FakeLoginService("app", status: .enabled, log: log)
        await make(agent: agent, mainApp: app).reconcileOnLaunch()
        #expect(log.entries == ["agent.unregister.start", "agent.unregister.end", "agent.register"])
        #expect(defaults.string(forKey: BackgroundService.fingerprintKey) == "fp-1")
    }

    @Test func failedReplacementKeepsTheOldFingerprint() async {
        defaults.set("fp-old", forKey: BackgroundService.fingerprintKey)
        let agent = FakeLoginService("agent", status: .enabled, log: log)
        agent.registerError = Boom()
        let app = FakeLoginService("app", status: .enabled, log: log)
        let service = make(agent: agent, mainApp: app)
        await service.reconcileOnLaunch()
        #expect(defaults.string(forKey: BackgroundService.fingerprintKey) == "fp-old")
        #expect(service.lastError != nil)
    }

    @Test(arguments: [nil, "fp-1", "fp-old"] as [String?])
    func requiresApprovalIsLeftAloneForEveryFingerprint(recorded: String?) async {
        if let recorded { defaults.set(recorded, forKey: BackgroundService.fingerprintKey) }
        let agent = FakeLoginService("agent", status: .requiresApproval, log: log)
        let app = FakeLoginService("app", status: .enabled, log: log)
        await make(agent: agent, mainApp: app).reconcileOnLaunch()
        #expect(log.entries.isEmpty)
        #expect(defaults.string(forKey: BackgroundService.fingerprintKey) == recorded)
    }

    @Test func choiceOffRetriesAFailedUnregister() async {
        defaults.set(false, forKey: BackgroundService.enabledKey)
        let agent = FakeLoginService("agent", status: .enabled, log: log)
        agent.unregisterError = Boom()
        let app = FakeLoginService("app", status: .notRegistered, log: log)
        let first = make(agent: agent, mainApp: app)
        await first.reconcileOnLaunch()
        #expect(first.lastError != nil)
        #expect(agent.status == .enabled)

        agent.unregisterError = nil
        log.entries.removeAll()
        await make(agent: agent, mainApp: app).reconcileOnLaunch()
        #expect(log.entries == ["agent.unregister.start", "agent.unregister.end"])
        #expect(agent.status == .notRegistered)
    }

    @Test func failedUnregisterSkipsTheReplacementRegister() async {
        defaults.set("fp-old", forKey: BackgroundService.fingerprintKey)
        let agent = FakeLoginService("agent", status: .enabled, log: log)
        agent.unregisterError = Boom()
        let app = FakeLoginService("app", status: .enabled, log: log)
        let service = make(agent: agent, mainApp: app)
        await service.reconcileOnLaunch()
        #expect(log.entries == ["agent.unregister.start", "agent.unregister.end"])
        #expect(defaults.string(forKey: BackgroundService.fingerprintKey) == "fp-old")
        #expect(service.lastError != nil)
    }

    @Test func failedMainAppRegistrationIsRetried() async {
        let agent = FakeLoginService("agent", log: log)
        let app = FakeLoginService("app", log: log)
        app.registerError = Boom()
        let service = make(agent: agent, mainApp: app)
        await service.reconcileOnLaunch()
        app.registerError = nil
        log.entries.removeAll()
        await make(agent: agent, mainApp: app).reconcileOnLaunch()
        #expect(log.entries == ["app.register"])
    }

    @Test func agentRegisterErrorStillRegistersTheMainApp() async {
        let agent = FakeLoginService("agent", log: log)
        agent.registerError = Boom()
        let app = FakeLoginService("app", log: log)
        let service = make(agent: agent, mainApp: app)
        await service.reconcileOnLaunch()
        #expect(log.entries == ["agent.register", "app.register"])
        #expect(service.lastError != nil)
    }

    @Test func toggleOffUnregistersBoth() async {
        let agent = FakeLoginService("agent", status: .enabled, log: log)
        let app = FakeLoginService("app", status: .enabled, log: log)
        let service = make(agent: agent, mainApp: app)
        await service.setEnabled(false)
        #expect(log.entries == ["agent.unregister.start", "agent.unregister.end",
                                "app.unregister.start", "app.unregister.end"])
        #expect(service.isEnabled == false)
        #expect(defaults.object(forKey: BackgroundService.fingerprintKey) == nil)
    }

    @Test func toggleOnRegistersBothOutsideApplications() async {
        defaults.set(false, forKey: BackgroundService.enabledKey)
        let agent = FakeLoginService("agent", log: log)
        let app = FakeLoginService("app", log: log)
        let service = make(agent: agent, mainApp: app, inApplications: false)
        await service.setEnabled(true)
        #expect(log.entries == ["agent.register", "app.register"])
        #expect(service.isEnabled)
    }

    @Test func secondRequestWaitsForTheFirst() async throws {
        let agent = FakeLoginService("agent", status: .enabled, log: log)
        agent.holdUnregister = true
        let app = FakeLoginService("app", status: .notRegistered, log: log)
        let service = make(agent: agent, mainApp: app)
        // Released on every path: a parked continuation that nobody resumes
        // hangs the run with no output, and .timeLimit cannot cancel it.
        defer { agent.release() }
        let first = Task { await service.setEnabled(false) }
        for _ in 0..<1000 where !log.entries.contains("agent.unregister.start") { await Task.yield() }
        try #require(log.entries.contains("agent.unregister.start"))
        #expect(service.isBusy)
        let second = Task { await service.setEnabled(true) }
        for _ in 0..<100 { await Task.yield() }
        #expect(!log.entries.contains("agent.register"))
        agent.release()
        await first.value
        await second.value
        #expect(log.entries == ["agent.unregister.start", "agent.unregister.end",
                                "agent.register", "app.register"])
        #expect(!service.isBusy)
    }

    @Test func statusLabelCoversEveryStatus() throws {
        #expect(BackgroundService.statusLabel(.enabled) == ("On", false))
        #expect(BackgroundService.statusLabel(.requiresApproval)
                == ("Needs approval in System Settings", true))
        #expect(BackgroundService.statusLabel(.notRegistered) == ("Off", false))
        #expect(BackgroundService.statusLabel(.notFound) == ("Not available in this build", false))
        let unknown = try #require(SMAppService.Status(rawValue: 99))
        #expect(BackgroundService.statusLabel(unknown) == ("Unknown", false))
    }

    @Test func applicationsFolderDetection() {
        let home = FileManager.default.homeDirectoryForCurrentUser
        #expect(BackgroundService.bundleIsInApplicationsFolder(
            bundleURL: URL(fileURLWithPath: "/Applications/Chronicle.app")))
        #expect(BackgroundService.bundleIsInApplicationsFolder(
            bundleURL: home.appendingPathComponent("Applications/Chronicle.app")))
        #expect(!BackgroundService.bundleIsInApplicationsFolder(
            bundleURL: URL(fileURLWithPath: "/Users/x/Projects/chronicle/dist/Chronicle.app")))
    }
}
