import SwiftUI
import ServiceManagement

struct SettingsView: View {
    @Environment(DaemonConnection.self) private var connection
    @Environment(TranscriptionAlertState.self) private var transcriptionAlert
    @Environment(BackgroundService.self) private var background
    @Environment(ScreenRecordingPromptController.self) private var prompt
    @Environment(MicController.self) private var mic
    @State private var isPauseInFlight: Bool = false
    /// The variant the picker is showing. Tracks the daemon's `variant` unless
    /// the user is mid-change; a change the daemon refuses is rolled back to
    /// it, so this never claims a selection the daemon did not take.
    @State private var selectedVariant: String = ""
    /// Why the daemon turned down the last model change, if it did. A rejection
    /// alters no daemon state, so without this the picker would silently snap
    /// back with nothing said.
    @State private var provisionRejection: String?

    var body: some View {
        TabView {
            generalTab.tabItem { Label("General", systemImage: "gear") }
        }
        .padding(20)
        .task {
            // A rejection describes one attempt. `.onChange` below catches the
            // stale case where the state moves, but not the one where the user
            // closes Settings and comes back to a message about a click they
            // made a while ago.
            provisionRejection = nil
            background.refreshStatus()
            connection.connect()
            if connection.lastStatus == nil {
                _ = try? await connection.requestStatus()
            }
            // `.onChange` does not fire for the value that is already there,
            // and by the time Settings opens the popover has usually populated
            // `lastStatus` — so the seed has to happen here too.
            syncSelection()
            await mic.recheck()
        }
        // Settings runs its own 1 Hz poll (design §4.1). View-owned, like the
        // popover's: a user who opens Settings without ever opening the popover
        // still sees live progress, and closing the window stops the polling
        // while the daemon's download carries on regardless.
        .task(id: pollKey) {
            while !Task.isCancelled, isProvisioningActive {
                try? await Task.sleep(for: .seconds(1))
                if Task.isCancelled { return }
                _ = try? await connection.requestStatus()
            }
        }
        .onChange(of: daemonVariant) { _, _ in syncSelection() }
        .onChange(of: selectedVariant) { _, new in
            // The daemon's own variant is the echo guard: this fires both when
            // the user picks something and when `syncSelection` follows the
            // daemon, and only the first of those is a request.
            guard !new.isEmpty, let daemonVariant, new != daemonVariant else { return }
            provision(new)
        }
        // A rejection describes one attempt, not a state, so it goes stale the
        // moment the state moves. Same reasoning as the popover's.
        .onChange(of: pollKey) { _, _ in provisionRejection = nil }
    }

