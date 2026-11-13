import AppKit
import TesseraKit
import XCTest

@testable import Tessera

/// The key routing of ``TerminalSurfaceView`` against a `[keys]` table
/// (#43, ADR 0036): Option chords the map binds run before the input
/// system composes them, Command chords bound to text are used once, and
/// chords without Command never reach a menu. On a core of the test's own,
/// whose `config.toml` carries the table, and a view on one of its
/// sessions; the app's core keeps the scheme's (absent) configuration, so
/// the installed menus keep the default key equivalents (⌘1 is Tab 1, ⌘E
/// Use Selection for Find): to this core they are a stale menu.
@MainActor
final class KeyBindingViewTests: XCTestCase {
    private var dir: URL!
    private var previous: String?
    private var core: TesseraApp?
    private var file: URL!
    private var view: TerminalSurfaceView!
    private var window: KeyControlledWindow!
    private var checkedLength = 0
    /// The commands the view ran through ``TerminalSurfaceView/runCommand``.
    private var ran: [String] = []

    private static let notFound = NSRange(location: NSNotFound, length: 0)

    private static let config = """
        [keys]
        'alt+t' = 'send_text:AT'
        'cmd+u' = 'send_text:CU'
        'cmd+j' = 'none'
        'ctrl+shift+c' = 'copy'
        'cmd+kp_1' = 'new_tab'
        'cmd+e' = 'new_window'
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
        // The recorder: `cat` in raw mode, writing its input to a file,
        // after lines enough for a scrollback.
        file = dir.appendingPathComponent("input.bin")
        let session = try core.spawn(
            .init(
                program: "/bin/sh",
                arguments: ["-c", #"seq 1 200; stty raw -echo; printf R; exec cat > "$0""#, file.path]))
        view = TerminalSurfaceView(session: session)
        view.runCommand = { [unowned self] in self.ran.append($0) }
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
        XCTAssertTrue(view.performKeyEquivalent(with: key("u", code: 32, [.command])), "send_text is used")
        route(key("u", code: 32, [.command]))
        XCTAssertTrue(view.performKeyEquivalent(with: key("j", code: 38, [.command])), "`none` is used")
        XCTAssertFalse(view.performKeyEquivalent(with: key("t", code: 17, [.command])), "New Tab's item runs it")
        try await assertBytes(Array("CUCU".utf8), "each ⌘U sends its text once")
    }

    /// `cmd+kp_1 = new_tab`: the menus read ⌘ on keypad 1 as ⌘1, Tab 1's
    /// key equivalent, and `cmd+e = new_window` while the installed menus
    /// still give ⌘E to Use Selection for Find (a key pressed before
    /// `on_config_applied` re-keys them). The view runs the bound command
    /// itself and keeps the chord from the menus (#317).
    func testACommandChordTheMenusWouldGiveAnotherItemRunsItsOwnCommand() throws {
        let bar = try XCTUnwrap(NSApp.mainMenu)
        let keypad1 = key("1", code: 83, [.command, .numericPad])
        let commandE = key("e", code: 14, [.command])
        XCTAssertNotNil(MainMenu.item(forCommand: "select_tab:1", in: bar), "the installed menus have Tab 1")
        XCTAssertTrue(MainMenu.keyEquivalent(of: "use_selection_for_find", in: bar, matches: commandE), "⌘E is stale")
        XCTAssertTrue(view.performKeyEquivalent(with: keypad1), "the menus never see ⌘ keypad 1")
        XCTAssertTrue(view.performKeyEquivalent(with: commandE), "nor the re-bound ⌘E")
        XCTAssertEqual(ran, ["new_tab", "new_window"])
        XCTAssertFalse(view.performKeyEquivalent(with: key("t", code: 17, [.command])), "⌘T's own item runs New Tab")
        XCTAssertEqual(ran, ["new_tab", "new_window"])
    }

    /// A Command chord no menu took reaches `keyDown`: a bound one runs
    /// there. On a Russian layout ⌘U types `г`; the core matches `cmd+u`
    /// by the key's US position.
    func testACommandChordTheMenusDidNotTakeRunsItsBindingFromKeyDown() async throws {
        // Up the responder chain the window would offer the key to
        // `performKeyEquivalent` again, which sends it too; cut there, only
        // the view's own fallback can.
        let next = view.nextResponder
        view.nextResponder = nil
        defer { view.nextResponder = next }
        view.processKeyDown(key("г", code: 32, [.command])) { XCTFail("Command skips the input system") }
        try await assertBytes(Array("CU".utf8), "the binding ran once from keyDown")
    }

    /// While text is marked the input method owns every key: a bound
    /// Command chord is not used as a key equivalent, and a bound Option
    /// chord goes to the input system, which composes it with the pending
    /// dead key (#317).
    func testBindingsWaitWhileTextIsMarked() async throws {
        view.setMarkedText("´", selectedRange: NSRange(location: 1, length: 0), replacementRange: Self.notFound)
        XCTAssertFalse(view.performKeyEquivalent(with: key("u", code: 32, [.command])), "⌘U is not used")
        XCTAssertTrue(view.hasMarkedText(), "the composition stays")
        var interpreted = false
        view.processKeyDown(key("†", ignoring: "t", code: 17, [.option])) {
            interpreted = true
            self.view.insertText("´†", replacementRange: Self.notFound)
        }
        XCTAssertTrue(interpreted, "⌥T went to the input system")
        XCTAssertFalse(view.hasMarkedText(), "which committed the composition")
        try await assertBytes(Array("´†".utf8), "the composed text, not `alt+t`'s AT")
        XCTAssertEqual(ran, [])
    }

    /// ⌘↑ (Page Up) during a composition (#415, simulated marked text; a
    /// real input method is a manual check): the view passes it on, the
    /// installed View menu's item is disabled so it does not scroll, and
    /// `keyDown` hands it to the input method; nothing reaches the child
    /// and the marked text stays. Without marked text the view runs the
    /// binding (`prompt_previous` by default) and the menu item scrolls, so
    /// both routes are live.
    func testCommandUpDuringACompositionNeitherScrollsNorReachesTheChild() async throws {
        let commandUp = key(
            MainMenu.functionKey(NSUpArrowFunctionKey), code: 126, [.command, .numericPad, .function])
        let menu = try XCTUnwrap(MainMenu.item(forCommand: "scroll_page_up", in: NSApp.mainMenu)?.menu)
        let localised = menu.items.map(\.allowsAutomaticKeyEquivalentLocalization)
        for item in menu.items {
            item.target = view
            item.allowsAutomaticKeyEquivalentLocalization = false
        }
        defer {
            for (item, localises) in zip(menu.items, localised) {
                item.target = nil
                item.allowsAutomaticKeyEquivalentLocalization = localises
            }
        }
        // As AppKit routes it: the view, the menus, then `keyDown`.
        let route = {
            if !self.view.performKeyEquivalent(with: commandUp), !NSApp.mainMenu!.performKeyEquivalent(with: commandUp) {
                self.view.processKeyDown(commandUp) {}
            }
        }
        XCTAssertGreaterThan(try view.session.scrollPosition.history, 0, "there is a scrollback")
        view.setMarkedText("か", selectedRange: NSRange(location: 1, length: 0), replacementRange: Self.notFound)
        XCTAssertFalse(view.performKeyEquivalent(with: commandUp), "the view passes it on")
        route()
        XCTAssertEqual(try view.session.scrollPosition.offset, 0, "it did not scroll")
        XCTAssertTrue(view.hasMarkedText(), "the marked text stays")
        XCTAssertEqual(view.attributedSubstring(forProposedRange: NSRange(location: 0, length: 1), actualRange: nil)?.string, "か")
        XCTAssertEqual(ran, [])
        try await assertBytes([], "nothing reached the child")
        view.unmarkText()
        route()
        XCTAssertEqual(ran, ["prompt_previous"], "without marked text the view runs ⌘↑'s binding")
        XCTAssertTrue(NSApp.mainMenu!.performKeyEquivalent(with: commandUp), "and the menu item is enabled")
        XCTAssertGreaterThan(try view.session.scrollPosition.offset, 0, "and scrolls")
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
