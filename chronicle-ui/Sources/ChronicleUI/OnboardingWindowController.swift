import Foundation
import Observation
import os

/// Shows the onboarding window, or brings it forward when it is open.
@MainActor
protocol OnboardingPresenter: AnyObject {
    func present(_ controller: OnboardingWindowController)
    /// Closes the window. The presenter then calls `didClose()`.
    func close()
}

/// Owns the onboarding key and decides when the window opens (CHR-94
/// design §1, §2.1).
@MainActor @Observable
final class OnboardingWindowController {
    private static let logger = Logger(subsystem: "dev.heuss.chronicle", category: "ui")

    private(set) var state: OnboardingState?
    private(set) var isOpen = false
    let model: OnboardingModel

    @ObservationIgnored private let reader: ScreenRecordingPermission
    @ObservationIgnored private let defaults: UserDefaults
    @ObservationIgnored private let presenter: OnboardingPresenter
    @ObservationIgnored private var launchChecked = false

    init(model: OnboardingModel, reader: ScreenRecordingPermission,
         defaults: UserDefaults = .standard, presenter: OnboardingPresenter) {
        self.model = model
        self.reader = reader
        self.defaults = defaults
        self.presenter = presenter
        state = Self.savedState(in: defaults)
    }

    /// Runs once per process. The key is read after the helper read returns,
    /// so a Fix… press during the read counts as `started` (design §1.2).
    func launchCheck() async {
        guard !launchChecked else { return }
        launchChecked = true
        let clock = ContinuousClock()
        let began = clock.now
        let read = await reader.read()
        let elapsed = began.duration(to: clock.now)
        Self.logger.debug("onboarding launch read: \(String(describing: read), privacy: .public) in \(String(describing: elapsed), privacy: .public)")
        let saved = Self.savedState(in: defaults)
        state = saved
        switch OnboardingLaunch.decide(state: saved, read: read) {
        case .stay: break
        case .markDone: save(.done)
        case .open: show(.firstUnsettled(screenRecordingDone: read == .authorized))
        }
    }

    /// The popover's Finish row. The model's last read can be stale while the
    /// window is closed, so this takes a fresh one.
    func resume() async {
        let read = await reader.read()
        show(.firstUnsettled(screenRecordingDone: read == .authorized))
    }

    func show(_ step: OnboardingStep) {
        if Self.savedState(in: defaults) == nil { save(.started) }
        model.focus = step
        if !isOpen {
            isOpen = true
            model.start()
        }
        presenter.present(self)
    }

    func finish() {
        save(.done)
        presenter.close()
    }

    /// Close: leaves the key as it is.
    func close() {
        presenter.close()
    }

    func didClose() {
        isOpen = false
        model.stop()
    }

    private func save(_ new: OnboardingState) {
        defaults.set(new.rawValue, forKey: OnboardingState.key)
        state = new
    }

    private static func savedState(in defaults: UserDefaults) -> OnboardingState? {
        defaults.string(forKey: OnboardingState.key).flatMap(OnboardingState.init(rawValue:))
    }
}
