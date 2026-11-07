import AppKit
import TesseraKit
import XCTest

@testable import Tessera

/// The key routing of ``TerminalSurfaceView`` against a `[keys]` table
/// (#43, ADR 0036): Option chords the map binds run before the input
/// system composes them, Command chords bound to text are used once, and
/// chords without Command never reach a menu. On a core of the test's own,
/// whose `config.toml` carries the table, and a view on one of its
/// sessions; the app's core keeps the scheme's (absent) configuration.
@MainActor
final class KeyBindingViewTests: XCTestCase {
    private var dir: URL!
    private var previous: String?
    private var core: TesseraApp?
    private var file: URL!
    private var view: TerminalSurfaceView!
    private var window: KeyControlledWindow!
    private var checkedLength = 0

    private static let notFound = NSRange(location: NSNotFound, length: 0)

    private static let config = """
        [keys]
        'alt+t' = 'send_text:AT'
        'cmd+e' = 'send_text:CE'
        'cmd+j' = 'none'
        'ctrl+shift+c' = 'copy'
        """

    override func setUp() async throws {
        dir = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-key-routing-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        try Self.config.write(to: dir.appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
        previous = ProcessInfo.processInfo.environment["TESSERA_CONFIG_DIR"]
        setenv("TESSERA_CONFIG_DIR", dir.path, 1)
        let core = try TesseraApp()
        self.core = core
        XCTAssertEqual(try core.keymap().warnings, [], "the table applies as written")
        // The recorder: `cat` in raw mode, writing its input to a file.
        file = dir.appendingPathComponent("input.bin")
        let session = try core.spawn(
            .init(program: "/bin/sh", arguments: ["-c", #"stty raw -echo; printf R; exec cat > "$0""#, file.path]))
        view = TerminalSurfaceView(session: session)
        window = KeyControlledWindow(contentRect: view.frame)
        window.contentView = view
        _ = window.makeFirstResponder(view)
        view.frameStatusOverride = .presented
        try await waitUntil("the recorder is in raw mode") { FileManager.default.fileExists(atPath: self.file.path) }
        try await Task.sleep(for: .milliseconds(100))
    }

    override func tearDown() async throws {
        window?.close()
        view = nil
        window = nil
        core?.shutdown()
        core = nil
        if let previous { setenv("TESSERA_CONFIG_DIR", previous, 1) } else { unsetenv("TESSERA_CONFIG_DIR") }
        try? FileManager.default.removeItem(at: dir)
    }

    func testAnOptionChordBindingRunsBeforeTheInputSystemComposes() async throws {
        // US Option-t types †; `alt+t` is bound, so the input system never
        // sees the key.
        view.processKeyDown(key("†", ignoring: "t", code: 17, [.option])) {
            self.view.insertText("†", replacementRange: Self.notFound)
        }
        // An Option chord the map does not bind still composes (Option-a, å).
        view.processKeyDown(key("å", ignoring: "a", code: 0, [.option])) {
            self.view.insertText("å", replacementRange: Self.notFound)
        }
        try await assertBytes(Array("AT".utf8) + [0xc3, 0xa5])
    }

    func testCommandBindingsAreUsedOnceAndCommandsGoToTheirItems() async throws {
        // As AppKit routes a key down: the first responder's key
        // equivalent, then the menus', then `keyDown` (the menus are left
        // out: their items would open windows here).
        let route = { (event: NSEvent) in
            if !self.view.performKeyEquivalent(with: event) {
                self.view.processKeyDown(event) { XCTFail("Command skips the input system") }
            }
        }
        XCTAssertTrue(view.performKeyEquivalent(with: key("e", code: 14, [.command])), "send_text is used")
        route(key("e", code: 14, [.command]))
        XCTAssertTrue(view.performKeyEquivalent(with: key("j", code: 38, [.command])), "`none` is used")
        XCTAssertFalse(view.performKeyEquivalent(with: key("t", code: 17, [.command])), "New Tab's item runs it")
        try await assertBytes(Array("CECE".utf8), "each ⌘E sends its text once")
    }

    func testAChordWithoutCommandNeverGoesOnAMenu() throws {
        let bar = MainMenu.makeMenuBar().bar
        MainMenu.apply(try XCTUnwrap(core).keymap(), to: bar)
        let copy = try XCTUnwrap(MainMenu.item(forCommand: "copy", in: bar))
        XCTAssertEqual(authoredKeyEquivalent(of: copy).key, "c")
        XCTAssertEqual(copy.keyEquivalentModifierMask, .command, "⌃⇧C stays off the menu")
        var withControl: [String] = []
        func walk(_ menu: NSMenu) {
            for item in menu.items {
                if item.keyEquivalentModifierMask.contains(.control), !item.keyEquivalent.isEmpty,
                    !item.keyEquivalentModifierMask.contains(.command)
                {
                    withControl.append(item.title)
                }
                item.submenu.map(walk)
            }
        }
        walk(bar)
        XCTAssertEqual(withControl, [], "no item carries a chord without Command")
    }

    // MARK: Helpers

    private func key(
        _ characters: String, ignoring: String? = nil, code: UInt16, _ flags: NSEvent.ModifierFlags = []
    ) -> NSEvent {
        NSEvent.keyEvent(
            with: .keyDown, location: .zero, modifierFlags: flags, timestamp: 0,
            windowNumber: window.windowNumber, context: nil, characters: characters,
            charactersIgnoringModifiers: ignoring ?? characters, isARepeat: false, keyCode: code)!
    }

    /// Asserts that exactly `expected` reached the PTY, up to a sentinel
    /// key sent after it (input reaches the recorder in order).
    private func assertBytes(
        _ expected: [UInt8], _ message: String = "", file: StaticString = #filePath, line: UInt = #line
    ) async throws {
        try view.session.send(key: .init(key: .char, codepoint: "~", text: "~"))
        var received: [UInt8] = []
        let previous = checkedLength
        try await waitUntil("the sentinel arrives", timeout: .seconds(20)) {
            received = (try? Array(Data(contentsOf: self.file))) ?? []
            return received.count > previous && received.last == UInt8(ascii: "~")
        }
        checkedLength = received.count
        let hex = { (bytes: [UInt8]) in bytes.map { String(format: "%02x", $0) }.joined(separator: " ") }
        XCTAssertEqual(hex(Array(received.dropLast())), hex(expected), message, file: file, line: line)
    }

    private func waitUntil(
        _ description: String, timeout: Duration = .seconds(5), _ condition: () -> Bool
    ) async throws {
        let deadline = ContinuousClock.now + timeout
        while !condition() {
            guard ContinuousClock.now < deadline else { return XCTFail("timed out after \(timeout): \(description)") }
            try await Task.sleep(for: .milliseconds(10))
        }
    }
}
