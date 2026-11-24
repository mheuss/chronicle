import AppKit
import CoreGraphics
import SwiftUI

struct OnboardingView: View {
    let controller: OnboardingWindowController
    @FocusState private var focused: OnboardingStep?

    private var model: OnboardingModel { controller.model }
    private var isComplete: Bool { controller.state == .done }

    var body: some View {
        VStack(alignment: .leading, spacing: 20) {
            screenRecordingSection
            Divider()
            micSection
            HStack {
                Spacer()
                Button(isComplete ? OnboardingCopy.closeButton : OnboardingCopy.doneButton) {
                    if isComplete { controller.close() } else { controller.finish() }
                }
                .keyboardShortcut(
                    model.screenRecordingStep.isDone && (isComplete || model.canFinish) ? .defaultAction : nil)
                .disabled(!isComplete && !model.canFinish)
            }
        }
        .padding(24)
        .frame(width: 480)
        .onAppear { focused = model.focus }
        .onChange(of: model.focus) { _, step in focused = step }
    }

    private func header(_ title: String, done: Bool) -> some View {
        HStack(spacing: 8) {
            Image(systemName: done ? "checkmark.circle.fill" : "circle")
                .foregroundStyle(done ? .green : .secondary)
                .accessibilityHidden(true)
            Text(title).font(.headline)
        }
        .accessibilityElement(children: .combine)
        .accessibilityValue(OnboardingCopy.stepValue(done: done))
    }

    private var screenRecordingSection: some View {
        let step = model.screenRecordingStep
        return VStack(alignment: .leading, spacing: 8) {
            header(OnboardingCopy.screenRecordingTitle, done: step.isDone)
            Text(OnboardingCopy.screenRecordingWhat)
            if let status = OnboardingCopy.screenRecordingStatus(step) {
                Text(status).foregroundStyle(.secondary)
            }
            if !step.isDone || !isComplete {
                Text(OnboardingCopy.screenRecordingSheets).foregroundStyle(.secondary)
            }
            if !step.isDone {
                Text(OnboardingCopy.screenRecordingHow)
                Button(OnboardingCopy.openSettingsButton) {
                    ScreenRecordingPrompt.openSettings(
                        request: { _ = CGRequestScreenCaptureAccess() },
                        open: { NSWorkspace.shared.open($0) })
                }
                .keyboardShortcut(.defaultAction)
                .focused($focused, equals: .screenRecording)
            }
        }
        .fixedSize(horizontal: false, vertical: true)
    }

    private var micSection: some View {
        let step = model.micStep
        return VStack(alignment: .leading, spacing: 8) {
            header(OnboardingCopy.micTitle, done: step.isSettled)
            Text(OnboardingCopy.micBody)
            if let status = OnboardingCopy.micStatus(step) {
                Text(status).foregroundStyle(.secondary)
            }
            switch step {
            case .locked, .ready, .noDaemon, .working:
                HStack {
                    Button(OnboardingCopy.turnOnButton) { Task { await model.turnOnMic() } }
                        .disabled(step != .ready)
                        .focused($focused, equals: .microphone)
                    Button(OnboardingCopy.skipButton) { model.skipMic() }
                        .disabled(step == .locked || step == .working)
                }
            default:
                EmptyView()
            }
        }
        .fixedSize(horizontal: false, vertical: true)
    }
}
