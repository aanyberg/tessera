import AppKit
import TesseraKit
import XCTest

@testable import Tessera

/// Shell > New Tab with Profile and New Window with Profile (#39): the
/// submenus list the configured profiles in file order under sanitised
/// titles, choosing one starts a session with that profile, a split starts
/// with its focused pane's profile, and a configuration the core applies
/// rebuilds them. The windows run `cat`, as in ``WindowTabTests``.
@MainActor
final class ProfileMenuTests: XCTestCase {
    private var appDelegate: AppDelegate!
    private var dir: URL?
    private var previous: String?
    private var core: TesseraApp?
    /// The test gave the app a core of its own (``AppDelegate/replaceCore(start:)``).
    private var replacedCore = false

    override func setUp() async throws {
        appDelegate = try XCTUnwrap(NSApp.delegate as? AppDelegate)
        appDelegate.spawnOptions = .init(program: "/bin/cat")
        XCTAssertTrue(appDelegate.windowControllers.isEmpty, "a previous test left windows open")
        previous = ProcessInfo.processInfo.environment["TESSERA_CONFIG_DIR"]
    }

    override func tearDown() async throws {
        for controller in appDelegate.windowControllers {
            controller.window?.close()
        }
        XCTAssertTrue(appDelegate.windowControllers.isEmpty)
        appDelegate.spawnOptions = .init()
        if replacedCore {
            // The next test's first session starts a core on its own
            // configuration again.
            try appDelegate.replaceCore(start: false)
        }
        if let bar = NSApp.mainMenu { MainMenu.setProfiles([], in: bar) }
        appDelegate = nil
        core?.shutdown()
        core = nil
        if let previous { setenv("TESSERA_CONFIG_DIR", previous, 1) } else { unsetenv("TESSERA_CONFIG_DIR") }
        if let dir { try? FileManager.default.removeItem(at: dir) }
    }

    private static func submenu(_ identifier: String, _ bar: NSMenu) throws -> (item: NSMenuItem, menu: NSMenu) {
        let item = try XCTUnwrap(MainMenu.item(forCommand: identifier, in: bar), identifier)
        return (item, try XCTUnwrap(item.submenu, identifier))
    }

    // MARK: Layout

    func testTheSubmenusFollowNewTabInTheShellMenuHiddenWithoutProfiles() throws {
        let bar = MainMenu.makeMenuBar().bar
        let shell = try XCTUnwrap(bar.items.first { $0.submenu?.title == "Shell" }?.submenu)
        XCTAssertEqual(
            Array(shell.items.prefix(4).map(\.title)),
            ["New Window", "New Tab", "New Window with Profile", "New Tab with Profile"])
        for identifier in [MainMenu.newWindowWithProfile, MainMenu.newTabWithProfile] {
            let (item, menu) = try Self.submenu(identifier, bar)
            XCTAssertTrue(item.isHidden, "\(identifier) without profiles")
            XCTAssertEqual(menu.items, [])
        }
    }

    func testTheSubmenusListTheProfilesInFileOrderWithSanitisedTitles() throws {
        let bar = MainMenu.makeMenuBar().bar
        let long = String(repeating: "p", count: 200)
        let names = [
            "zsh", "Build \u{202E}evil\u{202C}", "tab\tand\nnew  lines", "zsh", long, "\u{200B}\u{2066}",
            "zsh\u{200F}", "👩‍💻 dev",
            // C0, DEL and C1 controls (S3).
            "\u{1B}[31mred", "\u{7F}del", "\u{9B}2Jcsi",
            // Two profiles to the core: their bytes differ.
            "caf\u{E9}", "cafe\u{301}",
            // Nothing visible.
            "\u{3164}", "\u{2800}", "\u{200D}",
        ]
        MainMenu.setProfiles(names, in: bar)
        let expected: [(title: String, name: String)] = [
            ("zsh", "zsh"),
            ("Build evil", "Build \u{202E}evil\u{202C}"),
            ("tab and new lines", "tab\tand\nnew  lines"),
            // The second "zsh" is left out: the core starts the first.
            (String(repeating: "p", count: MainMenu.maxProfileTitleLength - 1) + "…", long),
            ("Unnamed Profile", "\u{200B}\u{2066}"),
            // Shows as "zsh" too: numbered, so the two can be told apart.
            ("zsh (2)", "zsh\u{200F}"),
            // A zero-width joiner stays, so the emoji stays one.
            ("👩‍💻 dev", "👩‍💻 dev"),
            ("[31mred", "\u{1B}[31mred"),
            ("del", "\u{7F}del"),
            ("2Jcsi", "\u{9B}2Jcsi"),
            ("caf\u{E9}", "caf\u{E9}"),
            ("caf\u{E9} (2)", "cafe\u{301}"),
            ("Unnamed Profile (2)", "\u{3164}"),
            ("Unnamed Profile (3)", "\u{2800}"),
            ("Unnamed Profile (4)", "\u{200D}"),
        ]
        let actions = [
            MainMenu.newWindowWithProfile: #selector(AppDelegate.openNewWindowWithProfile(_:)),
            MainMenu.newTabWithProfile: #selector(AppDelegate.openNewTabWithProfile(_:)),
        ]
        for (identifier, action) in actions {
            let (item, menu) = try Self.submenu(identifier, bar)
            XCTAssertFalse(item.isHidden)
            XCTAssertEqual(menu.items.map(\.title), expected.map(\.title), identifier)
            XCTAssertEqual(menu.items.map { $0.representedObject as? String }, expected.map(\.name), identifier)
            for profile in menu.items {
                XCTAssertEqual(profile.action, action)
                XCTAssertNil(profile.target, "up the responder chain")
                XCTAssertEqual(profile.keyEquivalent, "")
            }
            XCTAssertEqual(
                menu.items.map(\.identifier?.rawValue), expected.indices.map { "\(identifier):\($0)" },
                "no registry command's (ADR 0037)")
        }
        // Idempotent, and back to hidden.
        MainMenu.setProfiles(names, in: bar)
        XCTAssertEqual(try Self.submenu(MainMenu.newTabWithProfile, bar).menu.items.count, expected.count)
        MainMenu.setProfiles([], in: bar)
        XCTAssertTrue(try Self.submenu(MainMenu.newTabWithProfile, bar).item.isHidden)
        XCTAssertEqual(try Self.submenu(MainMenu.newTabWithProfile, bar).menu.items, [])
    }

