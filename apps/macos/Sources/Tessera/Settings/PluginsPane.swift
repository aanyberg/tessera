import SwiftUI
import TesseraKit

/// The Plugins pane's edits of `[plugins] enabled` (ADR 0047; ADR 0044,
/// Plugins pane).
///
/// A toggle writes the whole list, as shown (an edit not yet applied
/// included, ids this version does not have and the file's order kept),
/// with the id added at the end or every copy of it taken out: one set of
/// `plugins.enabled` on the write queue, refused and retried as every
/// other edit. From the moment one is queued until a configuration applied
/// after it, the toggles are disabled and a line says why, as the Profiles
/// pane's list is while its indices move.
extension SettingsModel {
    /// The key path the toggles write, and the key of their message.
    static let pluginsKey = "plugins.enabled"
    /// At most this many ids this version does not have are listed under
    /// the plugins, and a line for the rest (S5).
    static let maxUnknownPluginsShown = 16
    /// An unknown id is shown in at most this many characters.
    static let maxPluginIDShown = 80
    /// The warning for the ids past `TesseraPlugin.maxEnabled`, as the pane
    /// shows it (the core's configuration loader logs its own; the FFI does
    /// not pass it on).
    static let pluginsSkippedWarning =
        "[plugins] enabled lists more than \(TesseraPlugin.maxEnabled) ids; the rest are ignored"
    /// Why a toggle turned on at `TesseraPlugin.maxEnabled` ids or more did
    /// nothing; true of any number of them.
    static let pluginsFullRefusal =
        "[plugins] enabled is full: Tessera reads its first \(TesseraPlugin.maxEnabled) ids. "
        + "To turn this plugin on, list fewer than \(TesseraPlugin.maxEnabled) in config.toml."
    /// How many characters of a shortened id's end are kept.
    private static let pluginIDTailShown = 20

    /// Every id `[plugins] enabled` lists, as shown.
    private var listedPluginIDs: [String] {
        value(Self.pluginsKey)?.array?.compactMap(\.string) ?? []
    }

    /// The ids the host reads, as shown: the first `TesseraPlugin.maxEnabled`.
    private var enabledPluginIDs: ArraySlice<String> {
        listedPluginIDs.prefix(TesseraPlugin.maxEnabled)
    }

    /// Whether `[plugins] enabled` lists `id` among the ids the host reads,
    /// as shown.
    func isPluginEnabled(_ id: String) -> Bool {
        enabledPluginIDs.contains { $0.utf8.elementsEqual(id.utf8) }
    }

    /// Whether an edit of `[plugins] enabled` is queued, running, or written
    /// and not yet applied: until then the toggles do nothing.
    var pluginsChanging: Bool {
        isPending(Self.pluginsKey)
    }

    /// Why the toggles are disabled, while they are (VoiceOver reads it).
    var pluginsStatus: String? {
        pluginsChanging
            ? "Waiting for config.toml to be applied; plugins can be turned on or off once it is." : nil
    }

    /// The pane's warning while `[plugins] enabled` lists more ids than the
    /// host reads, with how many it ignores; `nil` otherwise (the core's
    /// configuration loader logs its own, which names only the cap).
    var pluginsSkipWarning: String? {
        let count = listedPluginIDs.count
        guard count > TesseraPlugin.maxEnabled else { return nil }
        let ignored = count - TesseraPlugin.maxEnabled
        return "\(Self.pluginsSkippedWarning) (\(ignored) \(ignored == 1 ? "id" : "ids") ignored)."
    }

    /// Lists `id` in `[plugins] enabled` (at the end, unless the host reads
    /// it already), or takes every copy of it out. Nothing while
    /// ``pluginsChanging``; at `TesseraPlugin.maxEnabled` ids, turning one on is
    /// refused with ``pluginsFullRefusal`` and writes nothing (the host
    /// would ignore the id).
    func setPlugin(_ id: String, enabled: Bool) {
        guard !pluginsChanging else { return }
        let listed = value(Self.pluginsKey)?.array ?? []
        let entry = TesseraConfigValue.string(id)
        if enabled {
            guard !isPluginEnabled(id) else { return }
            guard listed.count < TesseraPlugin.maxEnabled else {
                show(Self.pluginsFullRefusal, for: Self.pluginsKey)
                pluginsRefusedAt = listed
                return
            }
            set(Self.pluginsKey, .array(listed + [entry]))
        } else {
            // Byte for byte, as the host compares: another spelling of the
            // id (NFD for NFC) is another id, and stays.
            set(Self.pluginsKey, .array(listed.filter { $0.string.map { !$0.utf8.elementsEqual(id.utf8) } ?? true }))
        }
    }

