import AppKit
import XCTest

@testable import Tessera

/// The app is always dark, whatever the system's Light or Dark setting
/// (ADR 0043, #364). Nothing here sets an appearance: the app's own launch
/// does, and every window opened the way the app opens one inherits it.
@MainActor
final class AppearanceTests: XCTestCase {
    private var appDelegate: AppDelegate!
    private var dir: URL?
    private var previousConfigDir: String?
    private var replacedCore = false

    override func setUp() async throws {
        appDelegate = try XCTUnwrap(NSApp.delegate as? AppDelegate)
        appDelegate.spawnOptions = .init(program: "/bin/sh", arguments: ["-i"])
        XCTAssertTrue(appDelegate.windowControllers.isEmpty, "a previous test left windows open")
        XCTAssertNil(appDelegate.settingsWindowController, "a previous test left Settings open")
        previousConfigDir = ProcessInfo.processInfo.environment["TESSERA_CONFIG_DIR"]
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
        XCTAssertNil(appDelegate.settingsWindowController)
        if replacedCore {
            try appDelegate.replaceCore(start: false)
        }
        if let previousConfigDir {
            setenv("TESSERA_CONFIG_DIR", previousConfigDir, 1)
        } else {
            unsetenv("TESSERA_CONFIG_DIR")
        }
        if let dir { try? FileManager.default.removeItem(at: dir) }
        appDelegate.spawnOptions = .init()
        appDelegate = nil
    }

    /// Dark or light, as `appearance` resolves (an Increase Contrast dark
    /// appearance counts as dark).
    private static func isDark(_ appearance: NSAppearance) -> Bool {
        appearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua
    }

    /// The app's own appearance is set, not inherited from the system: on a
    /// host in Dark mode an unset `NSApp.appearance` would still look dark.
    func testTheAppSetsTheDarkAppearance() {
        XCTAssertEqual(NSApp.appearance?.name, .darkAqua)
        XCTAssertTrue(Self.isDark(NSApp.effectiveAppearance))
    }

    /// A terminal window opened from File > New Window, its find bar and the
    /// close confirmation's sheet as the app shows it are all dark.
    func testATerminalWindowItsFindBarAndItsCloseSheetAreDark() throws {
        MainMenu.run(command: "new_window")
        let controller = try XCTUnwrap(appDelegate.windowControllers.last, "New Window opened a window")
        let window = try XCTUnwrap(controller.window)
        XCTAssertNil(window.appearance, "the window inherits the app's appearance")
        XCTAssertTrue(Self.isDark(window.effectiveAppearance), "\(window.effectiveAppearance.name.rawValue)")

        let view = controller.focusedPane.view
        view.showFindBar(nil)
        let bar = try XCTUnwrap(view.findBar)
        XCTAssertTrue(Self.isDark(bar.effectiveAppearance), "\(bar.effectiveAppearance.name.rawValue)")

        AlertCloseConfirmationPresenter().confirm(
            CloseConfirmation(scope: .tab, processNames: ["sleep"]), sheetFor: window
        ) { _ in }
        let sheet = try XCTUnwrap(window.attachedSheet, "the alert is a sheet of the window")
        XCTAssertTrue(Self.isDark(sheet.effectiveAppearance), "\(sheet.effectiveAppearance.name.rawValue)")
        window.endSheet(sheet, returnCode: .alertSecondButtonReturn)
    }

    /// The Settings window opened from its menu item is dark.
    func testTheSettingsWindowIsDark() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent(
            "tessera-appearance-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        self.dir = dir
        try "".write(to: dir.appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
        setenv("TESSERA_CONFIG_DIR", dir.path, 1)
        replacedCore = true
        try appDelegate.replaceCore(start: true)

        MainMenu.run(command: "settings")
        let window = try XCTUnwrap(appDelegate.settingsWindowController?.window, "Settings opened")
        XCTAssertNil(window.appearance, "the window inherits the app's appearance")
        XCTAssertTrue(Self.isDark(window.effectiveAppearance), "\(window.effectiveAppearance.name.rawValue)")
    }
}
