import AppKit
import TesseraKit
import os

/// The menu bar: Tessera, Shell, Edit, View, Window and Help, laid out after
/// Terminal.app (D1). Every item sends its action up the responder chain (no
/// fixed target), so the terminal items (Clear to Start, the Edit and View
/// items) reach the key window's ``TerminalSurfaceView``, the tab items its
/// window, and the application items ``AppDelegate``.
///
/// Each item's `identifier` is its command's name in the core's registry
/// (`new_tab`, `select_tab:3`; ADR 0036). The key equivalents built here are
/// the registry's defaults; ``apply(_:to:)`` replaces them with the key map's
/// once the core runs and after every configuration it applies. A command
/// with several menu chords gets hidden alternates after its item
/// (``aliasPrefix``).
@MainActor
enum MainMenu {
    /// Number of tabs reachable with ⌘1 to ⌘9.
    static let numberedTabs = 9

    /// The identifier prefix of a hidden alternate: `alias:` and its
    /// command's name.
    static let aliasPrefix = "alias:"

    private static let log = Logger(subsystem: "com.aanyberg.tessera", category: "menu")

    /// Builds the menu bar, installs it and registers its Window menu with
    /// AppKit (which appends the window list to it).
    static func install(in app: NSApplication) {
        let (mainMenu, windowMenu, helpMenu) = makeMenuBar()
        app.mainMenu = mainMenu
        app.windowsMenu = windowMenu
        app.helpMenu = helpMenu
    }

    /// The menu bar with the default key equivalents, and its Window and
    /// Help menus.
    static func makeMenuBar() -> (bar: NSMenu, window: NSMenu, help: NSMenu) {
        let windowMenu = makeWindowMenu()
        let helpMenu = makeHelpMenu()
        let bar = NSMenu()
        for menu in [makeAppMenu(), makeShellMenu(), makeEditMenu(), makeViewMenu(), windowMenu, helpMenu] {
            bar.addItem(submenuItem(menu))
        }
        return (bar, windowMenu, helpMenu)
    }

