import AppKit

/// The Shell menu's pane commands (D1, ADR 0035): split the focused pane,
/// move the focus between panes, maximize one and equalise them. Their
/// actions go up the responder chain to the tab's
/// ``TerminalWindowController``. Each item's `identifier` is its command's
/// name in the core's registry, so `[keys]` rebinds it (ADR 0036).
@MainActor
enum PaneMenu {
    /// Adds the pane items to the end of `menu`.
    static func addItems(to menu: NSMenu) {
        menu.addItem(.separator())
        add(menu, "split_right", "Split Right", #selector(TerminalWindowController.splitPaneRight(_:)), "d")
        add(
            menu, "split_down", "Split Below", #selector(TerminalWindowController.splitPaneBelow(_:)), "d",
            [.command, .shift])
        let select = NSMenu(title: "Select Pane")
        let moves: [(String, String, Selector, Int)] = [
            ("focus_pane_left", "Left", #selector(TerminalWindowController.selectPaneLeft(_:)), NSLeftArrowFunctionKey),
            (
                "focus_pane_right", "Right", #selector(TerminalWindowController.selectPaneRight(_:)),
                NSRightArrowFunctionKey
            ),
            ("focus_pane_up", "Above", #selector(TerminalWindowController.selectPaneAbove(_:)), NSUpArrowFunctionKey),
            (
                "focus_pane_down", "Below", #selector(TerminalWindowController.selectPaneBelow(_:)),
                NSDownArrowFunctionKey
            ),
        ]
        for (command, title, action, key) in moves {
            add(select, command, title, action, MainMenu.functionKey(key), [.command, .option])
        }
        select.addItem(.separator())
        add(
            select, "focus_pane_previous", "Previous", #selector(TerminalWindowController.selectPreviousPane(_:)), "[")
        add(select, "focus_pane_next", "Next", #selector(TerminalWindowController.selectNextPane(_:)), "]")
        let item = NSMenuItem(title: select.title, action: nil, keyEquivalent: "")
        item.submenu = select
        menu.addItem(item)
        add(
            menu, "zoom_pane", "Maximize Pane", #selector(TerminalWindowController.toggleZoomPane(_:)), "\r",
            [.command, .shift])
        add(menu, "equalize_panes", "Equalize Panes", #selector(TerminalWindowController.equalizePanes(_:)))
    }

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
}
