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
    func addProfile(
        named base: String, keys: [(key: String, value: TesseraConfigValue)]
    ) throws(TesseraConfigEditError) -> TesseraAddedProfile
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
    /// The built-in plugins and their states, which the Plugins pane shows
    /// (ADR 0047).
    func plugins() throws -> [TesseraPlugin]
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
/// Every edit is one `tessera_config_set` or `_remove` of one key path, or
/// one `tessera_config_add_profile` for Add and Duplicate (#312), made in
/// order on one serial background queue (``writeQueue``); the
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
    /// Most lines a message next to a control shows.
    static let maxMessageLines = 12

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
    /// The built-in plugins, as last read (ADR 0047), in the core's order.
    private(set) var plugins: [TesseraPlugin] = []
    /// Whether the plugins could not be read when last asked.
    private(set) var pluginsUnreadable = false
    /// The plugins read again while one of them settles
    /// (``TesseraPlugin/isSettling``): the host starts and stops them on its
    /// own thread after the apply, and tells the app nothing more.
    @ObservationIgnored private var pluginRead: SettingsTimer?

    /// Values set but not yet seen in ``snapshot``, by key path; `.null`
    /// for a removed key.
    private var pending: [String: TesseraConfigValue] = [:]
    /// The snapshot generation when each key's latest edit was queued: the
    /// pending value goes once a later configuration has been applied and
    /// nothing is queued for the key.
    private var queuedAtGeneration: [String: UInt64] = [:]
    /// The applied `[[profiles]]` when an add or delete of a profile was
    /// queued, by its key path: its pending value goes only once the
    /// listed profiles are no longer these (#344 m-b), not with a
    /// configuration an earlier edit made.
    private var movedFrom: [String: [TesseraConfigValue]] = [:]
    /// Edits queued and not finished, by key path (an add of a profile by
    /// ``profilesKey``).
    private var inFlight: [String: Int] = [:]
    /// An Add or Duplicate (one `tessera_config_add_profile`), from the
    /// moment it is queued until the listed profiles show it or it fails.
    private var addition: Addition?
    /// Messages next to controls, by key path (or by the name of the action,
    /// as ``importThemeKey``), already sanitised.
    private(set) var messages: [String: String] = [:]
    private var throttles: [String: Throttle] = [:]
    /// The values sliders and steppers wrote and no configuration has shown
    /// yet, oldest first, at most ``maxUnappliedWrites`` per key: what tells
    /// this window's own values from a hand edit during a press (#517).
    @ObservationIgnored private var unappliedWrites: [String: [TesseraConfigValue]] = [:]
    /// Moves when a configuration is applied whose profiles are not the
    /// ones before it but for this window's own edits (a hand edit added,
    /// removed, reordered or changed one, whatever the names): the index a
    /// profile's controls name may now be another profile, so they start
    /// again from it.
    private(set) var profileListGeneration = 0
    /// The index of the profile an Add or Duplicate wrote, for the
    /// Profiles pane to select (``takeSelectionRequest()``): whichever pane
    /// shows the model then, so Settings opened again on a waiting model
    /// selects the copy too (#312 m5). Nothing holds the pane (#312 m1).
    private(set) var selectionRequest: Int?
    /// How long a written add or delete of a profile waits for a
    /// configuration that shows it before the lock goes (the Keys pane's
    /// settle rule, ``KeysModel/settleTimeout``): an apply the window
    /// missed, or another app that saved config.toml from a stale read and
    /// dropped it (#312 adversarial M4). Then the applied configuration is
    /// read again and the list says the add or delete did not apply.
    @ObservationIgnored var profileSettleTimeout: Duration = .seconds(2)
    /// The one ``profileSettleTimeout`` timer, of the add or delete that
    /// locks the list now.
    @ObservationIgnored private var profileSettle: SettingsTimer?
    /// Per key, the timer of ``reloadAfterRelease(_:after:)``: at most one, gone
    /// when it fires or the key is pressed again.
    @ObservationIgnored private var releaseSettles: [String: SettingsTimer] = [:]
    /// The add or delete ``releaseProfiles()`` stopped waiting for, and the
    /// notice it left: a configuration that shows it after all (a watch
    /// slower than the settle time) takes the notice back (#344 m2).
    @ObservationIgnored private var released: (change: ReleasedChange, notice: String)?
    /// `plugins.enabled` when a toggle was refused at the cap: the refusal
    /// stands until the list is another one (25.2 adversarial M1).
    @ObservationIgnored var pluginsRefusedAt: [TesseraConfigValue]?
    /// Why ``renameProfile(_:to:)`` last refused a name, while
    /// ``commitDraft()`` runs (#344 adversarial M5).
    @ObservationIgnored private var nameRefusal: String?

    @ObservationIgnored private weak var source: (any SettingsConfigSource)?
    /// The clock edits are coalesced by (and the app delegate times a closed
    /// window's waiting model by).
    @ObservationIgnored let clock: any SettingsClock
    /// The Settings window, which the panes' open panels are sheets on.
    @ObservationIgnored weak var window: NSWindow?
    /// The text typed into a field and not yet committed (one field has
    /// the focus at a time), and how to commit it: what moves the
    /// selection (a click on another profile, Add, Duplicate) commits it
    /// first, since the field goes before it could see the focus go.
    @ObservationIgnored private var draft: (owner: ObjectIdentifier, commit: @MainActor () -> Void)?

    /// How many written values ``unappliedWrites`` keeps per key (S5): 3 s
    /// of a drag at one write per ``coalescingInterval``. A value older
    /// than that, applied during a press, counts as a hand edit: the
    /// release then shows the file until the newer values apply.
    static let maxUnappliedWrites = 32
    /// The key of the message the theme import shows.
    static let importThemeKey = "action:import_theme"
    /// The key of the message the profile list shows.
    static let profilesKey = "action:profiles"
    /// The list's notice for a slider's or stepper's value of a profile
    /// held back and not written: an add or delete locked the profiles.
    static let heldValueNotice =
        "A profile’s value set as the list was locked was not saved; set it again once the list is unlocked."
    /// While a plugin settles, the plugins are read again this often…
    static let pluginSettleInterval: Duration = .milliseconds(50)
    /// …at most this many times after each configuration applied (2 s).
    static let pluginSettleReads = 40

    /// An add or delete of a profile no configuration showed in time.
    private enum ReleasedChange {
        case add(TesseraAddedProfile)
        /// The key path of the delete, and the profiles it was queued against.
        case delete(key: String, from: [TesseraConfigValue])
    }

    /// A profile being added.
    private struct Addition {
        /// The applied profiles when it was queued (or written again after
        /// a conflict): it ends once the listed profiles are no longer
        /// these (#344 m-b), not with a configuration an earlier edit made.
        var listed: [TesseraConfigValue]
        /// The keys sent besides the name, by key path within the profile.
        var keys: [String: TesseraConfigValue]
        /// The keys this window had set and not yet seen applied when it was
        /// queued: left out of both lists as the keys pending then are, so
        /// an earlier edit's configuration applied before the add's result
        /// comes back does not end it (#312 adversarial M1).
        var masks: [String: TesseraConfigValue]
        /// Where the core put it and what it named it, once written: shown
        /// there until the watch has applied the file.
        var added: TesseraAddedProfile?
    }

    /// One slider's or stepper's coalescing state.
    private struct Throttle {
        var lastWrite: ContinuousClock.Instant?
        var lastWritten: TesseraConfigValue?
        var timer: SettingsTimer?
        /// Pressed and not yet let go (``beginChange(_:)`` to
        /// ``endChange(_:)``).
        var dragging = false
        /// This press made a step, or found one held back when it began:
        /// only then has its release anything to write (#534).
        var stepped = false
        /// The key's applied value as last seen during the press.
        var seenApplied: TesseraConfigValue?
        /// A configuration applied during the press changed the key to a
        /// value this window did not write, and no step came after it.
        var overridden = false
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
    /// arrives; an add or delete of a profile keeps showing until the
    /// listed profiles change.
    func reload() {
        guard let source else {
            loadError = "Tessera’s core is not running."
            readPluginsAgain()
            return
        }
        do {
            let entries = profileEntries
            snapshot = try source.configSnapshot()
            readKeymap(source)
            loadError = nil
            if profileEntries != entries {
                profileListGeneration += 1
                // The draft's field names an index that may now be another
                // profile: its text goes, as the field does.
                draft = nil
            }
        } catch {
            loadError = "The configuration could not be read."
            Self.log.error("configuration not read: \(String(describing: error), privacy: .public)")
        }
        do {
            themes = try source.themes()
        } catch {
            Self.log.error("themes not read: \(String(describing: error), privacy: .public)")
        }
        // After the configuration, so an apply in between reloads again.
        readPluginsAgain()
        notePressedKeysApplied()
        let masks = pending
        let listed = snapshot["profiles"]?.array ?? []
        // Read once for every key that waits for the list (21.6a m5).
        var listedEntries: [[String: TesseraConfigValue]]?
        // A slider or stepper being dragged (pressed, not yet let go) keeps
        // the value it shows, not an earlier one of the drag that has
        // applied since (#344); one stepped with no press (VoiceOver, the
        // keyboard) has no release, so it waits only for its timer.
        for key in Array(pending.keys)
        where (inFlight[key] ?? 0) == 0 && throttles[key]?.timer == nil && throttles[key]?.dragging != true {
            if let queued = queuedAtGeneration[key], snapshot.generation <= queued { continue }
            // An add or delete waits for the list to change (it, or a hand
            // edit too), not for any later configuration (#344 m-b). Both
            // lists leave out the same keys, this window's pending ones.
            if let base = movedFrom[key] {
                let current = listedEntries ?? Self.entries(listed, masking: masks)
                listedEntries = current
                if Self.entries(base, masking: masks) == current { continue }
            }
            pending[key] = nil
            queuedAtGeneration[key] = nil
            movedFrom[key] = nil
        }
        endAdditionIfApplied(masking: masks)
        withdrawReleaseNotice()
        if messages[Self.pluginsKey] == Self.pluginsFullRefusal,
            value(Self.pluginsKey)?.array != pluginsRefusedAt
        {
            // A hand edit changed the list: the refusal was about the old one.
            messages[Self.pluginsKey] = nil
            pluginsRefusedAt = nil
        }
        if !profilesChanging {
            profileSettle?.cancel()
            profileSettle = nil
        }
    }

    /// What the configuration just read means for the keys pressed now: a
    /// value this window wrote is taken off ``unappliedWrites`` with the
    /// ones before it; any other new value is a hand edit, which the
    /// release shows (#517), and a step held back until now, taken before
    /// it, is not written over it.
    private func notePressedKeysApplied() {
        for key in Array(unappliedWrites.keys) where pending[key] == nil { unappliedWrites[key] = nil }
        for (key, var throttle) in throttles where throttle.dragging {
            let applied = snapshot[key]
            guard !Self.sameApplied(applied, throttle.seenApplied) else { continue }
            throttle.seenApplied = applied
            if let index = unappliedWrites[key]?.firstIndex(where: { Self.sameApplied($0, applied) }) {
                unappliedWrites[key]?.removeFirst(index + 1)
            } else {
                throttle.overridden = true
                throttle.timer?.cancel()
                throttle.timer = nil
            }
            throttles[key] = throttle
        }
    }

    /// Whether a value written and a value read back are the same: the core
    /// keeps some floats as `f32`, so 0.3 comes back as 0.30000001192….
    private static func sameApplied(_ first: TesseraConfigValue?, _ second: TesseraConfigValue?) -> Bool {
        if let first = first?.double, let second = second?.double { return Float(first) == Float(second) }
        return first == second
    }

    /// How many values of `key` wait in ``unappliedWrites`` (for tests).
    func unappliedWriteCount(_ key: String) -> Int {
        unappliedWrites[key]?.count ?? 0
    }

    /// Takes back the notice of ``releaseProfiles()`` once the applied
    /// configuration shows the add or delete after all. The rows after a
    /// late add move on again, with their messages. A new add or delete
    /// ends this: what the notice was about is no longer followed.
    private func withdrawReleaseNotice() {
        guard let released else { return }
        switch released.change {
        case .add(let added):
            let listed = snapshot["profiles"]?.array ?? []
            guard listed.indices.contains(added.index), listed[added.index]["name"]?.string == added.name
            else { return }
            shiftProfileMessages(from: added.index, by: 1)
        case .delete(let key, let base):
            guard deleteShown(key, .null, from: base) else { return }
        }
        self.released = nil
        if messages[Self.profilesKey] == released.notice { messages[Self.profilesKey] = nil }
    }

    /// Ends the written add of a profile once the configuration lists it
    /// where the core put it (the core gave it a name no other profile
    /// has), or once the listed profiles are no longer the ones it was
    /// queued against (a hand edit of the file was applied with it, or
    /// before it), `masks` (this window's pending keys) and the keys
    /// pending when it was queued left out of both.
    private func endAdditionIfApplied(masking masks: [String: TesseraConfigValue]) {
        guard let addition, let added = addition.added else { return }
        let listed = snapshot["profiles"]?.array ?? []
        let shown = listed.indices.contains(added.index) && listed[added.index]["name"]?.string == added.name
        let all = masks.merging(addition.masks) { current, _ in current }
        if shown || Self.entries(addition.listed, masking: all) != Self.entries(listed, masking: all) {
            self.addition = nil
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

    /// Reads the plugins again, and again while one settles: after every
    /// configuration applied (``reload()``) and when the Plugins pane shows
    /// (ADR 0047).
    func readPluginsAgain() {
        pluginRead?.cancel()
        pluginRead = nil
        readPlugins()
        settlePlugins(reads: Self.pluginSettleReads)
    }

    /// Reads the plugins (ADR 0047). With no core there are none to show,
    /// and the pane says they could not be read (#489 48b adversarial m4).
    /// A read that succeeds sets ``plugins`` and ``pluginsUnreadable`` only
    /// when they change: the reads while a plugin settles redraw the pane
    /// only when it does (#489 48b m2; Swift 6.1's `@Observable` notifies
    /// on equal values).
    private func readPlugins() {
        guard let source else {
            if !plugins.isEmpty { plugins = [] }
            if !pluginsUnreadable { pluginsUnreadable = true }
            return
        }
        do {
            let read = try source.plugins()
            if read != plugins { plugins = read }
            if pluginsUnreadable { pluginsUnreadable = false }
        } catch {
            if !pluginsUnreadable { pluginsUnreadable = true }
            Self.log.error("plugins not read: \(String(describing: error), privacy: .public)")
        }
    }

    /// Reads the plugins again after ``pluginSettleInterval`` while one of
    /// them settles, `reads` times at most; each configuration applied
    /// starts it again from ``reload()``.
    private func settlePlugins(reads: Int) {
        guard reads > 0, plugins.contains(where: \.isSettling) else { return }
        let timer = clock.schedule(after: Self.pluginSettleInterval) { [weak self] in
            guard let self else { return }
            self.pluginRead = nil
            self.readPlugins()
            self.settlePlugins(reads: reads - 1)
        }
        pluginRead = timer
    }

    /// Whether an edit of `key` is queued, running, or written and not yet
    /// applied.
    func isPending(_ key: String) -> Bool {
        pending[key] != nil
    }

    /// The writer edits go through, while the core runs.
    var writer: (any SettingsConfigWriting)? {
        source?.settingsWriter
    }

    /// The value at `key`: the one last set while it is not yet applied,
    /// else the configuration's. `nil` for a key the configuration leaves
    /// out (or one removed). While a profile added is written and not yet
    /// applied, its keys are the ones sent and the profiles after it (those
    /// of `profiles/`) are one further on, as they will be (#312).
    func value(_ key: String) -> TesseraConfigValue? {
        if let added = addition?.added, let (index, rest) = Self.profilePath(key), index >= added.index,
            rest.hasPrefix(".")
        {
            if index > added.index { return appliedValue("profiles[\(index - 1)]\(rest)") }
            let inner = String(rest.dropFirst())
            return inner == "name" ? .string(added.name) : addition?.keys[inner]
        }
        return appliedValue(key)
    }

    /// The value at `key` set and not yet applied, else the configuration's.
    private func appliedValue(_ key: String) -> TesseraConfigValue? {
        if let pending = pending[key] {
            return pending == .null ? nil : pending
        }
        return snapshot[key]
    }

    /// The message next to the control of `key`, if an edit of it failed.
    func message(_ key: String) -> String? {
        messages[key]
    }

    /// Shows `text` (the pane's own, or sanitised) next to the control of
    /// `key`: an edit the pane refused before writing it.
    func show(_ text: String, for key: String) {
        messages[key] = text
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
    /// when it has passed (with the latest value then). Nothing while a
    /// profile's key is locked (``profilesChanging``).
    func change(_ key: String, _ value: TesseraConfigValue) {
        guard !lockedProfileEdit(key, value) else { return }
        pending[key] = value
        var throttle = throttles[key] ?? Throttle()
        // A step after a hand edit applied is the latest word.
        throttle.overridden = false
        throttle.stepped = true
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

    /// The slider or stepper at `key` was pressed: until it is let go
    /// (``endChange(_:)``), a configuration applied keeps the value it
    /// shows.
    func beginChange(_ key: String) {
        releaseSettles.removeValue(forKey: key)?.cancel()
        var throttle = throttles[key] ?? Throttle()
        throttle.dragging = true
        throttle.stepped = throttle.timer != nil
        throttle.overridden = false
        throttle.seenApplied = snapshot[key]
        throttles[key] = throttle
    }

    /// The slider or stepper at `key` was released: the final value of a
    /// press that made a step is written if it was not yet. If a hand edit
    /// applied during the press and no step came after it, nothing is
    /// written and the reload the press held back runs: the edit shows, and
    /// the next step starts from it (#517). It runs after this event: the
    /// `NSSlider` of an app linked against the macOS 15 SDK sends one more
    /// action on mouse-up, with the knob's value, and SwiftUI takes that
    /// for a new edit if the binding has moved. Otherwise
    /// ``settleAfterRelease(_:)`` follows.
    func endChange(_ key: String) {
        guard let throttle = throttles[key] else { return }
        throttle.timer?.cancel()
        throttles[key]?.timer = nil
        if throttle.stepped, !throttle.overridden, let value = pending[key], value != throttle.lastWritten {
            writeThrottled(key, value)
        }
        throttles[key] = nil
        if throttle.overridden {
            reloadAfterRelease(key, after: .zero)
        } else {
            settleAfterRelease(key)
        }
    }

    /// A configuration applied since the last write of `key`, during the
    /// press, may be the last one: it can leave the key as it was or on an
    /// earlier value of the press (another app saved over the step before
    /// the watch read it), which looks like no hand edit. It can also be
    /// followed by the press's own value, so it does not settle the key at
    /// once: ``profileSettleTimeout`` later, more than the watch takes, the
    /// reload does, and the window shows what is applied (#517).
    private func settleAfterRelease(_ key: String) {
        guard pending[key] != nil else { return }
        if let queued = queuedAtGeneration[key], snapshot.generation <= queued { return }
        reloadAfterRelease(key, after: profileSettleTimeout)
    }

    private func reloadAfterRelease(_ key: String, after delay: Duration) {
        releaseSettles[key] = clock.schedule(after: delay) { [weak self] in
            self?.releaseSettles[key] = nil
            self?.reload()
        }
    }

    private func flushThrottle(_ key: String) {
        throttles[key]?.timer = nil
        guard let value = pending[key], value != throttles[key]?.lastWritten else { return }
        writeThrottled(key, value)
    }

    private func writeThrottled(_ key: String, _ value: TesseraConfigValue) {
        if profilesChanging, lockedProfileEdit(key, value) {
            // Held back since before an add or delete locked the profiles
            // (the index may name another profile once it applies): not
            // written, and the list says so (#344 adversarial M6). The
            // control shows what was written of the drag, if anything
            // waits to apply, else the file's value.
            pending[key] = queuedAtGeneration[key] != nil ? throttles[key]?.lastWritten : nil
            cancelThrottle(key)
            messages[Self.profilesKey] = Self.heldValueNotice
            return
        }
        throttles[key, default: Throttle()].lastWrite = clock.now
        throttles[key, default: Throttle()].lastWritten = value
        if value == snapshot[key], (inFlight[key] ?? 0) == 0, unappliedWrites[key]?.isEmpty != false {
            // Back where the file is: nothing to write. With a write not
            // applied yet the file has moved on from the snapshot (#530).
            pending[key] = nil
            return
        }
        var written = unappliedWrites[key] ?? []
        if written.count == Self.maxUnappliedWrites { written.removeFirst() }
        written.append(value)
        unappliedWrites[key] = written
        enqueue(key, value)
    }

    private func cancelThrottle(_ key: String) {
        throttles[key]?.timer?.cancel()
        throttles[key] = nil
    }

    /// Queues one edit: `value` set at `key`, or `key` removed for `nil`.
    private func enqueue(_ key: String, _ value: TesseraConfigValue?) {
        enqueue([Edit(key: key, value: value)])
    }

    /// One set (`value`) or removal (`nil`) of one key path.
    private struct Edit: Sendable {
        var key: String
        var value: TesseraConfigValue?
    }

    /// What became of one edit of a run: its result and whether it was
    /// run twice, or `nil` when an earlier edit of the run failed and it
    /// was not run.
    private typealias Outcome = (result: Result<Void, TesseraConfigEditError>, retried: Bool)?

    /// Queues `edits` to run in order, stopping at the first that fails
    /// (the later ones are not written). Nothing is queued while one is a
    /// profile's key and the profiles are locked (#344 m-d).
    private func enqueue(_ edits: [Edit]) {
        guard let source else {
            loadError = "Tessera’s core is not running."
            return
        }
        guard !edits.contains(where: { lockedProfileEdit($0.key, $0.value ?? .null) }) else { return }
        if edits.contains(where: { movesProfiles($0.key, $0.value ?? .null) }) {
            // A new lock: an earlier one's timer must not end it.
            profileSettle?.cancel()
            profileSettle = nil
            released = nil
        }
        for edit in edits {
            let written = edit.value ?? .null
            pending[edit.key] = written
            queuedAtGeneration[edit.key] = snapshot.generation
            movedFrom[edit.key] = movesProfiles(edit.key, written) ? snapshot["profiles"]?.array ?? [] : nil
            inFlight[edit.key, default: 0] += 1
            messages[edit.key] = nil
        }
        let writer = source.settingsWriter
        Self.writeQueue.async { [weak self] in
            var outcomes: [Outcome] = []
            for edit in edits {
                if outcomes.contains(where: { if case .failure? = $0?.result { true } else { false } }) {
                    outcomes.append(nil)
                    continue
                }
                var retried = false
                var result = Self.perform(writer, edit.key, edit.value)
                if case .failure(let error) = result, error.kind == .conflict {
                    // The file changed since the core read it: the core reads
                    // it again on the retry, so the other edit stays.
                    retried = true
                    result = Self.perform(writer, edit.key, edit.value)
                }
                outcomes.append((result, retried))
            }
            let done = outcomes
            Task { @MainActor [weak self] in
                guard let self else { return }
                for (edit, outcome) in zip(edits, done) {
                    self.finish(edit.key, outcome, written: edit.value ?? .null)
                }
            }
        }
    }

    /// Whether an edit of `key` to `value` (`.null` for a removal) is
    /// refused: a key of a profile while an add or delete waits (the
    /// profiles are locked), but for the delete itself, and a key of a
    /// profile past the applied ones, which would add a profile without a
    /// name (a new one goes through ``addProfile()``, #312).
    private func lockedProfileEdit(_ key: String, _ value: TesseraConfigValue) -> Bool {
        guard let (index, _) = Self.profilePath(key) else { return false }
        if profilesChanging { return !movesProfiles(key, value) }
        return index >= (snapshot["profiles"]?.array?.count ?? 0)
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

    /// An edit of `key` (to `written`, `.null` for a removal) has run, or
    /// was not run (`outcome` `nil`: an earlier edit of its run failed).
    private func finish(_ key: String, _ outcome: Outcome, written: TesseraConfigValue? = nil) {
        inFlight[key, default: 1] -= 1
        if inFlight[key] == 0 { inFlight[key] = nil }
        guard let (result, retried) = outcome else {
            // Not written; the failed edit's message says why.
            if inFlight[key] == nil {
                pending[key] = nil
                queuedAtGeneration[key] = nil
                movedFrom[key] = nil
                cancelThrottle(key)
            }
            return
        }
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
                movedFrom[key] = nil
                cancelThrottle(key)
            }
        }
        if retried {
            let base = movedFrom[key]
            reload()
            if case .success = result, inFlight[key] == nil, let written,
                !deleteShown(key, written, from: base)
            {
                // The external edit was applied before this one: show this
                // one until a configuration after it applies. A delete the
                // reload shows applied already is not waited for again
                // (21.6a m6): its list would not change again.
                pending[key] = written
                queuedAtGeneration[key] = snapshot.generation
                movedFrom[key] = movesProfiles(key, written) ? snapshot["profiles"]?.array ?? [] : nil
            }
        }
        if case .success = result, let written, movesProfiles(key, written), inFlight[key] == nil,
            pending[key] != nil
        {
            settleProfiles()
        }
    }

    /// Whether the applied profiles are `base` (the ones a delete of
    /// `key` was queued against) without the deleted one, this window's
    /// pending keys left out of both: the delete is applied.
    private func deleteShown(_ key: String, _ written: TesseraConfigValue, from base: [TesseraConfigValue]?) -> Bool {
        guard let base, movesProfiles(key, written), let (index, _) = Self.profilePath(key), base.indices.contains(index)
        else { return false }
        var after = base
        after.remove(at: index)
        return Self.entries(after, masking: pending) == Self.entries(snapshot["profiles"]?.array ?? [], masking: pending)
    }

    /// Starts ``profileSettleTimeout`` for the add or delete just written.
    private func settleProfiles() {
        profileSettle?.cancel()
        profileSettle = clock.schedule(after: profileSettleTimeout) { [weak self] in
            self?.releaseProfiles()
        }
    }

    /// ``profileSettleTimeout`` after an add or delete was written: the
    /// applied configuration is read again, and an add or delete it does
    /// not show either (another app rewrote config.toml without it, or the
    /// watch missed it) no longer locks the list, with a notice by it.
    private func releaseProfiles() {
        profileSettle = nil
        reload()
        var notice: String?
        var change: ReleasedChange?
        if let addition, let added = addition.added, (inFlight[Self.profilesKey] ?? 0) == 0 {
            self.addition = nil
            change = .add(added)
            // The rows after it are back where they were.
            shiftProfileMessages(from: added.index, by: -1)
            notice =
                "“\(MainMenu.profileTitle(added.name))” was written but is not in the configuration applied: "
                + "another app may have saved config.toml without it."
        }
        for (key, value) in pending where movesProfiles(key, value) && (inFlight[key] ?? 0) == 0 {
            change = movedFrom[key].map { .delete(key: key, from: $0) }
            pending[key] = nil
            queuedAtGeneration[key] = nil
            movedFrom[key] = nil
            notice =
                "The profile deleted is still in the configuration applied: another app may have saved "
                + "config.toml with it."
        }
        guard let notice else { return }
        Self.log.info("profile add or delete not shown by the applied configuration in time")
        messages[Self.profilesKey] = notice
        released = change.map { ($0, notice) }
        // The list is not the one the controls were made for.
        profileListGeneration += 1
        draft = nil
    }

    /// Moves the messages of the profiles at `start` and after by `offset`
    /// (an add put a profile at `start`: +1; it went again: -1, dropping
    /// those at `start`), so each stays with its profile (#312 adversarial
    /// M3).
    private func shiftProfileMessages(from start: Int, by offset: Int) {
        var moved: [String: String] = [:]
        for (key, text) in messages {
            guard let (index, rest) = Self.profilePath(key), index >= start else {
                moved[key] = text
                continue
            }
            let target = index + offset
            if target >= start { moved["profiles[\(target)]\(rest)"] = text }
        }
        messages = moved
    }

    /// What the user sees for `error`: the core's message as lines
    /// (``messageLines(_:)``), sanitised (S3).
    static func displayMessage(_ error: TesseraConfigEditError) -> String {
        let text = messageLines(error.message)
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

    /// `text` from the core as lines: the core writes a line break of the
    /// text it quotes (a TOML syntax error's excerpt of the file) as the
    /// two characters `\n` (S8), which start a new line here. Each line is
    /// ``sanitisedMessage(_:)``, empty ones are dropped, and at most
    /// ``maxMessageLines`` lines and ``maxMessageLength`` characters show.
    static func messageLines(_ text: String) -> String {
        var lines = text.components(separatedBy: "\\n").map(sanitisedMessage).filter { !$0.isEmpty }
        if lines.count > maxMessageLines {
            lines = Array(lines.prefix(maxMessageLines - 1)) + ["…"]
        }
        let joined = lines.joined(separator: "\n")
        return joined.count > maxMessageLength ? String(joined.prefix(maxMessageLength - 1)) + "…" : joined
    }

    // MARK: Drafts

    /// `owner`'s field has text typed and not committed; `commit` commits
    /// it. Replaces any other field's.
    func holdDraft(_ owner: AnyObject, commit: @escaping @MainActor () -> Void) {
        draft = (ObjectIdentifier(owner), commit)
    }

    /// `owner`'s field has no uncommitted text (any more); whether it held
    /// some until now (else it was committed already, or went with a hand
    /// edit of the profiles).
    @discardableResult
    func releaseDraft(_ owner: AnyObject) -> Bool {
        guard draft?.owner == ObjectIdentifier(owner) else { return false }
        draft = nil
        return true
    }

    /// Commits the text typed into a field and not yet committed, if any:
    /// before what moves the selection away from the field. A profile name
    /// refused by it shows by the list too, since its own message goes
    /// from view with the selection (#344 adversarial M5); the notice is
    /// returned.
    @discardableResult
    func commitDraft() -> String? {
        guard let draft else { return nil }
        self.draft = nil
        nameRefusal = nil
        draft.commit()
        guard let refusal = nameRefusal else { return nil }
        nameRefusal = nil
        let notice = "The name typed was not saved: \(refusal)"
        messages[Self.profilesKey] = notice
        return notice
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
                self.finish(key, (outcome.map { _ in () }, false))
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
        /// The file of `profiles/` that defines it (`profiles/<stem>.toml`,
        /// the core's `source`, sanitised as a message is, S3), or `nil`
        /// for a `[[profiles]]` entry of config.toml. The window edits
        /// none of a file's keys; the core refuses them (#312 m4).
        var file: String?

        var id: Int { index }
        /// The key path of `key` in this profile.
        func key(_ key: String) -> String { "profiles[\(index)].\(key)" }
    }

    /// The profiles in the order the configuration lists them: the
    /// `[[profiles]]` entries, then those of `profiles/`. A profile added
    /// and not yet applied shows where the core put it, the ones of
    /// `profiles/` after it.
    var profiles: [Profile] {
        var names = snapshot["profiles"]?.array ?? []
        if let added = addition?.added {
            names.insert(.table(["name": .string(added.name)]), at: min(max(added.index, 0), names.count))
        }
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
            let source = profile["source"]?.string
            let file = source.flatMap { $0 == "config" ? nil : SettingsModel.sanitisedMessage($0) }
            return Profile(index: index, name: name, title: title, file: file)
        }
    }

    /// Whether an add, duplicate or delete of a profile is queued, running
    /// or written and not yet applied. Edits name a profile by its index,
    /// which moves once the watch applies the file, so until then the list,
    /// its buttons and the profile's controls are disabled and the profile
    /// actions do nothing. A failed write ends it at once (its message by
    /// the list); a written one ends with the next configuration applied.
    var profilesChanging: Bool {
        addition != nil || pending.contains { movesProfiles($0.key, $0.value) }
    }

    /// Whether `value` (`.null` for a removal) at `key` deletes a profile:
    /// `profiles[i]` removed. (An add is ``addition``.)
    private func movesProfiles(_ key: String, _ value: TesseraConfigValue) -> Bool {
        guard let (_, rest) = Self.profilePath(key) else { return false }
        return rest.isEmpty && value == .null
    }

    /// `key`'s profile index and the rest of it (`.command`, or empty for
    /// `profiles[i]` itself), for a key path in a profile.
    private static func profilePath(_ key: String) -> (index: Int, rest: Substring)? {
        guard key.hasPrefix("profiles["), let close = key.firstIndex(of: "]"),
            let index = Int(key[key.index(key.startIndex, offsetBy: 9)..<close])
        else { return nil }
        return (index, key[key.index(after: close)...])
    }

    /// The applied profiles in file order, each as its values by key path
    /// (`appearance.theme`), but for the keys this window set and has not
    /// yet seen applied (so its own edits, written or still moving, change
    /// nothing here): two profiles named alike, or not named, still differ.
    private var profileEntries: [[String: TesseraConfigValue]] {
        Self.entries(snapshot["profiles"]?.array ?? [], masking: pending)
    }

    /// `profiles` each as its values by key path, but for the keys
    /// `pending` names.
    private static func entries(
        _ profiles: [TesseraConfigValue], masking pending: [String: TesseraConfigValue]
    ) -> [[String: TesseraConfigValue]] {
        profiles.enumerated().map { index, profile in
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

    /// The profiles' names as shown: the ones set and not yet applied
    /// included.
    private var shownProfileNames: [String] {
        profiles.map { value($0.key("name"))?.string ?? $0.name }
    }

    /// Most profiles the core reads, the `[[profiles]]` entries and those
    /// of `profiles/` together: its `MAX_PROFILES`, which the FFI does not
    /// export (the app mirrors it; a test on the core pins it). The
    /// configuration lists at most this many, so at this count the core
    /// refuses an add, which would push one out.
    static let maxProfiles = 256

    /// Whether Add and Duplicate can add a profile: none is being added
    /// or deleted, and fewer than ``maxProfiles`` are listed (21.6a m4).
    var canAddProfile: Bool {
        !profilesChanging && profiles.count < Self.maxProfiles
    }

    /// Why the profile list and the profile's controls are disabled, or
    /// Add and Duplicate are, while they are (VoiceOver reads it; the
    /// buttons' help shows it too, #344 m-f, 21.6a m4).
    var profilesStatus: String? {
        if profilesChanging { return "Waiting for config.toml to be applied; the profiles can be edited once it is." }
        if profiles.count >= Self.maxProfiles {
            return "Tessera reads at most \(Self.maxProfiles) profiles, those in profiles/ included; delete one to add another."
        }
        return nil
    }

    /// The Profiles pane selects the profile ``selectionRequest`` names,
    /// once; the request goes.
    func takeSelectionRequest() -> Int? {
        defer { selectionRequest = nil }
        return selectionRequest
    }

    /// Adds a profile named "New Profile" (numbered by the core if taken)
    /// after the `[[profiles]]` entries, before the profiles of
    /// `profiles/`; ``selectionRequest`` names it once it is written.
    /// `false` unless ``canAddProfile``. Text typed into a field is
    /// committed first.
    @discardableResult
    func addProfile() -> Bool {
        let notice = commitDraft()
        guard canAddProfile else { return false }
        enqueueAddition(named: "New Profile", keys: [])
        if let notice { messages[Self.profilesKey] = notice }
        return true
    }

    /// The keys of a profile the window edits, besides `name`.
    static let profileKeys = ["command", "working_directory"] + SettingsField.profileAppearanceKeys

    /// Adds a copy of the profile at `index` (an entry or a profile of
    /// `profiles/`) as Add does, named after it with " Copy" and with its
    /// ``profileKeys``; ``selectionRequest`` names the copy once it is
    /// written. `false` unless ``canAddProfile``. Text typed into a field
    /// is committed first, so the copy has it. The name and the keys are
    /// one edit of the core's, all or nothing: a name it refuses (" Copy"
    /// makes it longer than 128 bytes) or a value the loader warns about
    /// writes nothing, with the core's message by the list (#455, #312).
    @discardableResult
    func duplicateProfile(_ index: Int) -> Bool {
        let notice = commitDraft()
        let all = profiles
        guard canAddProfile, all.indices.contains(index) else { return false }
        let name = value(all[index].key("name"))?.string ?? all[index].name
        let keys = Self.profileKeys.compactMap { key in value(all[index].key(key)).map { (key: key, value: $0) } }
        enqueueAddition(named: name + " Copy", keys: keys)
        if let notice { messages[Self.profilesKey] = notice }
        return true
    }

    /// Queues one `tessera_config_add_profile` of a profile named after
    /// `base` with `keys`: the profiles are locked from now until the
    /// listed profiles show it, or it fails. A conflict (the file changed
    /// on disk) is retried once, as an edit is.
    private func enqueueAddition(named base: String, keys: [(key: String, value: TesseraConfigValue)]) {
        guard let source else {
            loadError = "Tessera’s core is not running."
            return
        }
        let key = Self.profilesKey
        messages[key] = nil
        inFlight[key, default: 0] += 1
        // A new lock: an earlier one's timer must not end it.
        profileSettle?.cancel()
        profileSettle = nil
        released = nil
        addition = Addition(
            listed: snapshot["profiles"]?.array ?? [],
            keys: Dictionary(keys, uniquingKeysWith: { $1 }), masks: pending, added: nil)
        let writer = source.settingsWriter
        Self.writeQueue.async { [weak self] in
            var retried = false
            var result = Self.performAddition(writer, base, keys)
            if case .failure(let error) = result, error.kind == .conflict {
                retried = true
                result = Self.performAddition(writer, base, keys)
            }
            let (outcome, again) = (result, retried)
            Task { @MainActor [weak self] in
                guard let self else { return }
                self.finishAddition(outcome, retried: again)
            }
        }
    }

    private nonisolated static func performAddition(
        _ writer: any SettingsConfigWriting, _ base: String, _ keys: [(key: String, value: TesseraConfigValue)]
    ) -> Result<TesseraAddedProfile, TesseraConfigEditError> {
        do throws(TesseraConfigEditError) {
            return .success(try writer.addProfile(named: base, keys: keys))
        } catch {
            return .failure(error)
        }
    }

    /// The add of a profile has run: written, it shows where the core put
    /// it until the watch applies it (or ``profileSettleTimeout`` has
    /// passed), the messages of the profiles after it move with them, and
    /// the pane selects it; refused, the list unlocks at once with the
    /// core's message by it.
    private func finishAddition(_ result: Result<TesseraAddedProfile, TesseraConfigEditError>, retried: Bool) {
        let key = Self.profilesKey
        inFlight[key, default: 1] -= 1
        if inFlight[key] == 0 { inFlight[key] = nil }
        guard var addition else { return }
        switch result {
        case .failure(let error):
            // S8: the message may quote the file; only its kind is logged.
            Self.log.info("profile not added: \(String(describing: error.kind), privacy: .public)")
            messages[key] = Self.displayMessage(error)
            self.addition = nil
            if retried { reload() }
        case .success(let added):
            if retried {
                // The external edit may have been applied before this one
                // (wait for a list after that one), or both were.
                reload()
                addition.listed = snapshot["profiles"]?.array ?? []
            }
            addition.added = added
            self.addition = addition
            shiftProfileMessages(from: added.index, by: 1)
            selectionRequest = added.index
            // The watch may have applied it already (a slow main thread).
            endAdditionIfApplied(masking: pending)
            if self.addition != nil { settleProfiles() }
        }
    }

    /// Renames the profile at `index`; `[general] default_profile` follows
    /// when it named this profile, so new tabs keep starting with it. The
    /// rename is written first and the default only once it was: a refused
    /// rename leaves both as they were. A name ``profileNameRefusal(_:at:)``
    /// refuses writes nothing and shows why by the name.
    func renameProfile(_ index: Int, to name: String) {
        let all = profiles
        guard !profilesChanging, all.indices.contains(index) else { return }
        let key = all[index].key("name")
        let current = value(key)?.string ?? all[index].name
        guard current != name else {
            messages[key] = nil
            return
        }
        if let refusal = profileNameRefusal(name, at: index) {
            messages[key] = refusal
            nameRefusal = refusal
            return
        }
        var edits = [Edit(key: key, value: .string(name))]
        let defaultKey = "general.default_profile"
        if value(defaultKey)?.string == current {
            edits.append(Edit(key: defaultKey, value: .string(name)))
            cancelThrottle(defaultKey)
        }
        cancelThrottle(key)
        enqueue(edits)
    }

    /// Why `name` cannot name the profile at `index`, if it cannot: a
    /// control character or line break, nothing visible, or the name of
    /// another profile, byte for byte as the core compares names (an NFD
    /// "café" is not the NFC one, 21.6a adversarial M4).
    func profileNameRefusal(_ name: String, at index: Int) -> String? {
        let breaks = name.unicodeScalars.contains {
            switch $0.properties.generalCategory {
            case .control, .lineSeparator, .paragraphSeparator: true
            default: false
            }
        }
        if breaks { return "A profile name cannot contain control characters or line breaks." }
        if Self.sanitisedMessage(name).isEmpty { return "A profile needs a name." }
        let others = shownProfileNames.enumerated().filter { $0.offset != index }.map(\.element)
        if others.contains(where: { $0.utf8.elementsEqual(name.utf8) }) {
            return "A profile named “\(MainMenu.profileTitle(name))” exists."
        }
        return nil
    }

    /// Deletes the profile at `index`, except the default profile (new tabs
    /// would lose it) and a profile of `profiles/` (the core edits only
    /// config.toml): `false` and a message by the list for those, and
    /// `false` while ``profilesChanging``.
    @discardableResult
    func deleteProfile(_ index: Int) -> Bool {
        let all = profiles
        guard !profilesChanging, all.indices.contains(index) else { return false }
        if let file = all[index].file {
            messages[Self.profilesKey] = "“\(all[index].title)” is defined in \(file); delete that file to remove it."
            return false
        }
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
