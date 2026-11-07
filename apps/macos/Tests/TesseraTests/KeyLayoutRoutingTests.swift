import AppKit
import TesseraKit
import XCTest

@testable import Tessera

/// Records the command each menu item ran: the target of every item of a
/// private menu bar the tests install as `NSApp.mainMenu`.
@MainActor
final class MenuCommandRecorder: NSObject {
    var ran: [String] = []

    @objc func record(_ sender: NSMenuItem) {
        ran.append(sender.identifier?.rawValue ?? "?")
    }
}

/// Command and Control chords on layouts other than US, and keypad chords,
/// routed as `AppKit` routes a key down (#43 adversarial B1, B2): the
/// base-layout key stands in only for a key beyond ASCII, so a Latin
/// layout's own key never runs the command of the US key in its place, and
/// a keypad chord bound to a command runs that command, not the item the
/// menus match its main-row character to. On a core of the test's own and
/// a menu bar whose items record instead of acting.
@MainActor
final class KeyLayoutRoutingTests: XCTestCase {
    private var dir: URL!
    private var previous: String?
    private var core: TesseraApp?
    private var file: URL!
    private var view: TerminalSurfaceView!
    private var window: KeyControlledWindow!
    private var previousMenu: NSMenu?
    private let recorder = MenuCommandRecorder()
    private var checkedLength = 0

    private static let notFound = NSRange(location: NSNotFound, length: 0)

