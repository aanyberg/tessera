import AppKit
import Observation
import TesseraKit
import os

/// Edits `config.toml` for the Settings window: the core's
/// ``TesseraConfigWriter``, or a stand-in in tests. Called on the model's
/// write queue, never on the main thread (ADR 0044).
protocol SettingsConfigWriting: Sendable {
    func set(_ key: String, to value: TesseraConfigValue) throws(TesseraConfigEditError)
    func remove(_ key: String) throws(TesseraConfigEditError)
    func importTheme(at file: URL) throws(TesseraConfigEditError) -> String
}

extension TesseraConfigWriter: SettingsConfigWriting {}

/// Where the Settings window reads the configuration and gets its writer:
/// the running ``TesseraApp``, or a stand-in in tests.
@MainActor
protocol SettingsConfigSource: AnyObject {
    func configSnapshot() throws -> TesseraConfigSnapshot
    func themes() throws -> [TesseraTheme]
    /// The key map in effect, which the Keys pane shows (ADR 0036).
    func keymap() throws -> TesseraKeymap
    var settingsWriter: any SettingsConfigWriting { get }
}

extension TesseraApp: SettingsConfigSource {
    var settingsWriter: any SettingsConfigWriting { configWriter }
}

/// The time the model coalesces slider and stepper edits by: the system's
/// monotonic clock and the main queue, or a manual clock in tests.
@MainActor
protocol SettingsClock: AnyObject {
    /// Monotonic time.
    var now: ContinuousClock.Instant { get }
    /// Runs `action` on the main actor once `delay` has passed, unless the
    /// returned timer is cancelled first.
    func schedule(after delay: Duration, _ action: @escaping @MainActor () -> Void) -> SettingsTimer
}

/// A scheduled ``SettingsClock`` action; cancelling it keeps it from
/// running.
@MainActor
final class SettingsTimer {
    private(set) var isCancelled = false

    func cancel() {
        isCancelled = true
    }
}

/// ``SettingsClock`` on `ContinuousClock` and the main queue.
@MainActor
final class SystemSettingsClock: SettingsClock {
    var now: ContinuousClock.Instant { .now }

    func schedule(after delay: Duration, _ action: @escaping @MainActor () -> Void) -> SettingsTimer {
        let timer = SettingsTimer()
        let seconds = Double(delay.components.seconds) + Double(delay.components.attoseconds) * 1e-18
        DispatchQueue.main.asyncAfter(deadline: .now() + seconds) {
            MainActor.assumeIsolated {
                guard !timer.isCancelled else { return }
                action()
            }
        }
        return timer
    }
}

/// What the Settings window shows and edits (ADR 0044): the configuration
/// in effect, read from the core when the window opens and again after every
/// configuration the core applies, with the edits not yet applied laid over
/// it.
///
/// Every edit is one `tessera_config_set` or `_remove` of one key path,
/// made in order on one serial background queue (``writeQueue``); the
/// core's configuration watch applies the saved file, and the model reads
/// it back on ``reload()``. Until then the control shows the value it was
/// set to. A slider or stepper being dragged writes at most once per
/// ``coalescingInterval``, and once more with the final value when it is
/// released. An edit the core refuses (the loader would warn) writes
/// nothing: the control shows the file's value again, and the core's
/// message (sanitised, S3) next to it. An edit that meets an external edit
/// of the file since the core read it (`Conflict`) is retried once, and the
/// model reloads.
@MainActor
@Observable
final class SettingsModel {
    /// The serial queue every edit runs on, shared by every model so edits
    /// keep their order even across a closed and reopened window.
    nonisolated static let writeQueue = DispatchQueue(label: "com.aanyberg.tessera.settings.write", qos: .userInitiated)
    /// Slider and stepper edits are written at most this often while the
    /// control is dragged.
    static let coalescingInterval: Duration = .milliseconds(100)
    /// Longest message shown next to a control, in characters.
    static let maxMessageLength = 400

    private static let log = Logger(subsystem: "com.aanyberg.tessera", category: "settings")

    /// The configuration in effect, as last read.
    private(set) var snapshot: TesseraConfigSnapshot
    /// The themes `appearance.theme` can name, as last read.
    private(set) var themes: [TesseraTheme] = []
    /// Why the configuration could not be read, if it could not.
    private(set) var loadError: String?
    /// The key map in effect, as last read, of ``snapshot``'s generation
    /// when both could be read alike; `nil` if it could not be read.
    private(set) var keymap: TesseraKeymap?
    /// The Keys pane's state: recording, conflicts and its edits of
    /// `[keys]`.
    @ObservationIgnored let keys: KeysModel

