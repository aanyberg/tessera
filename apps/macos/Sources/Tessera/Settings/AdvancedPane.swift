import SwiftUI

/// Advanced: scrollback, shell integration, clipboard writes by programs,
/// the enabled plugins (shown only, until the Plugins pane, #48), and
/// `config.toml` itself for the keys no pane shows.
struct AdvancedPane: View {
    let model: SettingsModel
    let actions: SettingsActions

    static let fields = [
        SettingsField(
            key: "terminal.scrollback_lines", title: "Scrollback lines", control: .integer,
            note: "For tabs opened from now on."),
        SettingsField(
            key: "terminal.shell_integration", title: "Shell integration",
            control: .choice([.init(value: "auto", title: "On (zsh and fish)"), .init(value: "off", title: "Off")]),
            note: "Marks prompts and commands, and reports the working directory, in the shells Tessera starts."),
        SettingsField(
            key: "terminal.clipboard_write", title: "Programs may write the clipboard (OSC 52)", control: .toggle),
    ]

    var body: some View {
        SettingsForm(model: model) {
            Section("Terminal") {
                ForEach(Self.fields) { field in
                    SettingsFieldRow(field: field, context: .init(model: model, path: field.key))
                }
            }
            Section("Plugins") {
                let enabled = model.value("plugins.enabled")?.array?.compactMap(\.string) ?? []
                if enabled.isEmpty {
                    Text("No plugins are enabled.")
                        .foregroundStyle(.secondary)
                } else {
                    ForEach(Array(enabled.enumerated()), id: \.offset) { _, plugin in
                        Text(MainMenu.profileTitle(plugin))
                    }
                }
            }
            Section {
                LabeledContent("config.toml") {
                    Button("Open config.toml") { actions.openConfigFile() }
                }
                Text("Every setting, with the ones this window does not show, in your text editor.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
        }
    }
}
