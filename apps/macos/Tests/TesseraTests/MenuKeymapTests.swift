import AppKit
import TesseraKit
import XCTest

@testable import Tessera

/// The menu bar and the core's key map (#43, ADR 0036): every item names a
/// command, the default map leaves every key equivalent as authored, and a
/// `[keys]` edit in `config.toml` re-binds an item through
/// `on_config_applied`. On a bar of the test's own and a core of its own,
/// so the installed menus and the app's core stay as they are.
@MainActor
final class MenuKeymapTests: XCTestCase {
    private var dir: URL!
    private var previous: String?
    private var core: TesseraApp?

    override func setUp() async throws {
        dir = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-menu-keymap-\(UUID().uuidString)")
        previous = ProcessInfo.processInfo.environment["TESSERA_CONFIG_DIR"]
    }

    override func tearDown() async throws {
        core?.shutdown()
        core = nil
        if let previous { setenv("TESSERA_CONFIG_DIR", previous, 1) } else { unsetenv("TESSERA_CONFIG_DIR") }
        try? FileManager.default.removeItem(at: dir)
    }

    /// A core on this test's directory, with `config` as its `config.toml`.
    private func start(config: String) throws -> TesseraApp {
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        try config.write(to: configFile, atomically: true, encoding: .utf8)
        setenv("TESSERA_CONFIG_DIR", dir.path, 1)
        let started = try TesseraApp()
        core = started
        return started
    }

    private var configFile: URL { dir.appendingPathComponent("config.toml") }

    /// Every item of `bar`, depth first, without submenu titles and
    /// separators, as "identifier key modifiers hidden".
    private static func shortcuts(_ bar: NSMenu) -> [String] {
        bar.items.flatMap { item -> [String] in
            if let submenu = item.submenu, item.identifier == nil { return shortcuts(submenu) }
            if item.isSeparatorItem { return [] }
            let (key, mask) = authoredKeyEquivalent(of: item)
            let modifiers = key.isEmpty ? 0 : mask.intersection(.deviceIndependentFlagsMask).rawValue
            let hidden = item.isHidden && item.allowsKeyEquivalentWhenHidden ? " hidden" : ""
            return ["\(item.identifier?.rawValue ?? "nil") [\(key) \(modifiers)]\(hidden)"]
                + (item.submenu.map(shortcuts) ?? [])
        }
    }

    private static func item(_ command: String, _ bar: NSMenu) throws -> NSMenuItem {
        try XCTUnwrap(MainMenu.item(forCommand: command, in: bar), command)
    }

    func testEveryItemNamesACommandOfTheKeyMap() throws {
        let keymap = try start(config: "").keymap()
        let bar = MainMenu.makeMenuBar().bar
        var names: [String] = []
        var unmapped: [String] = []
        var profileMenus: [String] = []
        func walk(_ menu: NSMenu) {
            for item in menu.items where !item.isSeparatorItem {
                if let submenu = item.submenu, item.identifier == nil {
                    walk(submenu)
                    continue
                }
                // Exempt: the profile submenus run no registry command, as
                // registry commands take no profile name (ADR 0037).
                if let name = item.identifier?.rawValue, name.hasPrefix(MainMenu.profileMenuPrefix) {
                    profileMenus.append(name)
                    continue
                }
                guard let name = item.identifier?.rawValue, !name.hasSuffix(":") else {
                    unmapped.append(item.identifier?.rawValue ?? item.title)
                    continue
                }
                names.append(name.hasPrefix(MainMenu.aliasPrefix) ? String(name.dropFirst(MainMenu.aliasPrefix.count)) : name)
                if let submenu = item.submenu { walk(submenu) }
            }
        }
        walk(bar)
        // 41, the ten pane items (ADR 0035, #38b), the five Find items (#46b),
        // the three prompt items (ADR 0039, #45a) and Copy with Formatting
        // (ADR 0042, #47).
        XCTAssertEqual(names.count, 60, "\(names)")
        // Nothing else is exempt: Open config.toml left the menu for the
        // Settings window's Advanced pane (#410).
        XCTAssertEqual(unmapped, [], "\(unmapped)")
        XCTAssertEqual(profileMenus, [MainMenu.newWindowWithProfile, MainMenu.newTabWithProfile])
        for name in names {
            XCTAssertNotNil(keymap.command(named: name), "\(name) is a command")
            XCTAssertNotEqual(keymap.command(named: name)?.owner, .planned, "\(name) runs")
        }
    }

    func testTheDefaultMapChangesNoKeyEquivalent() throws {
        let keymap = try start(config: "").keymap()
        let authored = MainMenu.makeMenuBar().bar
        let applied = MainMenu.makeMenuBar().bar
        MainMenu.apply(keymap, to: applied)
        XCTAssertEqual(Self.shortcuts(applied), Self.shortcuts(authored))
        MainMenu.apply(keymap, to: applied)
        XCTAssertEqual(Self.shortcuts(applied), Self.shortcuts(authored), "applying twice adds no alternate")
        XCTAssertTrue(Self.shortcuts(authored).contains("alias:font_bigger [= 1048576] hidden"))
    }

