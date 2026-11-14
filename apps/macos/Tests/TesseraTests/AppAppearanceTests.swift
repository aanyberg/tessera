import AppKit
import TesseraKit
import XCTest

@testable import Tessera

/// `[appearance] app_appearance` (#365): dark, light or the system's, read
/// when the core starts and after every configuration it applies, and set
/// on the app alone (``AppDelegate/applyAppAppearance(_:)``). Every window
/// here is opened the way the app opens one, on the app's own core, whose
/// configuration is a `config.toml` in a temporary directory (never the
/// user's). Nothing here sets an appearance or posts a notification: the
/// app's start of the core, its reload of the file and its Settings window
/// do.
@MainActor
final class AppAppearanceTests: XCTestCase {
    private var appDelegate: AppDelegate!
    private var dir: URL!
    private var previousConfigDir: String?

    override func setUp() async throws {
        appDelegate = try XCTUnwrap(NSApp.delegate as? AppDelegate)
        appDelegate.spawnOptions = .init(program: "/bin/cat")
        XCTAssertTrue(appDelegate.windowControllers.isEmpty, "a previous test left windows open")
        XCTAssertNil(appDelegate.settingsWindowController, "a previous test left Settings open")
        XCTAssertEqual(NSApp.appearance?.name, .darkAqua, "a previous test left another appearance")
        previousConfigDir = ProcessInfo.processInfo.environment["TESSERA_CONFIG_DIR"]
        dir = FileManager.default.temporaryDirectory.appendingPathComponent(
            "tessera-app-appearance-\(UUID().uuidString)")
        XCTAssertTrue(dir.path.hasPrefix("/"), "an absolute TESSERA_CONFIG_DIR")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        setenv("TESSERA_CONFIG_DIR", dir.path, 1)
    }

    override func tearDown() async throws {
        for controller in appDelegate.windowControllers {
            if let window = controller.window, let sheet = window.attachedSheet {
                window.endSheet(sheet, returnCode: .alertSecondButtonReturn)
            }
            controller.window?.close()
        }
        XCTAssertTrue(appDelegate.windowControllers.isEmpty)
        appDelegate.settingsWindowController?.close()
        try appDelegate.replaceCore(start: false)
        // The app's default, as the next test (and the app's launch) expects.
        AppDelegate.applyAppAppearance(.dark)
        if let previousConfigDir {
            setenv("TESSERA_CONFIG_DIR", previousConfigDir, 1)
        } else {
            unsetenv("TESSERA_CONFIG_DIR")
        }
        try? FileManager.default.removeItem(at: dir)
        appDelegate.spawnOptions = .init()
        appDelegate = nil
    }

    // MARK: Helpers

    private var configFile: URL { dir.appendingPathComponent("config.toml") }

    private func write(_ config: String) throws {
        try config.write(to: configFile, atomically: true, encoding: .utf8)
    }

    private static func config(_ value: String?, translucent: Bool = false) -> String {
        var lines = ["[appearance]"]
        if let value { lines.append("app_appearance = \"\(value)\"") }
        if translucent { lines += ["opacity = 0.6", "blur = true"] }
        return lines.joined(separator: "\n") + "\n"
    }

    /// Waits up to 10 s for `condition`.
    private func waitUntil(_ what: String, _ condition: () -> Bool) async throws {
        let deadline = ContinuousClock.now + .seconds(10)
        while !condition() {
            guard ContinuousClock.now < deadline else { return XCTFail("timed out: \(what)") }
            try await Task.sleep(for: .milliseconds(10))
        }
    }

    /// The system's own Light or Dark setting, read from the defaults, not
    /// from AppKit, which the app's appearance overrides.
    private static var systemIsDark: Bool {
        UserDefaults.standard.string(forKey: "AppleInterfaceStyle")?.caseInsensitiveCompare("dark") == .orderedSame
    }

    /// Dark or light, as `appearance` resolves (a high-contrast or vibrant
    /// variant counts as its base).
    private static func isDark(_ appearance: NSAppearance) -> Bool {
        appearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua
    }

