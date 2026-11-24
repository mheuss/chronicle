/// The saved onboarding progress (CHR-94 design §1.3). An absent key is `nil`.
enum OnboardingState: String, Sendable {
    case started, done

    static let key = "onboardingState"
}

enum OnboardingLaunchAction: Equatable, Sendable {
    case none, markDone, open
}

enum OnboardingLaunch {
    /// Only an absent key with an authorized read is an existing install.
    static func decide(state: OnboardingState?, read: HelperPermission) -> OnboardingLaunchAction {
        switch state {
        case .done: .none
        case .started: .open
        case nil: read == .authorized ? .markDone : .open
        }
    }
}
