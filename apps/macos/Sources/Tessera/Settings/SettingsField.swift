import AppKit
import SwiftUI
import TesseraKit
import UniformTypeIdentifiers

/// One setting a pane shows: its key path, its title and the kind of
/// control that edits it. The panes are built from these lists, and the
/// tests drive every one through the same bindings the controls use.
struct SettingsField: Identifiable {
    /// The control that edits a setting, and the type it writes.
    enum Control {
        /// A boolean.
        case toggle
        /// A string, one of `choices`.
        case choice([Choice])
        /// A string: a theme's id (``SettingsModel/themes``).
        case theme
        /// A string: a fixed-pitch font family.
        case font
        /// A string: a profile's name.
        case profile
        /// A float, dragged; written coalesced. `percent` shows it ×100.
        case slider(ClosedRange<Double>, step: Double, percent: Bool)
        /// A float, stepped; written coalesced.
        case stepper(ClosedRange<Double>, step: Double, unit: String)
        /// An integer, dragged in steps of 1; written coalesced. 0 shows
        /// `zeroTitle` instead of the number. No pane uses it in 0.2.0: it
        /// was the blur radius slider, which returns in 0.3.0 (#491).
        case integerSlider(ClosedRange<Int64>, zeroTitle: String)
        /// An integer, typed.
        case integer
        /// A string, typed; committed on Return or when the field loses
        /// the focus. Empty removes the key when `removeWhenEmpty`.
        case text(placeholder: String, removeWhenEmpty: Bool)
        /// A string, a directory: typed, or chosen with an open panel.
        case directory
        /// A profile's name: committed through ``SettingsModel/renameProfile(_:to:)``.
        case profileName
    }

    struct Choice: Hashable {
        var value: String
        var title: String
    }

    /// The key path (for a profile's field, under the profile:
    /// `appearance.theme` is `profiles[N].appearance.theme`).
    var key: String
    var title: String
    var control: Control
    /// A line under the control.
    var note: String?

    var id: String { key }

    /// `[appearance]` keys a profile may set (every `ProfileAppearance`
    /// key), in the order the panes show them.
    static let profileAppearanceKeys = [
        "appearance.theme", "appearance.font_family", "appearance.font_size", "appearance.opacity",
        "appearance.blur", "appearance.cursor_shape", "appearance.cursor_blink",
    ]

    static let cursorShapes = [
        Choice(value: "block", title: "Block"), Choice(value: "underline", title: "Underline"),
        Choice(value: "beam", title: "Bar"),
    ]
    /// The profile new tabs start with while `[general] default_profile`
    /// is not set or names no profile: the core's `BUILT_IN_PROFILE_NAME`.
    /// A `[[profiles]]` entry of this name is the one used.
    static let builtInProfileName = "Default"
    /// The default profile picker's item for ``builtInProfileName``, which
    /// leaves `default_profile` out.
    static let builtInProfileChoice = Choice(value: builtInProfileName, title: "Default (built-in)")

    /// `[appearance] app_appearance`'s values (#365).
    static let appAppearances = [
        Choice(value: "dark", title: "Dark"), Choice(value: "light", title: "Light"),
        Choice(value: "system", title: "System"),
    ]
}

/// Where a field's control reads and writes: the key path, and for a
/// profile's appearance key the `[appearance]` key it inherits.
struct SettingsFieldContext {
    var model: SettingsModel
    /// The key path written.
    var path: String
    /// The key path shown while ``path`` is not set: `[appearance]`'s key
    /// for a profile's appearance key.
    var fallback: String?
    /// The profile, for a profile's field.
    var profile: Int?

    /// The value shown.
    @MainActor var current: TesseraConfigValue? {
        model.value(path) ?? fallback.flatMap { model.value($0) }
    }
}

// MARK: Bindings

/// The bindings the controls use; the tests set them as the controls do.
extension SettingsField {
    @MainActor
    func boolBinding(_ context: SettingsFieldContext) -> Binding<Bool> {
        Binding(
            get: { context.current?.bool ?? false },
            set: { context.model.set(context.path, .bool($0)) })
    }

    /// A slider's or stepper's: every change coalesced, ``SettingsModel/endChange(_:)``
    /// on release (``endEditing(_:)``).
    @MainActor
    func doubleBinding(_ context: SettingsFieldContext) -> Binding<Double> {
        Binding(
            get: { context.current?.double ?? 0 },
            set: { context.model.change(context.path, .float(Self.rounded($0, control))) })
    }

