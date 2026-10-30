import AppKit
import XCTest

@testable import TesseraKit

/// A program's OSC 52 write reaches the pasteboard: a named one here, never
/// the user's general pasteboard.
@MainActor
final class ClipboardTests: XCTestCase {
    func testOSC52WriteReachesTheInjectedPasteboard() async throws {
        let pasteboard = NSPasteboard(name: NSPasteboard.Name("com.aanyberg.TesseraTests.clipboard"))
        defer { pasteboard.releaseGlobally() }
        // Older content of another type must go: the write replaces it all.
        pasteboard.clearContents()
        pasteboard.setString("<b>old</b>", forType: .html)
        let app = try TesseraApp()
        defer { app.shutdown() }
        app.pasteboard = pasteboard
        // base64 of "a" NUL "b" ESC "[31mc" LF (osc_spec.txt K28, K29).
        _ = try app.spawn(.init(program: "/bin/sh", arguments: ["-c", #"printf '\033]52;c;YQBiG1szMW1jCg==\007'; exec cat"#]))
        let deadline = ContinuousClock.now + .seconds(5)
        while pasteboard.string(forType: .string) == nil, ContinuousClock.now < deadline {
            try await Task.sleep(for: .milliseconds(20))
        }
        XCTAssertEqual(pasteboard.string(forType: .string), "a\u{0}b\u{1B}[31mc\n")
        XCTAssertNil(pasteboard.string(forType: .html))
        XCTAssertNotEqual(pasteboard.name, NSPasteboard.general.name)
    }
}
