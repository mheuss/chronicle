import Foundation
import Observation

/// Communicates with chronicle-daemon over a Unix domain socket.
///
/// Protocol: newline-delimited JSON request/response.
/// Socket path: ~/Library/Application Support/Chronicle/chronicle.sock
@MainActor
@Observable
final class DaemonConnection {

    // MARK: - State

    enum ConnectionState {
        case disconnected
        case connecting
        case connected
    }

    private(set) var state: ConnectionState = .disconnected
    private(set) var lastStatus: StatusResponse?

    // MARK: - Socket Path

    static let socketPath: String = {
        let appSupport = FileManager.default.urls(
            for: .applicationSupportDirectory,
            in: .userDomainMask
        ).first!
        return appSupport
            .appendingPathComponent("Chronicle")
            .appendingPathComponent("chronicle.sock")
            .path
    }()

    // MARK: - Constants

    private static let maxResponseSize = 64 * 1024

    // MARK: - Private

    private var session: ConnectionSession?
    private var reconnectTask: Task<Void, Never>?
    private var requestQueue: Task<Void, Never>?

    /// Establishes a connection and returns a connected descriptor.
    ///
    /// Injected because the production path resolves `Self.socketPath`, a
    /// static pointing at the real socket — without this seam the reconnect
    /// and teardown requirements cannot be tested.
    private let connectionFactory: @MainActor @Sendable () async throws -> Int32

    #if DEBUG
    var sessionForTesting: ConnectionSession? { session }
    #endif

    private let encoder = JSONEncoder()
    private let decoder: JSONDecoder = {
        let d = JSONDecoder()
        d.keyDecodingStrategy = .convertFromSnakeCase
        return d
    }()

    // MARK: - Init

    init() {
        self.connectionFactory = { try await Self.connectToDaemonSocket() }
    }

    /// Builds a connection whose descriptors come from `connectionFactory`
    /// instead of the real socket path.
    internal init(connectionFactory: @escaping @MainActor @Sendable () async throws -> Int32) {
        self.connectionFactory = connectionFactory
    }

    /// **Test-only.** Not for production use. Wires `DaemonConnection` to a
    /// pre-connected fd (e.g., one side of a `socketpair()`). The fd is taken
    /// over by the session built around it, which closes it once neither the
    /// reader nor a write still holds it.
    ///
    /// `setsockopt(SO_NOSIGPIPE)` failure here aborts via `precondition` so a
    /// genuine macOS quirk surfaces as a loud test-time crash rather than a
    /// confusing SIGPIPE later inside `brokenPipeDoesNotCrash`. Production
    /// callers go through `connectToDaemonSocket`, which checks the result and
    /// throws `IPCError.socketCreationFailed` on failure.
    ///
    /// This connection never reconnects, so its factory is never called.
    internal init(testingSocketFD fd: Int32) {
        var noSigPipe: Int32 = 1
        let result = setsockopt(
            fd,
            SOL_SOCKET,
            SO_NOSIGPIPE,
            &noSigPipe,
            socklen_t(MemoryLayout<Int32>.size)
        )
        precondition(result == 0, "SO_NOSIGPIPE failed in testing init: errno=\(errno)")
        self.connectionFactory = { preconditionFailure("testing connections do not reconnect") }
        let session = ConnectionSession(fd: fd, maxResponseSize: Self.maxResponseSize)
        session.start()
        self.session = session
        self.state = .connected
    }

    // MARK: - Lifecycle

