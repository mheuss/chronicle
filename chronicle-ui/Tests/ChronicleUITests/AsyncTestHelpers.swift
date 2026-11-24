import Synchronization

/// Counts calls from any isolation.
final class ReadCount: Sendable {
    private let value = Mutex(0)
    func bump() { value.withLock { $0 += 1 } }
    var count: Int { value.withLock { $0 } }
}

/// Holds helper reads until `release` is called, once. Every waiter is
/// released, so a test that spawns two reads by mistake fails instead of
/// hanging. A test that needs a second hold makes a second `HeldRead`.
actor HeldRead {
    private var waiters: [CheckedContinuation<Void, Never>] = []
    private var released = false
    func wait() async {
        if released { return }
        await withCheckedContinuation { waiters.append($0) }
    }
    func release() {
        released = true
        waiters.forEach { $0.resume() }
        waiters = []
    }
}

/// Set by a main-actor task before it calls the code under test, so the test
/// can see that the task has run.
@MainActor final class StartSignal { var started = false }
