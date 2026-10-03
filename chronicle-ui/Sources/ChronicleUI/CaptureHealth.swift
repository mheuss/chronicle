/// How capture is doing, from the latest status. The menu bar icon and the
/// Settings badge both classify with this.
enum CaptureHealth: Equatable {
    case noStatus, paused, running, notRunning

    /// Running only when the daemon reports capture `running`. Paused is checked
    /// first because the daemon reports "paused" whenever the pause flag is set.
    /// Any other state, including one this build does not know, is not running.
    static func classify(_ status: StatusResponse?) -> CaptureHealth {
        guard let capture = status?.data.capture else { return .noStatus }
        if capture.paused { return .paused }
        return capture.state == "running" ? .running : .notRunning
    }
}
