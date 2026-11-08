import SwiftUI

/// General: the profile new tabs start with, and whether closing a busy
/// window asks first.
struct GeneralPane: View {
    let model: SettingsModel

    static let fields = [
        SettingsField(
            key: "general.default_profile", title: "Default profile", control: .profile,
            note: "New windows and tabs start with this profile."),
        SettingsField(
            key: "general.confirm_close", title: "Confirm before closing a window with running jobs",
            control: .toggle),
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