    /// Begin connecting to the daemon. Auto-reconnects on failure.
    func connect() {
        guard reconnectTask == nil else { return }
        reconnectTask = Task {
            var delay: Duration = .seconds(1)
            while !Task.isCancelled {
                state = .connecting
                do {
                    let fd = try await connectionFactory()
                    let newSession = ConnectionSession(fd: fd, maxResponseSize: Self.maxResponseSize)
                    guard !Task.isCancelled else {
                        // Closes the descriptor. The old code broke here
                        // without closing what it had just built, leaking one
                        // descriptor for the life of the process.
                        newSession.close()
                        break
                    }
                    newSession.start()
                    session = newSession
                    state = .connected
                    delay = .seconds(1)
                    await raceHeartbeatAgainstSession(newSession)
                } catch is CancellationError {
                    break
                } catch {
                    // Establishment failed. The backoff below is shared with
                    // the race returning, which does not throw.
                }
                // Only this task's own session is ours to close. A cancelled
                // task is one `disconnect()` already cleaned up after, and by
                // the time it reaches here a newer `connect()` may have
                // published a session of its own.
                guard !Task.isCancelled else { break }
                closeSocket()
                state = .disconnected
                try? await Task.sleep(for: delay)
                delay = min(delay * 2, .seconds(30))
            }
        }
    }

    /// Disconnect and stop auto-reconnect.
    func disconnect() {
        reconnectTask?.cancel()
        reconnectTask = nil
        closeSocket()
        state = .disconnected
    }

    // MARK: - Requests

    /// Send a status request and return the response.
    func requestStatus() async throws -> StatusResponse {
        let response = try await sendRequest(
            IPCRequest(type: "status"),
            expecting: StatusResponse.self
        )
        lastStatus = response
        return response
    }

    /// Toggle microphone capture on the daemon. Returns the resulting state.
    /// Callers read the outcome from the returned `MicState`.
    func setMicEnabled(_ enabled: Bool) async throws -> MicState {
        let response = try await sendRequest(
            SetMicEnabledRequest(type: "set_mic_enabled", enabled: enabled),
            expecting: SetMicEnabledResponse.self
        )
        return response.state
    }

    /// Select (and download if needed) a whisper model variant.
    ///
    /// The daemon replies as soon as it accepts or rejects — it never waits
    /// for the download or the load (AD-9), so `ok` means "the provisioner
    /// took the job", not "the model is ready". Progress arrives via `Status`.
    ///
    /// `ok: false` collapses three cases the wire cannot tell apart: an
    /// unknown variant, a provision already in flight, and a daemon not yet
    /// ready. The returned `status` is what callers should render from.
    ///
    /// On success this issues one status refresh so the banner and Settings
    /// update immediately instead of waiting for the next poll tick.
    func setWhisperModel(_ variant: String) async throws -> SetWhisperModelResponse {
        struct Req: Codable, Sendable {
            let type: String
            let variant: String
        }
        let response = try await sendRequest(
            Req(type: "set_whisper_model", variant: variant),
            expecting: SetWhisperModelResponse.self
        )
        if response.ok {
            // The refresh is still best-effort, but its failure is NOT
            // harmless. Both views decide whether to run their 1 Hz
            // provisioning poll by reading `lastStatus`, so leaving it on the
            // pre-request state means neither loop starts: no progress bar,
            // and a fast failure invisible until the connection monitor's
            // 30-second tick. "One second of staleness" was only ever true
            // when the poll was already running.
            //
            // The reply we are holding carries the state the daemon just
            // entered, so on a failed refresh we publish that instead.
            //
            // Not a total fallback: with `lastStatus` still nil there is no
            // snapshot to splice into and this does nothing. Practically
            // unreachable — the model UI only renders once a transcription
            // block has arrived — but it is not a guarantee.
            //
            // `lastStatus` is read here, after the await, rather than
            // captured before it: a poll tick can land a fresher snapshot in
            // between, and overwriting that with this older reply block would
            // be a step backwards. Bounded to one tick and self-correcting
            // either way, since at the moment this matters the poll is not
            // yet running.
            if (try? await requestStatus()) == nil, let last = lastStatus {
                lastStatus = last.replacingTranscription(response.status)
            }
        }
        return response
    }

