import SwiftUI

/// Appearance: the app's own appearance (app-wide, #365), the theme (and
/// importing one), the font, the window's opacity, blur and blur radius, the
/// cursor and the dimming of inactive panes; but for the app's appearance,
/// the `[appearance]` table every profile follows where it sets nothing of
/// its own.
struct AppearancePane: View {
    let model: SettingsModel

    static let app = SettingsField(
        key: "appearance.app_appearance", title: "Appearance", control: .choice(SettingsField.appAppearances),
        note: "The windows, tab bars, find bar, sheets and Settings. The terminal keeps its theme's colours.")
    static let theme = SettingsField(key: "appearance.theme", title: "Theme", control: .theme)
    static let font = [
        SettingsField(key: "appearance.font_family", title: "Font", control: .font),
        SettingsField(
            key: "appearance.font_size", title: "Size", control: .stepper(4...512, step: 1, unit: "pt")),
    ]
    static let window = [
        SettingsField(
            key: "appearance.opacity", title: "Opacity", control: .slider(0...1, step: 0.01, percent: true)),
        SettingsField(
            key: "appearance.blur", title: "Blur the background", control: .toggle,
            note: "While the opacity is below 100 %. Reduce Transparency in System Settings turns both off."),
        SettingsField(
            key: "appearance.blur_radius", title: "Blur radius", control: .integerSlider(0...64, zeroTitle: "Off"),
            note: "Blurs the whole window evenly while every translucent pane of the tab blurs; Off keeps each pane's own blur."),
    ]
    static let cursor = [
        SettingsField(key: "appearance.cursor_shape", title: "Cursor", control: .choice(SettingsField.cursorShapes)),
        SettingsField(key: "appearance.cursor_blink", title: "Blinking cursor", control: .toggle),
    ]
    static let panes = [
        SettingsField(
            key: "appearance.dim_inactive_panes", title: "Dim inactive panes",
            control: .slider(0...0.8, step: 0.05, percent: true),
            note: "Darkens the panes of a split tab that do not have the keyboard focus."),
    ]

    /// Every field of the pane, in order.
    static var fields: [SettingsField] { [app, theme] + font + window + cursor + panes }

    var body: some View {
        SettingsForm(model: model) {
            Section("App") {
                SettingsFieldRow(field: Self.app, context: .init(model: model, path: Self.app.key))
            }
            Section("Theme") {
                SettingsFieldRow(field: Self.theme, context: .init(model: model, path: Self.theme.key))
                LabeledContent("") {
                    Button("Import Theme…") {
                        if let file = SettingsPanels.chooseThemeFile() {
                            model.importTheme(at: file)
                        }
                    }
                }
                SettingsMessage(text: model.message(SettingsModel.importThemeKey))
            }
            Section("Font") { rows(Self.font) }
            Section("Window") { rows(Self.window) }
            Section("Cursor") { rows(Self.cursor) }
            Section("Panes") { rows(Self.panes) }
        }
    }

    private func rows(_ fields: [SettingsField]) -> some View {
        ForEach(fields) { field in
            SettingsFieldRow(field: field, context: .init(model: model, path: field.key))
        }
    }
}
