import AppKit
import TesseraKit
import XCTest

@testable import Tessera

/// Records each confirmation and answers when the test says.
@MainActor
private final class RecordingPastePresenter: PasteConfirmationPresenter {
    var confirmations: [PasteConfirmation] = []
    var windows: [NSWindow] = []
    private var completions: [@MainActor (Bool) -> Void] = []
    var dismissals = 0

    func confirm(
        _ confirmation: PasteConfirmation, sheetFor window: NSWindow,
        completion: @escaping @MainActor (Bool) -> Void
    ) {
        confirmations.append(confirmation)
        windows.append(window)
        completions.append(completion)
    }

    func dismiss() {
        dismissals += 1
        answer(false)
    }

    /// Answers the oldest open confirmation.
    func answer(_ paste: Bool) {
        guard !completions.isEmpty else { return }
        completions.removeFirst()(paste)
    }
}

/// The paste guard (#471, ADR 0022 D-PASTE): the core decides from
/// `[terminal] paste_guard` and the program's bracketed-paste mode whether
/// a paste asks first, and makes the escaped, bounded preview the sheet
/// shows. These tests open tabs as the app does (`newWindow()`), with real
/// children that turn DECSET 2004 on or leave it off.
@MainActor
final class PasteGuardTests: XCTestCase {
    private var appDelegate: AppDelegate!
    private var previous: String?
    private var dir: URL?
    private var pasteboard: NSPasteboard!

    override func setUp() async throws {
        appDelegate = try XCTUnwrap(NSApp.delegate as? AppDelegate)
        previous = ProcessInfo.processInfo.environment["TESSERA_CONFIG_DIR"]
        XCTAssertTrue(appDelegate.windowControllers.isEmpty, "a previous test left windows open")
        // Never the user's clipboard.
        pasteboard = NSPasteboard(name: NSPasteboard.Name("com.aanyberg.tessera.tests.paste.\(UUID().uuidString)"))
    }

    override func tearDown() async throws {
        for controller in appDelegate.windowControllers {
            controller.window?.close()
        }
        XCTAssertTrue(appDelegate.windowControllers.isEmpty)
        appDelegate.spawnOptions = .init()
        if dir != nil {
            try appDelegate.replaceCore(start: false)
        }
        if let previous { setenv("TESSERA_CONFIG_DIR", previous, 1) } else { unsetenv("TESSERA_CONFIG_DIR") }
        if let dir { try? FileManager.default.removeItem(at: dir) }
        pasteboard.releaseGlobally()
        pasteboard = nil
        appDelegate = nil
    }

    // MARK: Helpers

    private func waitUntil(_ what: String, seconds: Int = 10, _ condition: () throws -> Bool) async throws {
        let deadline = ContinuousClock.now + .seconds(seconds)
        while try !condition() {
            guard ContinuousClock.now < deadline else { return XCTFail("timed out: \(what)") }
            try await Task.sleep(for: .milliseconds(10))
        }
    }

    /// Gives the app a core of its own on `config`.
    private func startCore(config: String) throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-paste-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        self.dir = dir
        try config.write(to: dir.appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
        setenv("TESSERA_CONFIG_DIR", dir.path, 1)
        try appDelegate.replaceCore(start: true)
    }

    /// A tab whose child turns bracketed paste on (or not), says `ready`,
    /// then echoes what it reads; its view pastes from the test's
    /// pasteboard and asks through `presenter`.
    private func openTab(
        bracketed: Bool, presenter: (any PasteConfirmationPresenter)? = nil
    ) async throws -> (TerminalWindowController, TerminalSurfaceView) {
        let mode = bracketed ? #"printf '\033[?2004h'; "# : ""
        return try await openTab(script: mode + "echo ready; exec cat", presenter: presenter)
    }

    /// A tab whose child runs `script`, which says `ready` when it waits.
    private func openTab(
        script: String, presenter: (any PasteConfirmationPresenter)? = nil
    ) async throws -> (TerminalWindowController, TerminalSurfaceView) {
        appDelegate.spawnOptions = .init(program: "/bin/sh", arguments: ["-c", script])
        let controller = try appDelegate.newWindow()
        let view = controller.focusedPane.view
        view.pasteboard = pasteboard
        if let presenter { view.pasteConfirmationPresenter = presenter }
        try await waitUntil("the child is ready") { try screen(of: view).contains("ready") }
        return (controller, view)
    }

    private func screen(of view: TerminalSurfaceView) throws -> String {
        try view.session.accessibleText().string
    }

