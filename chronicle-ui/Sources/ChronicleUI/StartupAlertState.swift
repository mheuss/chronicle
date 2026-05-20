import Foundation
import Observation

@Observable
final class StartupAlertState {
    var dismissed: Bool = false
    private(set) var hasShownThisLaunch: Bool = false

    func evaluate(status: StatusResponse) {
        guard !hasShownThisLaunch else { return }
        if status.data.capture?.paused == true {
            hasShownThisLaunch = true
        }
    }
}
