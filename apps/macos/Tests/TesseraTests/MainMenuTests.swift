import AppKit
import XCTest

@testable import Tessera

/// The menu bar the app installs at launch: every item's title, key
/// equivalent, modifiers and action, ⌘= reaching Bigger through its hidden
/// item, and the application items (Settings…, Tessera Help) without
/// starting an editor or a browser. The terminal items' effects are tested
/// on the view in ``TerminalSurfaceViewTests``.
@MainActor
final class MainMenuTests: XCTestCase {
    private var appDelegate: AppDelegate!
    private var opener: RecordingOpener!
    /// What the stand-in for the core's `createConfig()` returns; nothing
    /// is written there.
    private let configFile = URL(fileURLWithPath: "/nonexistent/tessera-tests/config.toml")
    /// What happened, in order: "create" and "open <path>".
    private var events: [String] = []

    override func setUp() async throws {
        appDelegate = try XCTUnwrap(NSApp.delegate as? AppDelegate)
        opener = RecordingOpener()
        appDelegate.opener = opener
        events = []
        // Never the core's createConfig(): it would create the configuration
        // directory and start watching it.
        appDelegate.configFileCreator = { [unowned self] in
            self.events.append("create")
            return self.configFile
        }
        opener.onOpen = { [unowned self] file in self.events.append("open \(file.path)") }
    }

    override func tearDown() async throws {
        appDelegate.opener = WorkspaceOpener()
        appDelegate.configFileCreator = nil
        appDelegate = nil
        opener = nil
    }

    // MARK: Layout

    /// One menu item as the test compares it: separators are "-".
    private struct Item: Equatable, CustomStringConvertible {
        var title: String
        var key = ""
        var modifiers: NSEvent.ModifierFlags = []
        var action: Selector?
        var hidden = false

        static let separator = Item(title: "-")

        init(title: String, key: String = "", modifiers: NSEvent.ModifierFlags = [], action: Selector? = nil, hidden: Bool = false) {
            self.title = title
            self.key = key
            self.modifiers = key.isEmpty ? [] : modifiers
            self.action = action
            self.hidden = hidden
        }

        init(_ item: NSMenuItem) {
            if item.isSeparatorItem {
                self = .separator
                return
            }
            let modifiers = item.keyEquivalentModifierMask.intersection(.deviceIndependentFlagsMask)
            // A hidden item counts only if its key equivalent works (D3).
            let hidden = item.isHidden && item.allowsKeyEquivalentWhenHidden
            self.init(title: item.title, key: item.keyEquivalent, modifiers: modifiers, action: item.action, hidden: hidden)
            if item.isHidden && !hidden {
                self.title += " (hidden, key equivalent off)"
            }
        }

        var description: String {
            let action = action.map(NSStringFromSelector) ?? "nil"
            return "\(title) [\(key) \(modifiers.rawValue)] \(action)\(hidden ? " hidden" : "")"
        }
    }

    private static let command: NSEvent.ModifierFlags = [.command]