    private func paste(_ text: String, into view: TerminalSurfaceView) {
        pasteboard.clearContents()
        pasteboard.setString(text, forType: .string)
        view.paste(nil)
    }

    // MARK: The model

    func testTheQuestionCountsLinesAndSaysWhyItAsks() {
        func confirmation(_ text: String, bracketed: Bool) -> PasteConfirmation {
            PasteConfirmation(preview: TesseraSession.pastePreview(text), bracketed: bracketed)
        }
        let lines = confirmation("make\nmake install\n", bracketed: false)
        XCTAssertEqual(lines.messageText, "Paste 2 lines?")
        XCTAssertTrue(lines.informativeText.contains("bracketed paste"), lines.informativeText)
        XCTAssertFalse(lines.informativeText.contains("control"), lines.informativeText)
        let controls = confirmation("rm \u{1b}[201~x", bracketed: true)
        XCTAssertEqual(controls.messageText, "Paste 1 line?")
        XCTAssertTrue(controls.informativeText.contains("control characters"), controls.informativeText)
        XCTAssertFalse(controls.informativeText.contains("bracketed"), "the program has it on")
        XCTAssertEqual(controls.shownText, "rm ^[[201~x", "escaped by the core")
        XCTAssertEqual(confirmation("ls", bracketed: true).informativeText, "Check the text before it is sent to the program.")

        // A Paste answered still holds when it names every risk there is now
        // (#748).
        let text = "echo a\u{1b}\necho b\n"
        let on = confirmation(text, bracketed: true)
        let off = confirmation(text, bracketed: false)
        XCTAssertEqual(on.risks, [.controls])
        XCTAssertEqual(off.risks, [.lineBreaksRun, .controls])
        XCTAssertFalse(on.covers(off), "bracketed paste went off: the lines now run")
        XCTAssertTrue(off.covers(on), "bracketed paste came on: one risk fewer")
        XCTAssertTrue(off.covers(off))
        let always = confirmation("echo a\necho b\n", bracketed: true)
        XCTAssertEqual(always.risks, [], "only `always` asks")
        XCTAssertFalse(always.covers(confirmation("echo a\necho b\n", bracketed: false)))
    }

    func testAMegabytePasteShowsItsFirstLinesAndCountsTheRest() {
        // 1 MiB in 20 000 lines, one of them 5 000 characters long.
        var text = String(repeating: "x", count: 5_000) + "\n"
        text += String(repeating: String(repeating: "y", count: 51) + "\n", count: 19_999)
        let start = ContinuousClock.now
        let confirmation = PasteConfirmation(preview: TesseraSession.pastePreview(text), bracketed: false)
        let elapsed = ContinuousClock.now - start
        XCTAssertEqual(confirmation.preview.lines, 20_000)
        XCTAssertEqual(confirmation.preview.moreLines, 20_000 - TesseraSession.PastePreview.maxLines)
        XCTAssertEqual(confirmation.messageText, "Paste \(20_000.formatted()) lines?")
        let shown = confirmation.shownText.components(separatedBy: "\n")
        XCTAssertEqual(shown.count, TesseraSession.PastePreview.maxLines + 1)
        XCTAssertEqual(shown.last, "… \(19_980.formatted()) more lines")
        XCTAssertEqual(shown.first?.count, 201, "200 characters and an ellipsis")
        XCTAssertLessThanOrEqual(confirmation.preview.text.utf8.count, TesseraSession.PastePreview.maxBytes)
        XCTAssertLessThan(elapsed, .seconds(1), "one pass over the paste (\(elapsed))")
        // The sheet holds only the preview.
        let sheet = PasteConfirmationSheet(confirmation)
        XCTAssertEqual(sheet.previewView.string, confirmation.shownText)
    }

    // MARK: Asking, as the app pastes