    /// Values set but not yet seen in ``snapshot``, by key path; `.null`
    /// for a removed key.
    private var pending: [String: TesseraConfigValue] = [:]
    /// The snapshot generation when each key's latest edit was queued: the
    /// pending value goes once a later configuration has been applied and
    /// nothing is queued for the key.
    private var queuedAtGeneration: [String: UInt64] = [:]
    /// Edits queued and not finished, by key path.
    private var inFlight: [String: Int] = [:]
    /// Messages next to controls, by key path (or by the name of the action,
    /// as ``importThemeKey``), already sanitised.
    private(set) var messages: [String: String] = [:]
    private var throttles: [String: Throttle] = [:]
    /// Moves when a configuration is applied whose profiles are not the
    /// ones before it but for this window's own edits (a hand edit added,
    /// removed, reordered or changed one, whatever the names): the index a
    /// profile's controls name may now be another profile, so they start
    /// again from it.
    private(set) var profileListGeneration = 0

    @ObservationIgnored private weak var source: (any SettingsConfigSource)?
    @ObservationIgnored private let clock: any SettingsClock

    /// The key of the message the theme import shows.
    static let importThemeKey = "action:import_theme"
    /// The key of the message the profile list shows.
    static let profilesKey = "action:profiles"

    /// One slider's or stepper's coalescing state.
    private struct Throttle {
        var lastWrite: ContinuousClock.Instant?
        var lastWritten: TesseraConfigValue?
        var timer: SettingsTimer?
    }

    init(source: any SettingsConfigSource, clock: any SettingsClock = SystemSettingsClock()) {
        self.source = source
        self.clock = clock
        snapshot = TesseraConfigSnapshot(generation: 0, config: .table([:]))
        keys = KeysModel()
        keys.settings = self
        reload()
    }

    // MARK: Reading

    /// Reads the configuration and the themes again: when the window opens
    /// and after every configuration the core applies. A value set and not
    /// yet applied keeps showing until a configuration applied after it
    /// arrives.
    func reload() {
        guard let source else {
            loadError = "Tessera’s core is not running."
            return
        }
        do {
            let entries = profileEntries
            snapshot = try source.configSnapshot()
            readKeymap(source)
            loadError = nil
            if profileEntries != entries { profileListGeneration += 1 }
        } catch {
            loadError = "The configuration could not be read."
            Self.log.error("configuration not read: \(String(describing: error), privacy: .public)")
        }
        do {
            themes = try source.themes()
        } catch {
            Self.log.error("themes not read: \(String(describing: error), privacy: .public)")
        }
        for key in Array(pending.keys) where (inFlight[key] ?? 0) == 0 && throttles[key]?.timer == nil {
            if let queued = queuedAtGeneration[key], snapshot.generation <= queued { continue }
            pending[key] = nil
            queuedAtGeneration[key] = nil
        }
    }

    /// Reads the key map. The configuration and the key map are read one
    /// after the other, so an apply in between can give them different
    /// generations (#348): both are read again, a few times at most, until
    /// they agree. The Keys pane edits nothing while they do not.
    private func readKeymap(_ source: any SettingsConfigSource) {
        do {
            var map = try source.keymap()
            for _ in 0..<3 where map.generation != snapshot.generation {
                snapshot = try source.configSnapshot()
                map = try source.keymap()
            }
            keymap = map
        } catch {
            keymap = nil
            Self.log.error("key map not read: \(String(describing: error), privacy: .public)")
        }
    }

    /// The writer edits go through, while the core runs.
    var writer: (any SettingsConfigWriting)? {
        source?.settingsWriter
    }

    /// The value at `key`: the one last set while it is not yet applied,
    /// else the configuration's. `nil` for a key the configuration leaves
    /// out (or one removed).
    func value(_ key: String) -> TesseraConfigValue? {
        if let pending = pending[key] {
            return pending == .null ? nil : pending
        }
        return snapshot[key]
    }

    /// The message next to the control of `key`, if an edit of it failed.
    func message(_ key: String) -> String? {
        messages[key]
    }

    /// Whether any edit is queued or running (for tests and the window's
    /// close, which need not wait).
    var isWriting: Bool {
        inFlight.values.contains { $0 > 0 }
    }

    // MARK: Writing

