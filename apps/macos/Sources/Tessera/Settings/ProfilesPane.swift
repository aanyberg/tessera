import SwiftUI

/// Profiles: the `[[profiles]]` list in file order, with Add, Duplicate and
/// Delete under it, and the selected profile's name, command, working
/// directory and appearance. Each appearance key either follows
/// `[appearance]` (Inherit, the key left out of the profile) or is the
/// profile's own.
struct ProfilesPane: View {
    let model: SettingsModel
    @State private var selection: Int?
    @State private var confirmingDelete: SettingsModel.Profile?

    static let fields = [
        SettingsField(key: "name", title: "Name", control: .profileName),
        SettingsField(
            key: "command", title: "Command", control: .text(placeholder: "Login shell", removeWhenEmpty: true),
            note: "A command line, split as a shell would; empty runs your login shell."),
        SettingsField(key: "working_directory", title: "Working directory", control: .directory),
    ]

    /// The appearance fields a profile may set: Appearance's, but for
    /// `dim_inactive_panes` (the window draws that).
    static var appearanceFields: [SettingsField] {
        AppearancePane.fields.filter { SettingsField.profileAppearanceKeys.contains($0.key) }
    }

    var body: some View {
        let profiles = model.profiles
        // An add or delete moves the indices edits name: wait for it.
        let changing = model.profilesChanging
        HStack(alignment: .top, spacing: 0) {
            VStack(alignment: .leading, spacing: 0) {
                List(selection: $selection) {
                    ForEach(profiles) { profile in
                        Text(profile.title)
                            .lineLimit(1)
                            .truncationMode(.tail)
                            .tag(profile.index)
                    }
                }
                .listStyle(.bordered(alternatesRowBackgrounds: false))
                .frame(height: 300)
                .disabled(changing)
                HStack(spacing: 4) {
                    Button {
                        if let index = model.addProfile() {
                            selection = index
                        }
                    } label: {
                        Image(systemName: "plus").accessibilityLabel("Add Profile")
                    }
                    Button {
                        if let selection, let copy = model.duplicateProfile(selection) {
                            self.selection = copy
                        }
                    } label: {
                        Image(systemName: "plus.square.on.square").accessibilityLabel("Duplicate Profile")
                    }
                    .disabled(selected(in: profiles) == nil)
                    Button {
                        confirmingDelete = selected(in: profiles)
                    } label: {
                        Image(systemName: "minus").accessibilityLabel("Delete Profile")
                    }
                    .disabled(selected(in: profiles) == nil)
                }
                .buttonStyle(.borderless)
                .disabled(changing)
                .padding(.top, 6)
                SettingsMessage(text: model.message(SettingsModel.profilesKey))
                    .padding(.top, 6)
            }
            .frame(width: 180)
            .padding([.leading, .top, .bottom], 20)

            if let profile = selected(in: profiles) {
                ProfileDetail(model: model, profile: profile)
                    .disabled(changing)
            } else {
                Text(profiles.isEmpty ? "No profiles. Add one with +." : "Select a profile.")
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, minHeight: 300)
                    .padding(20)
            }
        }
        .frame(width: 680)
        .fixedSize(horizontal: false, vertical: true)
        .confirmationDialog(
            "Delete the profile “\(confirmingDelete?.title ?? "")”?",
            isPresented: Binding(get: { confirmingDelete != nil }, set: { if !$0 { confirmingDelete = nil } })
        ) {
            Button("Delete", role: .destructive) {
                if let profile = confirmingDelete, model.deleteProfile(profile.index) {
                    selection = nil
                }
                confirmingDelete = nil
            }
        } message: {
            Text("Its entry is removed from config.toml.")
        }
    }

    private func selected(in profiles: [SettingsModel.Profile]) -> SettingsModel.Profile? {
        guard let selection else { return nil }
        return profiles.first { $0.index == selection }
    }
}

/// The selected profile's settings.
private struct ProfileDetail: View {
    let model: SettingsModel
    let profile: SettingsModel.Profile

    var body: some View {
        Form {
            Section {
                ForEach(ProfilesPane.fields) { field in
                    SettingsFieldRow(
                        field: field,
                        context: .init(model: model, path: profile.key(field.key), profile: profile.index))
                }
            }
            Section("Appearance") {
                ForEach(ProfilesPane.appearanceFields) { field in
                    let inherits = model.inherits(profile.index, field.key)
                    VStack(alignment: .leading, spacing: 4) {
                        SettingsFieldRow(
                            field: field,
                            context: .init(
                                model: model, path: profile.key(field.key), fallback: field.key,
                                profile: profile.index),
                            disabled: inherits)
                        Toggle(
                            "Inherit from Appearance",
                            isOn: Binding(
                                get: { model.inherits(profile.index, field.key) },
                                set: { model.setInherits(profile.index, field.key, $0) }))
                        .controlSize(.small)
                    }
                }
            }
        }
        .formStyle(.grouped)
        // Rows of another profile must not keep this one's draft text: nor
        // the same index once a hand edit put another profile there.
        .id([profile.index, model.profileListGeneration])
    }
}