    func testAMultiLinePasteAsksWithoutBracketedPasteAndSendsOnlyOnPaste() async throws {
        let presenter = RecordingPastePresenter()
        let (controller, view) = try await openTab(bracketed: false, presenter: presenter)
        paste("echo one\necho two\n", into: view)
        XCTAssertEqual(presenter.confirmations.count, 1, "asks: each line would run")
        XCTAssertIdentical(presenter.windows.first, controller.window, "on the tab's own window")
        XCTAssertEqual(presenter.confirmations.first?.preview.lines, 2)
        XCTAssertFalse(try XCTUnwrap(presenter.confirmations.first).bracketed)
        XCTAssertEqual(view.pasteCount, 0, "nothing sent before the answer")
        XCTAssertTrue(view.isConfirmingPaste)

        // A second paste while asking is refused, not queued.
        paste("echo three\necho four\n", into: view)
        XCTAssertEqual(presenter.confirmations.count, 1)

        presenter.answer(false)
        XCTAssertFalse(view.isConfirmingPaste)
        XCTAssertEqual(view.pasteCount, 0, "Cancel sends nothing")
        try await Task.sleep(for: .milliseconds(200))
        XCTAssertFalse(try screen(of: view).contains("echo one"))

        paste("echo one\necho two\n", into: view)
        presenter.answer(true)
        XCTAssertEqual(view.pasteCount, 1)
        try await waitUntil("cat echoes the paste") { try screen(of: view).contains("echo two") }
    }

    func testTheSamePasteWithBracketedPasteGoesStraightThrough() async throws {
        let presenter = RecordingPastePresenter()
        let (_, view) = try await openTab(bracketed: true, presenter: presenter)
        paste("echo one\necho two\n", into: view)
        XCTAssertEqual(presenter.confirmations.count, 0, "the program takes the lines as one paste")
        XCTAssertEqual(view.pasteCount, 1)
        try await waitUntil("cat echoes the paste") { try screen(of: view).contains("echo two") }
        // A one-line paste without controls never asks either.
        paste("ls", into: view)
        XCTAssertEqual(presenter.confirmations.count, 0)
        XCTAssertEqual(view.pasteCount, 2)
    }

    func testControlCharactersAskEvenWithBracketedPasteAndShowEscaped() async throws {
        let presenter = RecordingPastePresenter()
        let (_, view) = try await openTab(bracketed: true, presenter: presenter)
        paste("safe\u{1b}[201~rm -rf ~\u{7}\u{9b}", into: view)
        let confirmation = try XCTUnwrap(presenter.confirmations.first, "controls ask")
        XCTAssertTrue(confirmation.preview.hasControls)
        XCTAssertEqual(confirmation.shownText, "safe^[[201~rm -rf ~^G<U+009B>")
        XCTAssertFalse(
            confirmation.shownText.unicodeScalars.contains { $0.properties.generalCategory == .control },
            "nothing the sheet shows is a control (S3)")
    }

    func testTheGuardFollowsTheConfiguration() async throws {
        try startCore(config: "[terminal]\npaste_guard = \"always\"\n")
        let presenter = RecordingPastePresenter()
        let (_, view) = try await openTab(bracketed: true, presenter: presenter)
        paste("ls", into: view)
        XCTAssertEqual(presenter.confirmations.count, 1, "always asks, even one plain line")
        presenter.answer(false)
        for controller in appDelegate.windowControllers { controller.window?.close() }

        try startCore(config: "[terminal]\npaste_guard = \"off\"\n")
        let (_, offView) = try await openTab(bracketed: false, presenter: presenter)
        paste("echo one\necho two\u{1b}\n", into: offView)
        XCTAssertEqual(presenter.confirmations.count, 1, "off never asks")
        XCTAssertEqual(offView.pasteCount, 1)
    }

    // MARK: The program changes its mode while the sheet is up (#748)

    /// Lets the child's `read` go on: the paste is not sent yet, so the
    /// line the test types is all it reads.
    private func proceed(_ view: TerminalSurfaceView, until marker: String) async throws {
        try view.session.write(Array("go\r".utf8))
        try await waitUntil("the child says \(marker)") { try screen(of: view).contains(marker) }
    }