    func testAConfigEditRebindsTheMenusThroughOnConfigApplied() async throws {
        // The app's own path: the router hears `on_config_applied` and the
        // menus read the key map again.
        let core = try start(config: "[keys]\n")
        let router = SessionRouter()
        core.delegate = router
        let bar = MainMenu.makeMenuBar().bar
        var applied = 0
        router.onConfigurationApplied = { [weak core] in
            guard let core, let keymap = try? core.keymap() else { return XCTFail("no key map") }
            MainMenu.apply(keymap, to: bar)
            applied += 1
        }
        MainMenu.apply(try core.keymap(), to: bar)
        let newTab = try Self.item("new_tab", bar)
        XCTAssertEqual(newTab.keyEquivalent, "t")
        try """
            [keys]
            'cmd+u' = 'new_tab'
            'cmd+=' = 'font_smaller'
            'cmd+q' = 'none'
            'cmd+1' = 'select_tab:2'
            'cmd+alt+shift+f2' = 'clear_scrollback'
            """.write(to: configFile, atomically: true, encoding: .utf8)
        let deadline = ContinuousClock.now + .seconds(10)
        while newTab.keyEquivalent != "u", ContinuousClock.now < deadline {
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertGreaterThan(applied, 0, "on_config_applied reached the router")
        XCTAssertEqual(newTab.keyEquivalent, "u", "the new chord is the item's")
        XCTAssertEqual(newTab.keyEquivalentModifierMask, .command)
        // Re-binding a chord takes it from its default command (`[keys]` wins).
        XCTAssertEqual(authoredKeyEquivalent(of: try Self.item("select_tab:2", bar)).key, "1")
        XCTAssertEqual(authoredKeyEquivalent(of: try Self.item("select_tab:1", bar)).key, "", "⌘1 moved to Tab 2")
        XCTAssertEqual(authoredKeyEquivalent(of: try Self.item("quit", bar)).key, "", "`none` unbinds Quit's chord")
        // A function key with every modifier but Control.
        let clear = try Self.item("clear_scrollback", bar)
        XCTAssertEqual(authoredKeyEquivalent(of: clear).key, MainMenu.functionKey(NSF2FunctionKey))
        XCTAssertEqual(clear.keyEquivalentModifierMask, [.command, .option, .shift])
        // ⌘= moved from Bigger's hidden alternate to Smaller's item, whose
        // own ⌘- becomes the alternate.
        let shortcuts = Self.shortcuts(bar)
        XCTAssertFalse(shortcuts.contains { $0.hasPrefix("alias:font_bigger") }, "\(shortcuts)")
        XCTAssertEqual(authoredKeyEquivalent(of: try Self.item("font_bigger", bar)).key, "+")
        XCTAssertEqual(authoredKeyEquivalent(of: try Self.item("font_smaller", bar)).key, "=")
        XCTAssertTrue(shortcuts.contains("alias:font_smaller [- 1048576] hidden"), "\(shortcuts)")
        // An edit back restores the defaults.
        try "[keys]\n".write(to: configFile, atomically: true, encoding: .utf8)
        let restoreDeadline = ContinuousClock.now + .seconds(10)
        while newTab.keyEquivalent != "t", ContinuousClock.now < restoreDeadline {
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTAssertEqual(Self.shortcuts(bar), Self.shortcuts(MainMenu.makeMenuBar().bar), "back to the defaults")
    }

    /// A bound command runs as choosing its item would: only while the
    /// item validates as enabled, with a beep otherwise (#317).
    func testRunningACommandValidatesItsItemFirst() throws {
        let bar = MainMenu.makeMenuBar().bar
        let copy = try Self.item("copy", bar)
        let target = CopyTarget()
        copy.target = target
        var beeps = 0
        MainMenu.run(command: "copy", in: bar) { beeps += 1 }
        XCTAssertEqual(target.copies, 0, "a disabled item does not run")
        XCTAssertEqual(beeps, 1, "it beeps instead")
        target.enabled = true
        MainMenu.run(command: "copy", in: bar) { beeps += 1 }
        XCTAssertEqual(target.copies, 1, "an enabled one runs")
        XCTAssertEqual(beeps, 1)
    }
}

/// A Copy target whose item is enabled only when told.
@MainActor
private final class CopyTarget: NSObject, NSMenuItemValidation {
    var enabled = false
    private(set) var copies = 0

    @objc func copy(_ sender: Any?) { copies += 1 }

    func validateMenuItem(_ menuItem: NSMenuItem) -> Bool { enabled }
}