    private static let tesseraItems: [Item] = [
        Item(title: "About Tessera", action: #selector(NSApplication.orderFrontStandardAboutPanel(_:))),
        .separator,
        Item(title: "Settings…", key: ",", modifiers: command, action: #selector(AppDelegate.openConfigFile(_:))),
        .separator,
        Item(title: "Hide Tessera", key: "h", modifiers: command, action: #selector(NSApplication.hide(_:))),
        Item(
            title: "Hide Others", key: "h", modifiers: [.command, .option],
            action: #selector(NSApplication.hideOtherApplications(_:))),
        Item(title: "Show All", action: #selector(NSApplication.unhideAllApplications(_:))),
        .separator,
        Item(title: "Quit Tessera", key: "q", modifiers: command, action: #selector(NSApplication.terminate(_:))),
    ]

    private static let shellItems: [Item] = [
        Item(title: "New Window", key: "n", modifiers: command, action: #selector(AppDelegate.openNewWindow(_:))),
        Item(title: "New Tab", key: "t", modifiers: command, action: #selector(AppDelegate.openNewTab(_:))),
        .separator,
        Item(title: "Close Tab", key: "w", modifiers: command, action: #selector(NSWindow.performClose(_:))),
        Item(
            title: "Close Window", key: "w", modifiers: [.command, .shift],
            action: #selector(TerminalWindowController.closeTabGroup(_:))),
        .separator,
        Item(
            title: "Clear to Start", key: "k", modifiers: command,
            action: #selector(TerminalSurfaceView.clearScrollback(_:))),
    ]

    private static let editItems: [Item] = [
        Item(title: "Copy", key: "c", modifiers: command, action: #selector(TerminalSurfaceView.copy(_:))),
        Item(title: "Paste", key: "v", modifiers: command, action: #selector(TerminalSurfaceView.paste(_:))),
        Item(
            title: "Paste as Plain Text", key: "v", modifiers: [.command, .option, .shift],
            action: #selector(TerminalSurfaceView.pasteAsPlainText(_:))),
        Item(title: "Select All", key: "a", modifiers: command, action: #selector(TerminalSurfaceView.selectAll(_:))),
    ]

    private static let viewItems: [Item] = [
        Item(title: "Bigger", key: "+", modifiers: command, action: #selector(TerminalSurfaceView.increaseFontSize(_:))),
        Item(
            title: "Bigger", key: "=", modifiers: command,
            action: #selector(TerminalSurfaceView.increaseFontSize(_:)), hidden: true),
        Item(title: "Smaller", key: "-", modifiers: command, action: #selector(TerminalSurfaceView.decreaseFontSize(_:))),
        Item(title: "Actual Size", key: "0", modifiers: command, action: #selector(TerminalSurfaceView.resetFontSize(_:))),
        .separator,
        Item(title: "Show Tab Bar", action: #selector(NSWindow.toggleTabBar(_:))),
    ]

    private static let windowItems: [Item] = [
        Item(title: "Minimize", key: "m", modifiers: command, action: #selector(NSWindow.performMiniaturize(_:))),
        Item(title: "Zoom", action: #selector(NSWindow.performZoom(_:))),
        .separator,
        Item(title: "Show Previous Tab", key: "{", modifiers: command, action: #selector(NSWindow.selectPreviousTab(_:))),
        Item(title: "Show Next Tab", key: "}", modifiers: command, action: #selector(NSWindow.selectNextTab(_:))),
        Item(title: "Move Tab to New Window", action: #selector(NSWindow.moveTabToNewWindow(_:))),
        Item(title: "Merge All Windows", action: #selector(NSWindow.mergeAllWindows(_:))),
        Item(title: "Select Tab", action: #selector(NSMenu.submenuAction(_:))),
        .separator,
        Item(title: "Bring All to Front", action: #selector(NSApplication.arrangeInFront(_:))),
    ]

    private static let helpItems: [Item] = [
        Item(title: "Tessera Help", key: "?", modifiers: command, action: #selector(AppDelegate.openHelp(_:)))
    ]

    private static let expected: [(menu: String, items: [Item])] = [
        ("Tessera", tesseraItems), ("Shell", shellItems), ("Edit", editItems), ("View", viewItems),
        ("Window", windowItems), ("Help", helpItems),
    ]

    private func installedMenu(_ title: String) throws -> NSMenu {
        let bar = try XCTUnwrap(NSApp.mainMenu, "no menu bar installed")
        return try XCTUnwrap(bar.items.first { $0.submenu?.title == title }?.submenu, "no \(title) menu")
    }

    /// Items AppKit adds to the installed menus: the Close All alternate of
    /// Close Tab, the window list, and those it adds to Edit and View (Show
    /// All Tabs and Enter Full Screen, in a running app only).
    private static func isAppKitItem(_ item: NSMenuItem) -> Bool {
        let added: Set<String> = [
            "closeAll:", "makeKeyAndOrderFront:", "orderFrontCharacterPalette:", "startDictation:",
            "toggleFullScreen:", "toggleTabOverview:", "_handleInsertFromContactsCommand:",
        ]
        return item.action.map { added.contains(NSStringFromSelector($0)) } ?? false
    }

    func testTheMenuBarHasTheMenusInOrder() throws {
        let bar = try XCTUnwrap(NSApp.mainMenu)
        XCTAssertEqual(bar.items.map { $0.submenu?.title ?? $0.title }, Self.expected.map(\.menu))
        XCTAssertTrue(NSApp.windowsMenu === (try installedMenu("Window")))
        XCTAssertTrue(NSApp.helpMenu === (try installedMenu("Help")))
    }

    func testEveryItemHasItsTitleKeyEquivalentAndModifiers() throws {
        for (title, items) in Self.expected {
            let menu = try installedMenu(title)
            // AppKit adds items of its own (see `isAppKitItem`).
            var installed = menu.items.filter { !Self.isAppKitItem($0) }
            // And the AutoFill submenu, after a separator, at the end of Edit.
            while let last = installed.last, last.isSeparatorItem || last.title == "AutoFill" {
                installed.removeLast()
            }
            XCTAssertEqual(installed.map(Item.init).map(\.description), items.map(\.description), "\(title) menu")
            for item in installed where item.submenu == nil {
                XCTAssertNil(item.target, "\(title) > \(item.title) goes up the responder chain")
            }
        }
    }

    func testTheTabItemsSelectTheFirstNineTabs() throws {
        let menu = try XCTUnwrap(try installedMenu("Window").item(withTitle: "Select Tab")?.submenu)
        XCTAssertEqual(menu.items.map(\.title), (1...9).map { "Tab \($0)" })
        XCTAssertEqual(menu.items.map(\.keyEquivalent), (1...9).map(String.init))
        XCTAssertEqual(menu.items.map(\.tag), Array(0..<9))
        XCTAssertTrue(menu.items.allSatisfy { $0.action == #selector(TerminalWindowController.selectTabAtIndex(_:)) })
    }

    // MARK: Key equivalents

    /// A key-down event as the keyboard would deliver it.
    private func keyDown(_ characters: String, code: UInt16, _ flags: NSEvent.ModifierFlags) throws -> NSEvent {
        try XCTUnwrap(
            NSEvent.keyEvent(
                with: .keyDown, location: .zero, modifierFlags: flags, timestamp: 0, windowNumber: 0, context: nil,
                characters: characters, charactersIgnoringModifiers: characters, isARepeat: false, keyCode: code))
    }

    /// Sends `event` through the whole menu bar with the View menu's items
    /// aimed at a recorder, and returns the actions it received. The key
    /// equivalent matching is AppKit's; only the target is the test's (no
    /// window is key in the test host, so the responder chain would end in
    /// the app).
    private func viewMenuActions(for event: NSEvent) throws -> [String] {
        let menu = try installedMenu("View")
        let recorder = ZoomRecorder()
        for item in menu.items {
            item.target = recorder
        }
        defer {
            for item in menu.items {
                item.target = nil
            }
        }
        XCTAssertTrue(try XCTUnwrap(NSApp.mainMenu).performKeyEquivalent(with: event), "\(event) is a key equivalent")
        return recorder.actions
    }

    func testCommandEqualsAndCommandPlusBothZoomIn() throws {
        let zoomIn = [NSStringFromSelector(#selector(TerminalSurfaceView.increaseFontSize(_:)))]
        // US: = is unshifted, + is Shift-=, both on key code 24.
        XCTAssertEqual(try viewMenuActions(for: keyDown("=", code: 24, [.command])), zoomIn, "⌘= (D3)")
        XCTAssertEqual(try viewMenuActions(for: keyDown("+", code: 24, [.command, .shift])), zoomIn, "⇧⌘= types ⌘+")
        // Swedish and German: + is an unshifted key of its own.
        XCTAssertEqual(try viewMenuActions(for: keyDown("+", code: 27, [.command])), zoomIn, "⌘+")
        XCTAssertEqual(
            try viewMenuActions(for: keyDown("-", code: 27, [.command])),
            [NSStringFromSelector(#selector(TerminalSurfaceView.decreaseFontSize(_:)))])
        XCTAssertEqual(
            try viewMenuActions(for: keyDown("0", code: 29, [.command])),
            [NSStringFromSelector(#selector(TerminalSurfaceView.resetFontSize(_:)))])
    }

    // MARK: Application items

    /// Sends `title`'s action from the installed menu up the responder chain,
    /// as choosing it does.
    private func choose(_ title: String, in menuTitle: String) throws {
        let item = try XCTUnwrap(try installedMenu(menuTitle).item(withTitle: title))
        let action = try XCTUnwrap(item.action)
        XCTAssertTrue(NSApp.sendAction(action, to: nil, from: item), "\(title) reached a responder")
    }

    func testSettingsCreatesTheConfigFileThenOpensIt() throws {
        try choose("Settings…", in: "Tessera")
        XCTAssertEqual(events, ["create", "open \(configFile.path)"], "created (if missing) before it is opened")
        XCTAssertEqual(opener.opened, [configFile])
        XCTAssertEqual(opener.revealed, [])
        XCTAssertEqual(opener.urls, [])
        try choose("Settings…", in: "Tessera")
        XCTAssertEqual(events.filter { $0 == "create" }.count, 2, "the core's createConfig() is idempotent")
    }

    func testSettingsShowsTheFileInFinderWhenNothingOpensIt() throws {
        opener.editor = nil
        try choose("Settings…", in: "Tessera")
        XCTAssertEqual(events, ["create"])
        XCTAssertEqual(opener.opened, [])
        XCTAssertEqual(opener.revealed, [configFile])
    }

    func testSettingsShowsTheFileInFinderWhenOpeningItFails() throws {
        opener.openError = CocoaError(.fileReadNoPermission)
        try choose("Settings…", in: "Tessera")
        XCTAssertEqual(opener.opened, [configFile])
        XCTAssertEqual(opener.revealed, [configFile], "after the failed open")
    }

    func testTesseraHelpOpensTheReadme() throws {
        try choose("Tessera Help", in: "Help")
        XCTAssertEqual(opener.urls, [URL(string: "https://github.com/aanyberg/tessera#readme")!])
        XCTAssertEqual(opener.opened, [])
    }
}

/// Records what the app would open; opening succeeds at once unless the
/// test says otherwise.
@MainActor
final class RecordingOpener: ExternalOpener {
    private(set) var urls: [URL] = []
    private(set) var opened: [URL] = []
    private(set) var revealed: [URL] = []
    /// The editor ``editor(for:)`` finds, or `nil` for none.
    var editor: URL? = URL(fileURLWithPath: "/System/Applications/TextEdit.app")
    /// The error opening reports, if any.
    var openError: (any Error)?
    /// Called for every ``open(_:withApplicationAt:completion:)``.
    var onOpen: (URL) -> Void = { _ in }

    func openURL(_ url: URL) -> Bool {
        urls.append(url)
        return true
    }

    func editor(for file: URL) -> URL? { editor }

    func open(
        _ file: URL, withApplicationAt application: URL,
        completion: @escaping @MainActor @Sendable ((any Error)?) -> Void
    ) {
        opened.append(file)
        onOpen(file)
        completion(openError)
    }

    func reveal(_ file: URL) { revealed.append(file) }
}

/// Stands in for the key window's terminal view as the View items' target.
@MainActor
final class ZoomRecorder: NSObject {
    private(set) var actions: [String] = []

    @objc func increaseFontSize(_ sender: Any?) { actions.append(NSStringFromSelector(#selector(increaseFontSize(_:)))) }
    @objc func decreaseFontSize(_ sender: Any?) { actions.append(NSStringFromSelector(#selector(decreaseFontSize(_:)))) }
    @objc func resetFontSize(_ sender: Any?) { actions.append(NSStringFromSelector(#selector(resetFontSize(_:)))) }
}