    /// Search the daemon's OCR index. Returns up to `limit` screen-only hits
    /// ranked by relevance. Audio results are added in CHR-76.
    ///
    /// `limit`/`offset` are clamped into `UInt32` — the wire protocol uses
    /// `u32`, and the unchecked `UInt32(_:)` initializer would trap on a
    /// negative or oversized `Int`. Negative inputs clamp to 0, which the
    /// daemon then bounds-checks against its own ceiling.
    func search(_ query: String, limit: Int = 50, offset: Int = 0) async throws -> [SearchHit] {
        struct Req: Codable, Sendable {
            let type: String
            let query: String
            let limit: UInt32
            let offset: UInt32
        }
        let response = try await sendRequest(
            Req(
                type: "search",
                query: query,
                limit: UInt32(clamping: limit),
                offset: UInt32(clamping: offset)
            ),
            expecting: SearchResponse.self
        )
        return response.hits
    }

    /// Fetch a single screenshot's metadata by id. Returns `nil` when the id
    /// is unknown (e.g. retention cleanup removed the row).
    func getScreenshot(id: Int64) async throws -> SearchHit? {
        struct Req: Codable, Sendable {
            let type: String
            let id: Int64
        }
        let response = try await sendRequest(
            Req(type: "get_screenshot", id: id),
            expecting: GetScreenshotResponse.self
        )
        return response.hit
    }

    /// Pause screen + audio capture. Returns the resulting paused state.
    /// Issues a status refresh on success so the menu bar icon, Settings,
    /// and any banner observers see the new state immediately.
    func pauseCapture() async throws -> Bool {
        let response = try await sendRequest(
            IPCRequest(type: "pause_capture"),
            expecting: PauseResumeResponse.self
        )
        if response.ok {
            _ = try? await requestStatus()
        }
        return response.paused
    }

    /// Resume capture. Returns the resulting paused state.
    /// Issues a status refresh on success (see `pauseCapture`).
    func resumeCapture() async throws -> Bool {
        let response = try await sendRequest(
            IPCRequest(type: "resume_capture"),
            expecting: PauseResumeResponse.self
        )
        if response.ok {
            _ = try? await requestStatus()
        }
        return response.paused
    }

    /// Generic request helper. Its `requestQueue` chain keeps one request
    /// outstanding at a time, which is what holds AD-8 — at most one waiter on
    /// a live session. That rests on this being the sole production caller of
    /// `ConnectionSession.send`, not on the type system: `send` is internal, so
    /// a method reaching for it directly would compile and break the invariant.
    ///
    /// The unstructured `Task` here is intentional: caller cancellation must
    /// not abort socket I/O mid-write/read, or we'd leave the newline-delimited
    /// protocol in a corrupt state.
    ///
    /// The Task strongly captures `self` and is then stored in
    /// `self.requestQueue`, creating a temporary retain cycle. This is safe:
    /// `closeSocket()` closes the session, which resumes every waiting caller
    /// and wakes the reader, which lets the task complete and the cycle break.
    /// Hosts should still call `disconnect()` on teardown rather than relying
    /// on ARC alone — that's the only path that drops the cycle deterministically.
    private func sendRequest<Req: Encodable & Sendable, Res: Decodable & Sendable>(
        _ request: Req,
        expecting: Res.Type
    ) async throws -> Res {
        let prev = requestQueue
        guard let mySession = session else {
            throw IPCError.notConnected
        }
        // The Task closure inherits @MainActor from this enclosing context, so
        // accesses to `self.session`/`encoder`/`decoder` below are
        // actor-isolated and require no `await`.
        let task = Task<Res, Error> {
            _ = await prev?.value  // wait for predecessor
            // Session identity replaces the generation counter: a request that
            // waited through a reconnect is holding the previous session.
            // This is the live throw site on that path, not a backstop:
            // measured with a sentinel error, `queuedRequestAfterDisconnect`
            // throws from here, and only falls through to `send`'s own
            // `state == .live` guard if this one is removed. Both raise
            // `notConnected`, so no test can tell which fired — unpinnable, but
            // not unused.
            guard self.session === mySession else {
                throw IPCError.notConnected
            }
            let data = try self.encoder.encode(request)
            guard var line = String(data: data, encoding: .utf8) else {
                throw IPCError.encodingFailed
            }
            line.append("\n")
            let responseLine = try await mySession.send(line)
            let responseData = Data(responseLine.utf8)
            do {
                return try self.decoder.decode(Res.self, from: responseData)
            } catch let firstError {
                if let err = try? self.decoder.decode(ErrorResponse.self, from: responseData) {
                    throw IPCError.daemonError(err.message, code: err.code)
                }
                throw IPCError.malformedResponse(String(describing: firstError))
            }
        }
        requestQueue = Task { _ = try? await task.value }
        return try await task.value
    }