    private static func makeAppMenu() -> NSMenu {
        let menu = NSMenu(title: "Tessera")
        add(menu, "about", "About Tessera", #selector(NSApplication.orderFrontStandardAboutPanel(_:)))
        menu.addItem(.separator())
        // The Settings window (ADR 0044, #410): "Settings", no ellipsis.
        // config.toml opens from the window's Advanced pane.
        add(menu, "settings", "Settings", #selector(AppDelegate.openSettings(_:)), ",")
        menu.addItem(.separator())
        add(menu, "hide", "Hide Tessera", #selector(NSApplication.hide(_:)), "h")
        add(menu, "hide_others", "Hide Others", #selector(NSApplication.hideOtherApplications(_:)), "h", [.command, .option])
        add(menu, "show_all", "Show All", #selector(NSApplication.unhideAllApplications(_:)))
        menu.addItem(.separator())
        add(menu, "quit", "Quit Tessera", #selector(NSApplication.terminate(_:)), "q")
        return menu
    }

    private static func makeShellMenu() -> NSMenu {
        let menu = NSMenu(title: "Shell")
        add(menu, "new_window", "New Window", #selector(AppDelegate.openNewWindow(_:)), "n")
        add(menu, "new_tab", "New Tab", #selector(AppDelegate.openNewTab(_:)), "t")
        // The configured profiles (#39); see `setProfiles(_:in:)`.
        menu.addItem(profileMenuItem(newWindowWithProfile, "New Window with Profile"))
        menu.addItem(profileMenuItem(newTabWithProfile, "New Tab with Profile"))
        menu.addItem(.separator())
        // Closes the key window, which is the selected tab; AppKit closes the
        // tab bar's window once its last tab is gone.
        add(menu, "close_tab", "Close Tab", #selector(NSWindow.performClose(_:)), "w")
        add(menu, "close_window", "Close Window", #selector(TerminalWindowController.closeTabGroup(_:)), "w", [.command, .shift])
        menu.addItem(.separator())
        add(menu, "clear_scrollback", "Clear to Start", #selector(TerminalSurfaceView.clearScrollback(_:)), "k")
        PaneMenu.addItems(to: menu)
        return menu
    }

    /// Copy and Copy with Formatting are enabled while something is
    /// selected, Select All always;
    /// paste goes to the session, which sanitises it and brackets it when the
    /// terminal asks (D5).
    private static func makeEditMenu() -> NSMenu {
        let menu = NSMenu(title: "Edit")
        add(menu, "copy", "Copy", #selector(TerminalSurfaceView.copy(_:)), "c")
        add(menu, "copy_with_formatting", "Copy with Formatting", #selector(TerminalSurfaceView.copyWithFormatting(_:)))
        add(menu, "paste", "Paste", #selector(TerminalSurfaceView.paste(_:)), "v")
        add(menu, "paste_plain", "Paste as Plain Text", #selector(TerminalSurfaceView.pasteAsPlainText(_:)), "v", [.command, .option, .shift])
        add(menu, "select_all", "Select All", #selector(TerminalSurfaceView.selectAll(_:)), "a")
        menu.addItem(.separator())
        menu.addItem(submenuItem(makeFindMenu()))
        return menu
    }

    /// Edit > Find, for the focused pane's search (ADR 0038): Find Next
    /// (⌘G) moves toward older output, Find Previous (⇧⌘G) toward newer
    /// (D16a). The actions are the terminal view's, so they reach it from
    /// the find bar's field too.
    private static func makeFindMenu() -> NSMenu {
        let menu = NSMenu(title: "Find")
        add(menu, "find", "Find…", #selector(TerminalSurfaceView.showFindBar(_:)), "f")
        add(menu, "find_next", "Find Next", #selector(TerminalSurfaceView.findNextMatch(_:)), "g")
        add(menu, "find_previous", "Find Previous", #selector(TerminalSurfaceView.findPreviousMatch(_:)), "g", [.command, .shift])
        add(menu, "use_selection_for_find", "Use Selection for Find", #selector(TerminalSurfaceView.useSelectionForFind(_:)), "e")
        menu.addItem(.separator())
        add(menu, "copy_find_matches", "Copy All Matches", #selector(TerminalSurfaceView.copyAllFindMatches(_:)))
        return menu
    }

    /// Font zoom for the key window's terminal, 1 pt a step (D2), moving
    /// through its scrollback, and the tab bar.
    private static func makeViewMenu() -> NSMenu {
        let menu = NSMenu(title: "View")
        let bigger = add(menu, "font_bigger", "Bigger", #selector(TerminalSurfaceView.increaseFontSize(_:)), "+")
        // ⌘= as well (D3): "+" is a shifted key on US and many other
        // layouts, and ⌘= is what users press for it.
        menu.addItem(alias(of: bigger, .init(keyEquivalent: "=", modifiers: .command)))
        add(menu, "font_smaller", "Smaller", #selector(TerminalSurfaceView.decreaseFontSize(_:)), "-")
        add(menu, "font_reset", "Actual Size", #selector(TerminalSurfaceView.resetFontSize(_:)), "0")
        menu.addItem(.separator())
        // The scrollback, as in Terminal.app's View menu; ⌘↑ and ⌘↓ page
        // (wave 7 decision) or, with prompt marks, jump between prompts
        // (D4). Home and End are ⌘fn← and ⌘fn→ on a laptop.
        let scrollItems: [(String, String, Selector, Int)] = [
            ("scroll_to_top", "Scroll to Top", #selector(TerminalSurfaceView.scrollToTop(_:)), NSHomeFunctionKey),
            ("scroll_to_bottom", "Scroll to Bottom", #selector(TerminalSurfaceView.scrollToBottom(_:)), NSEndFunctionKey),
            ("scroll_page_up", "Page Up", #selector(TerminalSurfaceView.scrollPageUpInHistory(_:)), NSUpArrowFunctionKey),
            ("scroll_page_down", "Page Down", #selector(TerminalSurfaceView.scrollPageDownInHistory(_:)), NSDownArrowFunctionKey),
        ]
        for (command, title, action, key) in scrollItems {
            add(menu, command, title, action, functionKey(key))
        }
        // The shell's prompts, by its OSC 133 marks (ADR 0039). No key
        // equivalents of their own: ⌘↑ and ⌘↓, the chords of Page Up and
        // Page Down, run them when the terminal has the key (the core's
        // prompt rule), and they page without marks.
        add(menu, "prompt_previous", "Previous Prompt", #selector(TerminalSurfaceView.scrollToPreviousPrompt(_:)))
        add(menu, "prompt_next", "Next Prompt", #selector(TerminalSurfaceView.scrollToNextPrompt(_:)))
        add(menu, "select_last_output", "Select Last Output", #selector(TerminalSurfaceView.selectLastOutput(_:)))
        menu.addItem(.separator())
        // In View, as in Terminal.app and Safari, where AppKit adds Show All
        // Tabs and Enter Full Screen. It retitles this "Hide Tab Bar" while
        // the bar is shown.
        add(menu, "show_tab_bar", "Show Tab Bar", #selector(NSWindow.toggleTabBar(_:)))
        return menu
    }

    private static func makeWindowMenu() -> NSMenu {
        let menu = NSMenu(title: "Window")
        add(menu, "minimize", "Minimize", #selector(NSWindow.performMiniaturize(_:)), "m")
        add(menu, "zoom", "Zoom", #selector(NSWindow.performZoom(_:)))
        menu.addItem(.separator())
        // "{" and "}" are ⇧[ and ⇧]: the menu shows them as ⇧⌘[ and ⇧⌘].
        add(menu, "tab_previous", "Show Previous Tab", #selector(NSWindow.selectPreviousTab(_:)), "{")
        add(menu, "tab_next", "Show Next Tab", #selector(NSWindow.selectNextTab(_:)), "}")
        add(menu, "move_tab_to_new_window", "Move Tab to New Window", #selector(NSWindow.moveTabToNewWindow(_:)))
        add(menu, "merge_all_windows", "Merge All Windows", #selector(NSWindow.mergeAllWindows(_:)))
        menu.addItem(submenuItem(makeSelectTabMenu()))
        menu.addItem(.separator())
        add(menu, "bring_all_to_front", "Bring All to Front", #selector(NSApplication.arrangeInFront(_:)))
        return menu
    }

    private static func makeHelpMenu() -> NSMenu {
        let menu = NSMenu(title: "Help")
        // `openHelp(_:)`, not AppKit's `showHelp(_:)`: NSApplication answers
        // that one first, and there is no help book.
        add(menu, "help", "Tessera Help", #selector(AppDelegate.openHelp(_:)), "?")
        return menu
    }

    /// ⌘1 to ⌘9 select the first nine tabs of the key window's tab group.
    private static func makeSelectTabMenu() -> NSMenu {
        let menu = NSMenu(title: "Select Tab")
        for index in 0..<numberedTabs {
            let item = add(
                menu, "select_tab:\(index + 1)", "Tab \(index + 1)",
                #selector(TerminalWindowController.selectTabAtIndex(_:)), "\(index + 1)")
            item.tag = index
        }
        return menu
    }

    /// Adds an item for `command` (its `identifier`) with the registry's
    /// default key equivalent.
    @discardableResult
    private static func add(
        _ menu: NSMenu, _ command: String, _ title: String, _ action: Selector, _ key: String = "",
        _ modifiers: NSEvent.ModifierFlags = .command
    ) -> NSMenuItem {
        let item = menu.addItem(withTitle: title, action: action, keyEquivalent: key)
        item.keyEquivalentModifierMask = modifiers
        item.identifier = NSUserInterfaceItemIdentifier(command)
        return item
    }

    /// A hidden alternate of `item` for another chord of its command: the
    /// same title, action and tag. Hidden items take part in key
    /// equivalents only with `allowsKeyEquivalentWhenHidden`.
    private static func alias(of item: NSMenuItem, _ shortcut: TesseraKeymap.MenuShortcut) -> NSMenuItem {
        let alias = NSMenuItem(title: item.title, action: item.action, keyEquivalent: shortcut.keyEquivalent)
        alias.keyEquivalentModifierMask = shortcut.modifiers
        alias.tag = item.tag
        alias.identifier = NSUserInterfaceItemIdentifier(aliasPrefix + (item.identifier?.rawValue ?? ""))
        alias.isHidden = true
        alias.allowsKeyEquivalentWhenHidden = true
        return alias
    }

    // MARK: Key map

    /// Gives every item of `bar` whose `identifier` names a command the key
    /// map's menu chords: the first as its key equivalent (none if the
    /// command has none, as when `[keys]` rebinds or unbinds its chord), the
    /// rest as hidden alternates after it. Idempotent: earlier alternates
    /// are removed first. Items the key map does not name (the ones `AppKit`
    /// adds) are left alone.
    static func apply(_ keymap: TesseraKeymap, to bar: NSMenu) {
        for menu in menus(of: bar) {
            for item in menu.items where item.identifier?.rawValue.hasPrefix(aliasPrefix) ?? false {
                menu.removeItem(item)
            }
            for item in menu.items {
                guard let name = item.identifier?.rawValue, let command = keymap.command(named: name) else { continue }
                item.keyEquivalent = command.menu.first?.keyEquivalent ?? ""
                item.keyEquivalentModifierMask = command.menu.first?.modifiers ?? []
                var index = menu.index(of: item)
                for shortcut in command.menu.dropFirst() {
                    index += 1
                    menu.insertItem(alias(of: item, shortcut), at: index)
                }
            }
        }
    }

    /// The item of `bar` (searched with its submenus) that runs `command`,
    /// not one of its hidden alternates.
    static func item(forCommand command: String, in bar: NSMenu?) -> NSMenuItem? {
        guard let bar else { return nil }
        let identifier = NSUserInterfaceItemIdentifier(command)
        for menu in menus(of: bar) {
            if let item = menu.items.first(where: { $0.identifier == identifier }) { return item }
        }
        return nil
    }

    /// Whether `AppKit` will match `event` to the item of `bar` that runs
    /// `command` or to one of its hidden alternates: the item's key
    /// equivalent (as localised, which is what `AppKit` compares) is the
    /// event's key, ignoring case for a letter, and the modifiers agree
    /// (Shift aside for a symbol the key types with Shift, as ⌘? from ⇧⌘/).
    /// Strict: when in doubt it says no, and the caller runs the command
    /// itself, so it never runs twice or not at all. A keypad key is
    /// compared as its character, as `AppKit` does.
    static func keyEquivalent(of command: String, in bar: NSMenu?, matches event: NSEvent) -> Bool {
        guard let item = item(forCommand: command, in: bar), let menu = item.menu,
            let typed = event.charactersIgnoringModifiers, !typed.isEmpty
        else { return false }
        let relevant: NSEvent.ModifierFlags = [.command, .control, .option, .shift]
        let flags = event.modifierFlags.intersection(relevant)
        let names = [command, aliasPrefix + command]
        return menu.items.contains { candidate in
            guard let name = candidate.identifier?.rawValue, names.contains(name) else { return false }
            let key = candidate.keyEquivalent
            guard !key.isEmpty else { return false }
            var mask = candidate.keyEquivalentModifierMask.intersection(relevant)
            if key != key.lowercased() { mask.insert(.shift) }
            return (key.lowercased() == typed.lowercased() && flags == mask)
                || (key == typed && flags.subtracting(.shift) == mask.subtracting(.shift))
        }
    }

    /// Runs `command` as choosing its menu item would, if the item is
    /// enabled (validated first, as `AppKit` does for a key equivalent);
    /// beeps if it is not. A command with no menu item (one a later version
    /// runs) does nothing. `bar` and `beep` are the installed menus and the
    /// system beep; tests pass their own.
    static func run(
        command: String, in bar: NSMenu? = NSApp.mainMenu, beep: () -> Void = { NSSound.beep() }
    ) {
        guard let item = MainMenu.item(forCommand: command, in: bar), let menu = item.menu else {
            log.debug("no menu item runs \(command, privacy: .public)")
            return
        }
        menu.update()
        guard item.isEnabled else {
            beep()
            return
        }
        menu.performActionForItem(at: menu.index(of: item))
    }

    /// `bar` and every submenu under it, depth first.
    private static func menus(of bar: NSMenu) -> [NSMenu] {
        [bar] + bar.items.compactMap(\.submenu).flatMap(menus(of:))
    }

    /// The key equivalent for a function key (`NSUpArrowFunctionKey` and
    /// the like): its private-use character.
    static func functionKey(_ key: Int) -> String {
        Unicode.Scalar(UInt32(key)).map { String(Character($0)) } ?? ""
    }

    // MARK: Profiles

    /// The identifier prefix of the profile submenus. They run no command
    /// of the registry (whose commands take no profile name, ADR 0037), so
    /// the key map leaves them alone and they have no key equivalents.
    static let profileMenuPrefix = "profile_menu:"
    /// The identifier of New Window with Profile's item.
    static let newWindowWithProfile = profileMenuPrefix + "new_window"
    /// The identifier of New Tab with Profile's item.
    static let newTabWithProfile = profileMenuPrefix + "new_tab"
    /// Longest title, in characters, a profile's item shows; a longer name
    /// is cut and ends in "…".
    static let maxProfileTitleLength = 64

    /// An item for a profile submenu, hidden until there are profiles.
    private static func profileMenuItem(_ identifier: String, _ title: String) -> NSMenuItem {
        let item = submenuItem(NSMenu(title: title))
        item.identifier = NSUserInterfaceItemIdentifier(identifier)
        item.isHidden = true
        return item
    }

    /// Fills New Window with Profile and New Tab with Profile of `bar` with
    /// one item per name of `profiles` (the core's, in file order), titled
    /// ``profileTitle(_:)``, carrying the name as written in its
    /// `representedObject`. A name seen before, byte for byte, is left out
    /// (the core starts the first profile of a name; canonically equal
    /// spellings are two profiles to it), and a title seen before gets a
    /// number.
    /// Both are hidden while there is no profile. Idempotent; the app calls
    /// it at start and after every configuration the core applies.
    static func setProfiles(_ profiles: [String], in bar: NSMenu) {
        let actions: [(String, Selector)] = [
            (newWindowWithProfile, #selector(AppDelegate.openNewWindowWithProfile(_:))),
            (newTabWithProfile, #selector(AppDelegate.openNewTabWithProfile(_:))),
        ]
        var names = Set<[UInt8]>()
        var titles = Set<String>()
        var entries: [(name: String, title: String)] = []
        for name in profiles where names.insert(Array(name.utf8)).inserted {
            let base = profileTitle(name)
            var title = base
            var number = 2
            while !titles.insert(title).inserted {
                title = "\(base) (\(number))"
                number += 1
            }
            entries.append((name, title))
        }
        for (identifier, action) in actions {
            guard let item = item(forCommand: identifier, in: bar), let submenu = item.submenu else { continue }
            submenu.removeAllItems()
            for (index, entry) in entries.enumerated() {
                let profile = submenu.addItem(withTitle: entry.title, action: action, keyEquivalent: "")
                profile.representedObject = entry.name
                // Not AppKit's (the action's name): one the key map knows
                // is not a command.
                profile.identifier = NSUserInterfaceItemIdentifier("\(identifier):\(index)")
            }
            item.isHidden = entries.isEmpty
        }
    }

    /// What a profile's item shows for `name`, which comes from
    /// `config.toml` and may hold anything (docs/security.md S3): control
    /// and formatting characters (bidirectional overrides and isolates,
    /// zero-width characters; a zero-width joiner stays, for emoji) and
    /// unassigned or private-use code points are dropped, runs of white
    /// space become one space, and a title past
    /// ``maxProfileTitleLength`` characters is cut and ends in "…". "Unnamed
    /// Profile" when nothing visible is left (only default-ignorable
    /// characters, such as a lone joiner or a Hangul filler, or the blank
    /// braille pattern).
    static func profileTitle(_ name: String) -> String {
        var scalars = String.UnicodeScalarView()
        var pendingSpace = false
        for scalar in name.unicodeScalars {
            let category = scalar.properties.generalCategory
            if scalar.properties.isWhitespace {
                pendingSpace = !scalars.isEmpty
                continue
            }
            switch category {
            case .control, .lineSeparator, .paragraphSeparator, .surrogate, .privateUse, .unassigned:
                continue
            case .format where scalar != "\u{200D}":
                continue
            default:
                if pendingSpace { scalars.append(" ") }
                pendingSpace = false
                scalars.append(scalar)
            }
        }
        var title = String(scalars)
        if title.count > maxProfileTitleLength {
            title = String(title.prefix(maxProfileTitleLength - 1)) + "…"
        }
        let visible = title.unicodeScalars.contains {
            !$0.properties.isDefaultIgnorableCodePoint && $0 != "\u{2800}"
        }
        return visible ? title : "Unnamed Profile"
    }

    private static func submenuItem(_ menu: NSMenu) -> NSMenuItem {
        let item = NSMenuItem(title: menu.title, action: nil, keyEquivalent: "")
        item.submenu = menu
        return item
    }
}