    /// The key map leaves the profile items alone (ADR 0037).
    func testTheKeyMapLeavesTheProfileItemsAlone() throws {
        let dir = try configDir()
        try "".write(to: dir.appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
        setenv("TESSERA_CONFIG_DIR", dir.path, 1)
        let core = try TesseraApp()
        self.core = core
        let bar = MainMenu.makeMenuBar().bar
        MainMenu.setProfiles(["a", "b"], in: bar)
        MainMenu.apply(try core.keymap(), to: bar)
        let (item, menu) = try Self.submenu(MainMenu.newTabWithProfile, bar)
        XCTAssertEqual(item.keyEquivalent, "")
        XCTAssertEqual(menu.items.map(\.title), ["a", "b"])
        XCTAssertEqual(menu.items.map(\.keyEquivalent), ["", ""])
    }

    // MARK: Choosing a profile

    /// The item of `bar`'s `identifier` submenu titled `title`.
    private static func profileItem(_ title: String, _ identifier: String) throws -> NSMenuItem {
        let bar = try XCTUnwrap(NSApp.mainMenu)
        return try XCTUnwrap(try submenu(identifier, bar).menu.item(withTitle: title), title)
    }

    func testChoosingAProfileOpensATabOrWindowWithIt() throws {
        let first = try appDelegate.newWindow()
        XCTAssertNil(appDelegate.profile(of: first.focusedPane.session), "default_profile's")
        MainMenu.setProfiles(["work", "play"], in: try XCTUnwrap(NSApp.mainMenu))

        let tabItem = try Self.profileItem("work", MainMenu.newTabWithProfile)
        XCTAssertTrue(NSApp.sendAction(try XCTUnwrap(tabItem.action), to: appDelegate, from: tabItem))
        XCTAssertEqual(appDelegate.windowControllers.count, 2)
        let tab = try XCTUnwrap(appDelegate.windowControllers.last)
        XCTAssertEqual(appDelegate.profile(of: tab.focusedPane.session), "work")
        XCTAssertNoThrow(try tab.focusedPane.session.write(Array("hello\n".utf8)), "a live session")

        let windowItem = try Self.profileItem("play", MainMenu.newWindowWithProfile)
        XCTAssertTrue(NSApp.sendAction(try XCTUnwrap(windowItem.action), to: appDelegate, from: windowItem))
        XCTAssertEqual(appDelegate.windowControllers.count, 3)
        let window = try XCTUnwrap(appDelegate.windowControllers.last)
        XCTAssertEqual(appDelegate.profile(of: window.focusedPane.session), "play")
        let standalone = try XCTUnwrap(window.window)
        XCTAssertEqual(standalone.tabGroup?.windows ?? [standalone], [standalone], "a window of its own")
        XCTAssertNil(appDelegate.profile(of: first.focusedPane.session), "the first tab keeps its own")
    }

    func testASplitStartsWithTheFocusedPanesProfile() throws {
        let plain = try appDelegate.newWindow()
        let work = try appDelegate.newTab(in: plain.window, profile: "work")
        let workPane = try XCTUnwrap(try work.splitFocusedPane(.right), "the split is refused")
        XCTAssertEqual(appDelegate.profile(of: workPane.session), "work")
        // From the new pane, which has the focus now: still "work".
        let below = try XCTUnwrap(try work.splitFocusedPane(.below), "the split is refused")
        XCTAssertEqual(appDelegate.profile(of: below.session), "work")
        let plainPane = try XCTUnwrap(try plain.splitFocusedPane(.right), "the split is refused")
        XCTAssertNil(appDelegate.profile(of: plainPane.session), "default_profile's")
        // A closed session's profile is forgotten.
        let session = workPane.session
        work.window?.close()
        XCTAssertNil(appDelegate.profile(of: session))
    }

    // MARK: Live reload

    private func configDir() throws -> URL {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("tessera-profile-menu-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        self.dir = dir
        return dir
    }

    /// Gives the app a core of its own on `config` (`config.toml` in `dir`,
    /// else a fresh directory), returned with that directory.
    private func startAppCore(config: String, in dir: URL? = nil) throws -> URL {
        let dir = try dir ?? configDir()
        try config.write(to: dir.appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
        setenv("TESSERA_CONFIG_DIR", dir.path, 1)
        replacedCore = true
        try appDelegate.replaceCore(start: true)
        return dir
    }

    /// Waits up to 10 s for `condition`.
    private func waitUntil(_ what: String, _ condition: () -> Bool) async throws {
        let deadline = ContinuousClock.now + .seconds(10)
        while !condition() {
            guard ContinuousClock.now < deadline else { return XCTFail("timed out: \(what)") }
            try await Task.sleep(for: .milliseconds(10))
        }
    }

    /// The app's own path from a profile's item to the core: the session
    /// runs the profile's command in its working directory, and so does a
    /// split of its pane. The default profile runs `cat`, so a session
    /// started without the profile writes nothing.
    func testChoosingAProfileRunsItsCommandInItsDirectory() async throws {
        let dir = try configDir()
        let work = dir.appendingPathComponent("work")
        try FileManager.default.createDirectory(at: work, withIntermediateDirectories: true)
        let ran = work.appendingPathComponent("ran")
        _ = try startAppCore(
            config: """
                [general]
                default_profile = "plain"
                [[profiles]]
                name = "plain"
                command = "/bin/cat"
                [[profiles]]
                name = "work"
                command = "pwd -P > ran; exec cat"
                working_directory = "\(work.path)"
                """, in: dir)
        // Not `/bin/cat`: the profile's command runs for the login shell.
        appDelegate.spawnOptions = .init()
        let item = try Self.profileItem("work", MainMenu.newTabWithProfile)
        XCTAssertTrue(NSApp.sendAction(try XCTUnwrap(item.action), to: appDelegate, from: item))
        let tab = try XCTUnwrap(appDelegate.windowControllers.last)
        // `pwd -P`'s path: resolvingSymlinksInPath drops `/private`.
        let real = try XCTUnwrap(realpath(work.path, nil).map { path in
            defer { free(path) }
            return String(cString: path)
        })
        let wrote = { (try? String(contentsOf: ran, encoding: .utf8)) == real + "\n" }
        try await waitUntil("the profile's command ran in its directory", wrote)
        XCTAssertTrue(wrote())

        try FileManager.default.removeItem(at: ran)
        _ = try XCTUnwrap(try tab.splitFocusedPane(.right), "the split is refused")
        try await waitUntil("the split ran the profile's command", wrote)
        XCTAssertTrue(wrote())
    }

    /// The app's own reload path: a configuration the core applies
    /// rebuilds the menu bar's submenus (``AppDelegate/applyProfiles()``
    /// from the router's `onConfigurationApplied`).
    func testAReloadRebuildsTheSubmenus() async throws {
        let dir = try startAppCore(config: "[[profiles]]\nname = \"one\"\n[[profiles]]\nname = \"two\"\n")
        let bar = try XCTUnwrap(NSApp.mainMenu)
        let titles = { (identifier: String) in
            (try? Self.submenu(identifier, bar).menu.items.map(\.title)) ?? []
        }
        XCTAssertEqual(titles(MainMenu.newTabWithProfile), ["one", "two"])
        try "[[profiles]]\nname = \"three\"\n[[profiles]]\nname = \"one\"\n".write(
            to: dir.appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
        try await waitUntil("the submenus follow the file") {
            titles(MainMenu.newTabWithProfile) == ["three", "one"]
        }
        XCTAssertEqual(titles(MainMenu.newTabWithProfile), ["three", "one"])
        XCTAssertEqual(titles(MainMenu.newWindowWithProfile), ["three", "one"])
    }
}