    @MainActor
    func endEditing(_ context: SettingsFieldContext) {
        context.model.endChange(context.path)
    }

    /// A slider's or stepper's `onEditingChanged`: the model holds the
    /// value it shows while it is pressed, ``endEditing(_:)`` once it is
    /// let go.
    @MainActor
    func editingChanged(_ context: SettingsFieldContext) -> (Bool) -> Void {
        { editing in
            if editing {
                context.model.beginChange(context.path)
            } else {
                endEditing(context)
            }
        }
    }

    /// An integer slider's: as a slider's, writing the whole number nearest
    /// the knob, within the range.
    @MainActor
    func integerSliderBinding(_ context: SettingsFieldContext) -> Binding<Double> {
        Binding(
            get: { Double(context.current?.integer ?? 0) },
            set: { context.model.change(context.path, .integer(Self.rounded(integer: $0, control))) })
    }

    @MainActor
    func integerBinding(_ context: SettingsFieldContext) -> Binding<Int64> {
        Binding(
            get: { context.current?.integer ?? 0 },
            set: { context.model.set(context.path, .integer($0)) })
    }

    /// A choice's, a text field's (set on commit) and a profile name's. The
    /// default profile's shows ``builtInProfileName`` while it is not set,
    /// and choosing that removes it.
    @MainActor
    func stringBinding(_ context: SettingsFieldContext) -> Binding<String> {
        Binding(
            get: {
                if case .profile = control { return context.current?.string ?? Self.builtInProfileName }
                return context.current?.string ?? ""
            },
            set: { text in
                switch control {
                case .profile where text == Self.builtInProfileName:
                    context.model.remove(context.path)
                case .profileName:
                    guard let profile = context.profile else { return }
                    context.model.renameProfile(profile, to: text)
                case .text(_, removeWhenEmpty: true), .directory:
                    if text.isEmpty {
                        context.model.remove(context.path)
                    } else {
                        context.model.set(context.path, .string(text))
                    }
                default:
                    context.model.set(context.path, .string(text))
                }
            })
    }

    /// The items of a picker's menu: the values it writes, with titles
    /// sanitised (S3) where they come from the configuration or the system.
    @MainActor
    func choices(_ model: SettingsModel) -> [Choice]? {
        switch control {
        case .choice(let choices): choices
        case .theme: model.themes.map { .init(value: $0.id, title: MainMenu.profileTitle($0.name)) }
        case .font: FontFamilies.fixedPitch.map { .init(value: $0, title: MainMenu.profileTitle($0)) }
        case .profile:
            // A profile of the built-in's name is the one used: it shows
            // instead.
            (model.profiles.contains { $0.name == Self.builtInProfileName } ? [] : [Self.builtInProfileChoice])
                + model.profiles.map { .init(value: $0.name, title: $0.title) }
        default: nil
        }
    }

    /// An integer slider's value: the nearest whole number in its range.
    static func rounded(integer value: Double, _ control: Control) -> Int64 {
        guard case .integerSlider(let range, _) = control else { return Int64(value.rounded()) }
        let clamped = min(max(value.rounded(), Double(range.lowerBound)), Double(range.upperBound))
        return Int64(clamped)
    }

    /// A slider's value on its step, so a drag writes `0.85`, not
    /// `0.8500000000000001`; a stepper's to a tenth.
    static func rounded(_ value: Double, _ control: Control) -> Double {
        switch control {
        case .slider(let range, let step, _):
            let digits = max(0, Int((-log10(step)).rounded(.up)))
            let scale = pow(10, Double(digits))
            let stepped = ((value / step).rounded() * step * scale).rounded() / scale
            return min(max(stepped, range.lowerBound), range.upperBound)
        case .stepper(let range, _, _):
            // A size typed by hand (13.5) keeps its fraction as it steps.
            return min(max((value * 10).rounded() / 10, range.lowerBound), range.upperBound)
        default:
            return value
        }
    }
}

// MARK: Views

