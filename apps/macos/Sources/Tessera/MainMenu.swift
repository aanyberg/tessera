import AppKit

/// The menu bar. Every item sends its action up the responder chain (no
/// fixed target), so tab items act on the key window and the application
/// items reach ``AppDelegate``.
@MainActor
enum MainMenu {
    /// Number of tabs reachable with ⌘1 to ⌘9.
    static let numberedTabs = 9

    /// Builds the menu bar, installs it and registers its Window menu with
    /// AppKit (which appends the window list to it).
    static func install(in app: NSApplication) {
        let windowMenu = makeWindowMenu()
        let mainMenu = NSMenu()
        mainMenu.addItem(submenuItem(makeAppMenu()))
        mainMenu.addItem(submenuItem(makeShellMenu()))
        mainMenu.addItem(submenuItem(windowMenu))
        app.mainMenu = mainMenu
        app.windowsMenu = windowMenu
    }

    private static func makeAppMenu() -> NSMenu {
        let menu = NSMenu(title: "Tessera")
        menu.addItem(withTitle: "About Tessera", action: #selector(NSApplication.orderFrontStandardAboutPanel(_:)), keyEquivalent: "")
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
        // AppKit retitles this "Hide Tab Bar" while the bar is shown.
        menu.addItem(withTitle: "Show Tab Bar", action: #selector(NSWindow.toggleTabBar(_:)), keyEquivalent: "")
        menu.addItem(submenuItem(makeSelectTabMenu()))
        menu.addItem(.separator())
        menu.addItem(withTitle: "Bring All to Front", action: #selector(NSApplication.arrangeInFront(_:)), keyEquivalent: "")
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