    /// The issue's sequence: the sheet opens while bracketed paste is on and
    /// names only the controls; the program turns it off; Paste must not
    /// send lines that now run without saying so.
    func testAPasteConfirmedAfterBracketedPasteWentOffAsksAgainWithTheLineBreaks() async throws {
        let presenter = RecordingPastePresenter()
        let script = #"printf '\033[?2004h'; echo ready; read go; printf '\033[?2004l'; stty -echo; echo mode-off; exec cat -v"#
        let (_, view) = try await openTab(script: script, presenter: presenter)
        paste("echo a\u{1b}\necho b\n", into: view)
        let first = try XCTUnwrap(presenter.confirmations.first, "controls ask")
        XCTAssertTrue(first.bracketed)
        XCTAssertEqual(first.risks, [.controls])
        XCTAssertFalse(first.informativeText.contains("bracketed paste"), first.informativeText)

        try await proceed(view, until: "mode-off")
        presenter.answer(true)
        XCTAssertEqual(view.pasteCount, 0, "not sent on reasons that no longer hold")
        XCTAssertTrue(view.isConfirmingPaste)
        // On the next turn, once the answered sheet has ended.
        try await waitUntil("asks again") { presenter.confirmations.count == 2 }
        XCTAssertEqual(view.pasteCount, 0)
        XCTAssertTrue(view.isConfirmingPaste)
        let second = try XCTUnwrap(presenter.confirmations.last)
        XCTAssertFalse(second.bracketed)
        XCTAssertEqual(second.risks, [.lineBreaksRun, .controls])
        XCTAssertTrue(second.informativeText.contains("bracketed paste"), second.informativeText)
        XCTAssertEqual(second.preview, first.preview, "the same text")

        // The second Paste names what holds: the lines go, unbracketed.
        presenter.answer(true)
        XCTAssertEqual(view.pasteCount, 1)
        XCTAssertFalse(view.isConfirmingPaste)
        XCTAssertEqual(presenter.confirmations.count, 2)
        try await waitUntil("cat shows the paste") { try screen(of: view).contains("echo b") }
        XCTAssertFalse(try screen(of: view).contains("[200~"), "bracketed paste is off")
    }

    func testAPasteConfirmedAfterBracketedPasteCameOnGoesBracketedWithoutAskingAgain() async throws {
        let presenter = RecordingPastePresenter()
        let script = #"echo ready; read go; printf '\033[?2004h'; stty -echo; echo mode-on; exec cat -v"#
        let (_, view) = try await openTab(script: script, presenter: presenter)
        // The controls still ask once the mode is on, so the check asks
        // again; the answered question named them already.
        paste("echo a\u{1b}\necho b\n", into: view)
        let first = try XCTUnwrap(presenter.confirmations.first, "the lines would run")
        XCTAssertEqual(first.risks, [.lineBreaksRun, .controls])

        try await proceed(view, until: "mode-on")
        presenter.answer(true)
        XCTAssertEqual(presenter.confirmations.count, 1, "a risk gone needs no second answer")
        XCTAssertEqual(view.pasteCount, 1)
        XCTAssertFalse(view.isConfirmingPaste)
        try await waitUntil("cat shows the paste, bracketed") { try screen(of: view).contains("^[[200~echo a") }
    }

    /// The same with the real sheet, opened as the app opens it: Paste
    /// ends the first sheet and a second one says why it asks now.
    func testTheRealSheetAsksAgainWhenBracketedPasteWentOff() async throws {
        let script = #"printf '\033[?2004h'; echo ready; read go; printf '\033[?2004l'; stty -echo; echo mode-off; exec cat -v"#
        let (controller, view) = try await openTab(script: script)
        let window = try XCTUnwrap(controller.window)
        paste("echo a\u{1b}\necho b\n", into: view)
        let first = try XCTUnwrap(window.attachedSheet as? PasteConfirmationSheet, "the sheet is up")
        XCTAssertFalse(first.informativeLabel.stringValue.contains("bracketed paste"))

        try await proceed(view, until: "mode-off")
        first.pasteButton.performClick(nil)
        try await waitUntil("a second sheet") {
            (window.attachedSheet as? PasteConfirmationSheet).map { $0 !== first } ?? false
        }
        let second = try XCTUnwrap(window.attachedSheet as? PasteConfirmationSheet)
        XCTAssertTrue(second.informativeLabel.stringValue.contains("bracketed paste"), second.informativeLabel.stringValue)
        XCTAssertIdentical(second.defaultButtonCell, second.cancelButton.cell, "Cancel is the default again")
        XCTAssertEqual(view.pasteCount, 0)
        XCTAssertTrue(view.isConfirmingPaste)

        second.pasteButton.performClick(nil)
        try await waitUntil("Paste ends the sheet and sends") { window.attachedSheet == nil && view.pasteCount == 1 }
        XCTAssertFalse(view.isConfirmingPaste)
        try await waitUntil("cat shows the paste") { try screen(of: view).contains("echo b") }
    }

