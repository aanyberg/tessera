import AppKit
import SwiftUI

/// The Settings window (⌘,, ADR 0044): one per app, a toolbar of panes
/// (General, Appearance, Profiles, Keys, Plugins, Advanced), each a SwiftUI view in an
/// `NSHostingController` over the one ``SettingsModel``. AppKit hosts it
/// (D12) rather than a SwiftUI `Settings` scene, since the app has no
/// SwiftUI `App`; its frame is saved under ``autosaveName``.
///
/// The window owns the model, the model holds the core only weakly, and
/// the app delegate drops the controller when the window closes, so
/// closing it frees all three of the window's objects; but for a model
/// whose add or delete of a profile still waits for the watch, which the
/// app delegate keeps until then and opens Settings with again (#455).
@MainActor
final class SettingsWindowController: NSWindowController, NSWindowDelegate {
    /// The name the window's frame is saved under in the user's defaults.
    static let autosaveName = "TesseraSettings"

    /// One pane: its toolbar item and its view. A new pane is one more
    /// entry in ``panes``.
    struct Pane {
        var identifier: String
        var label: String
        var symbol: String
        var view: @MainActor (SettingsModel, SettingsActions) -> AnyView
    }

    /// The panes, in toolbar order.
    static let panes: [Pane] = [
        Pane(identifier: "general", label: "General", symbol: "gearshape") { model, _ in
            AnyView(GeneralPane(model: model))
        },
        Pane(identifier: "appearance", label: "Appearance", symbol: "paintpalette") { model, _ in
            AnyView(AppearancePane(model: model))
        },
        Pane(identifier: "profiles", label: "Profiles", symbol: "person.crop.rectangle.stack") { model, _ in
            AnyView(ProfilesPane(model: model))
        },
        Pane(identifier: "keys", label: "Keys", symbol: "keyboard") { model, _ in
            AnyView(KeysPane(model: model))
        },
        Pane(identifier: "plugins", label: "Plugins", symbol: "puzzlepiece.extension") { model, _ in
            AnyView(PluginsPane(model: model))
        },
        Pane(identifier: "advanced", label: "Advanced", symbol: "gearshape.2") { model, actions in
            AnyView(AdvancedPane(model: model, actions: actions))
        },
    ]

    let model: SettingsModel
    /// Called once the window has closed; the app delegate drops its
    /// reference to this controller.
    var onClose: (@MainActor () -> Void)?
    /// Whether the event being handled is an Option-click on a close
    /// button, which the app delegate turns into one Close All: this window
    /// then waits for its answer, as the terminal windows do (ADR 0031).
    var takesOptionClickOnCloseButton: (@MainActor () -> Bool)?

    /// `autosaveName` `nil` keeps the frame out of the user's defaults (for
    /// tests).
    init(model: SettingsModel, actions: SettingsActions, autosaveName: String? = SettingsWindowController.autosaveName) {
        self.model = model
        let tabs = SettingsTabViewController()
        tabs.model = model
        tabs.tabStyle = .toolbar
        tabs.canPropagateSelectedChildViewControllerTitle = true
        for pane in Self.panes {
            let hosting = NSHostingController(rootView: pane.view(model, actions))
            hosting.sizingOptions = [.preferredContentSize]
            hosting.title = pane.label
            let item = NSTabViewItem(viewController: hosting)
            item.identifier = pane.identifier
            item.label = pane.label
            item.image = NSImage(systemSymbolName: pane.symbol, accessibilityDescription: pane.label)
            tabs.addTabViewItem(item)
        }
        let window = NSWindow(contentViewController: tabs)
        window.styleMask = [.titled, .closable, .miniaturizable]
        window.toolbarStyle = .preference
        window.isReleasedWhenClosed = false
        window.identifier = NSUserInterfaceItemIdentifier("settings")
        super.init(window: window)
        window.delegate = self
        // The Keys pane records only this window's keys; the open panels
        // are sheets on it.
        model.keys.window = window
        model.window = window
        window.center()
        if let autosaveName {
            // Restores the saved frame, if there is one, and saves it from
            // now on.
            window.setFrameAutosaveName(autosaveName)
        }
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not used")
    }

    /// Closes, but for an Option-click on a close button.
    func windowShouldClose(_ sender: NSWindow) -> Bool {
        takesOptionClickOnCloseButton?() != true
    }

    /// The tab view controller of the panes.
    var tabs: NSTabViewController? {
        window?.contentViewController as? NSTabViewController
    }

    /// Shows the pane `identifier` (`"profiles"`).
    func select(pane identifier: String) {
        guard let tabs else { return }
        if let index = tabs.tabViewItems.firstIndex(where: { ($0.identifier as? String) == identifier }) {
            tabs.selectedTabViewItemIndex = index
        }
    }

    func windowWillClose(_ notification: Notification) {
        window?.delegate = nil
        // Text typed into a field and not committed is written, as a click
        // elsewhere would; the model then holds nothing of the field (whose
        // binding holds the model).
        model.commitDraft()
        // The panes hold the model. AppKit keeps a closed key window (as
        // the previous key window) until another one becomes key, so the
        // window lets go of the panes now; a closed controller is never
        // shown again.
        window?.contentViewController = nil
        let onClose = onClose
        self.onClose = nil
        onClose?()
    }
}

/// The panes' tab view controller. Text typed into a field and not yet
/// committed is written before another pane shows: the pane switch takes
/// the field away before it sees the focus go, and the field's text would
/// be lost (#455).
@MainActor
final class SettingsTabViewController: NSTabViewController {
    /// The model whose typed text is committed.
    weak var model: SettingsModel?

    override func tabView(_ tabView: NSTabView, willSelect tabViewItem: NSTabViewItem?) {
        model?.commitDraft()
        super.tabView(tabView, willSelect: tabViewItem)
    }
}

/// What a pane asks the app for beyond the configuration: opening
/// `config.toml` in an editor.
@MainActor
struct SettingsActions {
    var openConfigFile: @MainActor () -> Void
}