    // MARK: - Socket Operations

    /// Connects to the daemon's socket and returns the connected descriptor.
    private static func connectToDaemonSocket() async throws -> Int32 {
        let path = Self.socketPath
        let fd: Int32 = try await withCheckedThrowingContinuation { cont in
            DispatchQueue.global().async {
                let fd = socket(AF_UNIX, SOCK_STREAM, 0)
                guard fd >= 0 else {
                    cont.resume(throwing: IPCError.socketCreationFailed(errno: errno))
                    return
                }

                var noSigPipe: Int32 = 1
                let setoptResult = setsockopt(
                    fd,
                    SOL_SOCKET,
                    SO_NOSIGPIPE,
                    &noSigPipe,
                    socklen_t(MemoryLayout<Int32>.size)
                )
                guard setoptResult == 0 else {
                    let setoptErrno = errno
                    Darwin.close(fd)
                    cont.resume(throwing: IPCError.socketCreationFailed(errno: setoptErrno))
                    return
                }

                var addr = sockaddr_un()
                addr.sun_family = sa_family_t(AF_UNIX)
                let pathBytes = path.utf8CString
                guard pathBytes.count <= MemoryLayout.size(ofValue: addr.sun_path) else {
                    Darwin.close(fd)
                    cont.resume(throwing: IPCError.pathTooLong)
                    return
                }
                withUnsafeMutablePointer(to: &addr.sun_path) { ptr in
                    ptr.withMemoryRebound(to: CChar.self, capacity: pathBytes.count) { dest in
                        pathBytes.withUnsafeBufferPointer { src in
                            _ = memcpy(dest, src.baseAddress!, src.count)
                        }
                    }
                }

                let result = withUnsafePointer(to: &addr) { ptr in
                    ptr.withMemoryRebound(to: sockaddr.self, capacity: 1) { sockPtr in
                        Darwin.connect(fd, sockPtr, socklen_t(MemoryLayout<sockaddr_un>.size))
                    }
                }

                guard result == 0 else {
                    let connectErrno = errno
                    Darwin.close(fd)
                    cont.resume(throwing: IPCError.connectionFailed(errno: connectErrno))
                    return
                }

                cont.resume(returning: fd)
            }
        }

        return fd
    }

    /// Run the heartbeat and the session's completion against each other, and
    /// end the connection when either finishes.
    ///
    /// This does not close the session it is handed — `connect()`'s backoff
    /// tail does, on every path out of the loop body.
    ///
    /// The monitor's error is swallowed here and must stay swallowed:
    /// `cancelAll` makes `CancellationError` the ordinary way it ends, and
    /// `connect()` reads that error as "stop reconnecting", so propagating it
    /// leaves the reconnect loop for good rather than reaching the backoff.
    private func raceHeartbeatAgainstSession(_ racedSession: ConnectionSession) async {
        await withTaskGroup(of: Void.self) { group in
            group.addTask {
                _ = try? await self.monitorConnection()
            }
            group.addTask {
                await racedSession.waitUntilFinished()
            }

            _ = await group.next()
            // Without this, a session that finishes just after a heartbeat
            // waits out the rest of monitorConnection's 30 second sleep — the
            // delay this feature removes. `idleEOFReconnectsPromptly` fails
            // when it is removed. It is also the only thing that frees the
            // other child when the monitor exits first, though no test is in
            // that shape today.
            group.cancelAll()
            await group.waitForAll()
        }
    }

