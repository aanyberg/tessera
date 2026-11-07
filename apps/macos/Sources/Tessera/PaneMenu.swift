import AppKit

/// The Shell menu's pane commands (D1, ADR 0035): split the focused pane
/// and move the focus between panes. Their actions go up the responder
/// chain to the tab's ``TerminalWindowController``.
@MainActor
enum PaneMenu {
    /// Adds the pane items to the end of `menu`.
    static func addItems(to menu: NSMenu) {
        menu.addItem(.separator())
        menu.addItem(
            withTitle: "Split Right", action: #selector(TerminalWindowController.splitPaneRight(_:)), keyEquivalent: "d")
        menu.addItem(
            withTitle: "Split Below", action: #selector(TerminalWindowController.splitPaneBelow(_:)), keyEquivalent: "d"
        ).keyEquivalentModifierMask = [.command, .shift]
        let select = NSMenu(title: "Select Pane")
        let moves: [(String, Selector, Int)] = [
            ("Left", #selector(TerminalWindowController.selectPaneLeft(_:)), NSLeftArrowFunctionKey),
            ("Right", #selector(TerminalWindowController.selectPaneRight(_:)), NSRightArrowFunctionKey),
            ("Above", #selector(TerminalWindowController.selectPaneAbove(_:)), NSUpArrowFunctionKey),
            ("Below", #selector(TerminalWindowController.selectPaneBelow(_:)), NSDownArrowFunctionKey),
        ]
        for (title, action, key) in moves {
            select.addItem(withTitle: title, action: action, keyEquivalent: MainMenu.functionKey(key))
                .keyEquivalentModifierMask = [.command, .option]
        }
        let item = NSMenuItem(title: select.title, action: nil, keyEquivalent: "")
        item.submenu = select
        menu.addItem(item)
    }
}