    /// Whether the value makes the app draw dark.
    private static func drawsDark(_ value: String?) -> Bool {
        switch value {
        case "light": false
        case "system": systemIsDark
        default: true
        }
    }

    /// What `NSApp.appearance` is for `value`: `nil` follows the system.
    private static func appearanceName(_ value: String?) -> NSAppearance.Name? {
        switch value {
        case "light": .aqua
        case "system": nil
        default: .darkAqua
        }
    }

    private func assertDraws(
        dark: Bool, _ view: NSView?, _ what: String, file: StaticString = #filePath, line: UInt = #line
    ) {
        guard let view else { return XCTFail("\(what): missing", file: file, line: line) }
        XCTAssertNil(view.appearance, "\(what) sets no appearance of its own", file: file, line: line)
        XCTAssertEqual(
            Self.isDark(view.effectiveAppearance), dark, "\(what): \(view.effectiveAppearance.name.rawValue)",
            file: file, line: line)
    }

    private func assertDraws(
        dark: Bool, _ window: NSWindow?, _ what: String, file: StaticString = #filePath, line: UInt = #line
    ) {
        guard let window else { return XCTFail("\(what): missing", file: file, line: line) }
        XCTAssertNil(window.appearance, "\(what) sets no appearance of its own", file: file, line: line)
        XCTAssertEqual(
            Self.isDark(window.effectiveAppearance), dark, "\(what): \(window.effectiveAppearance.name.rawValue)",
            file: file, line: line)
    }

    /// Every terminal window open now and, in each, the pane's blur and the
    /// strip's blur and tint, which a translucent window has once its panes
    /// have a renderer (without one a pane counts as opaque).
    private func assertEveryWindowDraws(
        dark: Bool, translucent: Bool = true, _ what: String, file: StaticString = #filePath, line: UInt = #line
    ) {
        XCTAssertFalse(appDelegate.windowControllers.isEmpty, "\(what): windows open", file: file, line: line)
        for (index, controller) in appDelegate.windowControllers.enumerated() {
            let name = "\(what), window \(index)"
            assertDraws(dark: dark, controller.window, name, file: file, line: line)
            let content = controller.window?.contentView as? TerminalWindowContentView
            guard translucent, controller.panes.allSatisfy(\.view.surface.isAttached) else { continue }
            assertDraws(dark: dark, content?.titlebarBackdrop, "\(name): the title bar's blur", file: file, line: line)
            assertDraws(dark: dark, content?.titlebarTint, "\(name): the title bar's tint", file: file, line: line)
            for pane in controller.panes {
                assertDraws(dark: dark, pane.view.backdrop, "\(name): the pane's blur", file: file, line: line)
            }
        }
    }

    // MARK: Each value

    /// For each value, the app's appearance and, opened the way the app
    /// opens them (New Window, then a tab of it as ⌘T opens one), a terminal
    /// window and its tab, their blur, their find bar, a close sheet and
    /// the Settings window draw as the value says: no key and an unknown
    /// value are dark, `system` is `nil` (the system's). The previous value
    /// is another each time, so each one is a change.
    func testEachValueReachesEveryWindowTheAppOpens() async throws {
        for value in ["light", nil, "system", "light", "dark", "light", "unknown"] {
            let what = value ?? "no key"
            try write(Self.config(value, translucent: true))
            try appDelegate.replaceCore(start: false)
            MainMenu.run(command: "new_window")
            let first = try XCTUnwrap(appDelegate.windowControllers.last, "\(what): New Window opened a window")
            XCTAssertEqual(
                NSApp.appearance?.name, value == "unknown" ? .darkAqua : Self.appearanceName(value), "\(what): the app")
            let window = try XCTUnwrap(first.window)
            let tab = try appDelegate.newTab(in: window)
            XCTAssertTrue(tab.window?.tabGroup === window.tabGroup, "\(what): a tab of the window")
            let dark = Self.drawsDark(value)
            XCTAssertEqual(Self.isDark(NSApp.effectiveAppearance), dark, "\(what): the app draws")
            assertEveryWindowDraws(dark: dark, what)

            let view = first.focusedPane.view
            view.showFindBar(nil)
            assertDraws(dark: dark, view.findBar, "\(what): the find bar")

            AlertCloseConfirmationPresenter().confirm(
                CloseConfirmation(scope: .tab, processNames: ["sleep"]), sheetFor: window
            ) { _ in }
            let sheet = try XCTUnwrap(window.attachedSheet, "\(what): the alert is a sheet of the window")
            assertDraws(dark: dark, sheet, "\(what): the sheet")
            window.endSheet(sheet, returnCode: .alertSecondButtonReturn)

            MainMenu.run(command: "settings")
            assertDraws(dark: dark, appDelegate.settingsWindowController?.window, "\(what): Settings")
            appDelegate.settingsWindowController?.close()

            for controller in appDelegate.windowControllers {
                controller.window?.close()
            }
            XCTAssertTrue(appDelegate.windowControllers.isEmpty)
        }
    }