    private func monitorConnection() async throws {
        while !Task.isCancelled && session != nil {
            _ = try await requestStatus()
            try await Task.sleep(for: .seconds(30))
        }
    }

    private func closeSocket() {
        session?.close()
        session = nil
        requestQueue = nil
    }
}

// MARK: - Protocol Types

struct IPCRequest: Codable, Sendable {
    let type: String
}

enum MicState: String, Codable, Sendable {
    case off, on
    case permissionDenied = "permission_denied"
    case error
    /// A mic state this UI doesn't know yet (newer daemon). `AudioStats` is
    /// reached through an optional, but that is no protection — a present key
    /// whose value fails to decode throws instead of degrading to nil, and the
    /// throw kills the whole StatusResponse. Same reasoning as
    /// `TranscriptionState.unknown`; formal policy is CHR-77.
    case unknown

    init(from decoder: Decoder) throws {
        let raw = try decoder.singleValueContainer().decode(String.self)
        self = MicState(rawValue: raw) ?? .unknown
    }
}

struct SetMicEnabledRequest: Codable, Sendable {
    let type: String
    let enabled: Bool
}

struct SetMicEnabledResponse: Codable, Sendable {
    let type: String
    let ok: Bool
    let state: MicState
}

struct StatusResponse: Decodable, Sendable {
    let type: String
    let ok: Bool
    let data: StatusData

    /// A copy carrying a fresher transcription block, leaving every other
    /// field alone.
    ///
    /// Not a status refresh — one field arriving by a different route. The
    /// `set_whisper_model` reply already contains the state the daemon just
    /// entered, so when the follow-up refresh fails there is no reason to
    /// throw that away and show the user nothing. The rest of the snapshot
    /// stays as it was and the next poll replaces all of it.
    func replacingTranscription(_ stats: TranscriptionStats) -> StatusResponse {
        StatusResponse(
            type: type,
            ok: ok,
            data: StatusData(
                uptimeSecs: data.uptimeSecs,
                version: data.version,
                capture: data.capture,
                ocr: data.ocr,
                audio: data.audio,
                storage: data.storage,
                transcription: stats))
    }
}

struct StatusData: Decodable, Sendable {
    let uptimeSecs: UInt64
    let version: String
    let capture: CaptureStats?
    let ocr: OcrStats?
    let audio: AudioStats?
    let storage: StorageStats?
    /// Optional so an old daemon (no transcription block) decodes to nil
    /// instead of failing the whole status response (NFR-7).
    let transcription: TranscriptionStats?
}

struct CaptureStats: Codable, Sendable {
    let state: String
    let activeDisplays: Int
    let framesCaptured: UInt64
    let framesDropped: UInt64
    let framesProcessed: UInt64
    let framesFailed: UInt64
    let paused: Bool
}

struct OcrStats: Codable, Sendable {
    let enqueued: UInt64
    let dropped: UInt64
}

struct AudioStats: Codable, Sendable {
    let segmentsPersisted: UInt64
    let micState: MicState
}

struct StorageStats: Decodable, Sendable {
    let dbSizeBytes: UInt64
    let totalDiskUsageBytes: UInt64
    let screenshotCount: UInt64
    let audioSegmentCount: UInt64
    let oldestEntryMs: Int64?
    /// What the configured retention means. `nil` two ways: the daemon sent
    /// `null` because it could not read `retention_days`, or a daemon too old
    /// to send the field omitted it. Either way this one row degrades rather
    /// than the whole status block.
    let retention: Retention?
    /// Optional on the decoder, NOT on the wire — Rust sends these
    /// non-optionally. An older daemon omits them, and a non-optional field
    /// here would fail the decode of this entire block, taking disk usage and
    /// retention down with it. See docs/use-cases/ipc-compat.md.
    ///
    /// The pair is sampled rather than read atomically, so a ratio above 1.0
    /// is sampling skew, not data. That is a property of the current Rust
    /// implementation as of CHR-34, not of the wire format — the source of
    /// truth is `PipelineCounters::snapshot` in chronicle-daemon.
    let mediaServed: UInt64?
    /// Of `mediaServed`, how many had no file on disk.
    ///
    /// A non-zero value is NOT by itself an alarm: retention deletes files
    /// before rows within a batch, so a search served during a cleanup sees
    /// rows whose files are already gone.
    ///
    /// The counter only increments and resets at process start, so it cannot
    /// distinguish that from a real fault within one daemon lifetime — and it
    /// counts serve events, not distinct rows. Copy shown to a user should not
    /// present a non-zero reading as a failure.
    let mediaAbsent: UInt64?
}

