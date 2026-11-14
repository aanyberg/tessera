import AppKit
import Metal
import TesseraKit
import XCTest

@testable import Tessera

/// Prompt navigation and Select Last Output in `TerminalSurfaceView` (#45a,
/// ADR 0039): ⌘↑/⌘↓ through the view's key equivalent route jump between
/// the shell's OSC 133 prompts and page without them (D4); the View menu's
/// items validate and run.
///
/// The child prints two commands with their marks around 100 plain lines:
/// line 0 is the first prompt, 103 the second (its output `hi` on 104) and
/// 105 the live prompt, so the first prompt is in the history at any window
/// height the app opens with. As in ``TerminalScrollTests`` the view lives
/// in a ``KeyControlledWindow`` that is never on screen.
@MainActor
final class PromptNavigationTests: XCTestCase {
    private var appDelegate: AppDelegate!
    private var view: TerminalSurfaceView!
    private var window: KeyControlledWindow!

    private static let lines: UInt64 = 106

    override func setUp() async throws {
        appDelegate = try XCTUnwrap(NSApp.delegate as? AppDelegate)
        XCTAssertTrue(appDelegate.windowControllers.isEmpty, "a previous test left windows open")
    }

    override func tearDown() async throws {
        aimViewMenu(at: nil)
        window?.close()
        for controller in appDelegate.windowControllers {
            controller.window?.close()
        }
        appDelegate.spawnOptions = .init()
        appDelegate = nil
        view = nil
        window = nil
    }

    /// Opens a terminal whose child runs `script`, and waits until
    /// `ready` holds.
    private func open(_ script: String, until description: String, _ ready: @escaping () -> Bool) async throws {
        appDelegate.spawnOptions = .init(program: "/bin/sh", arguments: ["-c", script])
        let controller = try appDelegate.newWindow()
        view = controller.focusedPane.view
        window = KeyControlledWindow(contentRect: view.frame)
        window.contentView = view
        _ = window.makeFirstResponder(view)
        view.frameStatusOverride = .presented
        view.occlusionOverride = false
        // The menu items run through the responder chain, which ends in the
        // app in the test host (no window is key): aim them at the view.
        aimViewMenu(at: view)
        try await waitUntil(description, ready)
    }

    private func openWithMarks() async throws {
        let mark = { (payload: String) in "\\033]133;\(payload)\\007" }
        let first = "\(mark("A"))$ \(mark("B"))ls\\n\(mark("C"))out1\\nout2\\n\(mark("D;0"))"
        let second = "\(mark("A"))$ \(mark("B"))echo\\n\(mark("C"))hi\\n\(mark("D;0"))\(mark("A"))$ "
        try await open("printf '\(first)'; seq 1 100; printf '\(second)'; exec cat", until: "the marked output") {
            guard let position = try? self.view.session.scrollPosition else { return false }
            return position.history + UInt64(position.rows) == Self.lines
                && (try? self.view.session.hasCommandOutput) == true
        }
    }

    private func aimViewMenu(at target: AnyObject?) {
        guard let menu = NSApp.mainMenu?.items.first(where: { $0.submenu?.title == "View" })?.submenu else { return }
        for item in menu.items {
            item.target = target
        }
    }

    private var position: TesseraSession.ScrollPosition {
        get throws { try view.session.scrollPosition }
    }

    private func arrow(_ functionKey: Int, code: UInt16) throws -> NSEvent {
        let characters = MainMenu.functionKey(functionKey)
        return try XCTUnwrap(
            NSEvent.keyEvent(
                with: .keyDown, location: .zero, modifierFlags: [.command, .numericPad, .function], timestamp: 0,
                windowNumber: window.windowNumber, context: nil, characters: characters,
                charactersIgnoringModifiers: characters, isARepeat: false, keyCode: code))
    }

    private func commandUp() throws -> NSEvent { try arrow(NSUpArrowFunctionKey, code: 126) }
    private func commandDown() throws -> NSEvent { try arrow(NSDownArrowFunctionKey, code: 125) }

    func testCommandArrowsJumpBetweenPromptsAndStopAtTheEnds() async throws {
        try await openWithMarks()
        let history = try position.history
        // The key window's first responder sees the key before the menus.
        XCTAssertTrue(view.performKeyEquivalent(with: try commandUp()), "⌘↑ is the view's")
        XCTAssertEqual(try position.offset, history, "the first prompt, line 0, at the top")
        XCTAssertTrue(view.performKeyEquivalent(with: try commandUp()))
        XCTAssertEqual(try position.offset, history, "no older prompt: it stays, it does not page")
        XCTAssertTrue(view.performKeyEquivalent(with: try commandDown()))
        XCTAssertEqual(try position.offset, 0, "the second prompt is on the live screen")
    }

