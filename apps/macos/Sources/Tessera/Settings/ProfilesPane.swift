import SwiftUI

/// Profiles: the `[[profiles]]` entries, then the profiles of
/// `profiles/*.toml`, with Add, Duplicate and Delete under it, and the
/// selected profile's name, command, working directory and appearance. Each
/// appearance key either follows `[appearance]` (Inherit, the key left out
/// of the profile) or is the profile's own. A profile of `profiles/` shows
/// its file and is read-only here: the core edits only config.toml (#312
/// m4).
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
    /// `dim_inactive_panes` (the window draws it) and `app_appearance`
    /// (app-wide).
    static var appearanceFields: [SettingsField] {
        AppearancePane.fields.filter { SettingsField.profileAppearanceKeys.contains($0.key) }
    }

    var body: some View {
        let profiles = model.profiles
        // An add or delete moves the indices edits name: wait for it.
        let changing = model.profilesChanging
        VStack(alignment: .leading, spacing: 0) {
            if let error = model.loadError {
                // As the other panes do: also why an add or delete this
                // pane made went with the failure, which leaves no notice
                // by the list (#547, #570). Selectable, as the status line
                // is: it quotes the file.
                SettingsMessage(text: error, notSaved: false)
                    .textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding([.horizontal, .top], 20)
            }
            profilesAndDetail(profiles, changing: changing)
        }
        .frame(width: 680)
        .fixedSize(horizontal: false, vertical: true)
        // The profile an Add or Duplicate wrote, once written: also when
        // this pane shows a model a closed window left waiting (#312 m5).
        .onChange(of: model.selectionRequest, initial: true) {
            if let index = model.takeSelectionRequest() { selection = index }
        }
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

    /// The list with its buttons and status, and the selected profile.
    private func profilesAndDetail(_ profiles: [SettingsModel.Profile], changing: Bool) -> some View {
        HStack(alignment: .top, spacing: 0) {
            VStack(alignment: .leading, spacing: 0) {
                // A click on another profile first writes the text typed
                // into this one's field, which goes before it sees the
                // focus go (#344).
                List(selection: Binding(get: { selection }, set: { model.commitDraft(); selection = $0 })) {
                    ForEach(profiles) { profile in
                        Text(profile.title)
                            .lineLimit(1)
                            .truncationMode(.tail)
                            .help(profile.file.map(Self.fileNote) ?? "")
                            .tag(profile.index)
                    }
                }
                .listStyle(.bordered(alternatesRowBackgrounds: false))
                .frame(height: 300)
                .disabled(changing)
                HStack(spacing: 4) {
                    Button {
                        // Selected once written, where the core put it
                        // (the model's selection request).
                        model.addProfile()
                    } label: {
                        Image(systemName: "plus").accessibilityLabel("Add Profile")
                    }
                    .disabled(!model.canAddProfile)
                    .help(model.canAddProfile ? "Add a profile" : model.profilesStatus ?? "")
                    Button {
                        if let selection {
                            model.duplicateProfile(selection)
                        }
                    } label: {
                        Image(systemName: "plus.square.on.square").accessibilityLabel("Duplicate Profile")
                    }
                    .disabled(selected(in: profiles) == nil || !model.canAddProfile)
                    .help(model.canAddProfile ? "Duplicate the selected profile" : model.profilesStatus ?? "")
                    Button {
                        confirmingDelete = selected(in: profiles)
                    } label: {
                        Image(systemName: "minus").accessibilityLabel("Delete Profile")
                    }
                    .disabled(selected(in: profiles).map { $0.file != nil } ?? true)
                    .help(selected(in: profiles)?.file.map(Self.fileNote) ?? "Delete the selected profile")
                }
                .buttonStyle(.borderless)
                .disabled(changing)
                .padding(.top, 6)
                if let status = model.profilesStatus {
                    // Why the list and the profile, or Add and Duplicate,
                    // are disabled (#344 m-f, 21.6a m4).
                    Text(status)
                        .textSelection(.enabled)
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                        .padding(.top, 6)
                }
                // Mostly notices that say what happened themselves.
                SettingsMessage(text: model.message(SettingsModel.profilesKey), notSaved: false)
                    .padding(.top, 6)
            }
            .frame(width: 180)
            .padding([.leading, .top, .bottom], 20)

            if let profile = selected(in: profiles) {
                ProfileDetail(model: model, profile: profile, changing: changing)
            } else {
                Text(profiles.isEmpty ? "No profiles. Add one with +." : "Select a profile.")
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, minHeight: 300)
                    .padding(20)
            }
        }
    }

    /// What a profile of `profiles/` shows: `file` is the core's
    /// `source`, sanitised (``SettingsModel/Profile/file``, S3).
    static func fileNote(_ file: String) -> String {
        "Defined in \(file). Edit that file to change this profile."
    }

    private func selected(in profiles: [SettingsModel.Profile]) -> SettingsModel.Profile? {
        guard let selection else { return nil }
        return profiles.first { $0.index == selection }
    }
}

/// The selected profile's settings: disabled while the list is locked,
/// and for a profile of `profiles/`, which shows its file instead.
private struct ProfileDetail: View {
    let model: SettingsModel
    let profile: SettingsModel.Profile
    let changing: Bool

    var body: some View {
        Form {
            if let file = profile.file {
                Section {
                    // Plain text: the file name is the core's, sanitised.
                    Text(ProfilesPane.fileNote(file))
                        .textSelection(.enabled)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            Section {
                ForEach(ProfilesPane.fields) { field in
                    SettingsFieldRow(
                        field: field,
                        context: .init(
                            model: model, path: profile.key(field.key), profile: profile.index,
                            listGeneration: model.profileListGeneration))
                    .disabled(readOnly)
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
                                profile: profile.index, listGeneration: model.profileListGeneration),
                            disabled: inherits)
                        .disabled(readOnly)
                        Toggle(
                            "Inherit from Appearance",
                            isOn: Binding(
                                get: { model.inherits(profile.index, field.key) },
                                set: { model.setInherits(profile.index, field.key, $0) }))
                        .controlSize(.small)
                        .disabled(readOnly)
                    }
                }
            }
        }
        .formStyle(.grouped)
        // Rows of another profile must not keep this one's draft text: nor
        // the same index once a hand edit put another profile there.
        .id([profile.index, model.profileListGeneration])
    }

    /// No control edits: the list is locked, or the profile is a file's.
    private var readOnly: Bool {
        changing || profile.file != nil
    }
}
