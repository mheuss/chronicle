import Testing
@testable import ChronicleUI

@Suite(.watchdog)
struct OnboardingLaunchTests {
    static let rows: [(OnboardingState?, HelperPermission, OnboardingLaunchAction)] = [
        (nil, .authorized, .markDone),
        (nil, .denied, .open),
        (nil, .notDetermined, .open),
        (nil, .restricted, .open),
        (nil, .unavailable, .open),
        (.started, .authorized, .open),
        (.started, .denied, .open),
        (.started, .notDetermined, .open),
        (.started, .restricted, .open),
        (.started, .unavailable, .open),
        (.done, .authorized, .none),
        (.done, .denied, .none),
        (.done, .notDetermined, .none),
        (.done, .restricted, .none),
        (.done, .unavailable, .none),
    ]

    @Test(arguments: rows)
    func decide(state: OnboardingState?, read: HelperPermission, expected: OnboardingLaunchAction) {
        #expect(OnboardingLaunch.decide(state: state, read: read) == expected)
    }

    @Test func keyRawValues() {
        #expect(OnboardingState.key == "onboardingState")
        #expect(OnboardingState(rawValue: "started") == .started)
        #expect(OnboardingState(rawValue: "done") == .done)
        #expect(OnboardingState(rawValue: "") == nil)
    }
}
