@testable import TesseraKit
import XCTest

/// The OSC 52 write throttle (ADR 0022 D-RATE52), on synthetic instants: no
/// sleeping.
final class ClipboardThrottleTests: XCTestCase {
    private typealias Write = ClipboardThrottle.Write
    private let start = ContinuousClock.now
    private let interval = ClipboardThrottle.minimumInterval

    private func at(_ offset: Duration) -> ContinuousClock.Instant { start + offset }

    func testTheCapIsTenWritesASecond() {
        XCTAssertEqual(ClipboardThrottle.maxWritesPerSecond, 10)
        XCTAssertEqual(interval, .milliseconds(100))
    }

    func testTheFirstWriteAfterQuietIsImmediate() {
        var throttle = ClipboardThrottle()
        XCTAssertEqual(throttle.offer(Write(session: 1, text: "a"), at: at(.zero)), Write(session: 1, text: "a"))
        XCTAssertNil(throttle.held)
        XCTAssertNil(throttle.dueAt, "nothing held, nothing to release")
        // Quiet for the whole interval: immediate again.
        XCTAssertEqual(throttle.offer(Write(session: 2, text: "b"), at: at(interval)), Write(session: 2, text: "b"))
        XCTAssertNil(throttle.held)
    }

    func testASecondWriteWithinTheIntervalIsHeld() {
        var throttle = ClipboardThrottle()
        _ = throttle.offer(Write(session: 1, text: "a"), at: at(.zero))
        XCTAssertNil(throttle.offer(Write(session: 1, text: "b"), at: at(.milliseconds(99))))
        XCTAssertEqual(throttle.held, Write(session: 1, text: "b"))
        XCTAssertEqual(throttle.dueAt, at(interval))
    }

    func testTheLatestHeldWriteWinsAcrossSessions() {
        var throttle = ClipboardThrottle()
        _ = throttle.offer(Write(session: 1, text: "a"), at: at(.zero))
        XCTAssertNil(throttle.offer(Write(session: 1, text: "b"), at: at(.milliseconds(10))))
        XCTAssertNil(throttle.offer(Write(session: 2, text: "c"), at: at(.milliseconds(20))))
        XCTAssertNil(throttle.offer(Write(session: 1, text: "d"), at: at(.milliseconds(30))))
        XCTAssertEqual(throttle.held, Write(session: 1, text: "d"), "one held write, the latest")
        XCTAssertEqual(throttle.dueAt, at(interval), "later writes do not push the release back")
        XCTAssertEqual(throttle.release(at: at(interval)), Write(session: 1, text: "d"))
    }

    func testTheHeldWriteIsReleasedExactlyWhenTheIntervalHasPassed() {
        var throttle = ClipboardThrottle()
        _ = throttle.offer(Write(session: 1, text: "a"), at: at(.zero))
        _ = throttle.offer(Write(session: 1, text: "b"), at: at(.milliseconds(50)))
        XCTAssertNil(throttle.release(at: at(interval - .nanoseconds(1))), "not before")
        XCTAssertEqual(throttle.held, Write(session: 1, text: "b"), "still held")
        XCTAssertEqual(throttle.release(at: at(interval)), Write(session: 1, text: "b"), "at 100 ms")
        XCTAssertNil(throttle.held)
        XCTAssertNil(throttle.release(at: at(.seconds(5))), "released once")
        // The release counts as a write: the next one waits for its interval.
        XCTAssertNil(throttle.offer(Write(session: 1, text: "c"), at: at(interval + .milliseconds(50))))
        XCTAssertEqual(throttle.dueAt, at(interval * 2))
    }

    func testANewerWriteSupersedesAHeldWriteWhoseReleaseIsLate() {
        var throttle = ClipboardThrottle()
        _ = throttle.offer(Write(session: 1, text: "a"), at: at(.zero))
        _ = throttle.offer(Write(session: 1, text: "b"), at: at(.milliseconds(50)))
        XCTAssertEqual(throttle.offer(Write(session: 2, text: "c"), at: at(.milliseconds(150))), Write(session: 2, text: "c"))
        XCTAssertNil(throttle.held, "the older held write is not written after the newer one")
        XCTAssertNil(throttle.release(at: at(.seconds(1))))
    }

    /// #240: a write taken at a session's close counts as a pasteboard
    /// write: the next one waits for the interval from it.
    func testAWriteTakenAtACloseCountsAsAWrite() {
        var throttle = ClipboardThrottle()
        _ = throttle.offer(Write(session: 1, text: "a"), at: at(.zero))
        XCTAssertNil(throttle.offer(Write(session: 2, text: "b"), at: at(.milliseconds(10))))
        XCTAssertEqual(throttle.takeHeld(from: 2, at: at(.milliseconds(50))), Write(session: 2, text: "b"))
        XCTAssertNil(throttle.offer(Write(session: 1, text: "c"), at: at(.milliseconds(60))))
        XCTAssertEqual(throttle.held, Write(session: 1, text: "c"))
        XCTAssertEqual(throttle.dueAt, at(.milliseconds(150)), "100 ms after the write at the close")
    }

    func testDroppingTheHeldWrite() {
        var throttle = ClipboardThrottle()
        _ = throttle.offer(Write(session: 1, text: "a"), at: at(.zero))
        _ = throttle.offer(Write(session: 1, text: "b"), at: at(.milliseconds(50)))
        throttle.dropHeld()
        XCTAssertNil(throttle.held)
        XCTAssertNil(throttle.dueAt)
        XCTAssertNil(throttle.release(at: at(.seconds(1))))
    }

    /// 100 000 writes from two sessions over 10 simulated seconds, with a
    /// timer that releases the held write when it falls due: at most 10
    /// pasteboard writes in any simulated second, and the last write offered
    /// is the last one made.
    func testAFloodIsCappedAndEndsWithTheLastWrite() {
        var throttle = ClipboardThrottle()
        var written: [(ContinuousClock.Instant, Write)] = []
        let offers = 100_000
        let span = Duration.seconds(10)
        var last: Write?
        for index in 0..<offers {
            let now = at(span * index / offers)
            if let due = throttle.dueAt, due <= now, let write = throttle.release(at: due) {
                written.append((due, write))
            }
            let write = Write(session: UInt64(1 + index % 2), text: "w\(index)")
            last = write
            if let write = throttle.offer(write, at: now) {
                written.append((now, write))
            }
        }
        if let due = throttle.dueAt, let write = throttle.release(at: due) {
            written.append((due, write))
        }
        XCTAssertNil(throttle.held)
        XCTAssertLessThanOrEqual(written.count, 101)
        XCTAssertGreaterThanOrEqual(written.count, 100, "the cap is reached, not undershot")
        var perSecond: [Int: Int] = [:]
        for (instant, _) in written {
            let second = Int((instant - start) / .seconds(1))
            perSecond[second, default: 0] += 1
        }
        XCTAssertLessThanOrEqual(perSecond.values.max() ?? 0, ClipboardThrottle.maxWritesPerSecond, "\(perSecond)")
        let gaps = zip(written, written.dropFirst()).map { $1.0 - $0.0 }
        XCTAssertGreaterThanOrEqual(gaps.min() ?? interval, interval, "writes at least 100 ms apart")
        XCTAssertEqual(written.last?.1, last, "the final write is not lost")
    }
}