/// What the daemon's `retention_days` setting means, as sent by
/// `chronicle-ipc`'s `Retention`.
///
/// `Decodable` only — nothing in the UI encodes a status response, and Swift's
/// synthesized encoding would emit `{"days":{"_0":30}}`, nothing like the wire.
///
/// The decoder is hand-written: a synthesized one reads every declared field
/// regardless of `kind`, so a future variant with a wrongly typed payload would
/// throw and take the whole `StatusData` decode with it.
///
/// A `retention` that is not an object, or a `kind` absent or not a string,
/// still throws, deliberately: either means the peer is not the daemon we
/// think it is. `MicState`, `TranscriptionState`, `DaemonErrorCode` and
/// `SearchHitSource` all draw the same line.
enum Retention: Sendable, Equatable, Decodable {
    case days(UInt32)
    case disabled
    case invalid
    /// A kind this UI doesn't know yet (newer daemon). Same rule as
    /// `TranscriptionState`, `MicState` and `SearchHitSource` — see
    /// docs/use-cases/ipc-compat.md.
    case unknown

    private enum CodingKeys: String, CodingKey {
        case kind, value
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        switch try c.decode(String.self, forKey: .kind) {
        case "days":
            // `try?`, not `try`: a malformed value is not a day count, and
            // throwing would take StatusData into the CHR-6 reconnect loop.
            // The flattening means this one guard also covers an absent key.
            guard let value = try? c.decodeIfPresent(UInt32.self, forKey: .value) else {
                self = .unknown
                return
            }
            self = .days(value)
        case "disabled":
            self = .disabled
        case "invalid":
            self = .invalid
        default:
            self = .unknown
        }
    }
}

enum TranscriptionState: String, Codable, Sendable {
    case missing, downloading, verifying, loading, ready, error
    /// A state this UI doesn't know yet (newer daemon). Decoding to a
    /// fallback keeps the whole StatusResponse alive — a closed enum would
    /// throw and take status polling dark. Formal cross-version policy is
    /// CHR-77; views treat .unknown like .ready (no banner, no row alarm).
    case unknown

    init(from decoder: Decoder) throws {
        let raw = try decoder.singleValueContainer().decode(String.self)
        self = TranscriptionState(rawValue: raw) ?? .unknown
    }
}

struct ModelEntry: Codable, Sendable, Hashable {
    let variant: String
    let downloaded: Bool
    let sizeBytes: UInt64
}

struct TranscriptionStats: Codable, Sendable {
    let state: TranscriptionState
    /// The variant the daemon is acting on (settings value at boot, or the
    /// requested variant during/after a switch attempt).
    let variant: String
    /// The engine actually serving, if any. In `.error` state: nil means
    /// initial provisioning failed (transcription off); non-nil means a
    /// switch failed and this engine still serves.
    let loadedVariant: String?
    let error: String?
    /// Set only while downloading; total is nil until Content-Length is
    /// known — render an indeterminate bar, never divide by a 0 total.
    let downloadBytes: UInt64?
    let downloadTotalBytes: UInt64?
    let models: [ModelEntry]
}

/// Reply to `set_whisper_model`. `ok` is the accept/reject decision only;
/// `status` is the transcription block read after that decision, so an
/// accepted switch normally shows `.downloading` or `.loading` — but a fast
/// failure (a disk precheck, say) can already have moved it on.
struct SetWhisperModelResponse: Codable, Sendable {
    let type: String
    let ok: Bool
    let status: TranscriptionStats
}