    private var generalTab: some View {
        VStack(alignment: .leading, spacing: 16) {
            row("Status") { statusBadge }
            row("Transcription") { transcriptionBadge }
            // Only with a current Status's block to drive them, the same gate as
            // the row above. A current Status with no block, from a daemon too
            // old to send one, gets the "Requires daemon update" wording there,
            // and an empty picker beneath it would just be noise (design §4.3).
            if let transcription = connection.currentStatus?.data.transcription {
                row("Model") { modelPicker(transcription) }
                row("Downloaded") { Text(Self.downloadedSummary(transcription.models)) }
            }
            row("Disk usage") { Text(diskUsageText) }
            row("Retention") { Text(retentionText) }
            row("Oldest record") { Text(oldestText) }
            row("Run at login") {
                VStack(alignment: .leading, spacing: 4) {
                    Toggle("Capture in the background", isOn: Binding(
                        get: { background.isEnabled },
                        set: { on in Task { await background.setEnabled(on) } }))
                        .disabled(background.isBusy)
                    let label = BackgroundService.statusLabel(background.agentStatus)
                    HStack {
                        Text(label.text).foregroundStyle(.secondary)
                        if label.offersLoginItems {
                            Button("Open Login Items") {
                                SMAppService.openSystemSettingsLoginItems()
                            }
                        }
                    }
                    if let error = background.lastError {
                        Text(error).font(.caption).foregroundStyle(.red)
                    }
                }
            }
            row("Microphone") { microphoneRow }

            Divider()

            HStack {
                Spacer()
                Button(isPaused ? "Resume Capture" : "Pause Capture") {
                    togglePause()
                }
                .controlSize(.large)
                .keyboardShortcut(.defaultAction)
                .disabled(Self.pauseButtonDisabled(
                    inFlight: isPauseInFlight,
                    connected: connection.state == .connected,
                    hasCurrentStatus: connection.currentStatus != nil))
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private var micRow: MicRowState {
        MicRowState.from(
            permission: mic.permission.status,
            status: connection.currentStatus,
            connected: connection.state == .connected,
            mark: mic.mark)
    }

    @ViewBuilder private var microphoneRow: some View {
        let state = micRow
        VStack(alignment: .leading, spacing: 4) {
            Toggle("Record the microphone", isOn: Binding(
                get: { state.isOn },
                set: { on in Task { await mic.setEnabled(on) } }))
                .disabled(!state.isEnabled || mic.isBusy)
            if let caption = state.caption {
                Text(caption).font(.caption).foregroundStyle(.secondary)
            }
            if state.showsSettingsButton {
                Button("Open Microphone Settings") {
                    NSWorkspace.shared.open(MicRowState.settingsURL)
                }
            }
        }
    }

    private func row<Content: View>(_ label: String, @ViewBuilder content: () -> Content) -> some View {
        HStack(alignment: .firstTextBaseline) {
            Text(label).foregroundStyle(.secondary).frame(width: 120, alignment: .trailing)
            content()
            Spacer()
        }
    }

    private var availability: CaptureAvailability {
        connection.availability(background: background)
    }

    private var statusBadge: some View {
        HStack(spacing: 6) {
            Image(systemName: availability.badgeSymbol).foregroundStyle(availability.tint.color)
            Text(CaptureAvailabilityCopy.settingsText(availability))
            if availability == .needsScreenRecording {
                Button(CaptureAvailabilityCopy.fixButton) { prompt.present() }
                    .accessibilityLabel(CaptureAvailabilityCopy.fixButtonAccessibilityLabel)
            }
        }
    }

    /// Transcription state, straight from the daemon's status block.
    private var transcriptionBadge: some View {
        // .firstTextBaseline, unlike the other badges: the error branch is the
        // only badge whose text can wrap, and under default centre alignment
        // the icon floats to the middle of the paragraph while the row's
        // "Transcription" label stays on line 1.
        HStack(alignment: .firstTextBaseline, spacing: 6) {
            if let transcription = connection.currentStatus?.data.transcription {
                transcriptionBadgeBody(transcription)
            } else {
                // A current Status with no block is a daemon older than this UI.
                // With no current Status, say what the connection is doing.
                Text(TranscriptionRowFallback.text(
                    hasCurrentStatus: connection.currentStatus != nil,
                    availability: availability))
                    .foregroundStyle(.secondary)
            }
        }
    }

    /// [A11y] Every branch pairs its icon and colour with words that carry the
    /// state on their own (docs/standards/accessibility.md "Text and Color").
    ///
    /// Exhaustive, with no `default:` arm — a state added to
    /// `TranscriptionState` later must fail to compile here and get classified
    /// deliberately instead of inheriting whichever branch happened to catch it.
    @ViewBuilder
    private func transcriptionBadgeBody(_ transcription: TranscriptionStats) -> some View {
        switch transcription.state {
        case .ready:
            Image(systemName: "waveform").foregroundStyle(.green)
            Text(transcription.loadedVariant.map { "Active (\($0))" } ?? "Active")
        case .unknown:
            // A state this UI can't classify, i.e. a daemon NEWER than the UI.
            // The banner still stays quiet for it (plan Decisions, 2026-08-02
            // — don't nag during version skew), but the row must not answer
            // "is transcription serving?" with a green "Active" it cannot
            // back: `.unknown` can arrive with `loadedVariant` nil, and the
            // row would then claim Active while nothing is running.
            //
            // Same words as the no-block branch above, deliberately. Both are
            // version skew — that one is a daemon too old to send the block,
            // this one a daemon too new to be understood — so they read alike.
            Text(TranscriptionRowFallback.requiresDaemonUpdate).foregroundStyle(.secondary)
        case .missing:
            Image(systemName: "waveform.slash").foregroundStyle(.secondary)
            VStack(alignment: .leading, spacing: 6) {
                Text("Off — model not downloaded")
                // Settings needs its own way to start the first download. The
                // picker cannot be it: the model the user wants is already the
                // selection, so choosing it is not a change and `onChange`
                // never fires. Without this button, a user who dismissed the
                // popover banner could only enable transcription by switching
                // to a bigger variant and back, or by relaunching.
                provisionButton(transcription)
            }
        case .error:
            Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(.red)
            errorBody(transcription)
        case .downloading, .verifying, .loading:
            ProgressView().controlSize(.small)
            Text(transcription.state.rawValue.capitalized)
        }
    }

    /// The failed-transcription row: what went wrong, the daemon's own reason,
    /// and a way back.
    ///
    /// The words come from `TranscriptionBannerCopy`, not from a second copy of
    /// the branch logic. Both surfaces describe the same daemon state, and the
    /// plan's own text for this row ("Download failed — transcription is off")
    /// is false on the path that most often produces it: a corrupt-but-present
    /// model at boot reaches `.error` having downloaded nothing (plan
    /// Decisions, 2026-08-08).
    @ViewBuilder
    private func errorBody(_ transcription: TranscriptionStats) -> some View {
        let copy = TranscriptionBannerCopy(transcription)
        VStack(alignment: .leading, spacing: 6) {
            Text(copy.headline)
            if let detail = copy.detail {
                // This is the only free-form daemon string in Settings: every
                // other daemon-sourced text in this file is bounded, so this
                // is the only one whose length the daemon can make arbitrary.
                // (Stated as the property, not as a list of the others — an
                // enumeration here goes stale the moment a row is added.)
                //
                // The Settings scene sizes to content ideal width, so
                // uncapped, this row's ideal width just tracks the error
                // length with no ceiling — measured 579pt at 70
                // characters, 1439pt at 210. Those are row widths; the window
                // adds scene padding and tab chrome on top, so it opens far
                // past the `minWidth: 480` the Settings scene is given in
                // `ChronicleApp.swift`. The 300pt cap pins the row at 458pt
                // flat no matter how long the error is, which puts the window
                // at ~500pt — essentially on its floor. The fixedSize then
                // keeps the wrapped lines from being compressed away if this
                // row ever lands in a height-constrained container. Text
                // already wraps by default — neither modifier is undoing a
                // truncation, so don't "restore" a lineLimit here.
                Text(detail)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: 300, alignment: .leading)
                    .fixedSize(horizontal: false, vertical: true)
            }
            provisionButton(transcription)
        }
    }

    /// The button that starts — or retries — a provision, for the states that
    /// have one to offer.
    ///
    /// Title and target variant come from `TranscriptionBannerCopy.action`, the
    /// same source the popover's button uses, so the two surfaces cannot offer
    /// different things for one daemon state.
    @ViewBuilder
    private func provisionButton(_ transcription: TranscriptionStats) -> some View {
        if let action = TranscriptionBannerCopy(transcription).action {
            // [A11y] A labeled button, never a bare icon. Acts on the variant
            // the daemon is already working toward; picking a DIFFERENT one is
            // the picker's job, and it stays enabled for exactly that.
            Button(action.title) { provision(action.variant) }
                .controlSize(.small)
        }
    }

    /// The variant picker, plus the rejection line when the daemon says no.
    @ViewBuilder
    private func modelPicker(_ transcription: TranscriptionStats) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            // [A11y] A real labeled Picker, so it is keyboard-reachable and
            // VoiceOver announces what it selects. `labelsHidden` hides the
            // visual label — the row already carries "Model" — while keeping
            // the accessibility label that `Picker("")` would have thrown away.
            Picker("Model", selection: $selectedVariant) {
                // Keyed on the variant, NOT on the element: `ModelEntry` is
                // Hashable over all its fields, so a completed download flips
                // `downloaded` and churns row identity mid-list.
                ForEach(transcription.models, id: \.variant) { model in
                    Text(Self.pickerLabel(model)).tag(model.variant)
                }
            }
            .labelsHidden()
            .frame(maxWidth: 220)
            .disabled(!Self.pickerEnabled(
                connected: connection.state == .connected, transcription: transcription))
            if let provisionRejection {
                Text(provisionRejection)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: 300, alignment: .leading)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
    }

    // MARK: - Transcription formatting

    // All `nonisolated`. Conforming to `View` makes the whole type
    // `@MainActor`, which these pure functions of their arguments have no need
    // of — and inheriting it is not merely untidy. Observed, not theorised:
    // with `downloadedSummary` isolated, calling it from a test off the main
    // actor died with SIGTRAP inside the `map` closure, under
    // `swift_task_isCurrentExecutor` reached from a compiler-inserted
    // isolation check. `pickerLabel` and `pickerEnabled` were isolated exactly
    // the same way and did NOT trap. Whatever separates the two cases, it is
    // not something to rely on: leaving any of these isolated is a trap
    // waiting for the next edit, and none of them touches actor state anyway.

    /// One picker row: what the variant costs, and whether picking it starts a
    /// download. [A11y] Both facts are words, never colour or an icon.
    nonisolated static func pickerLabel(_ model: ModelEntry) -> String {
        let size = byteLabel(model.sizeBytes)
        return model.downloaded
            ? "\(model.variant) — \(size)"
            : "\(model.variant) — \(size), not downloaded"
    }

    /// What is on disk right now, at a glance. An absent variant shows a dash
    /// rather than a size — the picker is where the user goes to find out what
    /// a download would cost.
    nonisolated static func downloadedSummary(_ models: [ModelEntry]) -> String {
        guard !models.isEmpty else { return "—" }
        return models
            .map { $0.downloaded ? "\($0.variant) ✓ \(byteLabel($0.sizeBytes))" : "\($0.variant) —" }
            .joined(separator: " · ")
    }

    /// Whether the picker can accept a change. Delegates the "is something in
    /// flight?" question to `TranscriptionBannerCopy` rather than re-listing
    /// the states, so a state added later has exactly one exhaustive switch to
    /// be classified in.
    ///
    /// `.error` is deliberately enabled: a failed switch is precisely when the
    /// user wants to choose something else.
    nonisolated static func pickerEnabled(connected: Bool, transcription: TranscriptionStats?) -> Bool {
        guard connected, let transcription else { return false }
        return !TranscriptionBannerCopy(transcription).showsProgress
    }

    /// The disk-usage row, including the missing-media fact when there is one.
    ///
    /// `nonisolated` and closure-free by design — see
    /// docs/development/swiftui-concurrency.md. Conforming to `View` makes the
    /// whole type `@MainActor`, and a helper that inherits that isolation dies
    /// with SIGTRAP and no error message when a test calls it off the main
    /// actor. Measured: removing `nonisolated` here does NOT currently trap,
    /// because this body contains no closure — which is exactly the difference
    /// the note says not to rely on. It stays as prophylaxis for the next edit
    /// that adds one, not because a test enforces it.
    ///
    /// [A11y] The missing-media fact is words, never colour or an icon.
    /// Shown only when non-zero, so a healthy install reads exactly as before.
    ///
    /// The two counters are sampled independently under `Ordering::Relaxed`,
    /// so `absent` can briefly exceed `served`. That renders as a bare count
    /// rather than a ratio above 1.0.
    nonisolated static func diskUsageDescription(
        totalBytes: UInt64,
        screenshots: UInt64,
        audioSegments: UInt64,
        mediaServed: UInt64?,
        mediaAbsent: UInt64?
    ) -> String {
        var text = "\(byteLabel(totalBytes)) (\(screenshots) screenshots, "
            + "\(audioSegments) audio segments)"
        if let absent = mediaAbsent, absent > 0 {
            let noun = absent == 1 ? "a missing file" : "missing files"
            if let served = mediaServed, served >= absent {
                text += " — \(absent) of \(served) served had \(noun)"
            } else {
                text += " — \(absent) with \(noun)"
            }
        }
        return text
    }

    nonisolated private static func byteLabel(_ bytes: UInt64) -> String {
        // clamping, not the trapping initializer: a wrong label beats a crash
        // if a future field ever carries a sentinel above Int64.max.
        ByteCountFormatter.string(fromByteCount: Int64(clamping: bytes), countStyle: .file)
    }

    // MARK: - Transcription actions

    private var daemonVariant: String? {
        connection.lastStatus?.data.transcription?.variant
    }

    /// The signal both the poll and the rejection clear key off: the state is
    /// what changes when the daemon moves, and an unchanged state means neither
    /// has anything to do.
    private var pollKey: TranscriptionState? {
        connection.lastStatus?.data.transcription?.state
    }

    private var isProvisioningActive: Bool {
        TranscriptionBannerCopy(connection.lastStatus?.data.transcription).showsProgress
    }

    private func syncSelection() {
        if let daemonVariant { selectedVariant = daemonVariant }
    }

    /// Ask the daemon to switch to `variant`.
    ///
    /// Rolls the picker back on refusal. `ok: false` changes nothing on the
    /// daemon side, so leaving the picker on the refused variant would have it
    /// asserting a selection that never happened.
    private func provision(_ variant: String) {
        provisionRejection = nil
        // The banner is where this operation's progress and its failure live,
        // so a dismissal from an earlier one must not hide it.
        transcriptionAlert.provisionRequested()
        Task {
            do {
                let response = try await connection.setWhisperModel(variant)
                guard !response.ok else { return }
                provisionRejection = TranscriptionBannerCopy.rejection(response.status)
                syncSelection()
            } catch {
                provisionRejection = TranscriptionBannerCopy.unreachable
                syncSelection()
            }
        }
    }

    // MARK: - Capture

    /// With no current Status the button can't know whether capture is paused,
    /// so it can't say which action it takes.
    nonisolated static func pauseButtonDisabled(
        inFlight: Bool, connected: Bool, hasCurrentStatus: Bool
    ) -> Bool {
        inFlight || !connected || !hasCurrentStatus
    }

    private var isPaused: Bool {
        availability == .paused
    }

    private var diskUsageText: String {
        guard let storage = connection.lastStatus?.data.storage else { return "—" }
        return SettingsView.diskUsageDescription(
            totalBytes: storage.totalDiskUsageBytes,
            screenshots: storage.screenshotCount,
            audioSegments: storage.audioSegmentCount,
            mediaServed: storage.mediaServed,
            mediaAbsent: storage.mediaAbsent)
    }

    private var retentionText: String {
        guard let storage = connection.lastStatus?.data.storage else { return "—" }
        return RetentionCopy.text(for: storage.retention)
    }

    private var oldestText: String {
        guard let storage = connection.lastStatus?.data.storage,
              let oldest = storage.oldestEntryMs else { return "—" }
        return Date(timeIntervalSince1970: TimeInterval(oldest) / 1000).formatted(date: .abbreviated, time: .omitted)
    }

    private func togglePause() {
        let wantPause = !isPaused
        isPauseInFlight = true
        Task {
            defer { isPauseInFlight = false }
            do {
                _ = wantPause
                    ? try await connection.pauseCapture()
                    : try await connection.resumeCapture()
                _ = try? await connection.requestStatus()
            } catch {
                // Errors surface in logs; UI shows last-known state.
            }
        }
    }
}
