import Foundation
import os

/// Reads Screen Recording through the bundle's helper (ADR-022). The launch
/// check and the onboarding window share this one reader, so a call made
/// while a read runs waits for it instead of spawning a second helper.
@MainActor
final class ScreenRecordingPermission {
    private static let logger = Logger(subsystem: "dev.heuss.chronicle", category: "ui")

    nonisolated static let helperArguments = ["--permission-status", "screen-recording"]
    nonisolated static let helperTimeout: TimeInterval = 1

    private(set) var last: HelperPermission?
    private(set) var unavailableWarnings = 0

    private let readHelper: @Sendable () async -> String?
    private var inFlight: Task<HelperPermission, Never>?

    init(readHelper: (@Sendable () async -> String?)? = nil) {
        self.readHelper = readHelper ?? {
            await MicrophonePermission.runHelper(
                at: MicrophonePermission.helperURL(),
                arguments: Self.helperArguments,
                timeout: Self.helperTimeout)
        }
    }

    func read() async -> HelperPermission {
        if let inFlight { return await inFlight.value }
        let readHelper = readHelper
        let task = Task { HelperPermission(helperOutput: await readHelper()) }
        inFlight = task
        let value = await task.value
        inFlight = nil
        if value == .unavailable && last != .unavailable {
            unavailableWarnings += 1
            Self.logger.warning("screen recording permission helper gave no answer")
        }
        last = value
        return value
    }
}
