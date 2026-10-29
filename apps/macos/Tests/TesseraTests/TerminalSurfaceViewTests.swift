import AppKit
import Carbon
import Metal
import TesseraKit
import XCTest

@testable import Tessera

/// `TerminalSurfaceView` in a real window of the running app: keyboard and
/// input-method handling against the bytes that reach the PTY, resize
/// coalescing, display-link pausing and focus.
///
/// The keyboard cases are the #27 spec fixture (ime_input_fixture.txt, case
/// ids in the test names). A synthetic `NSEvent` passed to
/// `interpretKeyEvents` goes through this machine's real input source, so
/// the tests call the `NSTextInputClient` methods AppKit would call, in
/// AppKit's order, inside ``TerminalSurfaceView/processKeyDown(_:interpret:)``.
/// Real input methods are a manual check (ADR 0017).
///
/// Sessions run a recorder: `cat` in raw mode, writing its input to a file.
///
/// The view under test lives in a ``KeyControlledWindow`` that is never
/// ordered on screen, so nothing depends on whether the machine's session is
/// locked (test windows never key, never visible, no display-link ticks) or
/// unlocked (they become key and visible, and the link runs): the tests set
/// key state, occlusion and ticks themselves, the way AppKit's notifications
/// would. Setting `TESSERA_TEST_WINDOWS_KEY=1` (`TEST_RUNNER_` prefix through
/// xcodebuild) starts every test with the window key, as on an unlocked CI
/// runner; the suite must pass both ways.
@MainActor
final class TerminalSurfaceViewTests: XCTestCase {
    private var appDelegate: AppDelegate!
    private var file: URL!
    private var view: TerminalSurfaceView!
    private var window: KeyControlledWindow!

    /// Whether test windows start key.
    private static let startKey = ProcessInfo.processInfo.environment["TESSERA_TEST_WINDOWS_KEY"] == "1"

    private static let notFound = NSRange(location: NSNotFound, length: 0)

    override func setUp() async throws {
        appDelegate = try XCTUnwrap(NSApp.delegate as? AppDelegate)
        XCTAssertTrue(appDelegate.windowControllers.isEmpty, "a previous test left windows open")
        file = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-view-\(UUID().uuidString).bin")
        appDelegate.spawnOptions = .init(
            program: "/bin/sh", arguments: ["-c", #"stty raw -echo; printf R; exec cat > "$0""#, file.path])
        let controller = try appDelegate.newWindow()
        view = try XCTUnwrap(controller.contentView as? TerminalSurfaceView)
        // Off the app's window, whose key and visible state follow the
        // machine, into one the test controls.
        window = KeyControlledWindow(contentRect: view.frame)
        window.contentView = view
        _ = window.makeFirstResponder(view)
        setKey(Self.startKey)
        // Never the user's clipboard.
        view.pasteboard = NSPasteboard(name: NSPasteboard.Name("com.aanyberg.tessera.tests.\(UUID().uuidString)"))
        // The recorder prints R once raw mode is set.
        try await waitUntil("the recorder is in raw mode") { FileManager.default.fileExists(atPath: self.file.path) }
        try await Task.sleep(for: .milliseconds(100))
    }

    override func tearDown() async throws {
        view?.pasteboard.releaseGlobally()
        for extra in extraWindows + [window] {
            extra?.close()
        }
        extraWindows = []
        for controller in appDelegate.windowControllers {
            controller.window?.close()
        }
        appDelegate.spawnOptions = .init()
        try? FileManager.default.removeItem(at: file)
        appDelegate = nil
        view = nil
        window = nil
    }

    // MARK: Section A: Option dead keys and composition (US/ABC)

    func testA1OptionAInsertsÅWithoutAlt() async throws {
        press(key("å", ignoring: "a", code: 0, [.option])) { $0.insertText("å", replacementRange: Self.notFound) }
        try await assertBytes([0xc3, 0xa5])
        XCTAssertFalse(view.sentKeys.contains { $0.modifiers.contains(.alt) }, "ALT is never set on composed text (D8)")
    }

    func testOptionSpaceSendsANoBreakSpaceNotASpace() async throws {
        // Not composed (no marked text), but the text is not the Space key's
        // own character: sent as text only, or the core would send 20 (D2).
        press(key("\u{a0}", ignoring: " ", code: 49, [.option])) {
            $0.insertText("\u{a0}", replacementRange: Self.notFound)
        }
        try await assertBytes([0xc2, 0xa0])
    }

    // MARK: The Apple logo (U+F8FF) is text, not a function key

    func testAppleLogoOptionShiftKOnUSIsText() async throws {
        press(key("\u{F8FF}", ignoring: "K", code: 40, [.option, .shift])) {
            $0.insertText("\u{F8FF}", replacementRange: Self.notFound)
        }
        XCTAssertFalse(view.sentKeys.contains { $0.modifiers.contains(.alt) }, "no Alt on committed text (D8)")
        try await assertBytes([0xef, 0xa3, 0xbf], "U+F8FF, not ESC K")
    }

    func testAppleLogoOptionAOnSwedishIsText() async throws {
        press(key("\u{F8FF}", ignoring: "a", code: 0, [.option])) {
            $0.insertText("\u{F8FF}", replacementRange: Self.notFound)
        }
        try await assertBytes([0xef, 0xa3, 0xbf], "U+F8FF, not ESC a")
    }

    func testAppleLogoThroughTheRealInputSystem() async throws {
        // Option-Shift-K through this machine's layout and input system.
        let typed = Self.layoutCharacters(keyCode: 40, [.option, .shift])
        guard typed == "\u{F8FF}" else {
            throw XCTSkip("the current layout types \(typed.debugDescription) for Option-Shift-K, not the Apple logo")
        }
        view.keyDown(with: key(typed, ignoring: Self.layoutCharacters(keyCode: 40, [.shift]), code: 40, [.option, .shift]))
        try await assertBytes([0xef, 0xa3, 0xbf])
    }

    /// On Swedish, Norwegian and Finnish layouts Option-a types the Apple
    /// logo. Sent as ESC a, zsh would run the command line typed so far
    /// (accept-and-hold).
    func testAppleLogoDoesNotRunTheZshCommandLine() async throws {
        let marker = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-ran-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: marker) }
        appDelegate.spawnOptions = .init(program: "/bin/zsh", arguments: ["-f"])
        let controller = try appDelegate.newWindow()
        let zsh = try XCTUnwrap(controller.contentView as? TerminalSurfaceView)
        let zshWindow = KeyControlledWindow(contentRect: zsh.frame)
        extraWindows.append(zshWindow)
        zshWindow.contentView = zsh
        try await Task.sleep(for: .milliseconds(800))  // zsh -f starts its line editor
        for character in "touch \(marker.path)" {
            let text = String(character)
            zsh.processKeyDown(key(text, code: character == " " ? 49 : 0x7F)) {
                zsh.insertText(text, replacementRange: Self.notFound)
            }
        }
        zsh.processKeyDown(key("\u{F8FF}", ignoring: "a", code: 0, [.option])) {
            zsh.insertText("\u{F8FF}", replacementRange: Self.notFound)
        }
        try await Task.sleep(for: .milliseconds(1500))
        XCTAssertFalse(FileManager.default.fileExists(atPath: marker.path), "the command line ran without Return")
    }

    func testA3DeadAcuteThenE() async throws {
        press(key("", ignoring: "e", code: 14, [.option])) {
            $0.setMarkedText("´", selectedRange: NSRange(location: 1, length: 0), replacementRange: Self.notFound)
        }
        XCTAssertTrue(view.hasMarkedText())
        XCTAssertEqual(view.sentKeys, [], "nothing is sent while text is marked")
        press(key("e", code: 14)) { $0.insertText("é", replacementRange: Self.notFound) }
        XCTAssertFalse(view.hasMarkedText())
        try await assertBytes([0xc3, 0xa9])
    }

    func testA3DeadAcuteThenEWithUnmarkFirst() async throws {
        press(key("", ignoring: "e", code: 14, [.option])) {
            $0.setMarkedText("´", selectedRange: NSRange(location: 1, length: 0), replacementRange: Self.notFound)
        }
        press(key("e", code: 14)) { view in
            view.unmarkText()
            view.insertText("é", replacementRange: Self.notFound)
        }
        try await assertBytes([0xc3, 0xa9])
    }

    func testA4DeadKeyThenNonCombiningLetter() async throws {
        deadAcute()
        press(key("x", code: 7)) { $0.insertText("´x", replacementRange: Self.notFound) }
        try await assertBytes([0xc2, 0xb4, 0x78])
    }