    private func setUpCore(config: String?) async throws {
        dir = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-key-layout-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        if let config {
            try config.write(to: dir.appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
        }
        previous = ProcessInfo.processInfo.environment["TESSERA_CONFIG_DIR"]
        setenv("TESSERA_CONFIG_DIR", dir.path, 1)
        let core = try TesseraApp()
        self.core = core
        XCTAssertEqual(try core.keymap().warnings, [], "the table applies as written")
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
        let bar = MainMenu.makeMenuBar().bar
        MainMenu.apply(try core.keymap(), to: bar)
        func retarget(_ menu: NSMenu) {
            for item in menu.items {
                if item.identifier != nil {
                    item.target = recorder
                    item.action = #selector(MenuCommandRecorder.record(_:))
                }
                item.submenu.map(retarget)
            }
        }
        retarget(bar)
        previousMenu = NSApp.mainMenu
        NSApp.mainMenu = bar
    }

    override func tearDown() async throws {
        if let previousMenu { NSApp.mainMenu = previousMenu }
        window?.close()
        view = nil
        window = nil
        core?.shutdown()
        core = nil
        if let previous { setenv("TESSERA_CONFIG_DIR", previous, 1) } else { unsetenv("TESSERA_CONFIG_DIR") }
        if let dir { try? FileManager.default.removeItem(at: dir) }
    }

    /// No `[keys]`: Command on a Latin layout's own key runs nothing the
    /// menus do not match it to. French ⌘Z is the key where US has W,
    /// Dvorak ⌘' where US has Q, Turkish-F ⌘S where US has M. Russian ⌘Е
    /// (where US has T) is still New Tab. No Shift chord: the view reads
    /// the unshifted key from the layout in use, not the event's (French
    /// ⇧⌘Z is in the core's tests).
    func testCommandOnALatinLayoutsOwnKeyIsNotTheUSKeysCommand() async throws {
        try await setUpCore(config: nil)
        XCTAssertEqual(route(key("t", code: 17, [.command])), "menu")
        XCTAssertEqual(recorder.ran, ["new_tab"], "US ⌘T is its item's")
        let phantoms: [(String, String, UInt16, NSEvent.ModifierFlags)] = [
            ("French ⌘Z", "z", 13, [.command]),
            ("Dvorak ⌘'", "'", 12, [.command]),
            ("Dvorak ⌘J", "j", 8, [.command]),
            ("Dvorak ⌘Y", "y", 17, [.command]),
            ("Dvorak ⌘B", "b", 45, [.command]),
            ("Turkish-F ⌘S", "s", 46, [.command]),
        ]
        for (name, characters, code, flags) in phantoms {
            recorder.ran = []
            _ = route(key(characters, code: code, flags))
            XCTAssertEqual(recorder.ran, [], "\(name) runs no command")
        }
        recorder.ran = []
        XCTAssertEqual(route(key("е", code: 17, [.command])), "view")
        XCTAssertEqual(recorder.ran, ["new_tab"], "Russian ⌘Е is New Tab")
        try await assertBytes([], "no Command chord is encoded")
    }

    /// German: Z is where US has Y. With `ctrl+y` and `alt+y` bound,
    /// Control-Z still sends ^Z and Option-Z still types Ω.
    func testQwertzControlZWithCtrlYBinding() async throws {
        try await setUpCore(config: "[keys]\n'ctrl+y' = 'send_text:CY'\n'alt+y' = 'send_text:AY'\n")
        view.processKeyDown(key("\u{1a}", ignoring: "z", code: 16, [.control])) { XCTFail("Control skips the input system") }
        try await assertBytes([0x1a], "Control-Z is ^Z")
        view.processKeyDown(key("Ω", ignoring: "z", code: 16, [.option])) {
            self.view.insertText("Ω", replacementRange: Self.notFound)
        }
        try await assertBytes([0xce, 0xa9], "Option-Z types Ω")
        view.processKeyDown(key("\u{19}", ignoring: "y", code: 6, [.control])) { XCTFail("Control skips the input system") }
        try await assertBytes(Array("CY".utf8), "Control-Y is the binding")
    }

    /// A keypad chord bound to a command runs that command; the menus
    /// would give ⌘ keypad + to Bigger, ⌘ keypad 1 to Tab 1 and ⌘ keypad −
    /// to Smaller.
    func testKeypadCommandBindings() async throws {
        try await setUpCore(
            config: "[keys]\n'cmd+kp_add' = 'font_reset'\n'cmd+kp_1' = 'new_window'\n'cmd+kp_subtract' = 'new_tab'\n")
        let cases: [(String, String, UInt16, String)] = [
            ("⌘ keypad +", "+", 69, "font_reset"),
            ("⌘ keypad 1", "1", 83, "new_window"),
            ("⌘ keypad −", "-", 78, "new_tab"),
        ]
        for (name, characters, code, command) in cases {
            recorder.ran = []
            _ = route(key(characters, code: code, [.command, .numericPad]))
            XCTAssertEqual(recorder.ran, [command], name)
        }
        recorder.ran = []
        XCTAssertEqual(route(key("1", code: 18, [.command])), "menu", "main-row ⌘1 is still its item's")
        XCTAssertEqual(recorder.ran, ["select_tab:1"])
    }

    // MARK: Helpers

    /// As `AppKit` routes a key down: the first responder's key
    /// equivalent, the main menu's, then `keyDown`. Says which took it.
    private func route(_ event: NSEvent) -> String {
        if view.performKeyEquivalent(with: event) { return "view" }
        if NSApp.mainMenu?.performKeyEquivalent(with: event) == true { return "menu" }
        view.processKeyDown(event) { self.view.interpretKeyEvents([event]) }
        return "keyDown"
    }

    private func key(
        _ characters: String, ignoring: String? = nil, code: UInt16, _ flags: NSEvent.ModifierFlags = []
    ) -> NSEvent {
        NSEvent.keyEvent(
            with: .keyDown, location: .zero, modifierFlags: flags, timestamp: 0,
            windowNumber: window.windowNumber, context: nil, characters: characters,
            charactersIgnoringModifiers: ignoring ?? characters, isARepeat: false, keyCode: code)!
    }

    /// Asserts that exactly `expected` reached the PTY since the last
    /// check, up to a sentinel key sent after it.
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
        let segment = Array(received[min(previous, received.count)..<max(previous, received.count - 1)])
        checkedLength = received.count
        let hex = { (bytes: [UInt8]) in bytes.map { String(format: "%02x", $0) }.joined(separator: " ") }
        XCTAssertEqual(hex(segment), hex(expected), message, file: file, line: line)
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