    /// The second sheet answers Return as Cancel, like the first: it
    /// opens while the first one ends, and AppKit clears a key equivalent
    /// set then.
    func testReturnOnTheSecondSheetIsCancel() async throws {
        let script = #"printf '\033[?2004h'; echo ready; read go; printf '\033[?2004l'; stty -echo; echo mode-off; exec cat -v"#
        let (controller, view) = try await openTab(script: script)
        let window = try XCTUnwrap(controller.window)
        paste("echo a\u{1b}\necho b\n", into: view)
        let first = try XCTUnwrap(window.attachedSheet as? PasteConfirmationSheet, "the sheet is up")
        try await proceed(view, until: "mode-off")
        first.pasteButton.performClick(nil)
        try await waitUntil("a second sheet") {
            (window.attachedSheet as? PasteConfirmationSheet).map { $0 !== first } ?? false
        }
        let second = try XCTUnwrap(window.attachedSheet as? PasteConfirmationSheet)
        let returnKey = try XCTUnwrap(
            NSEvent.keyEvent(
                with: .keyDown, location: .zero, modifierFlags: [], timestamp: 0, windowNumber: second.windowNumber,
                context: nil, characters: "\r", charactersIgnoringModifiers: "\r", isARepeat: false, keyCode: 36))
        second.sendEvent(returnKey)
        try await waitUntil("Return ends the second sheet") { window.attachedSheet == nil && !view.isConfirmingPaste }
        XCTAssertEqual(view.pasteCount, 0, "Return is Cancel")
        XCTAssertFalse(view.isConfirmingPaste)
    }

    // MARK: The real sheet

    func testTheSheetDefaultsToCancelAndEscapeAndCommandPeriodCancel() async throws {
        let (controller, view) = try await openTab(bracketed: false)
        let window = try XCTUnwrap(controller.window)
        paste("echo one\necho \u{1b}two\n", into: view)
        let sheet = try XCTUnwrap(window.attachedSheet as? PasteConfirmationSheet, "the sheet is up")
        XCTAssertIdentical(sheet.defaultButtonCell, sheet.cancelButton.cell, "the default button is Cancel")
        XCTAssertEqual(sheet.pasteButton.keyEquivalent, "", "Paste takes a click")
        XCTAssertEqual(sheet.messageLabel.stringValue, "Paste 2 lines?")
        XCTAssertEqual(sheet.previewView.string, "echo one\necho ^[two")
        XCTAssertFalse(sheet.previewView.isEditable)

        // Return, as a key the sheet's window gets.
        let returnKey = try XCTUnwrap(
            NSEvent.keyEvent(
                with: .keyDown, location: .zero, modifierFlags: [], timestamp: 0, windowNumber: sheet.windowNumber,
                context: nil, characters: "\r", charactersIgnoringModifiers: "\r", isARepeat: false, keyCode: 36))
        XCTAssertTrue(sheet.performKeyEquivalent(with: returnKey), "Return presses a button")
        try await waitUntil("Return ends the sheet") { window.attachedSheet == nil && !view.isConfirmingPaste }
        XCTAssertEqual(view.pasteCount, 0, "Return is Cancel")

        // Escape and ⌘., as keys the sheet gets, with the focus it starts
        // with (Cancel, or the sheet itself when buttons take no focus) and
        // with the preview focused.
        for focusPreview in [false, true] {
            for (characters, modifiers, keyCode) in [("\u{1b}", NSEvent.ModifierFlags(), UInt16(53)), (".", .command, 47)] {
                let what = "\(keyCode == 53 ? "Escape" : "⌘.") with the \(focusPreview ? "preview" : "starting") focus"
                paste("echo one\necho two\n", into: view)
                let escaping = try XCTUnwrap(window.attachedSheet as? PasteConfirmationSheet, what)
                if focusPreview {
                    XCTAssertTrue(escaping.makeFirstResponder(escaping.previewView), what)
                } else {
                    XCTAssertFalse(escaping.firstResponder === escaping.previewView, what)
                }
                let key = try XCTUnwrap(
                    NSEvent.keyEvent(
                        with: .keyDown, location: .zero, modifierFlags: modifiers, timestamp: 0,
                        windowNumber: escaping.windowNumber, context: nil, characters: characters,
                        charactersIgnoringModifiers: characters, isARepeat: false, keyCode: keyCode))
                escaping.sendEvent(key)
                try await waitUntil("\(what) ends the sheet") { window.attachedSheet == nil && !view.isConfirmingPaste }
                XCTAssertEqual(view.pasteCount, 0, what)
            }
        }
        // The application's key equivalent pass reaches the sheet first.
        paste("echo one\necho two\n", into: view)
        let equivalent = try XCTUnwrap(window.attachedSheet as? PasteConfirmationSheet)
        let commandPeriod = try XCTUnwrap(
            NSEvent.keyEvent(
                with: .keyDown, location: .zero, modifierFlags: .command, timestamp: 0,
                windowNumber: equivalent.windowNumber, context: nil, characters: ".", charactersIgnoringModifiers: ".",
                isARepeat: false, keyCode: 47))
        XCTAssertTrue(equivalent.performKeyEquivalent(with: commandPeriod), "⌘. is the sheet's")
        try await waitUntil("⌘. ends the sheet") { window.attachedSheet == nil && !view.isConfirmingPaste }
        XCTAssertEqual(view.pasteCount, 0)

        paste("echo one\necho two\n", into: view)
        let again = try XCTUnwrap(window.attachedSheet as? PasteConfirmationSheet)
        again.pasteButton.performClick(nil)
        try await waitUntil("Paste ends the sheet and sends") { window.attachedSheet == nil && view.pasteCount == 1 }
        try await waitUntil("cat echoes the paste") { try screen(of: view).contains("echo two") }
    }