    /// The ids the host reads that are not built into this version (it
    /// ignores them), each once, as ``shownPluginID(_:)`` shows them: at
    /// most ``maxUnknownPluginsShown``, then a line for the rest.
    var unknownEnabledPlugins: [String] {
        let known = Set(plugins.map(\.id).map { Array($0.utf8) })
        var seen = Set<[UInt8]>()
        var shown: [String] = []
        var more = 0
        for id in enabledPluginIDs {
            let bytes = Array(id.utf8)
            guard !known.contains(bytes), seen.insert(bytes).inserted else { continue }
            if shown.count < Self.maxUnknownPluginsShown {
                shown.append(Self.shownPluginID(id))
            } else {
                more += 1
            }
        }
        if more > 0 { shown.append("and \(more) more") }
        return shown
    }

    /// `id` from the file as the footer shows it (S3): in quotes, printable
    /// ASCII as it is (a quote and a backslash escaped), every other code
    /// point as `\u{…}`, so ids that differ by an invisible or look-alike
    /// character (a zero-width or trailing space, another normalisation)
    /// show apart and nothing in one reaches the window unescaped. At most
    /// ``maxPluginIDShown`` characters: a longer id keeps its start and its
    /// end around an ellipsis, so ids that differ only near the end show
    /// apart too (those differing only in the part left out do not), and no
    /// escape is cut.
    static func shownPluginID(_ id: String) -> String {
        var whole = ""
        for scalar in id.unicodeScalars {
            whole += shownScalar(scalar)
            if whole.utf8.count > maxPluginIDShown {
                return "\"\(shortenedPluginID(id))\""
            }
        }
        return "\"\(whole)\""
    }

    /// `id`, too long to show whole, as its start, "…" and its end. Every
    /// piece is ASCII, so its bytes are its characters.
    private static func shortenedPluginID(_ id: String) -> String {
        var head = ""
        let headLimit = maxPluginIDShown - 1 - pluginIDTailShown
        for scalar in id.unicodeScalars {
            let piece = shownScalar(scalar)
            guard head.utf8.count + piece.utf8.count <= headLimit else { break }
            head += piece
        }
        var tail = ""
        for scalar in id.unicodeScalars.reversed() {
            let piece = shownScalar(scalar)
            guard tail.utf8.count + piece.utf8.count <= pluginIDTailShown else { break }
            tail = piece + tail
        }
        return head + "…" + tail
    }

    /// One code point of an id as ``shownPluginID(_:)`` shows it.
    private static func shownScalar(_ scalar: Unicode.Scalar) -> String {
        switch scalar.value {
        case 0x22: "\\\""
        case 0x5C: "\\\\"
        case 0x20...0x7E: String(scalar)
        default: "\\u{\(String(scalar.value, radix: 16))}"
        }
    }
}

/// Plugins: every plugin built into this version, each with a toggle that
/// lists it in `[plugins] enabled`, its version, id and capabilities, where
/// it stands and why it was refused or failed. The status bar its items go
/// to is #49.
struct PluginsPane: View {
    let model: SettingsModel

