import Foundation
import TesseraCore
@testable import TesseraKit
import XCTest

/// `TesseraSession.scroll(to:)` and `scrollPosition` round-tripped through
/// the C API: a 24-row session whose child printed 200 lines has 177 lines
/// of history (201 lines, the cursor's included, less the screen).
@MainActor
final class ScrollTests: XCTestCase {
    private var app: TesseraApp!

    private static let rows: UInt16 = 24
    private static let history: UInt64 = 177

    override func setUp() async throws {
        app = try TesseraApp()
    }

    override func tearDown() async throws {
        app?.shutdown()
        app = nil
    }

    /// A session with ``history`` lines of history whose child then copies
    /// its input to its output (raw mode, no echo), so a test can make it
    /// print.
    private func sessionWithHistory() async throws -> TesseraSession {
        let session = try app.spawn(
            .init(program: "/bin/sh", arguments: ["-c", "seq 1 200; stty raw -echo; exec cat"], rows: Self.rows))
        try await waitUntil("the history is complete") {
            (try? session.scrollPosition)?.history == Self.history
        }
        return session
    }

    func testTheTargetsAreTheCValues() {
        let targets: [(TesseraSession.ScrollTarget, TesseraScrollTarget, UInt64)] = [
            (.top, TESSERA_SCROLL_TARGET_TOP, 0), (.bottom, TESSERA_SCROLL_TARGET_BOTTOM, 0),
            (.pageUp, TESSERA_SCROLL_TARGET_PAGE_UP, 0), (.pageDown, TESSERA_SCROLL_TARGET_PAGE_DOWN, 0),
            (.offset(42), TESSERA_SCROLL_TARGET_OFFSET, 42), (.offset(.max), TESSERA_SCROLL_TARGET_OFFSET, .max),
        ]
        for (target, cTarget, offset) in targets {
            XCTAssertEqual(target.cTarget, cTarget, "\(target)")
            XCTAssertEqual(target.cOffset, offset, "\(target)")
        }
    }

    func testEveryTargetMovesTheViewWithinTheHistory() async throws {
        let session = try await sessionWithHistory()
        XCTAssertEqual(try session.scrollPosition, .init(offset: 0, history: Self.history, rows: Self.rows))
        let page = UInt64(Self.rows - 1)
        let steps: [(TesseraSession.ScrollTarget, UInt64)] = [
            (.pageDown, 0), (.pageUp, page), (.pageUp, 2 * page), (.pageDown, page), (.top, Self.history),
            (.pageUp, Self.history), (.offset(5), 5), (.offset(.max), Self.history), (.bottom, 0),
            (.offset(Self.history - 1), Self.history - 1), (.pageDown, Self.history - 1 - page),
        ]
        for (target, offset) in steps {
            try session.scroll(to: target)
            let position = try session.scrollPosition
            XCTAssertEqual(position.offset, offset, "after \(target)")
            XCTAssertEqual(position.history, Self.history, "after \(target)")
            XCTAssertEqual(position.rows, Self.rows, "after \(target)")
        }
    }

    func testTheAlternateScreenHasNoHistoryToScroll() async throws {
        let session = try await sessionWithHistory()
        try session.scroll(to: .pageUp)
        try session.write(Array("\u{1b}[?1049h".utf8))
        try await waitUntil("the alternate screen is shown") { (try? session.scrollPosition)?.history == 0 }
        XCTAssertEqual(try session.scrollPosition, .init(offset: 0, history: 0, rows: Self.rows))
        try session.scroll(to: .top)
        XCTAssertEqual(try session.scrollPosition.offset, 0, "nothing to scroll into")
        try session.write(Array("\u{1b}[?1049l".utf8))
        try await waitUntil("the primary screen is back") { (try? session.scrollPosition)?.history == Self.history }
        XCTAssertEqual(try session.scrollPosition.offset, 0, "back on the live screen")
    }

    func testAClosedSessionThrows() async throws {
        let session = try await sessionWithHistory()
        try session.close()
        XCTAssertThrowsError(try session.scroll(to: .top)) {
            XCTAssertEqual($0 as? TesseraCoreError, .sessionNotFound)
        }
        XCTAssertThrowsError(try session.scrollPosition) { XCTAssertEqual($0 as? TesseraCoreError, .sessionNotFound) }
    }

    /// Polls `condition` on the main actor until it holds or five seconds
    /// pass.
    private func waitUntil(_ description: String, _ condition: () -> Bool) async throws {
        let deadline = ContinuousClock.now + .seconds(5)
        while !condition() {
            guard ContinuousClock.now < deadline else {
                XCTFail("timed out: \(description)")
                return
            }
            try await Task.sleep(for: .milliseconds(10))
        }
    }
}