/// A stable, machine-readable failure reason from the daemon.
///
/// Tolerant decode for the same reason `TranscriptionState` has one: a closed
/// enum on a wire field throws on an unrecognized value, and that failure
/// takes the whole response down rather than just this field.
enum DaemonErrorCode: String, Codable, Sendable {
    case requestTooLarge = "request_too_large"
    case invalidUtf8 = "invalid_utf8"
    case invalidRequest = "invalid_request"
    case unknown

    init(from decoder: Decoder) throws {
        let raw = try decoder.singleValueContainer().decode(String.self)
        self = DaemonErrorCode(rawValue: raw) ?? .unknown
    }
}

struct ErrorResponse: Codable, Sendable {
    let type: String
    let ok: Bool
    /// The protocol reference tells clients to branch on this rather than on
    /// the human-readable message — and until now the one client could not,
    /// because it wasn't decoded. Optional so a daemon that predates the
    /// field still decodes.
    let code: DaemonErrorCode?
    let message: String
}

// MARK: - Search

struct SearchHit: Codable, Sendable, Identifiable, Hashable {
    let id: Int64
    let source: SearchHitSource
    let timestampMs: Int64
    let appName: String?
    let appBundleId: String?
    let windowTitle: String?
    let imagePath: String
    let snippet: String
    let rank: Double
}

enum SearchHitSource: String, Codable, Sendable {
    case screen
    /// A hit source this UI doesn't know yet. Rust reserves
    /// `SearchHitSource::Audio` for CHR-76, and `SearchHit.source` is not
    /// optional — a closed enum here would throw and take the entire
    /// SearchResponse down, so one audio hit would blank out every screen hit
    /// beside it and search would go dark. Degrade instead. Formal policy is
    /// CHR-77; this only guards decoding.
    case unknown

    init(from decoder: Decoder) throws {
        let raw = try decoder.singleValueContainer().decode(String.self)
        self = SearchHitSource(rawValue: raw) ?? .unknown
    }
}

struct SearchResponse: Codable, Sendable {
    let type: String
    let ok: Bool
    let hits: [SearchHit]
}

struct GetScreenshotResponse: Codable, Sendable {
    let type: String
    let ok: Bool
    let hit: SearchHit?
}

struct PauseResumeResponse: Codable, Sendable {
    let type: String
    let ok: Bool
    let paused: Bool
}

// MARK: - Errors

enum IPCError: Error, LocalizedError, Equatable {
    case socketCreationFailed(errno: Int32)
    case pathTooLong
    case connectionFailed(errno: Int32)
    case notConnected
    case writeFailed(errno: Int32)
    case readFailed(errno: Int32)
    case connectionClosed
    case encodingFailed
    case responseTooLarge
    /// The daemon refused the request. `code` is the stable, machine-readable
    /// reason the protocol tells clients to branch on; `nil` from a daemon
    /// that predates the field. Carried here rather than decoded and dropped,
    /// so a caller that wants to branch actually can.
    case daemonError(String, code: DaemonErrorCode?)
    case invalidUTF8
    case malformedResponse(String)

    var errorDescription: String? {
        switch self {
        case .socketCreationFailed(let e): "Socket setup failed: \(String(cString: strerror(e)))"
        case .pathTooLong: "Socket path too long"
        case .connectionFailed(let e): "Connection failed: \(String(cString: strerror(e)))"
        case .notConnected: "Not connected to daemon"
        case .writeFailed(let e): "Write failed: \(String(cString: strerror(e)))"
        case .readFailed(let e): "Read failed: \(String(cString: strerror(e)))"
        case .connectionClosed: "Connection closed by daemon"
        case .encodingFailed: "Failed to encode request"
        case .responseTooLarge: "Response exceeded maximum size"
        case .daemonError(let msg, _): "Daemon error: \(msg)"
        case .invalidUTF8: "Response contained invalid UTF-8"
        case .malformedResponse(let detail): "Malformed daemon response: \(detail)"
        }
    }
}