    // MARK: Live reload

    /// The trigger (#365, wave 18 P5): an edit of `config.toml` on disk,
    /// which the core's watch applies, changes every open window, two
    /// standalone windows and a tab, with nothing posted or called by the
    /// test. The strip keeps the theme's tint whatever the appearance.
    func testAnEditOfTheFileChangesEveryOpenWindow() async throws {
        try write(Self.config("dark", translucent: true))
        try appDelegate.replaceCore(start: false)
        let first = try appDelegate.newWindow()
        _ = try appDelegate.newTab(in: first.window)
        _ = try appDelegate.newWindow()
        XCTAssertEqual(appDelegate.windowControllers.count, 3)
        assertEveryWindowDraws(dark: true, "dark")
        let content = try XCTUnwrap(first.window?.contentView as? TerminalWindowContentView)
        let tint = content.titlebarTint?.segments
        if first.panes.allSatisfy(\.view.surface.isAttached) {
            XCTAssertEqual(tint?.isEmpty, false, "a translucent window's strip is tinted")
        }

        try write(Self.config("light", translucent: true))
        try await waitUntil("the reload makes the app light") { NSApp.appearance?.name == .aqua }
        assertEveryWindowDraws(dark: false, "light")
        XCTAssertEqual(content.titlebarTint?.segments, tint, "the strip keeps the theme's tint")

        try write(Self.config("system", translucent: true))
        try await waitUntil("the reload makes the app follow the system") { NSApp.appearance == nil }
        assertEveryWindowDraws(dark: Self.systemIsDark, "system")

        try write(Self.config(nil, translucent: true))
        try await waitUntil("the key gone, the app is dark again") { NSApp.appearance?.name == .darkAqua }
        assertEveryWindowDraws(dark: true, "no key")
        XCTAssertEqual(content.titlebarTint?.segments, tint, "the strip keeps the theme's tint")
    }

    // MARK: Settings

    /// The Settings picker (Appearance, App) writes `app_appearance` to
    /// `config.toml` through the model, and the open windows and Settings
    /// itself change once the core applies it, with no restart.
    func testTheSettingsPickerWritesTheKeyAndTheWindowsFollow() async throws {
        try write(Self.config(nil))
        try appDelegate.replaceCore(start: true)
        _ = try appDelegate.newWindow()
        MainMenu.run(command: "settings")
        let model = try XCTUnwrap(appDelegate.settingsWindowController?.model, "Settings opened")
        let field = AppearancePane.app
        XCTAssertEqual(field.key, "appearance.app_appearance")
        XCTAssertEqual(field.choices(model)?.map(\.value), ["dark", "light", "system"])
        XCTAssertEqual(model.value(field.key), .string("dark"), "the default")
        assertEveryWindowDraws(dark: true, translucent: false, "before")

        field.stringBinding(.init(model: model, path: field.key)).wrappedValue = "light"
        try await waitUntil("written") {
            ((try? String(contentsOf: self.configFile, encoding: .utf8)) ?? "").contains(#"app_appearance = "light""#)
        }
        try await waitUntil("applied") { NSApp.appearance?.name == .aqua }
        assertEveryWindowDraws(dark: false, translucent: false, "after")
        assertDraws(dark: false, appDelegate.settingsWindowController?.window, "Settings")
        XCTAssertEqual(model.value(field.key), .string("light"))
    }
}