    var body: some View {
        SettingsForm(model: model) {
            if model.pluginsUnreadable {
                Section {
                    SettingsMessage(text: "The plugins could not be read.", notSaved: false)
                }
            } else if model.plugins.isEmpty {
                Section {
                    Text("This version of Tessera has no plugins.")
                        .foregroundStyle(.secondary)
                }
            }
            ForEach(model.plugins) { plugin in
                Section {
                    PluginRow(model: model, plugin: plugin)
                }
            }
            Section {
                if let status = model.pluginsStatus {
                    Text(status)
                        .font(.callout)
                        .foregroundStyle(.secondary)
                }
                // The core's refusal (displayMessage, sanitised) or the
                // pane's own text at the cap.
                SettingsMessage(text: model.message(SettingsModel.pluginsKey))
                if let skipped = model.pluginsSkipWarning {
                    Label(skipped, systemImage: "exclamationmark.triangle")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                        .accessibilityLabel(skipped)
                }
                // Escaped by shownPluginID (S3).
                let unknown = model.unknownEnabledPlugins
                if !unknown.isEmpty {
                    Text("config.toml also lists plugins this version does not have: \(unknown.joined(separator: ", "))")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
                Text(
                    "Plugins are built into Tessera and run only while turned on, with only the capabilities listed. Turning one on or off writes [plugins] enabled in config.toml."
                )
                .font(.callout)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            }
        }
        // The host starts and stops plugins on its own thread: read them
        // again whenever the pane shows (ADR 0047).
        .onAppear { model.readPluginsAgain() }
    }

    /// The binding a plugin's toggle uses; the tests set it as the toggle
    /// does.
    @MainActor
    static func enabledBinding(_ model: SettingsModel, _ id: String) -> Binding<Bool> {
        Binding(
            get: { model.isPluginEnabled(id) },
            set: { model.setPlugin(id, enabled: $0) })
    }

    /// The plugin's name as shown (S3), or its id when nothing of the name
    /// is left.
    static func title(_ plugin: TesseraPlugin) -> String {
        let name = SettingsModel.sanitisedMessage(plugin.name)
        return name.isEmpty ? SettingsModel.sanitisedMessage(plugin.id) : name
    }

    /// The line under the name: version and id, sanitised.
    static func subtitle(_ plugin: TesseraPlugin) -> String {
        "Version \(SettingsModel.sanitisedMessage(plugin.version)) · \(SettingsModel.sanitisedMessage(plugin.id))"
    }

    /// What one capability lets the plugin do, as the pane names it; one
    /// this pane does not know is shown as the core names it, sanitised.
    static func capabilityTitle(_ capability: String) -> String {
        switch capability {
        case "sessions": "Sessions (titles and commands)"
        case "status_bar": "Status bar items"
        case "storage": "Storage (in memory)"
        case "output": "Terminal output"
        case "panels": "Side panels"
        case "commands": "Commands"
        case "fs": "Files"
        case "http": "Network"
        default: SettingsModel.sanitisedMessage(capability)
        }
    }

    /// The plugin's capabilities, as the pane names them.
    static func capabilities(_ plugin: TesseraPlugin) -> String {
        let titles = plugin.capabilities.map(capabilityTitle).filter { !$0.isEmpty }
        return titles.isEmpty ? "None" : titles.joined(separator: ", ")
    }

    /// Where the plugin stands, as its toggle shows it: starting or stopping
    /// while an edit or the host has yet to reach it, else its state.
    @MainActor
    static func status(_ model: SettingsModel, _ plugin: TesseraPlugin) -> String {
        let shown = model.isPluginEnabled(plugin.id)
        if shown != plugin.enabled || plugin.isSettling {
            return shown ? "Starting…" : "Stopping…"
        }
        return switch plugin.state {
        case .active: "Running"
        case .refused: "Not started"
        case .failed: "Failed"
        case .disabled: "Off"
        }
    }

    /// Why the plugin was refused or failed (the host's message, S3), and
    /// for a failed one how to try it again; `nil` otherwise, and while an
    /// edit or the host has yet to reach it.
    @MainActor
    static func problem(_ model: SettingsModel, _ plugin: TesseraPlugin) -> String? {
        guard plugin.state == .refused || plugin.state == .failed, plugin.enabled,
            model.isPluginEnabled(plugin.id)
        else { return nil }
        var text = SettingsModel.sanitisedMessage(plugin.message ?? "")
        if text.isEmpty {
            text = plugin.state == .refused ? "Refused." : "Failed."
        } else {
            text = text.prefix(1).uppercased() + text.dropFirst() + "."
        }
        if plugin.state == .failed {
            text += " Turn it off and on again to try again."
        }
        return text
    }
}

/// One plugin: its toggle, status, capabilities and problem.
private struct PluginRow: View {
    let model: SettingsModel
    let plugin: TesseraPlugin

    var body: some View {
        let title = PluginsPane.title(plugin)
        Toggle(isOn: PluginsPane.enabledBinding(model, plugin.id)) {
            VStack(alignment: .leading, spacing: 2) {
                Text(title)
                Text(PluginsPane.subtitle(plugin))
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .disabled(model.pluginsChanging)
        .accessibilityLabel(title)
        .accessibilityHint("Turns the plugin on or off in config.toml.")
        LabeledContent("Status", value: PluginsPane.status(model, plugin))
        LabeledContent("Capabilities") {
            Text(PluginsPane.capabilities(plugin))
                .multilineTextAlignment(.trailing)
                .fixedSize(horizontal: false, vertical: true)
        }
        if let problem = PluginsPane.problem(model, plugin) {
            Label(problem, systemImage: "exclamationmark.triangle.fill")
                .font(.callout)
                .foregroundStyle(.red)
                .fixedSize(horizontal: false, vertical: true)
                .accessibilityLabel(problem)
        }
    }
}