    func testThePreviewKeepsTheOrderOfRightToLeftText() async throws {
        let (controller, view) = try await openTab(bracketed: false)
        let window = try XCTUnwrap(controller.window)
        let text = "\u{05D0} ; rm -rf ~ ; \u{05D1} # echo hi"
        paste(text + "\n", into: view)
        let sheet = try XCTUnwrap(window.attachedSheet as? PasteConfirmationSheet, "the sheet is up")
        let preview = sheet.previewView
        XCTAssertEqual(preview.string, text)
        let storage = try XCTUnwrap(preview.textStorage)
        storage.enumerateAttribute(.paragraphStyle, in: NSRange(location: 0, length: storage.length)) { style, range, _ in
            let style = style as? NSParagraphStyle
            XCTAssertEqual(style?.baseWritingDirection, .leftToRight, "\(range)")
            XCTAssertEqual(style?.alignment, .left, "\(range)")
        }
        // Laid out, the command comes before the comment.
        let layout = try XCTUnwrap(preview.layoutManager)
        let container = try XCTUnwrap(preview.textContainer)
        layout.ensureLayout(for: container)
        func x(of part: String) -> CGFloat {
            let range = (preview.string as NSString).range(of: part)
            let glyphs = layout.glyphRange(forCharacterRange: range, actualCharacterRange: nil)
            return layout.boundingRect(forGlyphRange: glyphs, in: container).minX
        }
        XCTAssertLessThan(x(of: "rm"), x(of: "#"), "rm -rf shows before the #")
        XCTAssertLessThan(x(of: "\u{05D0}"), x(of: "rm"), "the line reads left to right")
        sheet.cancelOperation(nil)
        try await waitUntil("the sheet ends") { window.attachedSheet == nil && !view.isConfirmingPaste }
        XCTAssertEqual(view.pasteCount, 0)
    }

    func testClosingATabUnderItsPasteSheetFreesTheSheetTheTabAndTheSession() async throws {
        weak var weakController: TerminalWindowController?
        weak var weakView: TerminalSurfaceView?
        weak var weakSession: TesseraSession?
        weak var weakSheet: NSWindow?
        func openAndAsk() async throws {
            let (controller, view) = try await openTab(bracketed: false)
            weakController = controller
            weakView = view
            weakSession = view.session
            autoreleasepool {
                paste("echo one\necho two\n", into: view)
                weakSheet = controller.window?.attachedSheet
            }
        }
        try await openAndAsk()
        XCTAssertNotNil(weakSheet, "the paste asks on a sheet")
        autoreleasepool { weakController?.window?.close() }
        try? await waitUntil("the tab, its view, session and sheet are freed") {
            weakController == nil && weakView == nil && weakSession == nil && weakSheet == nil
        }
        XCTAssertNil(weakController, "controller")
        XCTAssertNil(weakView, "view")
        XCTAssertNil(weakSession, "session")
        XCTAssertNil(weakSheet, "sheet")
    }

    func testAPaneLeavingItsWindowEndsItsQuestion() async throws {
        let presenter = RecordingPastePresenter()
        let (_, view) = try await openTab(bracketed: false, presenter: presenter)
        paste("a\nb", into: view)
        XCTAssertTrue(view.isConfirmingPaste)
        view.removeFromSuperview()
        XCTAssertEqual(presenter.dismissals, 1)
        XCTAssertFalse(view.isConfirmingPaste)
        XCTAssertEqual(view.pasteCount, 0)
    }
}
