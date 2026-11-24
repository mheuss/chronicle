import AVFoundation
import Foundation
import os

/// A permission as `chronicle-daemon --permission-status <name>` reports it.
enum HelperPermission: Hashable, Sendable {
    case authorized, denied, notDetermined, restricted, unavailable

    /// Parse one helper output line. Anything else, or no output, is
    /// `.unavailable`, which no caller treats as a grant.
    init(helperOutput: String?) {
        switch helperOutput?.trimmingCharacters(in: .whitespacesAndNewlines) {
        case "authorized": self = .authorized
        case "denied": self = .denied
        case "not_determined": self = .notDetermined
        case "restricted": self = .restricted
        default: self = .unavailable
        }
    }
}

/// Reads microphone permission through the bundle's helper and asks for it
/// in this process. The UI's own `authorizationStatus` goes stale, so every
/// read goes through the helper (design §2.1).
@MainActor @Observable
final class MicrophonePermission {
    private static let logger = Logger(subsystem: "dev.heuss.chronicle", category: "ui")

    /// `nil` until the first read finishes.
    private(set) var status: HelperPermission?

    @ObservationIgnored private let readHelper: @Sendable () async -> String?
    @ObservationIgnored private let requestAccess: @Sendable () async -> Bool

    init(readHelper: (@Sendable () async -> String?)? = nil,
         requestAccess: (@Sendable () async -> Bool)? = nil) {
        self.readHelper = readHelper ?? {
            await MicrophonePermission.runHelper(at: MicrophonePermission.helperURL())
        }
        self.requestAccess = requestAccess ?? {
            await AVCaptureDevice.requestAccess(for: .audio)
        }
    }

    func refresh() async {
        let read = HelperPermission(helperOutput: await readHelper())
        if read == .unavailable {
            Self.logger.warning("microphone permission helper gave no answer")
        }
        status = read
    }

    func request() async -> Bool {
        await requestAccess()
    }

    nonisolated static func helperURL() -> URL {
        Bundle.main.bundleURL.appendingPathComponent("Contents/MacOS/chronicle-daemon")
    }

    /// Run the helper and return its stdout after a zero exit. Returns `nil`
    /// on a launch error, a non-zero exit, or no exit within `timeout`, in
    /// which case the helper is killed. `timeout` bounds the helper's exit
    /// only, so the helper must start no children of its own and write little
    /// output.
    nonisolated static func runHelper(
        at url: URL,
        arguments: [String] = ["--permission-status", "microphone"],
        timeout: TimeInterval = 1
    ) async -> String? {
        await withCheckedContinuation { continuation in
            DispatchQueue.global().async {
                let process = Process()
                process.executableURL = url
                process.arguments = arguments
                let output = Pipe()
                process.standardInput = FileHandle.nullDevice
                process.standardOutput = output
                process.standardError = FileHandle.nullDevice
                let exited = DispatchSemaphore(value: 0)
                process.terminationHandler = { _ in exited.signal() }
                do {
                    try process.run()
                } catch {
                    continuation.resume(returning: nil)
                    return
                }
                if exited.wait(timeout: .now() + timeout) == .timedOut {
                    kill(process.processIdentifier, SIGKILL)
                    continuation.resume(returning: nil)
                    return
                }
                guard process.terminationStatus == 0 else {
                    continuation.resume(returning: nil)
                    return
                }
                let data = output.fileHandleForReading.readDataToEndOfFile()
                continuation.resume(returning: String(decoding: data, as: UTF8.self))
            }
        }
    }
}