/// The row of one field: its control, and the message of a failed edit
/// under it.
struct SettingsFieldRow: View {
    let field: SettingsField
    let context: SettingsFieldContext
    var disabled = false

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            control
                .disabled(disabled)
            if let note = field.note {
                Text(note)
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            SettingsMessage(text: context.model.message(context.path))
        }
    }

    @ViewBuilder private var control: some View {
        switch field.control {
        case .toggle:
            Toggle(field.title, isOn: field.boolBinding(context))
        case .choice, .theme, .font, .profile:
            SettingsPicker(
                title: field.title, selection: field.stringBinding(context),
                choices: field.choices(context.model) ?? [])
        case .slider(let range, let step, let percent):
            let value = field.doubleBinding(context)
            LabeledContent(field.title) {
                HStack {
                    Slider(
                        value: value, in: range, step: step, onEditingChanged: field.editingChanged(context))
                    Text(percent ? "\(Int((value.wrappedValue * 100).rounded())) %" : String(format: "%.2f", value.wrappedValue))
                        .monospacedDigit()
                        .frame(minWidth: 44, alignment: .trailing)
                }
            }
        case .integerSlider(let range, let zeroTitle):
            let value = field.integerSliderBinding(context)
            LabeledContent(field.title) {
                HStack {
                    Slider(
                        value: value, in: Double(range.lowerBound)...Double(range.upperBound), step: 1,
                        onEditingChanged: field.editingChanged(context))
                    Text(value.wrappedValue == 0 ? zeroTitle : "\(Int64(value.wrappedValue))")
                        .monospacedDigit()
                        .frame(minWidth: 44, alignment: .trailing)
                }
            }
        case .stepper(let range, let step, let unit):
            let value = field.doubleBinding(context)
            LabeledContent(field.title) {
                Stepper(value: value, in: range, step: step, onEditingChanged: field.editingChanged(context)) {
                    Text("\(value.wrappedValue.formatted(.number.precision(.fractionLength(0...1)))) \(unit)")
                        .monospacedDigit()
                }
            }
        case .integer:
            TextField(field.title, value: field.integerBinding(context), format: .number)
                .multilineTextAlignment(.trailing)
        case .text(let placeholder, _):
            CommitTextField(
                title: field.title, placeholder: placeholder, value: field.stringBinding(context),
                drafts: context.model)
        case .profileName:
            CommitTextField(title: field.title, placeholder: "", value: field.stringBinding(context), drafts: context.model)
        case .directory:
            LabeledContent(field.title) {
                HStack {
                    CommitTextField(
                        title: "", placeholder: "Home folder", value: field.stringBinding(context),
                        drafts: context.model)
                    Button("Choose…") {
                        Self.chooseDirectory(SettingsPanels.directoryPanel(), for: field, context)
                    }
                }
            }
        }
    }
}

extension SettingsFieldRow {
    /// Choose… of a directory field: `panel` as a sheet on the Settings
    /// window, its directory written to the field's key. Text typed into
    /// a field is committed first and the field let go, so the directory
    /// chosen is the last write and the field shows it: the typed text
    /// was written over it once the field lost the focus (21.6a m7).
    static func chooseDirectory(
        _ panel: some SettingsOpenPanel, for field: SettingsField, _ context: SettingsFieldContext
    ) {
        let model = context.model
        model.commitDraft()
        model.window?.makeFirstResponder(nil)
        let generation = model.profileListGeneration
        SettingsPanels.present(panel, on: model.window) { directory in
            // A hand edit may have put another profile at this one's index
            // while the sheet was open.
            guard model.profileListGeneration == generation else { return }
            field.stringBinding(context).wrappedValue = directory.path(percentEncoded: false)
        }
    }
}

/// A picker over `choices`; a value not among them (written by hand, or a
/// theme that is gone) shows as an extra item, sanitised (S3).
struct SettingsPicker: View {
    let title: String
    let selection: Binding<String>
    let choices: [SettingsField.Choice]

    var body: some View {
        Picker(title, selection: selection) {
            ForEach(Self.items(choices, selection: selection.wrappedValue), id: \.value) { choice in
                Text(choice.title).tag(choice.value)
            }
        }
    }

    /// The menu's items: `choices`, and `selection` after them when it is
    /// none of them.
    static func items(_ choices: [SettingsField.Choice], selection: String) -> [SettingsField.Choice] {
        if choices.contains(where: { $0.value == selection }) { return choices }
        return choices + [.init(value: selection, title: MainMenu.profileTitle(selection))]
    }
}

/// A text field that writes when Return is pressed or it loses the focus,
/// not on every keystroke; it follows the value when that changes. Text
/// typed and not yet written is held with `drafts`
/// (``SettingsModel/holdDraft(_:commit:)``), which writes it before a click
/// elsewhere takes the field away; once `drafts` has committed it (or let
/// it go), the field writes it no more, so a value set since (Choose…)
/// stays.
struct CommitTextField: View {
    let title: String
    let placeholder: String
    let value: Binding<String>
    var drafts: SettingsModel?
    @State private var draft = ""
    @State private var token = DraftToken()
    @FocusState private var focused: Bool