    /// Sets `key` to `value`, unless it has that value already.
    func set(_ key: String, _ value: TesseraConfigValue) {
        cancelThrottle(key)
        guard self.value(key) != value else { return }
        enqueue(key, value)
    }

    /// Removes `key` so its default (or, in a profile, `[appearance]`'s)
    /// applies, unless it is not set.
    func remove(_ key: String) {
        cancelThrottle(key)
        guard pending[key] != .null, pending[key] != nil || snapshot[key] != nil else { return }
        enqueue(key, nil)
    }

    /// A slider or stepper at `key` moved to `value`: shown at once, written
    /// at once if nothing was written for ``coalescingInterval``, else
    /// when it has passed (with the latest value then).
    func change(_ key: String, _ value: TesseraConfigValue) {
        pending[key] = value
        var throttle = throttles[key] ?? Throttle()
        let now = clock.now
        if let last = throttle.lastWrite, now - last < Self.coalescingInterval {
            if throttle.timer == nil {
                throttle.timer = clock.schedule(after: Self.coalescingInterval - (now - last)) { [weak self] in
                    self?.flushThrottle(key)
                }
            }
            throttles[key] = throttle
            return
        }
        throttle.timer?.cancel()
        throttle.timer = nil
        throttles[key] = throttle
        writeThrottled(key, value)
    }

    /// The slider or stepper at `key` was released: its final value is
    /// written if it was not yet.
    func endChange(_ key: String) {
        guard let throttle = throttles[key] else { return }
        throttle.timer?.cancel()
        throttles[key]?.timer = nil
        if let value = pending[key], value != throttle.lastWritten {
            writeThrottled(key, value)
        }
        throttles[key] = nil
    }

    private func flushThrottle(_ key: String) {
        throttles[key]?.timer = nil
        guard let value = pending[key], value != throttles[key]?.lastWritten else { return }
        writeThrottled(key, value)
    }

    private func writeThrottled(_ key: String, _ value: TesseraConfigValue) {
        throttles[key, default: Throttle()].lastWrite = clock.now
        throttles[key, default: Throttle()].lastWritten = value
        if value == snapshot[key], (inFlight[key] ?? 0) == 0 {
            // Back where the file is: nothing to write.
            pending[key] = nil
            return
        }
        enqueue(key, value)
    }

    private func cancelThrottle(_ key: String) {
        throttles[key]?.timer?.cancel()
        throttles[key] = nil
    }

    /// Queues one edit: `value` set at `key`, or `key` removed for `nil`.
    private func enqueue(_ key: String, _ value: TesseraConfigValue?) {
        guard let source else {
            loadError = "Tessera’s core is not running."
            return
        }
        pending[key] = value ?? .null
        queuedAtGeneration[key] = snapshot.generation
        inFlight[key, default: 0] += 1
        messages[key] = nil
        let writer = source.settingsWriter
        Self.writeQueue.async { [weak self] in
            var retried = false
            var result = Self.perform(writer, key, value)
            if case .failure(let error) = result, error.kind == .conflict {
                // The file changed since the core read it: the core reads
                // it again on the retry, so the other edit stays.
                retried = true
                result = Self.perform(writer, key, value)
            }
            let outcome = result
            Task { @MainActor [weak self] in
                self?.finish(key, outcome, retried: retried, written: value ?? .null)
            }
        }
    }

    private nonisolated static func perform(
        _ writer: any SettingsConfigWriting, _ key: String, _ value: TesseraConfigValue?
    ) -> Result<Void, TesseraConfigEditError> {
        do throws(TesseraConfigEditError) {
            if let value {
                try writer.set(key, to: value)
            } else {
                try writer.remove(key)
            }
            return .success(())
        } catch {
            return .failure(error)
        }
    }

    /// An edit of `key` (to `written`, `.null` for a removal) has run.
    private func finish(
        _ key: String, _ result: Result<Void, TesseraConfigEditError>, retried: Bool,
        written: TesseraConfigValue? = nil
    ) {
        inFlight[key, default: 1] -= 1
        if inFlight[key] == 0 { inFlight[key] = nil }
        if case .failure(let error) = result {
            // S8: the message may quote the file; only its kind is logged.
            Self.log.info("setting not saved: \(String(describing: error.kind), privacy: .public)")
            messages[key] = Self.displayMessage(error)
            if let written, movesProfiles(key, written) {
                // No row shows the key of an add or delete: the list does.
                messages[Self.profilesKey] = Self.displayMessage(error)
            }
            if inFlight[key] == nil {
                pending[key] = nil
                queuedAtGeneration[key] = nil
                cancelThrottle(key)
            }
        }
        if retried {
            reload()
            if case .success = result, inFlight[key] == nil, let written {
                // The external edit was applied before this one: show this
                // one until a configuration after it applies.
                pending[key] = written
                queuedAtGeneration[key] = snapshot.generation
            }
        }
    }

