import Foundation

/// A single pending callback that fires after a delay: a repeating timer's
/// stand-in, so the autoscroll's pace can be driven by a clock a test
/// controls.
@MainActor
protocol AutoscrollScheduling: AnyObject {
    var now: ContinuousClock.Instant { get }
    /// Replaces any pending callback.
    func schedule(after delay: Duration, _ action: @escaping @MainActor () -> Void)
    func cancel()
}

/// The production scheduler: a one-shot `Timer` in the common run loop
/// modes, so it fires while a drag is being tracked.
@MainActor
final class TimerAutoscrollScheduler: AutoscrollScheduling {
    private var timer: Timer?

    var now: ContinuousClock.Instant { .now }

    func schedule(after delay: Duration, _ action: @escaping @MainActor () -> Void) {
        cancel()
        let seconds = Double(delay.components.seconds) + Double(delay.components.attoseconds) * 1e-18
        let timer = Timer(timeInterval: seconds, repeats: false) { _ in
            MainActor.assumeIsolated { action() }
        }
        RunLoop.main.add(timer, forMode: .common)
        self.timer = timer
    }

    func cancel() {
        timer?.invalidate()
        timer = nil
    }
}

/// Scrolls the view while a selection drag has the pointer outside it, at
/// iTerm2's pace (`iTermSelectionScrollHelper`): the first tick comes
/// 100 ms after the pointer leaves, each tick shortens the wait by 5 %
/// (never below 10 ms), and a tick scrolls `elapsed / wait` lines, at least
/// 1 and at most 100, so a late tick catches up. It keeps ticking with the
/// pointer still and stops when told to (the button went up, or the pointer
/// came back).
@MainActor
final class SelectionAutoscroller {
    enum Direction: Equatable {
        /// The pointer is above the view: towards older output.
        case up
        /// The pointer is below the view: towards the live screen.
        case down
    }

    static let initialDelay: Duration = .milliseconds(100)
    static let minimumDelay: Duration = .milliseconds(10)
    static let speedUp = 0.95
    static let maxLinesPerTick = 100

    /// Replaced by tests with a clock they advance.
    var scheduler: any AutoscrollScheduling
    /// Called on every tick with the direction and the lines to scroll.
    var onTick: (Direction, Int) -> Void = { _, _ in }

    private(set) var direction: Direction?
    private var delay = SelectionAutoscroller.initialDelay
    private var lastTick = ContinuousClock.Instant.now

    init(scheduler: any AutoscrollScheduling = TimerAutoscrollScheduler()) {
        self.scheduler = scheduler
    }

    /// The pointer is outside the view in `direction`, or inside (`nil`).
    /// Staying outside in the same direction keeps the running pace.
    func update(direction new: Direction?) {
        guard new != direction else { return }
        stop()
        guard let new else { return }
        direction = new
        delay = Self.initialDelay
        lastTick = scheduler.now
        scheduleNext()
    }

    func stop() {
        direction = nil
        scheduler.cancel()
    }

    private func scheduleNext() {
        scheduler.schedule(after: delay) { [weak self] in self?.tick() }
    }

    private func tick() {
        guard let direction else { return }
        let now = scheduler.now
        let lines = min(max(Int((now - lastTick) / delay), 1), Self.maxLinesPerTick)
        lastTick = now
        delay = max(Self.minimumDelay, delay * Self.speedUp)
        // Scheduled first: the tick may end the gesture (and with it this).
        scheduleNext()
        onTick(direction, lines)
    }
}