    /// Who holds this field's draft with the model.
    final class DraftToken {}

    var body: some View {
        TextField(title, text: $draft, prompt: placeholder.isEmpty ? nil : Text(placeholder))
            .focused($focused)
            .onSubmit(commit)
            .onAppear { draft = value.wrappedValue }
            .onDisappear { drafts?.releaseDraft(token) }
            .onChange(of: value.wrappedValue) { _, new in
                if !focused { draft = new }
            }
            .onChange(of: draft) { _, text in
                guard let drafts else { return }
                if text == value.wrappedValue {
                    drafts.releaseDraft(token)
                } else {
                    drafts.holdDraft(token) { [value] in
                        if text != value.wrappedValue { value.wrappedValue = text }
                    }
                }
            }
            .onChange(of: focused) { _, isFocused in
                if !isFocused { commit() }
            }
    }

    private func commit() {
        let held = drafts?.releaseDraft(token) ?? true
        if held, draft != value.wrappedValue {
            value.wrappedValue = draft
        }
        // A value refused before it was written (a profile name) shows the
        // one in effect again once the field is left.
        if !focused { draft = value.wrappedValue }
    }
}

/// The message of a failed edit, if there is one.
struct SettingsMessage: View {
    let text: String?

    var body: some View {
        if let text {
            Label(text, systemImage: "exclamationmark.triangle.fill")
                .font(.callout)
                .foregroundStyle(.red)
                .fixedSize(horizontal: false, vertical: true)
                .accessibilityLabel("Not saved: \(text)")
        }
    }
}

/// The fixed-pitch font families installed, in name order, read once.
@MainActor
enum FontFamilies {
    static let fixedPitch: [String] = {
        let manager = NSFontManager.shared
        let fixedPitch = UInt(NSFontTraitMask.fixedPitchFontMask.rawValue)
        return manager.availableFontFamilies.filter { family in
            // Each member is [name, style, weight, traits]; one fixed-pitch
            // face makes it a terminal font.
            manager.availableMembers(ofFontFamily: family)?.contains { member in
                member.count > 3 && ((member[3] as? UInt) ?? 0) & fixedPitch != 0
            } ?? false
        }.sorted { $0.localizedStandardCompare($1) == .orderedAscending }
    }()
}

/// What `SettingsPanels.present` may do with an open panel: `NSOpenPanel`
/// is the only conformer outside tests. A test seam: a recording panel
/// shows how `present` ran it without a real panel, whose out-of-process
/// bridge can block the main thread for a minute (#492).
@MainActor
protocol SettingsOpenPanel: AnyObject {
    var url: URL? { get }
    func beginSheetModal(for window: NSWindow, completionHandler handler: @escaping (NSApplication.ModalResponse) -> Void)
    func begin(completionHandler handler: @escaping (NSApplication.ModalResponse) -> Void)
    func runModal() -> NSApplication.ModalResponse
}

extension NSOpenPanel: SettingsOpenPanel {}

/// The open panels the panes show, as sheets on the Settings window.
@MainActor
enum SettingsPanels {
    /// The panel that chooses a directory.
    static func directoryPanel() -> NSOpenPanel {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.allowsMultipleSelection = false
        panel.prompt = "Choose"
        return panel
    }

    /// The panel that chooses a colour scheme file (`.itermcolors`,
    /// `.yaml`, `.yml`).
    static func themeFilePanel() -> NSOpenPanel {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = false
        panel.canChooseFiles = true
        panel.allowsMultipleSelection = false
        panel.allowedContentTypes = ["itermcolors", "yaml", "yml"].compactMap { UTType(filenameExtension: $0) }
        panel.message = "Choose an iTerm2 colour scheme (.itermcolors) or a base16 scheme (.yaml)."
        panel.prompt = "Import"
        return panel
    }

    /// Shows `panel` as a sheet on `window` (on its own without one, not
    /// app-modal) and calls `chosen` with what was chosen, if anything was.
    static func present(
        _ panel: some SettingsOpenPanel, on window: NSWindow?, chosen: @escaping @MainActor (URL) -> Void
    ) {
        let done: (NSApplication.ModalResponse) -> Void = { response in
            guard response == .OK, let url = panel.url else { return }
            MainActor.assumeIsolated { chosen(url) }
        }
        if let window {
            panel.beginSheetModal(for: window, completionHandler: done)
        } else {
            panel.begin(completionHandler: done)
        }
    }
}
