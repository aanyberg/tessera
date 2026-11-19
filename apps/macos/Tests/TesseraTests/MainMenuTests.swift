import AppKit
import TesseraKit
import XCTest

@testable import Tessera

/// The menu bar the app installs at launch: every item's title, key
/// equivalent, modifiers and action, ⌘= reaching Bigger through its hidden
/// item, and Tessera Help without starting a browser; Open config.toml
/// (the Settings window's Advanced pane) is in ``SettingsTests``. The
/// terminal items' effects are tested on the view in
/// ``TerminalSurfaceViewTests``, the scrollback items' (and their key
/// equivalents) in ``TerminalScrollTests``.
@MainActor
final class MainMenuTests: XCTestCase {
    private var appDelegate: AppDelegate!
    private var opener: RecordingOpener!
    /// What the stand-in for the core's `createConfig()` returns; nothing
    /// is written there.
    private let configFile = URL(fileURLWithPath: "/nonexistent/tessera-tests/config.toml")
    /// What happened, in order: "create" and "open <path>".
    private var events: [String] = []
    /// The default key map, put back on the menus after a test re-binds them.
    private var defaultKeymap: TesseraKeymap?
    private var cores: [TesseraApp] = []
    private var configDirs: [URL] = []

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
        if let defaultKeymap, let bar = NSApp.mainMenu {
            MainMenu.apply(defaultKeymap, to: bar)
        }
        for core in cores { core.shutdown() }
        cores = []
        for dir in configDirs { try? FileManager.default.removeItem(at: dir) }
        configDirs = []
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
        /// Shown in place of the item before it while ⌥ is held.
        var alternate = false

        static let separator = Item(title: "-")

        init(
            title: String, key: String = "", modifiers: NSEvent.ModifierFlags = [], action: Selector? = nil,
            hidden: Bool = false, alternate: Bool = false
        ) {
            self.title = title
            self.key = key
            self.modifiers = key.isEmpty ? [] : modifiers
            self.action = action
            self.hidden = hidden
            self.alternate = alternate
        }

        @MainActor init(_ item: NSMenuItem) {
            if item.isSeparatorItem {
                self = .separator
                return
            }
            let (key, mask) = authoredKeyEquivalent(of: item)
            let modifiers = mask.intersection(.deviceIndependentFlagsMask)
            // A hidden item counts only if its key equivalent works (D3).
            let hidden = item.isHidden && item.allowsKeyEquivalentWhenHidden
            self.init(
                title: item.title, key: key, modifiers: modifiers, action: item.action, hidden: hidden,
                alternate: item.isAlternate)
            if item.isHidden && !hidden {
                self.title += " (hidden, key equivalent off)"
            }
        }

