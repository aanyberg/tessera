import SwiftUI

/// General: the profile new tabs start with, whether closing a busy window
/// asks first, whether a tab marks a remote shell in its title, and
/// whether a long command that finishes out of sight notifies (#470).
struct GeneralPane: View {
    let model: SettingsModel

    static let fields = [
        SettingsField(
            key: "general.default_profile", title: "Default profile", control: .profile,
            note: "New windows and tabs start with this profile."),
        SettingsField(
            key: "general.confirm_close", title: "Confirm before closing a window with running jobs",
            control: .toggle),
        SettingsField(
            key: "general.remote_indicator", title: "Remote host",
            control: .choice([
                SettingsField.Choice(value: "title", title: "Show in tab title"),
                SettingsField.Choice(value: "off", title: "Don’t show"),
            ]),
            note: "Tabs connected to another machine with ssh start their title with ⇢ and the host name."),
        SettingsField(
            key: "notifications.command_finished", title: "Notify when a command finishes",
            control: .choice([
                SettingsField.Choice(value: "unfocused", title: "When its pane is not in front"),
                SettingsField.Choice(value: "off", title: "Never"),
            ]),
            note: "Posts a notification and marks the tab. Needs shell integration. The first one asks macOS for permission."),
        SettingsField(
            key: "notifications.command_finished_after", title: "Only for commands longer than (seconds)",
            control: .integer, note: "1 to 86400."),
    ]

    var body: some View {
        SettingsForm(model: model) {
            Section {
                ForEach(Self.fields) { field in
                    SettingsFieldRow(field: field, context: .init(model: model, path: field.key))
                }
            }
        }
    }
}

/// A pane's grouped form, with the reason the configuration could not be
/// read above it when it could not.
struct SettingsForm<Content: View>: View {
    let model: SettingsModel
    @ViewBuilder let content: Content

    var body: some View {
        Form {
            if let error = model.loadError {
                Section {
                    SettingsMessage(text: error, notSaved: false)
                }
            }
            content
        }
        .formStyle(.grouped)
        .frame(width: 560)
        .fixedSize(horizontal: false, vertical: true)
    }
}