    func testCommandArrowsPageWithoutMarks() async throws {
        // All 1000 lines and the empty line under them: output still arriving
        // between the keys would move the anchored offset (#443).
        try await open("seq 1 1000; exec cat", until: "the history") {
            guard let position = try? self.view.session.scrollPosition else { return false }
            return position.history + UInt64(position.rows) == 1001
        }
        let page = UInt64(try position.rows - 1)
        XCTAssertTrue(view.performKeyEquivalent(with: try commandUp()))
        XCTAssertEqual(try position.offset, page, "⌘↑ pages up as before")
        XCTAssertTrue(view.performKeyEquivalent(with: try commandDown()))
        XCTAssertEqual(try position.offset, 0, "⌘↓ pages back down")
        let item = NSMenuItem(title: "", action: #selector(TerminalSurfaceView.selectLastOutput(_:)), keyEquivalent: "")
        XCTAssertFalse(view.validateMenuItem(item), "no command output to select")
    }

    func testSelectLastOutputSelectsItForCopy() async throws {
        try await openWithMarks()
        let item = try XCTUnwrap(MainMenu.item(forCommand: "select_last_output", in: NSApp.mainMenu))
        XCTAssertTrue(view.validateMenuItem(item))
        XCTAssertFalse(view.hasSelection)
        MainMenu.run(command: "select_last_output")
        XCTAssertTrue(view.hasSelection)
        XCTAssertEqual(try view.session.selectionText(), "hi")
    }

    func testCopyAfterSelectLastOutputGivesTheExactText() async throws {
        let mark = { (payload: String) in "\\033]133;\(payload)\\007" }
        let output = "line one\\n\\n  indented   \\nlast\\n"
        let script = "\(mark("A"))$ \(mark("B"))cmd\\n\(mark("C"))\(output)\(mark("D;0"))\(mark("A"))$ "
        try await open("printf '\(script)'; exec cat", until: "the marked output") {
            (try? self.view.session.hasCommandOutput) == true
        }
        view.pasteboard = NSPasteboard(name: NSPasteboard.Name("com.aanyberg.tessera.tests.\(UUID().uuidString)"))
        defer { view.pasteboard.releaseGlobally() }
        view.pasteboard.clearContents()
        view.selectLastOutput(nil)
        view.copy(nil)
        XCTAssertEqual(
            view.pasteboard.string(forType: .string), "line one\n\n  indented\nlast",
            "interior blank line and indentation kept, trailing blanks dropped")
    }

    func testSelectLastOutputIsDisabledWhileComposing() async throws {
        try await openWithMarks()
        let item = NSMenuItem(title: "", action: #selector(TerminalSurfaceView.selectLastOutput(_:)), keyEquivalent: "")
        XCTAssertTrue(view.validateMenuItem(item))
        view.setMarkedText(
            "に", selectedRange: NSRange(location: 1, length: 0), replacementRange: NSRange(location: NSNotFound, length: 0))
        XCTAssertFalse(view.validateMenuItem(item), "it may scroll, and nothing scrolls while composing (ADR 0032)")
        view.unmarkText()
        XCTAssertTrue(view.validateMenuItem(item))
    }

    private func requireSurface() throws {
        guard MTLCreateSystemDefaultDevice() != nil else {
            throw XCTSkip("no Metal device on this machine: the test counts rendered frames")
        }
        guard view.surface.isAttached else {
            throw XCTSkip("the core could not attach a renderer to the view (no GPU can present, or no font)")
        }
    }

    /// The jump and the selection ask for a frame themselves: no output
    /// comes after them to draw the view.
    func testAJumpAndASelectionAskForAFrameWithoutOutput() async throws {
        try await openWithMarks()
        try requireSurface()
        view.sessionDidOutput()
        view.displayLinkDidFire()
        var renders = view.renderCount
        view.scrollToPreviousPrompt(nil)
        XCTAssertGreaterThan(try position.offset, 0)
        view.displayLinkDidFire()
        XCTAssertEqual(view.renderCount, renders + 1, "the jump is drawn")
        view.scrollToNextPrompt(nil)
        XCTAssertEqual(try position.offset, 0)
        view.displayLinkDidFire()
        renders = view.renderCount
        view.displayLinkDidFire()
        XCTAssertEqual(view.renderCount, renders, "nothing new to draw")
        view.selectLastOutput(nil)
        XCTAssertTrue(view.hasSelection)
        view.displayLinkDidFire()
        XCTAssertEqual(view.renderCount, renders + 1, "the selection is drawn")
    }

    func testThePromptItemsRunFromTheMenu() async throws {
        try await openWithMarks()
        let history = try position.history
        MainMenu.run(command: "prompt_previous")
        XCTAssertEqual(try position.offset, history)
        MainMenu.run(command: "prompt_next")
        XCTAssertEqual(try position.offset, 0)
    }

    /// Polls `condition` on the main actor until it holds or thirty seconds
    /// pass.
    private func waitUntil(_ description: String, _ condition: () -> Bool) async throws {
        // 30 s: a child's first output can take that long on a loaded host.
        let deadline = ContinuousClock.now + .seconds(30)
        while !condition() {
            guard ContinuousClock.now < deadline else {
                XCTFail("timed out: \(description)")
                return
            }
            try await Task.sleep(for: .milliseconds(10))
        }
    }
}
