import AppKit

/// The menu bar: Tessera, Shell, Edit, View, Window and Help, laid out after
/// Terminal.app (D1). Every item sends its action up the responder chain (no
/// fixed target), so the terminal items (Clear to Start, the Edit and View
/// items) reach the key window's ``TerminalSurfaceView``, the tab items its
/// window, and the application items ``AppDelegate``. The key equivalents are
/// fixed until the configurable key map (#43).
@MainActor
enum MainMenu {
    /// Number of tabs reachable with ⌘1 to ⌘9.
    static let numberedTabs = 9

    /// Builds the menu bar, installs it and registers its Window menu with
    /// AppKit (which appends the window list to it).
    static func install(in app: NSApplication) {
        let windowMenu = makeWindowMenu()
        let helpMenu = makeHelpMenu()
        let mainMenu = NSMenu()
        for menu in [makeAppMenu(), makeShellMenu(), makeEditMenu(), makeViewMenu(), windowMenu, helpMenu] {
            mainMenu.addItem(submenuItem(menu))
        }
        app.mainMenu = mainMenu
        app.windowsMenu = windowMenu
        app.helpMenu = helpMenu
    }

    private static func makeAppMenu() -> NSMenu {
        let menu = NSMenu(title: "Tessera")
        menu.addItem(withTitle: "About Tessera", action: #selector(NSApplication.orderFrontStandardAboutPanel(_:)), keyEquivalent: "")
        menu.addItem(.separator())
        // Opens config.toml in the default editor until the Preferences
        // window (#40) replaces it (D6). Titled "Settings…": AppKit retitles
        // a "Preferences…" item with ⌘, to it anyway (macOS 13 and later).
        menu.addItem(withTitle: "Settings…", action: #selector(AppDelegate.openConfigFile(_:)), keyEquivalent: ",")
        menu.addItem(.separator())
        menu.addItem(withTitle: "Hide Tessera", action: #selector(NSApplication.hide(_:)), keyEquivalent: "h")
        menu.addItem(withTitle: "Hide Others", action: #selector(NSApplication.hideOtherApplications(_:)), keyEquivalent: "h")
            .keyEquivalentModifierMask = [.command, .option]
        menu.addItem(withTitle: "Show All", action: #selector(NSApplication.unhideAllApplications(_:)), keyEquivalent: "")
        menu.addItem(.separator())
        menu.addItem(withTitle: "Quit Tessera", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")
        return menu
    }

    private static func makeShellMenu() -> NSMenu {
        let menu = NSMenu(title: "Shell")
        menu.addItem(withTitle: "New Window", action: #selector(AppDelegate.openNewWindow(_:)), keyEquivalent: "n")
        menu.addItem(withTitle: "New Tab", action: #selector(AppDelegate.openNewTab(_:)), keyEquivalent: "t")
        menu.addItem(.separator())
        // Closes the key window, which is the selected tab; AppKit closes the
        // tab bar's window once its last tab is gone.
        menu.addItem(withTitle: "Close Tab", action: #selector(NSWindow.performClose(_:)), keyEquivalent: "w")
        menu.addItem(withTitle: "Close Window", action: #selector(TerminalWindowController.closeTabGroup(_:)), keyEquivalent: "w")
            .keyEquivalentModifierMask = [.command, .shift]
        menu.addItem(.separator())
        menu.addItem(withTitle: "Clear to Start", action: #selector(TerminalSurfaceView.clearScrollback(_:)), keyEquivalent: "k")
        return menu
    }

    /// Copy and Select All stay disabled until the selection model (#30);
    /// paste goes to the session, which sanitises it and brackets it when the
    /// terminal asks (D5).
    private static func makeEditMenu() -> NSMenu {
        let menu = NSMenu(title: "Edit")
        menu.addItem(withTitle: "Copy", action: #selector(TerminalSurfaceView.copy(_:)), keyEquivalent: "c")
        menu.addItem(withTitle: "Paste", action: #selector(TerminalSurfaceView.paste(_:)), keyEquivalent: "v")
        menu.addItem(withTitle: "Paste as Plain Text", action: #selector(TerminalSurfaceView.pasteAsPlainText(_:)), keyEquivalent: "v")
            .keyEquivalentModifierMask = [.command, .option, .shift]
        menu.addItem(withTitle: "Select All", action: #selector(TerminalSurfaceView.selectAll(_:)), keyEquivalent: "a")
        return menu
    }

    /// Font zoom for the key window's terminal, 1 pt a step (D2), and the
    /// tab bar.
    private static func makeViewMenu() -> NSMenu {
        let menu = NSMenu(title: "View")
        menu.addItem(withTitle: "Bigger", action: #selector(TerminalSurfaceView.increaseFontSize(_:)), keyEquivalent: "+")
        // ⌘= as well (D3): "+" is a shifted key on US and many other
        // layouts, and ⌘= is what users press for it. Hidden items take part
        // in key equivalents only with `allowsKeyEquivalentWhenHidden`.
        let equals = menu.addItem(withTitle: "Bigger", action: #selector(TerminalSurfaceView.increaseFontSize(_:)), keyEquivalent: "=")
        equals.isHidden = true
        equals.allowsKeyEquivalentWhenHidden = true
        menu.addItem(withTitle: "Smaller", action: #selector(TerminalSurfaceView.decreaseFontSize(_:)), keyEquivalent: "-")
        menu.addItem(withTitle: "Actual Size", action: #selector(TerminalSurfaceView.resetFontSize(_:)), keyEquivalent: "0")
        menu.addItem(.separator())
        // In View, as in Terminal.app and Safari, where AppKit adds Show All
        // Tabs and Enter Full Screen. It retitles this "Hide Tab Bar" while
        // the bar is shown.
        menu.addItem(withTitle: "Show Tab Bar", action: #selector(NSWindow.toggleTabBar(_:)), keyEquivalent: "")
        return menu
    }

    private static func makeWindowMenu() -> NSMenu {
        let menu = NSMenu(title: "Window")
        menu.addItem(withTitle: "Minimize", action: #selector(NSWindow.performMiniaturize(_:)), keyEquivalent: "m")
        menu.addItem(withTitle: "Zoom", action: #selector(NSWindow.performZoom(_:)), keyEquivalent: "")
        menu.addItem(.separator())
        // "{" and "}" are ⇧[ and ⇧]: the menu shows them as ⇧⌘[ and ⇧⌘].
        menu.addItem(withTitle: "Show Previous Tab", action: #selector(NSWindow.selectPreviousTab(_:)), keyEquivalent: "{")
        menu.addItem(withTitle: "Show Next Tab", action: #selector(NSWindow.selectNextTab(_:)), keyEquivalent: "}")
        menu.addItem(withTitle: "Move Tab to New Window", action: #selector(NSWindow.moveTabToNewWindow(_:)), keyEquivalent: "")
        menu.addItem(withTitle: "Merge All Windows", action: #selector(NSWindow.mergeAllWindows(_:)), keyEquivalent: "")
        menu.addItem(submenuItem(makeSelectTabMenu()))
        menu.addItem(.separator())
        menu.addItem(withTitle: "Bring All to Front", action: #selector(NSApplication.arrangeInFront(_:)), keyEquivalent: "")
        return menu
    }

    private static func makeHelpMenu() -> NSMenu {
        let menu = NSMenu(title: "Help")
        // `openHelp(_:)`, not AppKit's `showHelp(_:)`: NSApplication answers
        // that one first, and there is no help book.
        menu.addItem(withTitle: "Tessera Help", action: #selector(AppDelegate.openHelp(_:)), keyEquivalent: "?")
        return menu
    }

    /// ⌘1 to ⌘9 select the first nine tabs of the key window's tab group.
    private static func makeSelectTabMenu() -> NSMenu {
        let menu = NSMenu(title: "Select Tab")
        for index in 0..<numberedTabs {
            let item = menu.addItem(
                withTitle: "Tab \(index + 1)",
                action: #selector(TerminalWindowController.selectTabAtIndex(_:)),
                keyEquivalent: "\(index + 1)"
            )
            item.tag = index
        }
        return menu
    }

    private static func submenuItem(_ menu: NSMenu) -> NSMenuItem {
        let item = NSMenuItem(title: menu.title, action: nil, keyEquivalent: "")
        item.submenu = menu
        return item
    }
}