    func testA5DeadKeyThenSpaceInsertsTheBareAccent() async throws {
        deadAcute()
        press(key(" ", code: 49)) { $0.insertText("´", replacementRange: Self.notFound) }
        try await assertBytes([0xc2, 0xb4], "not 20: the text wins over the space key (D2)")
    }

    func testA6DeadKeyThenEscapeSendsNothing() async throws {
        deadAcute()
        press(key("\u{1b}", code: 53)) {
            $0.setMarkedText("", selectedRange: NSRange(location: 0, length: 0), replacementRange: Self.notFound)
        }
        XCTAssertFalse(view.hasMarkedText())
        deadAcute()
        press(key("\u{1b}", code: 53)) { $0.doCommand(by: #selector(NSResponder.cancelOperation(_:))) }
        try await assertBytes([], "the Escape that cancels a composition is the input method's (D3)")
    }

    // MARK: Section B: Swedish layout

    func testB1SwedishÖ() async throws {
        press(key("ö", code: 41)) { $0.insertText("ö", replacementRange: Self.notFound) }
        try await assertBytes([0xc3, 0xb6])
    }

    func testB2SwedishShiftÖ() async throws {
        press(key("Ö", ignoring: "ö", code: 41, [.shift])) { $0.insertText("Ö", replacementRange: Self.notFound) }
        try await assertBytes([0xc3, 0x96])
    }

    func testB3SwedishÅAndÄKeys() async throws {
        press(key("å", code: 33)) { $0.insertText("å", replacementRange: Self.notFound) }
        press(key("ä", code: 39)) { $0.insertText("ä", replacementRange: Self.notFound) }
        try await assertBytes([0xc3, 0xa5, 0xc3, 0xa4])
    }

    func testB4ControlÖSendsNoText() throws {
        press(key("ö", ignoring: "ö", code: 41, [.control])) { _ in XCTFail("Control skips the input system") }
        let sent = try XCTUnwrap(view.sentKeys.last)
        XCTAssertNil(sent.text, "a control combination carries no text")
        XCTAssertTrue(sent.modifiers.contains(.control))
        XCTAssertEqual(sent.codepoint, "ö")
        XCTAssertEqual(sent.baseLayout, ";", "the US key at the same position")
        // What Ctrl-ö encodes to is the core's decision (today `;`, as
        // xterm sends for Ctrl-; without modifyOtherKeys); the view's part is
        // that no control character travels as text.
    }

    // MARK: Section C: Japanese composition

    private func typeNihongo() {
        let steps: [(String, String, UInt16)] = [
            ("n", "n", 45), ("i", "に", 34), ("h", "にh", 4), ("o", "にほ", 31), ("n", "にほn", 45),
            ("g", "にほんg", 5), ("o", "にほんご", 31),
        ]
        for (typed, preedit, code) in steps {
            press(key(typed, code: code)) {
                $0.setMarkedText(
                    preedit, selectedRange: NSRange(location: (preedit as NSString).length, length: 0),
                    replacementRange: Self.notFound)
            }
            XCTAssertTrue(view.hasMarkedText())
        }
        XCTAssertEqual(view.sentKeys, [], "nothing is sent during composition (E1)")
        XCTAssertEqual(view.markedTextOverlay?.text, "にほんご", "the composition is drawn")
    }

    func testC1HiraganaCommittedWithReturn() async throws {
        typeNihongo()
        press(key("\r", code: 36)) { $0.insertText("にほんご", replacementRange: Self.notFound) }
        XCTAssertFalse(view.hasMarkedText())
        XCTAssertNil(view.markedTextOverlay)
        XCTAssertEqual(view.sentKeys, [.text("にほんご")], "one text-only event, never a paste (D1)")
        try await assertBytes(
            [0xe3, 0x81, 0xab, 0xe3, 0x81, 0xbb, 0xe3, 0x82, 0x93, 0xe3, 0x81, 0x94], "and no 0d for the Return")
    }

    func testC2KanjiConversionWithSpaceThenReturn() async throws {
        typeNihongo()
        press(key(" ", code: 49)) {
            $0.setMarkedText("日本語", selectedRange: NSRange(location: 3, length: 0), replacementRange: Self.notFound)
        }
        XCTAssertNotNil(view.attributedSubstring(forProposedRange: NSRange(location: 0, length: 3), actualRange: nil))
        press(key("\r", code: 36)) { $0.insertText("日本語", replacementRange: Self.notFound) }
        try await assertBytes([0xe6, 0x97, 0xa5, 0xe6, 0x9c, 0xac, 0xe8, 0xaa, 0x9e], "no 20 for the Space")
    }

    func testC3EscapeCancelsTheComposition() async throws {
        typeNihongo()
        press(key("\u{1b}", code: 53)) { view in
            view.setMarkedText("", selectedRange: NSRange(location: 0, length: 0), replacementRange: Self.notFound)
            view.unmarkText()
        }
        XCTAssertFalse(view.hasMarkedText())
        typeNihongo()
        press(key("\u{1b}", code: 53)) { $0.doCommand(by: #selector(NSResponder.cancelOperation(_:))) }
        try await assertBytes([], "no 1b")
    }

    func testC4PlainEscape() async throws {
        press(key("\u{1b}", code: 53)) { $0.doCommand(by: #selector(NSResponder.cancelOperation(_:))) }
        try await assertBytes([0x1b])
    }

    func testC5AndC6BackspaceEditsOnlyThePreedit() async throws {
        press(key("n", code: 45)) {
            $0.setMarkedText("にほ", selectedRange: NSRange(location: 2, length: 0), replacementRange: Self.notFound)
        }
        press(key("\u{7f}", code: 51)) {
            $0.setMarkedText("に", selectedRange: NSRange(location: 1, length: 0), replacementRange: Self.notFound)
        }
        press(key("\u{7f}", code: 51)) {
            $0.setMarkedText("", selectedRange: NSRange(location: 0, length: 0), replacementRange: Self.notFound)
        }
        XCTAssertFalse(view.hasMarkedText())
        try await assertBytes([], "no 7f: the last Backspace still belonged to the input method")
    }

    func testC7ArrowsInsideTheCompositionSendNothing() async throws {
        typeNihongo()
        press(key("\u{f702}", code: 123, [.numericPad, .function])) {
            $0.setMarkedText("にほんご", selectedRange: NSRange(location: 2, length: 0), replacementRange: Self.notFound)
        }
        try await assertBytes([])
    }

    func testC8FocusLossDiscardsTheComposition() async throws {
        press(key("n", code: 45)) {
            $0.setMarkedText("にほ", selectedRange: NSRange(location: 2, length: 0), replacementRange: Self.notFound)
        }
        XCTAssertTrue(view.resignFirstResponder())
        XCTAssertFalse(view.hasMarkedText())
        XCTAssertNil(view.markedTextOverlay)
        try await assertBytes([], "discarded, not committed (D5)")
    }

    // MARK: Section D: Shift-Space and editing keys

    func testD1ShiftSpace() async throws {
        press(key(" ", code: 49, [.shift])) { $0.insertText(" ", replacementRange: Self.notFound) }
        try await assertBytes([0x20])
    }

    func testD2ShiftSpaceWhileComposing() async throws {
        press(key("n", code: 45)) {
            $0.setMarkedText("に", selectedRange: NSRange(location: 1, length: 0), replacementRange: Self.notFound)
        }
        press(key(" ", code: 49, [.shift])) {
            $0.setMarkedText("に", selectedRange: NSRange(location: 1, length: 0), replacementRange: Self.notFound)
        }
        try await assertBytes([])
    }

    func testD3EditingKeysFromTheirKeyCodes() async throws {
        let cases: [(NSEvent, Selector, [UInt8])] = [
            (key("\r", code: 36), #selector(NSResponder.insertNewline(_:)), [0x0d]),
            (key("\r", code: 36, [.option]), #selector(NSResponder.insertNewlineIgnoringFieldEditor(_:)), [0x1b, 0x0d]),
            (key("\u{7f}", code: 51), #selector(NSResponder.deleteBackward(_:)), [0x7f]),
            (key("\u{f728}", code: 117, [.function]), #selector(NSResponder.deleteForward(_:)), Array("\u{1b}[3~".utf8)),
            (key("\t", code: 48), #selector(NSResponder.insertTab(_:)), [0x09]),
            (key("\u{19}", ignoring: "\t", code: 48, [.shift]), #selector(NSResponder.insertBacktab(_:)), Array("\u{1b}[Z".utf8)),
            (key("\u{1b}", code: 53), #selector(NSResponder.cancelOperation(_:)), [0x1b]),
            (key("\u{f700}", code: 126, [.numericPad, .function]), #selector(NSResponder.moveUp(_:)), Array("\u{1b}[A".utf8)),
            (key("\u{f701}", code: 125, [.numericPad, .function]), #selector(NSResponder.moveDown(_:)), Array("\u{1b}[B".utf8)),
            (key("\u{f703}", code: 124, [.numericPad, .function]), #selector(NSResponder.moveRight(_:)), Array("\u{1b}[C".utf8)),
            (key("\u{f702}", code: 123, [.numericPad, .function]), #selector(NSResponder.moveLeft(_:)), Array("\u{1b}[D".utf8)),
        ]
        var expected: [UInt8] = []
        for (event, selector, bytes) in cases {
            press(event) { $0.doCommand(by: selector) }
            expected += bytes
        }
        try await assertBytes(expected)
    }

    func testD3SelectorsNameKeysWhoseKeyCodeNamesNothing() async throws {
        // A key code no layout uses, as a remapped key or custom key binding
        // would produce: the selector decides.
        let selectors: [(Selector, [UInt8])] = [
            (#selector(NSResponder.insertNewline(_:)), [0x0d]),
            (#selector(NSResponder.insertBacktab(_:)), Array("\u{1b}[Z".utf8)),
            (#selector(NSResponder.moveWordLeft(_:)), Array("\u{1b}[1;3D".utf8)),
            (#selector(NSResponder.moveToEndOfLine(_:)), Array("\u{1b}[F".utf8)),
        ]
        var expected: [UInt8] = []
        for (selector, bytes) in selectors {
            press(key("", code: 0x7F)) { $0.doCommand(by: selector) }
            expected += bytes
        }
        // The key code wins over a selector that disagrees with it (a remapped
        // binding): Return stays Return (D6).
        press(key("\r", code: 36)) { $0.doCommand(by: #selector(NSResponder.moveUp(_:))) }
        expected += [0x0d]
        // An unknown selector falls back to the key's own character.
        press(key("q", code: 12)) { $0.doCommand(by: Selector(("someCustomAction:"))) }
        expected += [0x71]
        try await assertBytes(expected)
    }

    func testFunctionKeyPrivateUseTextIsNotSent() async throws {
        press(key("\u{f704}", code: 122, [.function])) { $0.insertText("\u{f704}", replacementRange: Self.notFound) }
        try await assertBytes(Array("\u{1b}OP".utf8), "F1, not U+F704")
    }

    func testCommandKeysSkipTheInputSystemAndTheSession() async throws {
        // Command combinations that are not menu key equivalents (Cmd-V, Cmd-K,
        // Cmd-A while there is no Edit menu) go up the responder chain; the
        // core would encode them as `CSI u` sequences the shell prints.
        for (character, code) in [("v", UInt16(9)), ("k", 40), ("a", 0), ("c", 8)] {
            press(key(character, code: code, [.command])) { _ in XCTFail("Command skips the input system") }
            press(key(character.uppercased(), ignoring: character, code: code, [.command, .shift])) { _ in
                XCTFail("Command skips the input system")
            }
        }
        XCTAssertEqual(view.sentKeys, [], "no Command key reaches the session")
        try await assertBytes([])
    }

    func testControlLetterSendsItsControlByte() async throws {
        press(key("\u{3}", ignoring: "c", code: 8, [.control])) { _ in XCTFail("Control skips the input system") }
        try await assertBytes([0x03])
    }

    func testControlCharactersAreStrippedFromCommittedText() async throws {
        // A multi-line commit (dictation, the character viewer) types its
        // printable text: never CR, LF or another control.
        view.insertText("ab\ncd\r\u{7}e\u{85}f\u{7f}", replacementRange: Self.notFound)
        view.setMarkedText("に", selectedRange: NSRange(location: 1, length: 0), replacementRange: Self.notFound)
        press(key("\r", code: 36)) { $0.insertText("に\nほ", replacementRange: Self.notFound) }
        press(key("x", code: 7)) { $0.insertText("x\u{1b}", replacementRange: Self.notFound) }
        // Only a control: the key itself.
        press(key("\r", code: 36)) { $0.insertText("\r", replacementRange: Self.notFound) }
        try await assertBytes(Array("abcdef".utf8) + [0xe3, 0x81, 0xab, 0xe3, 0x81, 0xbb, 0x78, 0x0d])
    }

    // MARK: Section E: invariants

    func testE2AndE3MarkedAndSelectedRanges() {
        XCTAssertEqual(view.markedRange(), Self.notFound)
        XCTAssertEqual(view.selectedRange(), Self.notFound)
        view.setMarkedText("にほ", selectedRange: NSRange(location: 1, length: 0), replacementRange: Self.notFound)
        XCTAssertEqual(view.markedRange(), NSRange(location: 0, length: 2))
        XCTAssertEqual(view.selectedRange(), NSRange(location: 1, length: 0))
        view.setMarkedText("𠮷", selectedRange: NSRange(location: 2, length: 0), replacementRange: Self.notFound)
        XCTAssertEqual(view.markedRange(), NSRange(location: 0, length: 2), "UTF-16 units")
        view.insertText("𠮷", replacementRange: Self.notFound)
        XCTAssertFalse(view.hasMarkedText())
        XCTAssertEqual(view.markedRange(), Self.notFound)
        XCTAssertEqual(view.selectedRange(), Self.notFound)
        XCTAssertEqual(view.characterIndex(for: .zero), NSNotFound)
        XCTAssertEqual(
            view.attributedSubstring(forProposedRange: NSRange(location: 0, length: 1), actualRange: nil), nil)
    }

    func testE4AstralCommitOutsideAKeyPress() async throws {
        // A commit by clicking a candidate, the character viewer or
        // dictation arrives outside keyDown.
        view.setMarkedText("𠮷", selectedRange: NSRange(location: 2, length: 0), replacementRange: Self.notFound)
        view.insertText(NSAttributedString(string: "𠮷"), replacementRange: Self.notFound)
        view.insertText("é", replacementRange: Self.notFound)
        try await assertBytes([0xf0, 0xa0, 0xae, 0xb7, 0xc3, 0xa9])
    }

    func testE5FirstRectIsTheCompositionCellOnScreen() throws {
        var actual = NSRange(location: 99, length: 99)
        let rect = view.firstRect(forCharacterRange: NSRange(location: 0, length: 0), actualRange: &actual)
        XCTAssertEqual(actual, NSRange(location: 0, length: 0))
        let cell = view.compositionCellRect()
        let expected = window.convertToScreen(view.convert(cell, to: nil))
        XCTAssertEqual(rect, expected)
        XCTAssertGreaterThan(rect.height, 0)
    }

    func testFirstRectIsTheCursorCellOnScreen() throws {
        // 3x (not the host's 1x or 2x): 12 px of padding (4 pt), 24 x 48 px
        // cells (8 x 16 pt), cursor at row 2, column 3.
        view.metrics = Self.metrics(cursorRow: 2, cursorCol: 3)
        let viewOnScreen = window.convertToScreen(view.convert(view.bounds, to: nil))
        let rect = view.firstRect(forCharacterRange: NSRange(location: 0, length: 0), actualRange: nil)
        // In points: 4 of padding, 8 x 16 cells.
        let padding: CGFloat = 4
        let cellWidth: CGFloat = 8
        let cellHeight: CGFloat = 16
        let left: CGFloat = padding + 3 * cellWidth
        let top: CGFloat = padding + 2 * cellHeight
        XCTAssertEqual(rect.minX, viewOnScreen.minX + left, accuracy: 0.01)
        XCTAssertEqual(rect.maxY, viewOnScreen.maxY - top, accuracy: 0.01)
        XCTAssertEqual(rect.width, cellWidth, accuracy: 0.01)
        XCTAssertEqual(rect.height, cellHeight, accuracy: 0.01)

        // On a wide character the rect spans both columns.
        view.metrics?.cursorWide = true
        let wide = view.firstRect(forCharacterRange: NSRange(location: 0, length: 0), actualRange: nil)
        XCTAssertEqual(wide.minX, rect.minX, accuracy: 0.01)
        XCTAssertEqual(wide.width, 2 * cellWidth, accuracy: 0.01)

        // Without a surface, the view's top-left corner.
        view.metrics = nil
        let fallback = view.firstRect(forCharacterRange: NSRange(location: 0, length: 0), actualRange: nil)
        XCTAssertEqual(fallback.minX, viewOnScreen.minX, accuracy: 0.01)
        XCTAssertEqual(fallback.maxY, viewOnScreen.maxY, accuracy: 0.01)
        XCTAssertGreaterThan(fallback.height, 0)
    }

    func testTheSurfaceMetricsAreReadOnAttachResizeAndRender() throws {
        try requireSurface()
        // Geometry only: the cursor fields may move between two reads.
        func geometry(_ metrics: TesseraSurface.Metrics?) -> TesseraSurface.Metrics? {
            metrics.map {
                var copy = $0
                (copy.cursorRow, copy.cursorCol, copy.cursorOnScreen, copy.cursorWide) = (0, 0, false, false)
                return copy
            }
        }
        let attached = try XCTUnwrap(view.metrics, "read on attach")
        XCTAssertEqual(geometry(attached), geometry(try view.surface.metrics()))
        XCTAssertEqual(attached.widthPx, view.pixelSize.widthPx)
        view.metrics = nil
        view.sessionDidOutput()
        view.occlusionOverride = false
        view.displayLinkDidFire()
        XCTAssertEqual(geometry(view.metrics), geometry(try view.surface.metrics()), "read at every render")
        view.metrics = nil
        view.setFrameSize(NSSize(width: 500, height: 320))
        XCTAssertEqual(view.metrics?.widthPx, view.pixelSize.widthPx, "read after a resize")
        XCTAssertEqual(geometry(view.metrics), geometry(try view.surface.metrics()))
        window.close()
        XCTAssertNil(view.metrics, "gone with the surface")
    }

    func testTheCompositionFollowsMetricsReadAtARender() throws {
        try requireSurface()
        view.setMarkedText("に", selectedRange: NSRange(location: 1, length: 0), replacementRange: Self.notFound)
        let before = try XCTUnwrap(view.markedTextOverlay).frame
        // A zoom the view has not seen yet: its next render reads the
        // larger cell and moves the overlay with it.
        try view.surface.setFontSize(26)
        view.sessionDidOutput()
        view.occlusionOverride = false
        view.displayLinkDidFire()
        let after = try XCTUnwrap(view.markedTextOverlay).frame
        XCTAssertGreaterThan(after.height, before.height)
        XCTAssertEqual(after.height, view.compositionCellRect().height, accuracy: 0.01)
    }

    /// Metrics for a 3x surface with 12 px of padding and 24 x 48 px cells
    /// (4 pt and 8 x 16 pt), 24 x 80 cells. 3x is no host's backing scale,
    /// so geometry that used the window's scale instead of these would be
    /// off.
    private static func metrics(cursorRow: UInt16 = 0, cursorCol: UInt16 = 0) -> TesseraSurface.Metrics {
        let padding: UInt32 = 12
        let width: UInt32 = 2 * padding + 80 * 24
        let height: UInt32 = 2 * padding + 24 * 48
        return .init(
            widthPx: width, heightPx: height, scale: 3, cellWidthPx: 24, cellHeightPx: 48,
            baselinePx: 36, paddingPx: padding, rows: 24, cols: 80, fontSizePt: 13, cursorRow: cursorRow,
            cursorCol: cursorCol, cursorOnScreen: true, cursorWide: false)
    }

    /// ``metrics(cursorRow:cursorCol:)`` in pixels, typed for the tests'
    /// arithmetic.
    private static let scalePx: CGFloat = 3
    private static let paddingPx: CGFloat = 12
    private static let cellWidthPx: CGFloat = 24
    private static let cellHeightPx: CGFloat = 48

    /// The view point `x`, `y` pixels from the top-left corner at
    /// ``scalePx``.
    private func point(px x: CGFloat, _ y: CGFloat) -> NSPoint {
        let height: CGFloat = view.bounds.height
        let fromTop: CGFloat = y / Self.scalePx
        return NSPoint(x: x / Self.scalePx, y: height - fromTop)
    }

    func testAHugeCompositionKeepsTheOverlayWithinTheView() {
        view.setMarkedText(
            String(repeating: "あ", count: 100_000), selectedRange: NSRange(location: 0, length: 0),
            replacementRange: Self.notFound)
        let overlay = try? XCTUnwrap(view.markedTextOverlay)
        XCTAssertLessThanOrEqual(overlay?.frame.maxX ?? .infinity, view.bounds.width + 0.5)
        XCTAssertLessThanOrEqual(overlay?.text.count ?? .max, TerminalSurfaceView.maxOverlayCharacters)
        XCTAssertEqual(view.markedRange(), NSRange(location: 0, length: 100_000), "the composition itself is kept")
    }

    func testMarkedTextOverlayFollowsTheComposition() {
        XCTAssertNil(view.markedTextOverlay)
        view.setMarkedText("に", selectedRange: NSRange(location: 1, length: 0), replacementRange: Self.notFound)
        let overlay = try? XCTUnwrap(view.markedTextOverlay)
        XCTAssertEqual(overlay?.text, "に")
        XCTAssertEqual(overlay?.frame.minX ?? 0, view.compositionCellRect().minX, accuracy: 0.5)
        view.unmarkText()
        XCTAssertNil(view.markedTextOverlay)
    }

    // MARK: Focus and cursor

    func testFocusFollowsFirstResponderAndKeyWindow() {
        XCTAssertTrue(view.acceptsFirstResponder)
        XCTAssertTrue(window.initialFirstResponder === view)
        setKey(false)
        _ = view.resignFirstResponder()
        XCTAssertTrue(view.becomeFirstResponder())
        XCTAssertEqual(view.sentFocus, false, "first responder of a window that is not key")
        setKey(true)
        XCTAssertEqual(view.sentFocus, true)
        setKey(false)
        XCTAssertEqual(view.sentFocus, false)
        setKey(true)
        XCTAssertTrue(view.resignFirstResponder())
        XCTAssertEqual(view.sentFocus, false)
        setKey(true)
        XCTAssertEqual(view.sentFocus, false, "a key window whose first responder is elsewhere")
        XCTAssertTrue(view.becomeFirstResponder())
        XCTAssertEqual(view.sentFocus, true, "first responder of a key window")
    }

    func testLosingKeyWindowDiscardsTheComposition() async throws {
        press(key("n", code: 45)) {
            $0.setMarkedText("にほ", selectedRange: NSRange(location: 2, length: 0), replacementRange: Self.notFound)
        }
        view.windowKeyChanged(isKey: false)
        XCTAssertFalse(view.hasMarkedText())
        XCTAssertNil(view.markedTextOverlay)
        try await assertBytes([], "discarded, not committed (D5)")
    }

    // MARK: Resize coalescing

    private func requireSurface() throws {
        guard MTLCreateSystemDefaultDevice() != nil else {
            throw XCTSkip("no Metal device on this machine: the surface tests need a GPU")
        }
        guard view.surface.isAttached else {
            throw XCTSkip("the core could not attach a renderer to the view (no GPU can present, or no font)")
        }
    }

    func testLiveResizeReachesTheCoreAtMostOncePerTick() throws {
        try requireSurface()
        let resizesBefore = view.coreResizeCount
        let ticksBefore = view.tickCount
        view.viewWillStartLiveResize()
        for step in 0..<200 {
            view.setFrameSize(NSSize(width: 400 + step, height: 300 + step / 2))
            if step % 10 == 9 {
                view.displayLinkDidFire()
            }
        }
        // A final size after the last tick, which only the end of the live
        // resize sends.
        view.setFrameSize(NSSize(width: 777, height: 555))
        let resizesDuring = view.coreResizeCount - resizesBefore
        view.viewDidEndLiveResize()

        let ticks = view.tickCount - ticksBefore
        let resizes = view.coreResizeCount - resizesBefore
        XCTAssertGreaterThanOrEqual(ticks, 20)
        XCTAssertLessThanOrEqual(resizesDuring, ticks, "at most one resize per tick during the live resize")
        XCTAssertLessThanOrEqual(resizes, ticks + 1)
        XCTAssertGreaterThanOrEqual(resizes, 2, "ticks pass resizes on during the drag")
        XCTAssertEqual(view.surface.size, view.pixelSize, "the core has the last size")
        XCTAssertEqual(view.pixelSize.widthPx, UInt32((777 * window.backingScaleFactor).rounded()))
    }

    func testResizeOutsideALiveResizeReachesTheCoreAtOnce() throws {
        try requireSurface()
        let before = view.coreResizeCount
        view.setFrameSize(NSSize(width: 500, height: 320))
        XCTAssertEqual(view.coreResizeCount, before + 1)
        XCTAssertEqual(view.surface.size, view.pixelSize)
        view.setFrameSize(NSSize(width: 500, height: 320))
        XCTAssertEqual(view.coreResizeCount, before + 1, "an unchanged size is not sent again")
    }

    // MARK: Display link

    func testOccludedWindowRendersNothingUntilVisible() async throws {
        try requireSurface()
        view.occlusionOverride = true
        XCTAssertTrue(view.isDisplayLinkPaused)
        let before = view.renderCount
        // A second of output while occluded, with the refreshes a running
        // link would deliver (60 Hz). The hosted test app's windows are never
        // on screen, so a real link does not fire here at all.
        let deadline = ContinuousClock.now + .seconds(1)
        while ContinuousClock.now < deadline {
            try view.session.write(Array("x".utf8))
            view.sessionDidOutput()
            view.displayLinkDidFire()
            try await Task.sleep(for: .milliseconds(16))
        }
        XCTAssertEqual(view.renderCount, before, "no frame while occluded")

        view.occlusionOverride = false
        XCTAssertFalse(view.isDisplayLinkPaused, "the link resumes")
        view.displayLinkDidFire()
        XCTAssertEqual(view.renderCount, before + 1, "the next refresh renders")
    }

    func testRendersOnlyWhenSomethingChanged() async throws {
        try requireSurface()
        view.occlusionOverride = false
        setKey(false)
        _ = view.resignFirstResponder()
        view.displayLinkDidFire()
        let before = view.renderCount
        view.displayLinkDidFire()
        view.displayLinkDidFire()
        XCTAssertEqual(view.renderCount, before, "nothing changed")
        view.sessionDidOutput()
        view.sessionDidOutput()
        view.displayLinkDidFire()
        view.displayLinkDidFire()
        XCTAssertEqual(view.renderCount, before + 1, "output is rendered once per tick")
        _ = view.becomeFirstResponder()
        setKey(true)
        view.displayLinkDidFire()
        XCTAssertEqual(view.renderCount, before + 2, "a focus change is rendered")
        _ = view.resignFirstResponder()
        view.displayLinkDidFire()
        XCTAssertEqual(view.renderCount, before + 3, "a focus change is rendered")
    }

    /// What the session router calls on every view when the core re-themes
    /// them (`appearanceDidChange(opaque:)`): the window and layer follow the
    /// background's opacity, and an idle view draws the new colours.
    func testAnAppearanceChangeFollowsOpacityAndRedraws() throws {
        try requireSurface()
        view.occlusionOverride = false
        view.displayLinkDidFire()
        XCTAssertTrue(view.isDisplayLinkPaused, "idle")
        XCTAssertTrue(view.metalLayer.isOpaque, "the default theme is opaque")

        view.applyLayerOpacity(opaque: false)
        XCTAssertTrue(view.metalLayer.isOpaque, "the layer is the core's, switched by the next frame")
        XCTAssertFalse(window.isOpaque)
        XCTAssertEqual(window.backgroundColor, .clear)
        XCTAssertFalse(view.isDisplayLinkPaused, "a re-theme wakes the link")
        let before = view.renderCount
        view.displayLinkDidFire()
        XCTAssertEqual(view.renderCount, before + 1, "and is drawn")

        view.applyLayerOpacity(opaque: true)
        XCTAssertTrue(window.isOpaque)
        XCTAssertEqual(window.backgroundColor, .windowBackgroundColor)
        view.displayLinkDidFire()
        XCTAssertEqual(view.renderCount, before + 2)
    }

    /// A view without a surface draws nothing, so a translucent re-theme
    /// leaves its window opaque (not see-through onto the desktop) and its
    /// link idle.
    func testAnAppearanceChangeLeavesAViewWithoutASurfaceAlone() throws {
        try requireSurface()
        view.occlusionOverride = false
        view.displayLinkDidFire()
        XCTAssertTrue(view.isDisplayLinkPaused, "idle")
        try view.surface.detach()
        view.applyLayerOpacity(opaque: false)
        XCTAssertTrue(window.isOpaque)
        XCTAssertTrue(view.isDisplayLinkPaused)
    }

    func testTheLinkIdlesWithNothingToDraw() throws {
        try requireSurface()
        view.occlusionOverride = false
        view.displayLinkDidFire()
        XCTAssertTrue(view.isDisplayLinkPaused, "nothing changed: the link idles")
        view.sessionDidOutput()
        XCTAssertFalse(view.isDisplayLinkPaused, "output wakes it")
        view.displayLinkDidFire()
        XCTAssertTrue(view.isDisplayLinkPaused)
        view.viewWillStartLiveResize()
        view.setFrameSize(NSSize(width: 610, height: 410))
        XCTAssertFalse(view.isDisplayLinkPaused, "a coalesced resize wakes it")
        view.viewDidEndLiveResize()
    }

    func testTheLinkIdlesWithoutASurface() throws {
        try requireSurface()
        view.occlusionOverride = false
        view.displayLinkDidFire()
        XCTAssertTrue(view.isDisplayLinkPaused)
        // As after a first attach that failed: no surface, no re-attach.
        try view.surface.detach()
        view.sessionDidOutput()
        XCTAssertTrue(view.isDisplayLinkPaused, "nothing can be drawn: the link idles")
    }

    func testReparentingWithinTheWindowKeepsOneLinkAndOneSetOfObservers() throws {
        let link = try XCTUnwrap(view.displayLink)
        let observers = view.windowObserverCount
        let host = NSView(frame: view.frame)
        window.contentView = host
        host.addSubview(view)
        let relinked = try XCTUnwrap(view.displayLink)
        XCTAssertEqual(view.windowObserverCount, observers)
        let other = NSView(frame: host.bounds)
        host.addSubview(other)
        other.addSubview(view)  // straight from one superview to another
        XCTAssertTrue(view.displayLink === relinked, "no second link")
        XCTAssertEqual(view.windowObserverCount, observers, "no second set of observers")
        _ = link
        XCTAssertTrue(view.window === window)
    }

    func testAFailingReattachStopsAfterAFewAttemptsAndResumesOnResize() throws {
        try requireSurface()
        let interval = TerminalSurfaceView.reattachInterval
        TerminalSurfaceView.reattachInterval = .zero
        defer { TerminalSurfaceView.reattachInterval = interval }
        view.occlusionOverride = false
        view.displayLinkDidFire()
        // Another surface object for the session detaches the core's surface,
        // as a lost swapchain would, and then holds the session, so every
        // re-attach fails.
        let rogue = TesseraSurface(session: view.session)
        try? rogue.detach()
        view.sessionDidOutput()
        XCTAssertNotNil(view.metrics, "kept until a read finds the surface gone")
        view.displayLinkDidFire()  // the render finds the surface gone
        XCTAssertFalse(view.surface.isAttached)
        XCTAssertNil(view.metrics, "that render's read cleared the cached metrics")
        try rogue.attach(layer: CAMetalLayer(), widthPx: 100, heightPx: 100, scale: 1)
        for _ in 0..<20 {
            view.displayLinkDidFire()
        }
        XCTAssertNil(view.metrics, "nothing reads them while detached")
        XCTAssertEqual(view.reattachFailures, TerminalSurfaceView.maxReattachFailures, "attempts stop")
        XCTAssertTrue(view.isDisplayLinkPaused, "the link idles after giving up")
        // Once the session is free, a size change tries again.
        try rogue.detach()
        view.displayLinkDidFire()
        XCTAssertFalse(view.surface.isAttached, "no attempt without a change")
        view.setFrameSize(NSSize(width: view.frame.width + 10, height: view.frame.height))
        XCTAssertFalse(view.isDisplayLinkPaused)
        view.displayLinkDidFire()
        XCTAssertTrue(view.surface.isAttached)
        XCTAssertEqual(view.reattachFailures, 0)
        XCTAssertNotNil(view.metrics, "the re-attach read them again")
    }

    func testASurfaceLostBeforeAWindowMoveIsRetriedInTheNewWindow() throws {
        try requireSurface()
        let interval = TerminalSurfaceView.reattachInterval
        TerminalSurfaceView.reattachInterval = .zero
        defer { TerminalSurfaceView.reattachInterval = interval }
        view.occlusionOverride = false
        view.displayLinkDidFire()
        let rogue = TesseraSurface(session: view.session)
        try? rogue.detach()
        view.sessionDidOutput()
        view.displayLinkDidFire()  // the render finds the surface gone
        try rogue.attach(layer: CAMetalLayer(), widthPx: 100, heightPx: 100, scale: 1)
        for _ in 0..<(TerminalSurfaceView.maxReattachFailures - 1) {
            view.displayLinkDidFire()
        }
        XCTAssertFalse(view.surface.isAttached)
        // Mid-retry, the view moves to another window, then the session frees up.
        let other = KeyControlledWindow(contentRect: view.frame)
        extraWindows.append(other)
        try XCTUnwrap(other.contentView).addSubview(view)
        try rogue.detach()
        view.occlusionOverride = false
        for _ in 0..<3 {
            view.displayLinkDidFire()
        }
        XCTAssertTrue(view.surface.isAttached, "retried in the new window")
    }

    func testBecomingVisibleRedraws() throws {
        try requireSurface()
        view.occlusionOverride = false
        view.displayLinkDidFire()
        let before = view.renderCount
        view.occlusionOverride = true
        XCTAssertTrue(view.isDisplayLinkPaused)
        view.occlusionOverride = false
        XCTAssertFalse(view.isDisplayLinkPaused, "a window that shows again is redrawn")
        view.displayLinkDidFire()
        XCTAssertEqual(view.renderCount, before + 1)
    }

    func testClosingTheWindowStopsTheDisplayLinkAndDetaches() throws {
        try requireSurface()
        window.close()
        XCTAssertTrue(view.isDisplayLinkPaused)
        XCTAssertFalse(view.surface.isAttached)
    }

    // MARK: Mouse

    func testPointerPositionsBecomeCells() {
        view.metrics = Self.metrics()
        // 12 px of padding, then 24 x 48 px cells, from the top-left corner,
        // at the metrics' scale of 3.
        let x: CGFloat = Self.paddingPx + 3 * Self.cellWidthPx + 1
        let y: CGFloat = Self.paddingPx + 2 * Self.cellHeightPx + 1
        let (cell, pixels) = view.cell(at: point(px: x, y))
        XCTAssertEqual(cell.col, 3)
        XCTAssertEqual(cell.row, 2)
        XCTAssertEqual(pixels.x, x, accuracy: 0.01)
        XCTAssertEqual(pixels.y, y, accuracy: 0.01)
        let (outside, _) = view.cell(at: NSPoint(x: -50, y: view.bounds.height + 50))
        XCTAssertEqual(outside.col, 0)
        XCTAssertEqual(outside.row, 0)
    }

    func testTheGridsCornersAndPaddingBandMapToEdgeCells() {
        view.metrics = Self.metrics()
        // Points from the top-left corner, in pixels at scale 3.
        func cell(_ x: CGFloat, _ y: CGFloat) -> [UInt16] {
            let (cell, _) = view.cell(at: point(px: x, y))
            return [cell.row, cell.col]
        }
        let pad: CGFloat = Self.paddingPx
        let width: CGFloat = Self.cellWidthPx
        let height: CGFloat = Self.cellHeightPx
        let right: CGFloat = pad + 80 * width
        let bottom: CGFloat = pad + 24 * height
        // The four corner cells, just inside the grid.
        XCTAssertEqual(cell(pad, pad), [0, 0])
        XCTAssertEqual(cell(right - 1, pad), [0, 79])
        XCTAssertEqual(cell(pad, bottom - 1), [23, 0])
        XCTAssertEqual(cell(right - 1, bottom - 1), [23, 79])
        // The padding band on every side belongs to the nearest cell.
        XCTAssertEqual(cell(1, 1), [0, 0])
        XCTAssertEqual(cell(pad - 1, 150), [2, 0], "(150 - 12) / 48")
        XCTAssertEqual(cell(right + pad - 1, pad), [0, 79])
        XCTAssertEqual(cell(150, bottom + pad - 1), [23, 5], "(150 - 12) / 24")
        // Beyond the surface (a view larger than the grid), still clamped.
        XCTAssertEqual(cell(right + 500, bottom + 500), [23, 79])
        // A cell boundary belongs to the cell it starts.
        let second: (x: CGFloat, y: CGFloat) = (pad + width, pad + height)
        XCTAssertEqual(cell(second.x, second.y), [1, 1])
        XCTAssertEqual(cell(second.x - 0.5, second.y - 0.5), [0, 0])
    }

    func testMouseAndScrollEventsReachTheSession() async throws {
        for type in [NSEvent.EventType.leftMouseDown, .leftMouseDragged, .leftMouseUp, .rightMouseDown, .otherMouseDown] {
            let event = try XCTUnwrap(
                NSEvent.mouseEvent(
                    with: type, location: NSPoint(x: 20, y: 20), modifierFlags: [], timestamp: 0,
                    windowNumber: window.windowNumber, context: nil, eventNumber: 0, clickCount: 1, pressure: 1))
            switch type {
            case .leftMouseDown: view.mouseDown(with: event)
            case .leftMouseDragged: view.mouseDragged(with: event)
            case .leftMouseUp: view.mouseUp(with: event)
            case .rightMouseDown: view.rightMouseDown(with: event)
            default: view.otherMouseDown(with: event)
            }
        }
        let scroll = try XCTUnwrap(
            CGEvent(scrollWheelEvent2Source: nil, units: .line, wheelCount: 1, wheel1: 3, wheel2: 0, wheel3: 0)
                .flatMap(NSEvent.init(cgEvent:)))
        view.scrollWheel(with: scroll)
        try await assertBytes([], "mouse reporting is not on, and the screen has no scrollback")
    }

    // MARK: Menu actions: Edit

    private func menuItem(_ action: Selector) -> NSMenuItem {
        NSMenuItem(title: NSStringFromSelector(action), action: action, keyEquivalent: "")
    }

    private func isEnabled(_ action: Selector) -> Bool {
        view.validateMenuItem(menuItem(action))
    }

    func testPasteSendsThePasteboardTextToTheSession() async throws {
        view.pasteboard.clearContents()
        view.pasteboard.setString("echo pasted", forType: .string)
        XCTAssertTrue(isEnabled(#selector(TerminalSurfaceView.paste(_:))))
        XCTAssertTrue(isEnabled(#selector(TerminalSurfaceView.pasteAsPlainText(_:))))
        view.paste(nil)
        XCTAssertEqual(view.pasteCount, 1)
        try await assertBytes(Array("echo pasted".utf8), "bracketed paste is off: the text as is")
        view.pasteboard.clearContents()
        view.pasteboard.setString("plain", forType: .string)
        view.pasteAsPlainText(nil)
        XCTAssertEqual(view.pasteCount, 2)
        try await assertBytes(Array("echo pasted~plain".utf8), "after the first sentinel")
    }

    func testPasteWithNoTextOnThePasteboardSendsNothing() async throws {
        view.pasteboard.clearContents()
        XCTAssertFalse(isEnabled(#selector(TerminalSurfaceView.paste(_:))), "nothing to paste")
        XCTAssertFalse(isEnabled(#selector(TerminalSurfaceView.pasteAsPlainText(_:))))
        view.paste(nil)
        view.pasteAsPlainText(nil)
        // Something other than text: still nothing to paste.
        view.pasteboard.setData(Data([1, 2, 3]), forType: .png)
        view.paste(nil)
        XCTAssertFalse(isEnabled(#selector(TerminalSurfaceView.paste(_:))))
        // An empty string is no paste either.
        view.pasteboard.clearContents()
        view.pasteboard.setString("", forType: .string)
        view.paste(nil)
        XCTAssertEqual(view.pasteCount, 0, "no call into the core")
        try await assertBytes([])
    }

    func testAPasteLargerThanTheCoreTakesIsRefusedBeforeTheCore() async throws {
        // Past the 16 MiB of pending input a paste may take, counted in
        // UTF-16 (free), and in UTF-8 when that could be larger: two-byte é
        // makes 9 Mi characters 18 MiB. Neither reaches the core, which would
        // transcode it all before refusing it.
        for text in [String(repeating: "x", count: 16 << 20 + 1), String(repeating: "é", count: 9 << 20)] {
            view.pasteboard.clearContents()
            view.pasteboard.setString(text, forType: .string)
            view.paste(nil)
        }
        XCTAssertEqual(view.pasteCount, 0, "no call into the core")
        try await assertBytes([])
    }

    func testThePasteLimitCountsUTF8() {
        let limit = TerminalSurfaceView.maxPasteBytes
        XCTAssertTrue(TerminalSurfaceView.fitsPasteLimit(String(repeating: "x", count: limit)))
        XCTAssertFalse(TerminalSurfaceView.fitsPasteLimit(String(repeating: "x", count: limit + 1)))
        // é is 2 bytes, 日 3, 🎉 4 (two UTF-16 units).
        XCTAssertTrue(TerminalSurfaceView.fitsPasteLimit(String(repeating: "é", count: limit / 2)))
        XCTAssertFalse(TerminalSurfaceView.fitsPasteLimit(String(repeating: "é", count: limit / 2) + "x"))
        XCTAssertTrue(TerminalSurfaceView.fitsPasteLimit(String(repeating: "日", count: limit / 3)))
        XCTAssertFalse(TerminalSurfaceView.fitsPasteLimit(String(repeating: "日", count: limit / 3 + 1)))
        XCTAssertTrue(TerminalSurfaceView.fitsPasteLimit(String(repeating: "🎉", count: limit / 4)))
        XCTAssertFalse(TerminalSurfaceView.fitsPasteLimit(String(repeating: "🎉", count: limit / 4) + "x"))
    }

    func testPasteAndClearToStartAreDisabledOnceTheShellExited() {
        view.pasteboard.clearContents()
        view.pasteboard.setString("text", forType: .string)
        let paste = #selector(TerminalSurfaceView.paste(_:))
        let plain = #selector(TerminalSurfaceView.pasteAsPlainText(_:))
        let clear = #selector(TerminalSurfaceView.clearScrollback(_:))
        XCTAssertTrue(isEnabled(paste))
        XCTAssertTrue(isEnabled(plain))
        XCTAssertTrue(isEnabled(clear))
        view.sessionDidExit(code: 0, signal: 0)
        XCTAssertFalse(isEnabled(paste), "nothing reaches an exited shell")
        XCTAssertFalse(isEnabled(plain))
        XCTAssertFalse(isEnabled(clear))
    }

    func testCopyAndSelectAllAreDisabledWithoutASelection() async throws {
        let copy = #selector(TerminalSurfaceView.copy(_:))
        let selectAll = #selector(TerminalSurfaceView.selectAll(_:))
        XCTAssertFalse(view.hasSelection)
        XCTAssertFalse(isEnabled(copy))
        XCTAssertFalse(isEnabled(selectAll))
        // Wired, so the selection model (#30) only has to supply the text.
        view.hasSelection = true
        XCTAssertTrue(isEnabled(copy))
        XCTAssertTrue(isEnabled(selectAll))
        view.pasteboard.clearContents()
        view.copy(nil)
        view.selectAll(nil)
        XCTAssertNil(view.pasteboard.string(forType: .string), "nothing to copy yet")
        try await assertBytes([])
    }

    // MARK: Menu actions during a composition

    /// Paste, ⌘K and zoom leave a composition alone (ADR 0017): the paste is
    /// sent ahead of it, and the input method commits it later as usual.
    func testPasteDuringACompositionIsSentAndKeepsTheComposition() async throws {
        typeNihongo()
        view.pasteboard.clearContents()
        view.pasteboard.setString("ls", forType: .string)
        view.paste(nil)
        XCTAssertTrue(view.hasMarkedText())
        XCTAssertEqual(view.markedTextOverlay?.text, "にほんご")
        try await assertBytes(Array("ls".utf8), "the paste only, not the composition")
    }

    func testClearToStartAndZoomDuringACompositionKeepIt() throws {
        try requireSurface()
        typeNihongo()
        view.clearScrollback(nil)
        XCTAssertEqual(view.markedTextOverlay?.text, "にほんご")
        let before = try XCTUnwrap(view.markedTextOverlay).frame
        view.increaseFontSize(nil)
        XCTAssertEqual(view.markedTextOverlay?.text, "にほんご")
        let after = try XCTUnwrap(view.markedTextOverlay).frame
        XCTAssertGreaterThan(after.height, before.height, "the overlay follows the larger cells")
    }

    // MARK: Menu actions: Clear to Start

    func testClearToStartMovesThePromptLineToTheTopAndRedraws() async throws {
        try requireSurface()
        // A session that prints 100 lines, marks the file and waits.
        let marker = file.appendingPathExtension("printed")
        defer { try? FileManager.default.removeItem(at: marker) }
        appDelegate.spawnOptions = .init(
            program: "/bin/sh", arguments: ["-c", #"seq 1 100; : > "$0"; exec cat"#, marker.path])
        let controller = try appDelegate.newWindow()
        let printed = try XCTUnwrap(controller.contentView as? TerminalSurfaceView)
        let other = KeyControlledWindow(contentRect: printed.frame)
        extraWindows.append(other)
        other.contentView = printed
        try XCTSkipUnless(printed.surface.isAttached, "the second view has no surface")
        try await waitUntil("seq has printed") { FileManager.default.fileExists(atPath: marker.path) }
        try await waitUntil("the output has reached the screen") {
            guard let metrics = try? printed.surface.metrics() else { return false }
            return metrics.rows > 1 && metrics.cursorRow == metrics.rows - 1
        }
        // The rest of the output, and its delivery to the view.
        try await Task.sleep(for: .milliseconds(200))
        // Idle: drawn, nothing left to draw, the link paused.
        printed.occlusionOverride = false
        printed.displayLinkDidFire()
        XCTAssertTrue(printed.isDisplayLinkPaused, "idle before ⌘K")
        let renders = printed.renderCount
        XCTAssertTrue(printed.validateMenuItem(menuItem(#selector(TerminalSurfaceView.clearScrollback(_:)))))

        printed.clearScrollback(nil)
        XCTAssertEqual(try printed.surface.metrics().cursorRow, 0, "the prompt line is the top row (#107)")
        XCTAssertFalse(printed.isDisplayLinkPaused, "the core sends no output for it: the view wakes its link")
        printed.displayLinkDidFire()
        XCTAssertEqual(printed.renderCount, renders + 1, "the cleared screen is drawn")
    }

    // MARK: Menu actions: font zoom

    private func fontSize(_ view: TerminalSurfaceView) throws -> Float {
        try view.surface.metrics().fontSizePt
    }

    func testBiggerSmallerAndActualSizeStepTheFontSize() throws {
        try requireSurface()
        let base = try fontSize(view)
        XCTAssertEqual(view.attachedFontSizePt, base, "recorded at attach")
        let cell = try view.surface.metrics().cellHeightPx
        view.occlusionOverride = false
        view.displayLinkDidFire()
        XCTAssertTrue(view.isDisplayLinkPaused)

        view.increaseFontSize(nil)
        XCTAssertEqual(try fontSize(view), base + 1, "1 pt a step (D2)")
        XCTAssertEqual(view.metrics?.fontSizePt, base + 1, "the view's metrics follow at once")
        XCTAssertGreaterThan(try view.surface.metrics().cellHeightPx, cell, "larger cells")
        XCTAssertFalse(view.isDisplayLinkPaused, "a zoom redraws")
        view.displayLinkDidFire()
        view.increaseFontSize(nil)
        XCTAssertEqual(try fontSize(view), base + 2)
        XCTAssertTrue(isEnabled(#selector(TerminalSurfaceView.resetFontSize(_:))))

        view.decreaseFontSize(nil)
        view.decreaseFontSize(nil)
        view.decreaseFontSize(nil)
        XCTAssertEqual(try fontSize(view), base - 1)
        XCTAssertEqual(view.zoomedFontSizePt, base - 1)

        view.displayLinkDidFire()
        view.resetFontSize(nil)
        XCTAssertEqual(try fontSize(view), base, "⌘0: the size the view was attached with")
        XCTAssertEqual(view.metrics?.fontSizePt, base)
        XCTAssertEqual(try view.surface.metrics().cellHeightPx, cell)
        XCTAssertNil(view.zoomedFontSizePt)
        XCTAssertFalse(view.isDisplayLinkPaused)
        XCTAssertFalse(isEnabled(#selector(TerminalSurfaceView.resetFontSize(_:))), "already the actual size")
    }

    func testZoomIsPerView() throws {
        try requireSurface()
        appDelegate.spawnOptions = .init(program: "/bin/cat")
        let controller = try appDelegate.newWindow()
        let second = try XCTUnwrap(controller.contentView as? TerminalSurfaceView)
        let other = KeyControlledWindow(contentRect: second.frame)
        extraWindows.append(other)
        other.contentView = second
        try XCTSkipUnless(second.surface.isAttached, "the second view has no surface")
        let base = try fontSize(second)

        view.increaseFontSize(nil)
        view.increaseFontSize(nil)
        XCTAssertEqual(try fontSize(view), base + 2)
        XCTAssertEqual(try fontSize(second), base, "the other tab keeps its size")
        second.decreaseFontSize(nil)
        XCTAssertEqual(try fontSize(second), base - 1)
        XCTAssertEqual(try fontSize(view), base + 2)
    }

    func testBiggerAndSmallerAreDisabledAtTheCoresBounds() throws {
        try requireSurface()
        let bigger = #selector(TerminalSurfaceView.increaseFontSize(_:))
        let smaller = #selector(TerminalSurfaceView.decreaseFontSize(_:))
        XCTAssertTrue(isEnabled(bigger))
        XCTAssertTrue(isEnabled(smaller))
        XCTAssertFalse(isEnabled(#selector(TerminalSurfaceView.resetFontSize(_:))), "at the actual size")

        // Half a step below the largest size: one step clamps to it.
        try view.surface.setFontSize(511.5)
        view.increaseFontSize(nil)
        XCTAssertEqual(try fontSize(view), 512)
        XCTAssertFalse(isEnabled(bigger), "at the core's largest size")
        XCTAssertTrue(isEnabled(smaller))
        view.increaseFontSize(nil)
        XCTAssertEqual(try fontSize(view), 512)

        try view.surface.setFontSize(4.5)
        view.decreaseFontSize(nil)
        XCTAssertEqual(try fontSize(view), 4)
        XCTAssertFalse(isEnabled(smaller), "at the core's smallest size")
        XCTAssertTrue(isEnabled(bigger))

        // Without a surface there is nothing to zoom.
        view.metrics = nil
        XCTAssertFalse(isEnabled(bigger))
        XCTAssertFalse(isEnabled(smaller))
        XCTAssertFalse(isEnabled(#selector(TerminalSurfaceView.resetFontSize(_:))))
    }

    func testTheZoomSurvivesMovingTheTabToAnotherWindow() throws {
        try requireSurface()
        let base = try fontSize(view)
        view.increaseFontSize(nil)
        view.increaseFontSize(nil)
        // Leaving the window detaches the surface; the new window attaches a
        // new one, which the core starts at the configured size.
        let other = KeyControlledWindow(contentRect: view.frame)
        extraWindows.append(other)
        other.contentView = view
        XCTAssertTrue(view.surface.isAttached)
        XCTAssertEqual(view.attachedFontSizePt, base)
        XCTAssertEqual(try fontSize(view), base + 2, "the zoom is applied again")
        view.resetFontSize(nil)
        XCTAssertEqual(try fontSize(view), base)
    }

    // MARK: Menu actions: zoom and configuration edits (#100)

    /// A configuration edit reaches the view as the core would deliver it:
    /// the core has already set every view to a new `font_size` (here
    /// `newSize`, or no change), then reports the appearance change.
    private func applyConfigurationEdit(fontSize newSize: Float?) throws {
        if let newSize {
            try view.surface.setFontSize(newSize)
        }
        view.applyLayerOpacity(opaque: true)
    }

    func testANewConfiguredFontSizeEndsTheZoom() throws {
        try requireSurface()
        let base = try fontSize(view)
        view.increaseFontSize(nil)
        view.increaseFontSize(nil)
        XCTAssertEqual(view.zoomedFontSizePt, base + 2)
        try applyConfigurationEdit(fontSize: base + 5)
        XCTAssertNil(view.zoomedFontSizePt, "the core set the view to the new size")
        XCTAssertEqual(view.attachedFontSizePt, base + 5, "the new actual size")
        XCTAssertEqual(view.metrics?.fontSizePt, base + 5)
        XCTAssertFalse(isEnabled(#selector(TerminalSurfaceView.resetFontSize(_:))), "already at the actual size")
        view.decreaseFontSize(nil)
        view.resetFontSize(nil)
        XCTAssertEqual(try fontSize(view), base + 5, "⌘0 returns to the new configured size")
    }

    func testAnEditWithoutAFontSizeChangeKeepsTheZoom() throws {
        try requireSurface()
        let base = try fontSize(view)
        view.increaseFontSize(nil)
        try applyConfigurationEdit(fontSize: nil)
        XCTAssertEqual(view.zoomedFontSizePt, base + 1, "colours only: still zoomed")
        XCTAssertEqual(view.attachedFontSizePt, base)
        view.resetFontSize(nil)
        XCTAssertEqual(try fontSize(view), base)
    }

    func testANewConfiguredFontSizeBecomesTheActualSizeOfAnUnzoomedView() throws {
        try requireSurface()
        let base = try fontSize(view)
        try applyConfigurationEdit(fontSize: base + 3)
        XCTAssertNil(view.zoomedFontSizePt)
        XCTAssertEqual(view.attachedFontSizePt, base + 3)
        XCTAssertEqual(view.metrics?.fontSizePt, base + 3, "the view's metrics follow")
        view.increaseFontSize(nil)
        XCTAssertEqual(view.zoomedFontSizePt, base + 4)
        view.resetFontSize(nil)
        XCTAssertEqual(try fontSize(view), base + 3)
    }

    func testANewConfiguredSizeEqualToTheZoomCannotBeTold() throws {
        // The documented edge: the size alone cannot show the edit, so the
        // view stays zoomed and ⌘0 returns to the old configured size.
        try requireSurface()
        let base = try fontSize(view)
        view.increaseFontSize(nil)
        try applyConfigurationEdit(fontSize: base + 1)
        XCTAssertEqual(view.zoomedFontSizePt, base + 1)
        XCTAssertEqual(view.attachedFontSizePt, base)
    }

    // MARK: Helpers

    /// Windows a test made besides ``window``; closed in tear-down.
    private var extraWindows: [KeyControlledWindow] = []

    /// Makes the view's window key or not, and tells the view as AppKit's
    /// `didBecomeKey`/`didResignKey` notifications would.
    private func setKey(_ isKey: Bool) {
        window.isKeyForTest = isKey
        view.windowKeyChanged(isKey: isKey)
    }

    /// A key-down event: `characters` with `ignoring` as the characters
    /// without modifiers (the same by default).
    private func key(
        _ characters: String, ignoring: String? = nil, code: UInt16, _ flags: NSEvent.ModifierFlags = []
    ) -> NSEvent {
        NSEvent.keyEvent(
            with: .keyDown, location: .zero, modifierFlags: flags, timestamp: 0,
            windowNumber: window.windowNumber, context: nil, characters: characters,
            charactersIgnoringModifiers: ignoring ?? characters, isARepeat: false, keyCode: code)!
    }

    /// What the current keyboard layout types for `keyCode` with `flags`.
    static func layoutCharacters(keyCode: UInt16, _ flags: NSEvent.ModifierFlags) -> String {
        guard let source = TISCopyCurrentKeyboardLayoutInputSource()?.takeRetainedValue(),
            let pointer = TISGetInputSourceProperty(source, kTISPropertyUnicodeKeyLayoutData)
        else { return "" }
        let data = Unmanaged<CFData>.fromOpaque(pointer).takeUnretainedValue() as Data
        var modifiers: UInt32 = 0
        if flags.contains(.shift) { modifiers |= UInt32(shiftKey >> 8) }
        if flags.contains(.option) { modifiers |= UInt32(optionKey >> 8) }
        var deadKeys: UInt32 = 0
        var characters = [UniChar](repeating: 0, count: 8)
        var length = 0
        data.withUnsafeBytes { raw in
            guard let layout = raw.bindMemory(to: UCKeyboardLayout.self).baseAddress else { return }
            _ = UCKeyTranslate(
                layout, keyCode, UInt16(kUCKeyActionDown), modifiers, UInt32(LMGetKbdType()), 0, &deadKeys,
                characters.count, &length, &characters)
        }
        return String(utf16CodeUnits: characters, count: length)
    }

    /// Presses `event`, with `inputSystem` making the calls AppKit's input
    /// system would make for it.
    private func press(_ event: NSEvent, inputSystem: (TerminalSurfaceView) -> Void) {
        view.processKeyDown(event) { inputSystem(view) }
    }

    private func deadAcute() {
        press(key("", ignoring: "e", code: 14, [.option])) {
            $0.setMarkedText("´", selectedRange: NSRange(location: 1, length: 0), replacementRange: Self.notFound)
        }
    }

    /// Asserts that exactly `expected` reached the PTY: sends a sentinel key
    /// and compares everything before it.
    private func assertBytes(
        _ expected: [UInt8], _ message: String = "", file: StaticString = #filePath, line: UInt = #line
    ) async throws {
        try view.session.send(key: .init(key: .char, codepoint: "~", text: "~"))
        var received: [UInt8] = []
        try await waitUntil("the sentinel arrives") {
            received = (try? Array(Data(contentsOf: self.file))) ?? []
            return received.last == UInt8(ascii: "~")
        }
        // The recorder's own "R" is output, not input: the file holds input only.
        let hex = { (bytes: [UInt8]) in bytes.map { String(format: "%02x", $0) }.joined(separator: " ") }
        XCTAssertEqual(hex(Array(received.dropLast())), hex(expected), message, file: file, line: line)
    }

    /// Polls `condition` on the main actor until it holds or `timeout`
    /// passes.
    private func waitUntil(
        _ description: String, timeout: Duration = .seconds(5), _ condition: () -> Bool
    ) async throws {
        let deadline = ContinuousClock.now + timeout
        while !condition() {
            guard ContinuousClock.now < deadline else {
                XCTFail("timed out after \(timeout): \(description)")
                return
            }
            try await Task.sleep(for: .milliseconds(10))
        }
    }
}

/// A window whose key state the test sets. It is never ordered on screen and
/// cannot become key for real, so AppKit never changes its state or sends it
/// key notifications, whether or not the machine's session is locked.
@MainActor
final class KeyControlledWindow: NSWindow {
    var isKeyForTest = false

    init(contentRect: NSRect) {
        super.init(contentRect: contentRect, styleMask: [.titled, .resizable], backing: .buffered, defer: false)
        isReleasedWhenClosed = false
    }

    override var isKeyWindow: Bool { isKeyForTest }
    override var canBecomeKey: Bool { false }
    override var canBecomeMain: Bool { false }
}
