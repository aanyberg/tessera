import SwiftUI

/// General: the profile new tabs start with, whether closing a busy window
/// asks first, and whether a tab marks a remote shell in its title.
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
                    SettingsMessage(text: error)
                }
            }
            content
        }
        .formStyle(.grouped)
        .frame(width: 560)
        .fixedSize(horizontal: false, vertical: true)
    }
}
