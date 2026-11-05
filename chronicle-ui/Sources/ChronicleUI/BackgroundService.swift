import Foundation
import ServiceManagement
import os

/// The SMAppService calls `BackgroundService` needs, so tests can pass a fake.
@MainActor
protocol LoginService: AnyObject {
    var status: SMAppService.Status { get }
    func register() throws
    func unregister() async throws
}

extension SMAppService: LoginService {}

/// Registers the capture daemon's launch agent and the UI's login item, and
/// keeps both in line with the stored on/off choice.
@MainActor @Observable
final class BackgroundService {
    nonisolated static let agentPlist = "dev.heuss.chronicle.daemon.plist"
    nonisolated static let enabledKey = "backgroundCaptureEnabled"
    nonisolated static let fingerprintKey = "registeredFingerprint"
    private static let logger = Logger(subsystem: "dev.heuss.chronicle", category: "ui")

    @ObservationIgnored private let agent: LoginService
    @ObservationIgnored private let mainApp: LoginService
    @ObservationIgnored private let defaults: UserDefaults
    @ObservationIgnored private let fingerprint: String
    @ObservationIgnored private let isInApplicationsFolder: Bool
    @ObservationIgnored private var tail: Task<Void, Never>?
    @ObservationIgnored private var pending = 0

    private(set) var agentStatus: SMAppService.Status
    private(set) var lastError: String?
    private(set) var isBusy = false
    private(set) var isEnabled: Bool

    init(agent: LoginService? = nil,
         mainApp: LoginService? = nil,
         defaults: UserDefaults = .standard,
         fingerprint: String? = nil,
         isInApplicationsFolder: Bool? = nil) {
        let agent = agent ?? SMAppService.agent(plistName: Self.agentPlist)
        self.agent = agent
        self.mainApp = mainApp ?? SMAppService.mainApp
        self.defaults = defaults
        self.fingerprint = fingerprint ?? Self.currentFingerprint()
        self.isInApplicationsFolder = isInApplicationsFolder ?? Self.bundleIsInApplicationsFolder()
        self.agentStatus = agent.status
        self.isEnabled = defaults.object(forKey: Self.enabledKey) as? Bool ?? true
    }

    func reconcileOnLaunch() async {
        await enqueue(allowOutsideApplications: false)
    }

    func setEnabled(_ on: Bool) async {
        isEnabled = on
        defaults.set(on, forKey: Self.enabledKey)
        await enqueue(allowOutsideApplications: true)
    }

    func refreshStatus() {
        agentStatus = agent.status
    }

    /// Runs one reconcile at a time, in call order. The SDK makes re-registering
    /// safe only after the async unregister completes, and the old daemon holds
    /// the instance lock until it exits.
    private func enqueue(allowOutsideApplications: Bool) async {
        pending += 1
        isBusy = true
        let previous = tail
        let next = Task {
            await previous?.value
            await reconcile(allowOutsideApplications: allowOutsideApplications)
        }
        tail = next
        await next.value
        pending -= 1
        isBusy = pending > 0
    }

    private func reconcile(allowOutsideApplications: Bool) async {
        defer { agentStatus = agent.status }
        if isEnabled && !(isInApplicationsFolder || allowOutsideApplications) { return }
        lastError = nil

        guard isEnabled else {
            for service in [agent, mainApp]
            where service.status == .enabled || service.status == .requiresApproval {
                await attempt { try await service.unregister() }
            }
            if agent.status == .notRegistered {
                defaults.removeObject(forKey: Self.fingerprintKey)
            }
            return
        }

        switch agent.status {
        case .notRegistered:
            if await attempt({ try self.agent.register() }) { recordFingerprint() }
        case .enabled where defaults.string(forKey: Self.fingerprintKey) != fingerprint:
            if await attempt({ try await self.agent.unregister() }),
               await attempt({ try self.agent.register() }) {
                recordFingerprint()
            }
        case .notFound:
            report("The background agent is missing from this build.")
        default:
            break
        }

        if mainApp.status == .notRegistered {
            await attempt { try self.mainApp.register() }
        }
    }

    @discardableResult
    private func attempt(_ operation: () async throws -> Void) async -> Bool {
        do {
            try await operation()
            return true
        } catch {
            report(error.localizedDescription)
            return false
        }
    }

    private func report(_ message: String) {
        lastError = message
        Self.logger.error("\(message, privacy: .public)")
    }

    private func recordFingerprint() {
        defaults.set(fingerprint, forKey: Self.fingerprintKey)
    }

    nonisolated static func statusLabel(_ status: SMAppService.Status)
        -> (text: String, offersLoginItems: Bool) {
        switch status {
        case .enabled: ("On", false)
        case .requiresApproval: ("Needs approval in System Settings", true)
        case .notRegistered: ("Off", false)
        case .notFound: ("Not available in this build", false)
        @unknown default: ("Unknown", false)
        }
    }

    /// `CFBundleVersion` changes only with a new commit, so the daemon binary's
    /// and plist's modification time and size are folded in to catch a rebuild.
    nonisolated static func currentFingerprint(bundle: Bundle = .main) -> String {
        let version = bundle.object(forInfoDictionaryKey: "CFBundleVersion") as? String ?? "0"
        let contents = bundle.bundleURL.appendingPathComponent("Contents")
        let files = ["MacOS/chronicle-daemon", "Library/LaunchAgents/\(agentPlist)"]
        let parts = files.map { path -> String in
            let attributes = try? FileManager.default.attributesOfItem(
                atPath: contents.appendingPathComponent(path).path)
            let mtime = (attributes?[.modificationDate] as? Date)?.timeIntervalSince1970 ?? 0
            let size = (attributes?[.size] as? NSNumber)?.int64Value ?? 0
            return "\(mtime)-\(size)"
        }
        return ([version] + parts).joined(separator: "|")
    }

    nonisolated static func bundleIsInApplicationsFolder(
        bundleURL: URL = Bundle.main.bundleURL) -> Bool {
        let parent = bundleURL.deletingLastPathComponent().standardizedFileURL.path
        let userApplications = FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Applications").standardizedFileURL.path
        return parent == "/Applications" || parent == userApplications
    }
}