    /// What the user sees for `error`: the core's message, sanitised (S3).
    static func displayMessage(_ error: TesseraConfigEditError) -> String {
        let text = sanitisedMessage(error.message)
        if !text.isEmpty { return text }
        return switch error.kind {
        case .refused: "Tessera would not use this value."
        case .conflict: "config.toml changed on disk; try again."
        case .invalidArgument: "This value cannot be written."
        case .io: "config.toml could not be read or written."
        case .core: "The setting could not be saved."
        }
    }

    /// `text` from the core or the file as one line for the window (S3):
    /// control and formatting characters (bidirectional overrides and
    /// isolates, zero-width characters) and unassigned and private-use code
    /// points dropped, white space runs as one space, at most
    /// ``maxMessageLength`` characters.
    static func sanitisedMessage(_ text: String) -> String {
        var scalars = String.UnicodeScalarView()
        var pendingSpace = false
        for scalar in text.unicodeScalars {
            if scalar.properties.isWhitespace {
                pendingSpace = !scalars.isEmpty
                continue
            }
            switch scalar.properties.generalCategory {
            case .control, .format, .lineSeparator, .paragraphSeparator, .surrogate, .privateUse, .unassigned:
                continue
            default:
                if pendingSpace { scalars.append(" ") }
                pendingSpace = false
                scalars.append(scalar)
            }
        }
        let line = String(scalars)
        return line.count > maxMessageLength ? String(line.prefix(maxMessageLength - 1)) + "…" : line
    }

    // MARK: Themes

    /// Imports the colour scheme at `file` into `themes/` and selects it;
    /// the importer's message next to Import Theme when it refuses the file.
    func importTheme(at file: URL) {
        guard let source else { return }
        let writer = source.settingsWriter
        let key = Self.importThemeKey
        messages[key] = nil
        inFlight[key, default: 0] += 1
        Self.writeQueue.async { [weak self] in
            let result: Result<String, TesseraConfigEditError>
            do throws(TesseraConfigEditError) {
                result = .success(try writer.importTheme(at: file))
            } catch {
                result = .failure(error)
            }
            let outcome = result
            Task { @MainActor [weak self] in
                guard let self else { return }
                self.finish(key, outcome.map { _ in () }, retried: false)
                guard case .success(let id) = outcome else { return }
                self.reload()
                self.set("appearance.theme", .string(id))
            }
        }
    }

    // MARK: Profiles

    /// One `[[profiles]]` entry as the window lists it.
    struct Profile: Identifiable, Equatable {
        /// Its place in `[[profiles]]`, what its key paths name.
        var index: Int
        /// Its name as written.
        var name: String
        /// Its name as shown: ``MainMenu/profileTitle(_:)`` (S3), numbered
        /// when an earlier profile shows the same.
        var title: String

        var id: Int { index }
        /// The key path of `key` in this profile.
        func key(_ key: String) -> String { "profiles[\(index)].\(key)" }
    }

    /// The profiles in file order.
    var profiles: [Profile] {
        let names = snapshot["profiles"]?.array ?? []
        var titles = Set<String>()
        return names.enumerated().map { index, profile in
            let name = profile["name"]?.string ?? ""
            let base = MainMenu.profileTitle(name)
            var title = base
            var number = 2
            while !titles.insert(title).inserted {
                title = "\(base) (\(number))"
                number += 1
            }
            return Profile(index: index, name: name, title: title)
        }
    }

    /// Whether an add, duplicate or delete of a profile is queued, running
    /// or written and not yet applied. Edits name a profile by its index,
    /// which moves once the watch applies the file, so until then the list,
    /// its buttons and the profile's controls are disabled and the profile
    /// actions do nothing. A failed write ends it at once (its message by
    /// the list); a written one ends with the next configuration applied.
    var profilesChanging: Bool {
        pending.contains { movesProfiles($0.key, $0.value) }
    }