        var description: String {
            let action = action.map(NSStringFromSelector) ?? "nil"
            return "\(title) [\(key) \(modifiers.rawValue)] \(action)\(hidden ? " hidden" : "")"
                + (alternate ? " alternate" : "")
        }
    }

    private static let command: NSEvent.ModifierFlags = [.command]

    private static let tesseraItems: [Item] = [
        Item(title: "About Tessera", action: #selector(NSApplication.orderFrontStandardAboutPanel(_:))),
        .separator,
        // "Settings", as installed at launch: AppKit adds no ellipsis (#410).
        Item(title: "Settings", key: ",", modifiers: command, action: #selector(AppDelegate.openSettings(_:))),
        .separator,
        Item(title: "Hide Tessera", key: "h", modifiers: command, action: #selector(NSApplication.hide(_:))),
        Item(
            title: "Hide Others", key: "h", modifiers: [.command, .option],
            action: #selector(NSApplication.hideOtherApplications(_:))),
        Item(title: "Show All", action: #selector(NSApplication.unhideAllApplications(_:))),
        .separator,
        Item(title: "Quit Tessera", key: "q", modifiers: command, action: #selector(NSApplication.terminate(_:))),
        // Titled for the test host's "Close windows when quitting an
        // application" (#464; QuitAndCloseAllTests flips it).
        Item(
            title: AppDelegate.quitAlternateTitle(quitKeepsWindows: WindowStateStore.quitKeepsWindows(.standard)),
            key: "q", modifiers: [.command, .option], action: #selector(AppDelegate.quitWithTheOtherWindowChoice(_:)),
            alternate: true),
    ]

    private static let shellItems: [Item] = [
        Item(title: "New Window", key: "n", modifiers: command, action: #selector(AppDelegate.openNewWindow(_:))),
        Item(title: "New Tab", key: "t", modifiers: command, action: #selector(AppDelegate.openNewTab(_:))),
        // Hidden until the core lists profiles (#39, ``ProfileMenuTests``);
        // the test host's configuration has none.
        Item(title: "New Window with Profile (hidden, key equivalent off)", action: #selector(NSMenu.submenuAction(_:))),
        Item(title: "New Tab with Profile (hidden, key equivalent off)", action: #selector(NSMenu.submenuAction(_:))),
        .separator,
        Item(title: "Close Tab", key: "w", modifiers: command, action: #selector(NSWindow.performClose(_:))),
        // Tessera's own, in place of AppKit's `closeAll:` (#464).
        Item(
            title: "Close All", key: "w", modifiers: [.command, .option],
            action: #selector(AppDelegate.closeAllWindows(_:)), alternate: true),
        Item(
            title: "Close Window", key: "w", modifiers: [.command, .shift],
            action: #selector(TerminalWindowController.closeTabGroup(_:))),
        .separator,
        Item(
            title: "Clear to Start", key: "k", modifiers: command,
            action: #selector(TerminalSurfaceView.clearScrollback(_:))),
        // The pane items (ADR 0035; the submenu's in PaneTests).
        .separator,
        Item(
            title: "Split Right", key: "d", modifiers: command,
            action: #selector(TerminalWindowController.splitPaneRight(_:))),
        Item(
            title: "Split Below", key: "d", modifiers: [.command, .shift],
            action: #selector(TerminalWindowController.splitPaneBelow(_:))),
        Item(title: "Select Pane", action: #selector(NSMenu.submenuAction(_:))),
        Item(
            title: "Maximize Pane", key: "\r", modifiers: [.command, .shift],
            action: #selector(TerminalWindowController.toggleZoomPane(_:))),
        Item(title: "Equalize Panes", action: #selector(TerminalWindowController.equalizePanes(_:))),
    ]

    private static let editItems: [Item] = [
        Item(title: "Copy", key: "c", modifiers: command, action: #selector(TerminalSurfaceView.copy(_:))),
        Item(title: "Copy with Formatting", action: #selector(TerminalSurfaceView.copyWithFormatting(_:))),
        Item(title: "Paste", key: "v", modifiers: command, action: #selector(TerminalSurfaceView.paste(_:))),
        Item(
            title: "Paste as Plain Text", key: "v", modifiers: [.command, .option, .shift],
            action: #selector(TerminalSurfaceView.pasteAsPlainText(_:))),
        Item(title: "Select All", key: "a", modifiers: command, action: #selector(TerminalSurfaceView.selectAll(_:))),
        .separator,
        Item(title: "Find", action: #selector(NSMenu.submenuAction(_:))),
    ]

    /// Edit > Find (#46b): Find Next moves toward older output (D16a).
    private static let findItems: [Item] = [
        Item(title: "Find…", key: "f", modifiers: command, action: #selector(TerminalSurfaceView.showFindBar(_:))),
        Item(title: "Find Next", key: "g", modifiers: command, action: #selector(TerminalSurfaceView.findNextMatch(_:))),
        Item(
            title: "Find Previous", key: "g", modifiers: [.command, .shift],
            action: #selector(TerminalSurfaceView.findPreviousMatch(_:))),
        Item(
            title: "Use Selection for Find", key: "e", modifiers: command,
            action: #selector(TerminalSurfaceView.useSelectionForFind(_:))),
        .separator,
        Item(title: "Copy All Matches", action: #selector(TerminalSurfaceView.copyAllFindMatches(_:))),
    ]

    private static let viewItems: [Item] = [
        Item(title: "Bigger", key: "+", modifiers: command, action: #selector(TerminalSurfaceView.increaseFontSize(_:))),
        Item(
            title: "Bigger", key: "=", modifiers: command,
            action: #selector(TerminalSurfaceView.increaseFontSize(_:)), hidden: true),
        Item(title: "Smaller", key: "-", modifiers: command, action: #selector(TerminalSurfaceView.decreaseFontSize(_:))),
        Item(title: "Actual Size", key: "0", modifiers: command, action: #selector(TerminalSurfaceView.resetFontSize(_:))),
        .separator,
        Item(
            title: "Scroll to Top", key: "\u{F729}", modifiers: command,
            action: #selector(TerminalSurfaceView.scrollToTop(_:))),
        Item(
            title: "Scroll to Bottom", key: "\u{F72B}", modifiers: command,
            action: #selector(TerminalSurfaceView.scrollToBottom(_:))),
        Item(
            title: "Page Up", key: "\u{F700}", modifiers: command,
            action: #selector(TerminalSurfaceView.scrollPageUpInHistory(_:))),
        Item(
            title: "Page Down", key: "\u{F701}", modifiers: command,
            action: #selector(TerminalSurfaceView.scrollPageDownInHistory(_:))),
        Item(title: "Previous Prompt", action: #selector(TerminalSurfaceView.scrollToPreviousPrompt(_:))),
        Item(title: "Next Prompt", action: #selector(TerminalSurfaceView.scrollToNextPrompt(_:))),
        Item(title: "Select Last Output", action: #selector(TerminalSurfaceView.selectLastOutput(_:))),
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

    /// Items AppKit adds to the installed menus: the window list, and those
    /// it adds to Edit and View (Show All Tabs and Enter Full Screen, in a
    /// running app only). Not its Close All (`closeAll:`), which it adds
    /// only to a menu bar without a ⌥⌘W item: Tessera's own takes its
    /// place (#464), so one would fail this test.
    private static func isAppKitItem(_ item: NSMenuItem) -> Bool {
        let added: Set<String> = [
            "makeKeyAndOrderFront:", "orderFrontCharacterPalette:", "startDictation:",
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
            // The Quit alternate's title follows the setting as the menu is
            // validated.
            if title == "Tessera" { menu.update() }
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

    func testTheFindItemsAreInEditFind() throws {
        let menu = try XCTUnwrap(try installedMenu("Edit").item(withTitle: "Find")?.submenu)
        XCTAssertEqual(menu.items.map(Item.init).map(\.description), Self.findItems.map(\.description))
        for item in menu.items where !item.isSeparatorItem {
            XCTAssertNil(item.target, "Find > \(item.title) goes up the responder chain")
        }
        XCTAssertNil(try installedMenu("Edit").item(withTitle: "Find")?.identifier, "the submenu runs no command")
    }

    func testTheTabItemsSelectTheFirstNineTabs() throws {
        let menu = try XCTUnwrap(try installedMenu("Window").item(withTitle: "Select Tab")?.submenu)
        XCTAssertEqual(menu.items.map(\.title), (1...9).map { "Tab \($0)" })
        XCTAssertEqual(menu.items.map { authoredKeyEquivalent(of: $0).key }, (1...9).map(String.init))
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
        // The events are a US keyboard's: match them against the key
        // equivalents as set, not as localised to the current layout.
        let localised = menu.items.map(\.allowsAutomaticKeyEquivalentLocalization)
        for item in menu.items {
            item.target = recorder
            item.allowsAutomaticKeyEquivalentLocalization = false
        }
        defer {
            for (item, localises) in zip(menu.items, localised) {
                item.target = nil
                item.allowsAutomaticKeyEquivalentLocalization = localises
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

    func testABoundCommandRunsItsMenuItem() throws {
        // What a chord bound to a command does once the key map resolved it
        // (a chord without Command, which no menu matches).
        MainMenu.run(command: "help")
        XCTAssertEqual(opener.urls, [AppDelegate.helpURL])
        MainMenu.run(command: "copy_mode")
        MainMenu.run(command: "no_such_command")
        XCTAssertEqual(opener.urls.count, 1, "a command without an item does nothing")
        XCTAssertEqual(events, [])
    }

    // MARK: Re-bound alternates

    /// A core of the test's own on `config`, shut down in `tearDown`; the
    /// test's environment is put back at once, the core has read it.
    private func startCore(config: String) throws -> TesseraApp {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-menu-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        configDirs.append(dir)
        try config.write(to: dir.appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
        let previous = ProcessInfo.processInfo.environment["TESSERA_CONFIG_DIR"]
        setenv("TESSERA_CONFIG_DIR", dir.path, 1)
        defer {
            if let previous { setenv("TESSERA_CONFIG_DIR", previous, 1) } else { unsetenv("TESSERA_CONFIG_DIR") }
        }
        let core = try TesseraApp()
        cores.append(core)
        return core
    }

    /// Close All on a key other than Close Tab's cannot be Close Tab's
    /// alternate: it is an ordinary visible item that runs on its chord,
    /// Close Tab does not change, AppKit adds no Close All of its own, and
    /// the default key map makes it the alternate again.
    func testCloseAllReboundToAnotherKeyIsAVisibleItemThatWorks() throws {
        let bar = try XCTUnwrap(NSApp.mainMenu)
        defaultKeymap = try startCore(config: "").keymap()
        let previousPresenter = appDelegate.closeConfirmationPresenter
        let presenter = RecordingCloseConfirmationPresenter()
        appDelegate.closeConfirmationPresenter = presenter
        appDelegate.spawnOptions = .init(program: "/bin/sh", arguments: ["-i"])
        defer {
            appDelegate.closeConfirmationPresenter = previousPresenter
            appDelegate.spawnOptions = .init()
            for controller in appDelegate.windowControllers { controller.window?.close() }
        }
        // First: the app's first window starts its core, which applies its own key map.
        _ = try appDelegate.newWindow()
        let config = "[keys]\n'cmd+alt+w' = 'none'\n'cmd+alt+j' = 'close_all_windows'\n"
        MainMenu.apply(try startCore(config: config).keymap(), to: bar)
        NSApp.mainMenu = bar
        bar.update()

        let closeAll = try XCTUnwrap(MainMenu.item(forCommand: "close_all_windows", in: bar))
        let closeTab = try XCTUnwrap(MainMenu.item(forCommand: "close_tab", in: bar))
        let menu = try XCTUnwrap(closeAll.menu)
        menu.update()
        XCTAssertFalse(closeAll.isAlternate, "another key than Close Tab's: an ordinary item")
        XCTAssertFalse(closeAll.isHidden, "visible")
        XCTAssertEqual(authoredKeyEquivalent(of: closeAll).key, "j")
        XCTAssertEqual(authoredKeyEquivalent(of: closeAll).modifiers, [.command, .option])
        XCTAssertEqual(authoredKeyEquivalent(of: closeTab).key, "w", "Close Tab keeps its key")
        XCTAssertEqual(authoredKeyEquivalent(of: closeTab).modifiers, [.command])
        XCTAssertFalse(closeTab.isAlternate)
        XCTAssertFalse(closeTab.isHidden)
        XCTAssertFalse(
            menu.items.contains { $0.action == NSSelectorFromString("closeAll:") }, "AppKit's Close All stays away")

        let chord = try XCTUnwrap(TesseraTests.keyDown("j", keyCode: 38))
        XCTAssertTrue(bar.performKeyEquivalent(with: chord), "⌥⌘J is Close All")
        XCTAssertTrue(appDelegate.windowControllers.isEmpty, "and closes every window")
        XCTAssertTrue(presenter.modals.isEmpty, "idle: nothing asks")

        MainMenu.apply(try XCTUnwrap(defaultKeymap), to: bar)
        XCTAssertTrue(closeAll.isAlternate, "⌥⌘W again: the alternate again")
    }

    /// Identifiers of the items from `first` on, `count` of them.
    private func identifiers(from first: NSMenuItem, count: Int) throws -> [String] {
        let menu = try XCTUnwrap(first.menu)
        let start = menu.index(of: first)
        XCTAssertLessThanOrEqual(start + count, menu.numberOfItems)
        guard start >= 0, start + count <= menu.numberOfItems else { return [] }
        return menu.items[start..<(start + count)].map { $0.identifier?.rawValue ?? "" }
    }

    /// A second chord on Quit or Close Tab goes behind the item's ⌥
    /// alternate, which only works directly under its item; applying the
    /// key map again gives the same order. On the item's own letter the
    /// alternate stays one; on another letter that comes first the item
    /// has another key, and the alternate is an ordinary item, still there.
    func testASecondChordStaysBehindTheOptionAlternate() throws {
        let bar = try XCTUnwrap(NSApp.mainMenu)
        defaultKeymap = try startCore(config: "").keymap()
        let expected = [
            ("quit", ["quit", "quit_keep_windows", "alias:quit"]),
            ("close_tab", ["close_tab", "close_all_windows", "alias:close_tab"]),
        ]
        let configs = [
            ("[keys]\n'cmd+ctrl+q' = 'quit'\n'cmd+ctrl+w' = 'close_tab'\n", true),
            ("[keys]\n'cmd+ctrl+u' = 'quit'\n'cmd+ctrl+j' = 'close_tab'\n", false),
        ]
        for (config, staysAlternate) in configs {
            let keymap = try startCore(config: config).keymap()
            for pass in ["applied", "applied again"] {
                MainMenu.apply(keymap, to: bar)
                for (command, order) in expected {
                    let context = "\(command), \(pass), \(config)"
                    let item = try XCTUnwrap(MainMenu.item(forCommand: command, in: bar))
                    XCTAssertEqual(try identifiers(from: item, count: order.count), order, context)
                    let menu = try XCTUnwrap(item.menu)
                    let index = menu.index(of: item)
                    guard index + 2 < menu.numberOfItems else { return XCTFail("\(context): too few items") }
                    XCTAssertFalse(item.isAlternate, context)
                    XCTAssertEqual(menu.items[index + 1].isAlternate, staysAlternate, context)
                    XCTAssertTrue(menu.items[index + 2].isHidden, "\(context): the second chord")
                    XCTAssertTrue(menu.items[index + 2].allowsKeyEquivalentWhenHidden, context)
                }
            }
        }
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

/// `item`'s key equivalent and modifiers as the menu sets them. AppKit
/// localises them to the current keyboard layout (⌘} reads as ⇧⌘ä on a
/// Swedish one, the hidden ⌘= as ⌘`), so they are read with that
/// localisation off, then it is restored: the tests pass on any layout.
@MainActor
func authoredKeyEquivalent(of item: NSMenuItem) -> (key: String, modifiers: NSEvent.ModifierFlags) {
    let localises = item.allowsAutomaticKeyEquivalentLocalization
    item.allowsAutomaticKeyEquivalentLocalization = false
    defer { item.allowsAutomaticKeyEquivalentLocalization = localises }
    return (item.keyEquivalent, item.keyEquivalentModifierMask)
}