    /// Whether `value` (`.null` for a removal) at `key` adds or deletes a
    /// profile: `profiles[i]` removed, or a key of a profile past the
    /// configuration's last.
    private func movesProfiles(_ key: String, _ value: TesseraConfigValue) -> Bool {
        guard key.hasPrefix("profiles["), let close = key.firstIndex(of: "]"),
            let index = Int(key[key.index(key.startIndex, offsetBy: 9)..<close])
        else { return false }
        let rest = key[key.index(after: close)...]
        if rest.isEmpty { return value == .null }
        return index >= (snapshot["profiles"]?.array?.count ?? 0)
    }

    /// The applied profiles in file order, each as its values by key path
    /// (`appearance.theme`), but for the keys this window set and has not
    /// yet seen applied (so its own edits, written or still moving, change
    /// nothing here): two profiles named alike, or not named, still differ.
    private var profileEntries: [[String: TesseraConfigValue]] {
        let pending = pending
        return (snapshot["profiles"]?.array ?? []).enumerated().map { index, profile in
            var values: [String: TesseraConfigValue] = [:]
            func add(_ value: TesseraConfigValue, at path: String) {
                if case .table(let table) = value {
                    for (key, value) in table { add(value, at: path.isEmpty ? key : "\(path).\(key)") }
                } else if pending["profiles[\(index)].\(path)"] == nil {
                    values[path] = value
                }
            }
            add(profile, at: "")
            return values
        }
    }

    /// `base`, or `base 2`, `base 3`… : the first no profile is named.
    private func unusedProfileName(_ base: String) -> String {
        let names = Set(profiles.map(\.name))
        var name = base
        var number = 2
        while names.contains(name) {
            name = "\(base) \(number)"
            number += 1
        }
        return name
    }

    /// Adds a profile named "New Profile" (numbered if taken) at the end;
    /// returns its index (`nil` while ``profilesChanging``).
    @discardableResult
    func addProfile() -> Int? {
        guard !profilesChanging else { return nil }
        let index = profiles.count
        messages[Self.profilesKey] = nil
        set("profiles[\(index)].name", .string(unusedProfileName("New Profile")))
        return index
    }

    /// The keys of a profile the window edits, besides `name`.
    static let profileKeys = ["command", "working_directory"] + SettingsField.profileAppearanceKeys

    /// Adds a copy of the profile at `index` at the end, named after it
    /// with " Copy"; returns the copy's index (`nil` while
    /// ``profilesChanging``).
    @discardableResult
    func duplicateProfile(_ index: Int) -> Int? {
        let all = profiles
        guard !profilesChanging, all.indices.contains(index) else { return nil }
        let copy = all.count
        messages[Self.profilesKey] = nil
        set("profiles[\(copy)].name", .string(unusedProfileName(all[index].name + " Copy")))
        for key in Self.profileKeys {
            if let value = value(all[index].key(key)) {
                set("profiles[\(copy)].\(key)", value)
            }
        }
        return copy
    }

    /// Renames the profile at `index`; `[general] default_profile` follows
    /// when it named this profile, so new tabs keep starting with it.
    func renameProfile(_ index: Int, to name: String) {
        let all = profiles
        guard !profilesChanging, all.indices.contains(index), all[index].name != name else { return }
        set(all[index].key("name"), .string(name))
        if value("general.default_profile")?.string == all[index].name {
            set("general.default_profile", .string(name))
        }
    }

    /// Deletes the profile at `index`, except the default profile (new tabs
    /// would lose it): `false` and a message by the list for that one, and
    /// `false` while ``profilesChanging``.
    @discardableResult
    func deleteProfile(_ index: Int) -> Bool {
        let all = profiles
        guard !profilesChanging, all.indices.contains(index) else { return false }
        if value("general.default_profile")?.string == all[index].name {
            messages[Self.profilesKey] =
                "“\(all[index].title)” is the default profile. Choose another default profile in General first."
            return false
        }
        messages[Self.profilesKey] = nil
        remove("profiles[\(index)]")
        return true
    }

    /// Whether the profile at `index` takes `key` (`appearance.theme`) from
    /// `[appearance]`.
    func inherits(_ index: Int, _ key: String) -> Bool {
        value("profiles[\(index)].\(key)") == nil
    }

    /// Makes the profile at `index` take `key` from `[appearance]`
    /// (removes its own), or gives it its own, starting at `[appearance]`'s
    /// value.
    func setInherits(_ index: Int, _ key: String, _ inherits: Bool) {
        guard !profilesChanging else { return }
        let path = "profiles[\(index)].\(key)"
        if inherits {
            remove(path)
        } else if let base = value(key) {
            set(path, base)
        }
    }
}
