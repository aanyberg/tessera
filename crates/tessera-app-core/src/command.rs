//! The command registry and the key map (#43, ADR 0036).
//!
//! [`COMMANDS`] lists every application command a key can be bound to,
//! with its default chords: the shortcuts the menus have always shown, so
//! a user without a `[keys]` table sees no change. [`Keymap::compile`]
//! lays the `[keys]` entries of a [`Config`](tessera_config::Config) over
//! those defaults; [`Keymap::resolve`] finds the binding of a key event
//! without allocating, and [`Keymap::chords`] gives a command's chords,
//! which the frontend shows as menu key equivalents
//! ([`menu_shortcut`]).
//!
//! The chord and value notation, and the warnings for entries that break
//! it, are `tessera_config`'s ([`tessera_config::Chord`]); the warnings
//! for entries that name no command, or that shadow a menu item or a
//! macOS chord, are this module's ([`Keymap::warnings`]).

use std::fmt;
use std::sync::LazyLock;

use tessera_config::{
    Chord, ChordKey, ChordModifiers, KeyAction, KeyEntry, Keys, MAX_WARNING_PATH_LEN, Warning,
    escaped, escaped_value,
};
use tessera_term::{Key, KeyEvent, Modifiers};

/// Who provides a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CommandOwner {
    /// The frontend's own menu item, which the user expects macOS
    /// semantics from (Quit, Hide, Minimize); taking its chord warns.
    System,
    /// A Tessera command with a menu item today.
    App,
    /// A command a later feature implements (prompt navigation, copy
    /// mode). It may be bound now; until the feature
    /// lands, nothing runs it.
    Planned,
}

/// One command of the registry.
#[derive(Debug)]
pub struct CommandSpec {
    /// The name written in `[keys]` values (`new_tab`).
    pub name: &'static str,
    /// The parameter, for a command written with one (`select_tab:3` is
    /// `select_tab` with `"3"`): each parameter value is its own entry.
    pub parameter: Option<&'static str>,
    /// The default chords, canonical, the first being the one a menu shows
    /// (the others are hidden aliases).
    pub defaults: &'static [&'static str],
    /// Who provides it.
    pub owner: CommandOwner,
}

macro_rules! commands {
    ($( $name:literal $(: $param:literal)? => [$($chord:literal),*] $owner:ident ),* $(,)?) => {
        /// Every command a key can be bound to, in menu order. The index
        /// of an entry is its [`CommandId`].
        pub static COMMANDS: &[CommandSpec] = &[
            $(CommandSpec {
                name: $name,
                parameter: commands!(@param $($param)?),
                defaults: &[$($chord),*],
                owner: CommandOwner::$owner,
            }),*
        ];
    };
    (@param) => { None };
    (@param $param:literal) => { Some($param) };
}

commands! {
    "about" => [] System,
    "settings" => ["cmd+,"] App,
    "hide" => ["cmd+h"] System,
    "hide_others" => ["cmd+alt+h"] System,
    "show_all" => [] System,
    "quit" => ["cmd+q"] System,
    // Quit's ⌥ alternate: Quit and Keep Windows, or Quit and Close All
    // Windows when macOS keeps windows (ADR 0031, #464).
    "quit_keep_windows" => ["cmd+alt+q"] System,
    "new_window" => ["cmd+n"] App,
    "new_tab" => ["cmd+t"] App,
    "close_tab" => ["cmd+w"] App,
    // Close Tab's ⌥ alternate, Close All: every window, one question (#464).
    "close_all_windows" => ["cmd+alt+w"] App,
    "close_window" => ["cmd+shift+w"] App,
    "clear_scrollback" => ["cmd+k"] App,
    "copy" => ["cmd+c"] App,
    "copy_with_formatting" => [] App,
    "paste" => ["cmd+v"] App,
    "paste_plain" => ["cmd+alt+shift+v"] App,
    "select_all" => ["cmd+a"] App,
    "font_bigger" => ["cmd+shift+=", "cmd+="] App,
    "font_smaller" => ["cmd+-"] App,
    "font_reset" => ["cmd+0"] App,
    "scroll_to_top" => ["cmd+home"] App,
    "scroll_to_bottom" => ["cmd+end"] App,
    // ⌘↑ / ⌘↓ (D4, N1): Previous / Next Prompt when the scrollback has a
    // prompt mark, else these; the prompt rule applies to these default
    // chords only, never to a user's binding.
    "scroll_page_up" => ["cmd+up"] App,
    "scroll_page_down" => ["cmd+down"] App,
    "show_tab_bar" => [] System,
    "minimize" => ["cmd+m"] System,
    "zoom" => [] System,
    "tab_previous" => ["cmd+shift+["] App,
    "tab_next" => ["cmd+shift+]"] App,
    "move_tab_to_new_window" => [] System,
    "merge_all_windows" => [] System,
    "select_tab": "1" => ["cmd+1"] App,
    "select_tab": "2" => ["cmd+2"] App,
    "select_tab": "3" => ["cmd+3"] App,
    "select_tab": "4" => ["cmd+4"] App,
    "select_tab": "5" => ["cmd+5"] App,
    "select_tab": "6" => ["cmd+6"] App,
    "select_tab": "7" => ["cmd+7"] App,
    "select_tab": "8" => ["cmd+8"] App,
    "select_tab": "9" => ["cmd+9"] App,
    "bring_all_to_front" => [] System,
    "help" => ["cmd+shift+/"] App,
    "split_right" => ["cmd+d"] App,
    "split_down" => ["cmd+shift+d"] App,
    "focus_pane_left" => ["cmd+alt+left"] App,
    "focus_pane_right" => ["cmd+alt+right"] App,
    "focus_pane_up" => ["cmd+alt+up"] App,
    "focus_pane_down" => ["cmd+alt+down"] App,
    "focus_pane_previous" => ["cmd+["] App,
    "focus_pane_next" => ["cmd+]"] App,
    "zoom_pane" => ["cmd+shift+enter"] App,
    "equalize_panes" => [] App,
    "find" => ["cmd+f"] App,
    "find_next" => ["cmd+g"] App,
    "find_previous" => ["cmd+shift+g"] App,
    "use_selection_for_find" => ["cmd+e"] App,
    "copy_find_matches" => [] App,
    // Default chords: ⌘↑ / ⌘↓ through scroll_page_up / _down (N1, see
    // `Keymap::prompt_rule`).
    "prompt_previous" => [] App,
    "prompt_next" => [] App,
    "select_last_output" => [] App,
    "copy_mode" => ["cmd+shift+c"] Planned,
}

/// Chords macOS itself puts on menu items (Enter Full Screen, Emoji &
/// Symbols, Show All Tabs) or uses for window tabs: binding one warns,
/// since the two compete for the key (N10).
const RESERVED: [(&str, &str); 5] = [
    ("cmd+ctrl+f", "Enter Full Screen"),
    ("cmd+ctrl+space", "Emoji & Symbols"),
    ("ctrl+tab", "Show Next Tab"),
    ("ctrl+shift+tab", "Show Previous Tab"),
    ("cmd+shift+\\", "Show All Tabs"),
];

/// A command of [`COMMANDS`], by index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CommandId(u16);

/// Why a `[keys]` value names no command.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CommandError {
    /// No command has this name.
    #[error("unknown command `{0}`")]
    Unknown(String),
    /// The command is written without a parameter.
    #[error("command `{0}` takes no parameter")]
    TakesNoParameter(&'static str),
    /// The command needs a parameter (`select_tab:1`).
    #[error("command `{0}` needs a parameter")]
    MissingParameter(&'static str),
    /// A parameter the command does not have.
    #[error("parameter of `{name}` out of range {first}..={last}")]
    ParameterOutOfRange {
        /// The command.
        name: &'static str,
        /// Its first parameter.
        first: &'static str,
        /// Its last parameter.
        last: &'static str,
    },
}

impl CommandId {
    /// The command at `index` of [`COMMANDS`].
    #[must_use]
    pub fn from_index(index: usize) -> Option<Self> {
        (index < COMMANDS.len()).then(|| Self::at(index))
    }

    /// The id of an index known to be in range.
    fn at(index: usize) -> Self {
        Self(u16::try_from(index).unwrap_or(u16::MAX))
    }

    /// Its index in [`COMMANDS`].
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }

    /// Its registry entry.
    #[must_use]
    pub fn spec(self) -> &'static CommandSpec {
        &COMMANDS[self.index()]
    }

    /// The command `name` with `parameter` (`select_tab`, `Some("3")`).
    pub fn find(name: &str, parameter: Option<&str>) -> Result<Self, CommandError> {
        let mut entries = COMMANDS
            .iter()
            .enumerate()
            .filter(|(_, spec)| spec.name == name)
            .peekable();
        let Some(&(_, first)) = entries.peek() else {
            return Err(CommandError::Unknown(escaped(name, MAX_WARNING_PATH_LEN)));
        };
        match (first.parameter, parameter) {
            (None, Some(_)) => return Err(CommandError::TakesNoParameter(first.name)),
            (Some(_), None) => return Err(CommandError::MissingParameter(first.name)),
            _ => {}
        }
        let mut last = first;
        for (index, spec) in entries {
            if spec.parameter == parameter {
                return Ok(Self::at(index));
            }
            last = spec;
        }
        Err(CommandError::ParameterOutOfRange {
            name: first.name,
            first: first.parameter.unwrap_or_default(),
            last: last.parameter.unwrap_or_default(),
        })
    }

    /// Every command, in registry order.
    pub fn all() -> impl Iterator<Item = Self> {
        (0..COMMANDS.len()).map(Self::at)
    }
}

/// Prints the command as written in `[keys]`: `new_tab`, `select_tab:3`.
impl fmt::Display for CommandId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let spec = self.spec();
        match spec.parameter {
            Some(parameter) => write!(f, "{}:{parameter}", spec.name),
            None => f.write_str(spec.name),
        }
    }
}

/// What a key is bound to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Binding<'a> {
    /// A command; the frontend runs it.
    Command(CommandId),
    /// `send_text`: these bytes go to the PTY as they are.
    SendText(&'a [u8]),
    /// `none`: the key is swallowed.
    Ignore,
}

/// Whether a `[keys]` entry took effect ([`KeymapEntry::effect`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum KeyEffect {
    /// It binds its chord: a command, `send_text` or `none`.
    Bound,
    /// `unbound`: it took its chord from every command, and the key types.
    Unbound,
    /// Left out because an earlier entry for the same chord took effect
    /// (the first entry wins; an `unbound` entry is left out the same way).
    Shadowed,
    /// Left out for itself: a chord or value that does not parse, an
    /// unknown command or parameter, an invisible character.
    Refused,
}

impl KeyEffect {
    /// The name the key map JSON uses: `bound`, `unbound`, `shadowed` or
    /// `refused`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bound => "bound",
            Self::Unbound => "unbound",
            Self::Shadowed => "shadowed",
            Self::Refused => "refused",
        }
    }
}

/// The value of a [`KeymapEntry`] as written, escaped and capped at
/// [`tessera_config::MAX_ENTRY_VALUE_TEXT_LEN`] bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EntryValue {
    /// A command (`select_tab:3`), `unbound`, `none`, or a value that does
    /// not parse, as written.
    Text(String),
    /// The text after `send_text:`, as written (its escapes not decoded).
    SendText(String),
    /// The value is not a string.
    NotAString,
}

impl EntryValue {
    /// `text`, already escaped and capped, split at a `send_text:` prefix.
    fn of(text: String) -> Self {
        match text.strip_prefix("send_text:") {
            Some(rest) => Self::SendText(rest.to_owned()),
            None => Self::Text(text),
        }
    }
}

/// One of the first [`tessera_config::MAX_KEY_BINDINGS`] `[keys]` entries,
/// in document order, and what became of it ([`Keymap::entries`]): what a
/// settings view lists. Every text is escaped as warnings are (S3) and
/// capped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeymapEntry<'a> {
    /// Its place in the table, from 0, refused entries counted.
    pub index: usize,
    /// The table key as written.
    pub key: String,
    /// The chord, canonical when printed; `None` when the key does not
    /// parse.
    pub chord: Option<Chord>,
    /// The value as written.
    pub value: EntryValue,
    /// Whether it took effect.
    pub effect: KeyEffect,
    /// The entry's warning: why it was left out, or the caveat it is
    /// honoured with.
    pub warning: Option<&'a Warning>,
}

/// A chord's binding in the compiled table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    Command(CommandId),
    /// Index into [`Keymap::texts`].
    Text(usize),
    Ignore,
}

/// The lookup key of a chord: its modifier bits and its key.
type Probe = (u8, ChordKey);

/// The words of [`Keymap::ascii_filter`]: 128 bits for each of the 16
/// modifier sets.
pub(crate) const ASCII_FILTER_WORDS: usize = 32;

/// The registry's default chords, parsed once: entry `i` holds the
/// [`CommandSpec::defaults`] of command `i`, in order. Every registry chord
/// parses (a unit test checks); one that did not would be left out.
fn registry_defaults() -> &'static [Vec<Chord>] {
    static DEFAULTS: LazyLock<Vec<Vec<Chord>>> = LazyLock::new(|| {
        COMMANDS
            .iter()
            .map(|spec| {
                spec.defaults
                    .iter()
                    .filter_map(|text| Chord::parse(text).ok())
                    .collect()
            })
            .collect()
    });
    &DEFAULTS
}

/// The defaults and a `[keys]` table, compiled for lookup.
///
/// [`Keymap::default`] is the map of an empty table: the registry's
/// default chords alone.
#[derive(Debug, Clone)]
pub struct Keymap {
    /// The entries compiled, to tell whether a new configuration changes
    /// them.
    keys: Vec<tessera_config::KeyBinding>,
    /// The table's [`Keys::entries`], for [`Keymap::entries`] and to tell
    /// whether a new configuration changes them.
    sources: Vec<KeyEntry>,
    /// The table's [`Keys::overflow`].
    overflow: Option<Warning>,
    /// For each of `keys`: whether it took effect, and the index in
    /// `warnings` of its warning.
    outcomes: Vec<(KeyEffect, Option<usize>)>,
    /// Bit `m` is set when some chord in `table` has modifier bits `m`.
    mask: u16,
    /// Bit `c % 64` of word `2 * m + c / 64` is set when some chord in
    /// `table` has modifier bits `m` and the ASCII character key `c`.
    ascii: [u64; ASCII_FILTER_WORDS],
    /// The chords of each command, by [`CommandId::index`]: the user's in
    /// document order, then the defaults no entry took.
    chords: Vec<Vec<Chord>>,
    /// Sorted by probe; a chord with a US shifted symbol also appears as
    /// the symbol without Shift (`cmd+shift+/` as `cmd+?`), so a layout
    /// that types the symbol on its own key matches it too.
    table: Vec<(Probe, Slot)>,
    texts: Vec<Box<[u8]>>,
    warnings: Vec<Warning>,
    /// The registry default chords no `[keys]` entry names, sorted: what
    /// [`Keymap::is_default`] looks up.
    defaults: Vec<Chord>,
}

/// The registry defaults alone, as compiled from an empty `[keys]` table.
impl Default for Keymap {
    fn default() -> Self {
        Self::compile(&Keys::default())
    }
}

impl Keymap {
    /// Lays `keys` over the registry defaults. An entry naming an unknown
    /// command (or a wrong parameter) is left out with a warning and
    /// claims no chord; of the other entries with the same chord (an
    /// `unbound` one included) the first wins and each later one is left
    /// out with a warning. An entry is honoured with a warning when it
    /// shadows plain typing ([`Chord::shadows_typing`]; never an entry left
    /// out, #430 m4), takes a chord macOS uses
    /// (⌃⌘F, ⌃Tab and the like), or takes the default chord of a system command, or
    /// replaces a menu command's default chord with text or `none`. An
    /// entry whose chord is a default of another command takes it from
    /// that command; `unbound` takes it from every command.
    ///
    /// Each warning is logged (`tracing::warn!`); [`Keymap::compile_quietly`]
    /// logs none.
    #[must_use]
    pub fn compile(keys: &Keys) -> Self {
        let map = Self::compile_quietly(keys);
        for warning in &map.warnings {
            tracing::warn!(target: "tessera_config", "{warning}");
        }
        map
    }

    /// [`Keymap::compile`] without logging its warnings: for a check that
    /// compares them ([`Keymap::warnings`]) rather than applies the map,
    /// such as a settings edit's, without a scoped tracing subscriber,
    /// which churns tracing's callsite cache on every call (#342 M6).
    #[must_use]
    pub fn compile_quietly(keys: &Keys) -> Self {
        let defaults: Vec<(Chord, CommandId)> = CommandId::all()
            .flat_map(|id| {
                registry_defaults()[id.index()]
                    .iter()
                    .map(move |&chord| (chord, id))
            })
            .collect();
        let mut map = Self {
            keys: keys.bindings().to_vec(),
            sources: keys.entries().to_vec(),
            overflow: keys.overflow().cloned(),
            outcomes: Vec::with_capacity(keys.bindings().len()),
            mask: 0,
            ascii: [0; ASCII_FILTER_WORDS],
            chords: vec![Vec::new(); COMMANDS.len()],
            table: Vec::new(),
            texts: Vec::new(),
            warnings: Vec::new(),
            defaults: Vec::new(),
        };
        // The chord of each entry kept so far, with its key as written:
        // the first kept entry of a chord wins it.
        let mut taken: Vec<(Chord, &str)> = Vec::new();
        for binding in keys.bindings() {
            let first_warning = map.warnings.len();
            let effect = map.lay(binding, &defaults, &mut taken);
            let warning = (map.warnings.len() > first_warning).then_some(first_warning);
            map.outcomes.push((effect, warning));
        }
        for (chord, id) in defaults {
            if !taken.iter().any(|&(taken, _)| taken == chord) {
                map.chords[id.index()].push(chord);
                map.insert(chord, Slot::Command(id));
            }
            if !map.keys.iter().any(|binding| binding.chord == chord) {
                map.defaults.push(chord);
            }
        }
        map.defaults.sort_unstable();
        map.table.sort_by_key(|&(probe, _)| probe);
        map
    }

    /// Lays one entry over the map: claims its chord when it is the first
    /// kept entry for it, and pushes its warnings (the first is the one
    /// [`Keymap::entries`] reports).
    fn lay<'k>(
        &mut self,
        binding: &'k tessera_config::KeyBinding,
        defaults: &[(Chord, CommandId)],
        taken: &mut Vec<(Chord, &'k str)>,
    ) -> KeyEffect {
        let warn = |reason: String| Warning::KeyBinding {
            key: escaped(&binding.key, MAX_WARNING_PATH_LEN),
            reason,
        };
        let command = match &binding.action {
            KeyAction::Command { name, parameter } => {
                match CommandId::find(name, parameter.as_deref()) {
                    Ok(id) => Some(id),
                    Err(error) => {
                        self.warnings.push(warn(format!("{error}; ignored")));
                        return KeyEffect::Refused;
                    }
                }
            }
            _ => None,
        };
        let chord = binding.chord;
        // After the command check: an entry left out claims no chord, so a
        // later valid entry of the same chord is honoured (#317). Before
        // the `unbound` return: an `unbound` entry after a kept one is left
        // out with this warning too, never silently (#430 m6).
        if let Some(&(_, first)) = taken.iter().find(|&&(taken, _)| taken == chord) {
            let first = escaped(first, MAX_WARNING_PATH_LEN);
            self.warnings.push(warn(format!(
                "same chord as `{first}` (the first entry wins); ignored"
            )));
            return KeyEffect::Shadowed;
        }
        taken.push((chord, &binding.key));
        let slot = match (&binding.action, command) {
            (_, Some(id)) => Slot::Command(id),
            (KeyAction::SendText(bytes), None) => {
                self.texts.push(bytes.clone().into_boxed_slice());
                Slot::Text(self.texts.len() - 1)
            }
            (KeyAction::Ignore, None) => Slot::Ignore,
            _ => return KeyEffect::Unbound,
        };
        if chord.shadows_typing() {
            self.warnings.push(warn(
                "shadows plain typing of this key; honoured".to_owned(),
            ));
        }
        if let Some((_, what)) = RESERVED
            .iter()
            .find(|(text, _)| Chord::parse(text).ok() == Some(chord))
        {
            self.warnings
                .push(warn(format!("macOS uses this chord for {what}; honoured")));
        }
        if let Some(&(_, owner)) = defaults.iter().find(|(default, _)| *default == chord)
            && slot != Slot::Command(owner)
            && (!matches!(slot, Slot::Command(_)) || owner.spec().owner == CommandOwner::System)
        {
            self.warnings.push(warn(format!(
                "shadows the menu item of `{owner}`, which loses this chord; honoured"
            )));
        }
        if let Slot::Command(id) = slot {
            self.chords[id.index()].push(chord);
        }
        self.insert(chord, slot);
        KeyEffect::Bound
    }

    /// Adds `chord` (and its shifted-symbol form) to the table.
    fn insert(&mut self, chord: Chord, slot: Slot) {
        self.push((chord.modifiers.bits(), chord.key), slot);
        if let Some(symbol) = chord.shifted_symbol() {
            let modifiers = chord.modifiers.without(ChordModifiers::SHIFT);
            self.push((modifiers.bits(), ChordKey::Char(symbol)), slot);
        }
    }

    fn push(&mut self, probe: Probe, slot: Slot) {
        self.mask |= 1 << probe.0;
        if let ChordKey::Char(c) = probe.1
            && c.is_ascii()
        {
            self.ascii[usize::from(probe.0) * 2 + c as usize / 64] |= 1 << (c as usize % 64);
        }
        self.table.push((probe, slot));
    }

    /// The `[keys]` entries this map was compiled from.
    #[cfg(test)]
    pub(crate) fn keys(&self) -> &[tessera_config::KeyBinding] {
        &self.keys
    }

    /// Whether this map was compiled from a table equal to `keys`: the
    /// same bindings, the same refused entries and the same overflow, so
    /// [`Keymap::entries`] would not change.
    pub(crate) fn compiled_from(&self, keys: &Keys) -> bool {
        self.keys == keys.bindings()
            && self.sources == keys.entries()
            && self.overflow.as_ref() == keys.overflow()
    }

    /// The first [`tessera_config::MAX_KEY_BINDINGS`] `[keys]` entries the
    /// map was compiled from, in document order, refused ones included,
    /// with whether each took effect and its warning; the canonical chord
    /// of a [`KeyEffect::Bound`] command entry is among that command's
    /// [`Keymap::chords`]. The texts are escaped and capped here; nothing
    /// of a refused entry but its escaped form was kept.
    pub fn entries(&self) -> impl Iterator<Item = KeymapEntry<'_>> + '_ {
        self.sources
            .iter()
            .enumerate()
            .map(move |(index, source)| match source {
                KeyEntry::Refused(refused) => KeymapEntry {
                    index,
                    key: refused.key.clone(),
                    chord: refused.chord,
                    value: refused
                        .value
                        .clone()
                        .map_or(EntryValue::NotAString, EntryValue::of),
                    effect: KeyEffect::Refused,
                    warning: Some(&refused.warning),
                },
                &KeyEntry::Binding(at) => {
                    let binding = &self.keys[at];
                    let (effect, warning) = self.outcomes[at];
                    KeymapEntry {
                        index,
                        key: escaped(&binding.key, MAX_WARNING_PATH_LEN),
                        chord: Some(binding.chord),
                        value: EntryValue::of(escaped_value(&binding.value)),
                        effect,
                        warning: warning.map(|at| &self.warnings[at]),
                    }
                }
                // `KeyEntry` is non-exhaustive; a later kind lists as
                // refused, without text.
                _ => KeymapEntry {
                    index,
                    key: String::new(),
                    chord: None,
                    value: EntryValue::NotAString,
                    effect: KeyEffect::Refused,
                    warning: None,
                },
            })
    }

    /// The warning for the `[keys]` entries past
    /// [`tessera_config::MAX_KEY_BINDINGS`], when the table had more.
    #[must_use]
    pub fn overflow(&self) -> Option<&Warning> {
        self.overflow.as_ref()
    }

    /// Bit `m` is set when some chord has modifier bits `m` (Shift 1, Alt
    /// 2, Ctrl 4, Command 8): [`admits`] tells from it whether an event
    /// can have a binding at all.
    pub(crate) fn modifier_mask(&self) -> u16 {
        self.mask
    }

    /// The character keys the chords use, by modifier set: what
    /// [`may_match`] reads, bit `c % 64` of word `2 * m + c / 64` for the
    /// ASCII key `c` with modifier bits `m`.
    pub(crate) fn ascii_filter(&self) -> [u64; ASCII_FILTER_WORDS] {
        self.ascii
    }

    /// The binding of `event`, if any. Lock keys are ignored; an event
    /// with Hyper or Meta matches nothing. Tried in turn: the key with its
    /// modifiers; with Shift held, the shifted character as typed (never
    /// lowercased: ⌘⇧T is not ⌘T) without Shift (so `cmd+?` matches on
    /// any layout that types `?`); when the key is a character that is
    /// not ASCII, the key the base (US) layout has in its place (`cmd+t`
    /// on a Cyrillic layout). A Latin layout's own ASCII key is never
    /// replaced: French ⌘Z is not ⌘W, German ⌃Z is not `ctrl+y`. This is
    /// tessera-term's rule for Ctrl and Alt on non-Latin layouts.
    /// Allocates nothing.
    #[must_use]
    pub fn resolve(&self, event: &KeyEvent) -> Option<Binding<'_>> {
        if !admits(self.mask, event.modifiers) {
            return None;
        }
        let modifiers = event.modifiers.without_locks();
        let bits = modifiers.bits();
        let key = chord_key(event.key);
        if let Some(key) = key
            && let Some(slot) = self.find((bits, key))
        {
            return Some(self.binding(slot));
        }
        if modifiers.contains(Modifiers::SHIFT)
            && let Some(shifted) = event.shifted_key
            && let Some(slot) =
                self.find((bits & !Modifiers::SHIFT.bits(), ChordKey::Char(shifted)))
        {
            return Some(self.binding(slot));
        }
        if let Key::Char(own) = event.key
            && !own.is_ascii()
            && let Some(base) = event.base_layout_key
            && key != Some(char_key(base))
            && let Some(slot) = self.find((bits, char_key(base)))
        {
            return Some(self.binding(slot));
        }
        None
    }

    fn find(&self, probe: Probe) -> Option<Slot> {
        self.table
            .binary_search_by_key(&probe, |&(probe, _)| probe)
            .ok()
            .map(|index| self.table[index].1)
    }

    fn binding(&self, slot: Slot) -> Binding<'_> {
        match slot {
            Slot::Command(id) => Binding::Command(id),
            Slot::Text(index) => Binding::SendText(&self.texts[index]),
            Slot::Ignore => Binding::Ignore,
        }
    }

    /// The chords bound to `id`: the `[keys]` entries naming it, in
    /// document order, then its defaults that no entry took. A menu item
    /// shows the first one [`menu_shortcut`] accepts. Every map, the
    /// [`Keymap::default`] one included, has an entry for every command.
    #[must_use]
    pub fn chords(&self, id: CommandId) -> &[Chord] {
        self.chords.get(id.index()).map_or(&[], Vec::as_slice)
    }

    /// Whether `chord` is bound by its registry default, not by a `[keys]`
    /// entry (which may name the same command): the ⌘↑ / ⌘↓ prompt rule
    /// (Previous / Next Prompt when the scrollback has a prompt mark)
    /// applies only to a default binding, never to a user's (N1). A binary
    /// search over the defaults compiled with the map; allocates nothing.
    #[must_use]
    pub fn is_default(&self, chord: Chord) -> bool {
        self.defaults.binary_search(&chord).is_ok()
    }

    /// The command a key event bound to `command` runs: `prompt_previous`
    /// for `scroll_page_up` and `prompt_next` for `scroll_page_down` when
    /// the event is that command's registry default chord (⌘↑ / ⌘↓) and no
    /// `[keys]` entry binds it ([`Keymap::is_default`]), else `command`
    /// itself (D4, N1). The prompt commands page when the scrollback holds
    /// no prompt mark, so the default chords page as before in a shell
    /// without integration; a user's own binding of the chord is never
    /// upgraded.
    #[must_use]
    pub(crate) fn prompt_rule(&self, event: &KeyEvent, command: CommandId) -> CommandId {
        let upgrade = match command.spec().name {
            "scroll_page_up" => "prompt_previous",
            "scroll_page_down" => "prompt_next",
            _ => return command,
        };
        let Some(key) = chord_key(event.key) else {
            return command;
        };
        let probe = (event.modifiers.without_locks().bits(), key);
        match registry_defaults()[command.index()].first() {
            Some(&chord)
                if (chord.modifiers.bits(), chord.key) == probe && self.is_default(chord) =>
            {
                CommandId::find(upgrade, None).unwrap_or(command)
            }
            _ => command,
        }
    }

    /// The warnings of the entries the registry left out or honoured with
    /// a caveat, in document order. Logged when compiled.
    #[must_use]
    pub fn warnings(&self) -> &[Warning] {
        &self.warnings
    }
}

/// Whether an event with `modifiers` can match a chord of a map whose
/// [`Keymap::modifier_mask`] is `mask`: its modifiers (locks aside, and
/// with no Hyper or Meta), or with Shift held the same without Shift (a
/// shifted symbol), have a bit in `mask`.
pub(crate) fn admits(mask: u16, modifiers: Modifiers) -> bool {
    let modifiers = modifiers.without_locks();
    if modifiers.intersects(Modifiers::HYPER | Modifiers::META) {
        return false;
    }
    let bits = modifiers.bits();
    mask & (1 << bits) != 0
        || (bits & Modifiers::SHIFT.bits() != 0
            && mask & (1 << (bits & !Modifiers::SHIFT.bits())) != 0)
}

/// Whether [`Keymap::resolve`] can find a binding for `event` in a map
/// whose [`Keymap::modifier_mask`] is `mask` and whose
/// [`Keymap::ascii_filter`] word `i` is `word(i)`; `false` only when it
/// finds none. Every probe `resolve` tries with an ASCII character key is
/// looked up in the filter; any other key passes once the mask admits it.
/// Lets a frontend's unbound Command keys (⌘J, ⌘⇧E) skip the map's lock
/// and its binary search.
pub(crate) fn may_match(event: &KeyEvent, mask: u16, word: impl Fn(usize) -> u64) -> bool {
    if !admits(mask, event.modifiers) {
        return false;
    }
    let Key::Char(c) = event.key else {
        return true;
    };
    // Below 16: `admits` refused Hyper and Meta, and the locks are off.
    let modifiers = event.modifiers.without_locks();
    let bits = modifiers.bits();
    let bit = |bits: u8, key: char| {
        word(usize::from(bits) * 2 + key as usize / 64) & (1 << (key as usize % 64)) != 0
    };
    let has = |bits: u8, c: char| {
        let key = if c.is_ascii() {
            c.to_ascii_lowercase()
        } else {
            match char_key(c) {
                // The Kelvin sign lowercases to `k`.
                ChordKey::Char(key) if key.is_ascii() => key,
                _ => return true,
            }
        };
        bit(bits, key)
    };
    // The shifted character is looked up as typed, as `resolve` does.
    has(bits, c)
        || (modifiers.contains(Modifiers::SHIFT)
            && event.shifted_key.is_some_and(|shifted| {
                !shifted.is_ascii() || bit(bits & !Modifiers::SHIFT.bits(), shifted)
            }))
        || (!c.is_ascii() && event.base_layout_key.is_some_and(|base| has(bits, base)))
}

/// The chord key of an event's key, `None` for keys no chord names. A
/// character is lowercased (the event's key should already be the
/// unshifted character).
fn chord_key(key: Key) -> Option<ChordKey> {
    Some(match key {
        Key::Char(c) => char_key(c),
        Key::Escape => ChordKey::Escape,
        Key::Enter => ChordKey::Enter,
        Key::Tab => ChordKey::Tab,
        Key::Backspace => ChordKey::Backspace,
        Key::Insert => ChordKey::Insert,
        Key::Delete => ChordKey::Delete,
        Key::Left => ChordKey::Left,
        Key::Right => ChordKey::Right,
        Key::Up => ChordKey::Up,
        Key::Down => ChordKey::Down,
        Key::PageUp => ChordKey::PageUp,
        Key::PageDown => ChordKey::PageDown,
        Key::Home => ChordKey::Home,
        Key::End => ChordKey::End,
        Key::F(n) => ChordKey::F(n),
        Key::Kp0 => ChordKey::KpDigit(0),
        Key::Kp1 => ChordKey::KpDigit(1),
        Key::Kp2 => ChordKey::KpDigit(2),
        Key::Kp3 => ChordKey::KpDigit(3),
        Key::Kp4 => ChordKey::KpDigit(4),
        Key::Kp5 => ChordKey::KpDigit(5),
        Key::Kp6 => ChordKey::KpDigit(6),
        Key::Kp7 => ChordKey::KpDigit(7),
        Key::Kp8 => ChordKey::KpDigit(8),
        Key::Kp9 => ChordKey::KpDigit(9),
        Key::KpDecimal => ChordKey::KpDecimal,
        Key::KpDivide => ChordKey::KpDivide,
        Key::KpMultiply => ChordKey::KpMultiply,
        Key::KpSubtract => ChordKey::KpSubtract,
        Key::KpAdd => ChordKey::KpAdd,
        Key::KpEnter => ChordKey::KpEnter,
        Key::KpEqual => ChordKey::KpEqual,
        _ => return None,
    })
}

/// `c` lowercased when its lowercase is one character.
fn char_key(c: char) -> ChordKey {
    let mut lower = c.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(lower), None) => ChordKey::Char(lower),
        _ => ChordKey::Char(c),
    }
}

/// A chord as an `AppKit` menu item carries it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MenuShortcut {
    /// `NSMenuItem.keyEquivalent`: one character, lowercase; a function
    /// key is its `NS…FunctionKey` private-use character.
    pub key_equivalent: char,
    /// `NSMenuItem.keyEquivalentModifierMask`, as chord modifiers.
    pub modifiers: ChordModifiers,
}

/// How a menu item shows `chord`, or `None` when it cannot: only chords
/// with Command go on menu items (a menu key equivalent is matched before
/// the terminal sees the key, so `ctrl+c` or `f5` there would steal it from
/// the shell), and a keypad key cannot be told from its main-row twin.
///
/// A chord with Shift and a key that has a US shifted symbol shows the
/// symbol without Shift (`cmd+shift+/` is `?` with Command), as the menus
/// always have; letters keep an explicit Shift (`cmd+shift+w` is `w` with
/// Command and Shift).
#[must_use]
pub fn menu_shortcut(chord: Chord) -> Option<MenuShortcut> {
    if !chord.modifiers.contains(ChordModifiers::SUPER) || chord.key.is_keypad() {
        return None;
    }
    if let Some(symbol) = chord.shifted_symbol() {
        return Some(MenuShortcut {
            key_equivalent: symbol,
            modifiers: chord.modifiers.without(ChordModifiers::SHIFT),
        });
    }
    let function = |offset: u32| char::from_u32(0xF700 + offset);
    let key_equivalent = match chord.key {
        ChordKey::Char(c) => c,
        ChordKey::Up => function(0x00)?,
        ChordKey::Down => function(0x01)?,
        ChordKey::Left => function(0x02)?,
        ChordKey::Right => function(0x03)?,
        ChordKey::F(n) => function(0x03 + u32::from(n))?,
        ChordKey::Insert => function(0x27)?,
        ChordKey::Delete => function(0x28)?,
        ChordKey::Home => function(0x29)?,
        ChordKey::End => function(0x2B)?,
        ChordKey::PageUp => function(0x2C)?,
        ChordKey::PageDown => function(0x2D)?,
        ChordKey::Enter => '\r',
        ChordKey::Tab => '\t',
        ChordKey::Escape => '\u{1b}',
        ChordKey::Backspace => '\u{8}',
        _ => return None,
    };
    Some(MenuShortcut {
        key_equivalent,
        modifiers: chord.modifiers,
    })
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::*;

    #[test]
    fn every_default_chord_is_canonical_and_bound_once() {
        let mut seen = Vec::new();
        for spec in COMMANDS {
            for text in spec.defaults {
                let chord = Chord::parse(text).unwrap();
                assert_eq!(chord.to_string(), *text, "{}", spec.name);
                assert!(!seen.contains(&chord), "{text} is a default twice");
                seen.push(chord);
            }
        }
    }

    #[test]
    fn find_names_every_command_and_rejects_the_rest() {
        for id in CommandId::all() {
            let spec = id.spec();
            assert_eq!(CommandId::find(spec.name, spec.parameter), Ok(id));
        }
        assert!(matches!(
            CommandId::find("select_tab", Some("0")),
            Err(CommandError::ParameterOutOfRange {
                first: "1",
                last: "9",
                ..
            })
        ));
        assert_eq!(
            CommandId::find("select_tab", None),
            Err(CommandError::MissingParameter("select_tab"))
        );
        assert_eq!(
            CommandId::find("new_tab", Some("1")),
            Err(CommandError::TakesNoParameter("new_tab"))
        );
        assert_eq!(
            CommandId::find("nope", None),
            Err(CommandError::Unknown("nope".into()))
        );
    }

    /// [`Keymap::resolve`] without the modifier mask or the binary search:
    /// every probe looked for in the whole table.
    fn resolve_slowly(map: &Keymap, event: &KeyEvent) -> Option<Slot> {
        let modifiers = event.modifiers.without_locks();
        if modifiers.intersects(Modifiers::HYPER | Modifiers::META) {
            return None;
        }
        let bits = modifiers.bits();
        let mut probes = Vec::new();
        if let Some(key) = chord_key(event.key) {
            probes.push((bits, key));
        }
        if modifiers.contains(Modifiers::SHIFT)
            && let Some(shifted) = event.shifted_key
        {
            probes.push((bits & !Modifiers::SHIFT.bits(), ChordKey::Char(shifted)));
        }
        // The US key stands in only for a character beyond ASCII.
        let latin = match event.key {
            Key::Char(c) => u32::from(c) < 0x80,
            _ => true,
        };
        if let Some(base) = event.base_layout_key.filter(|_| !latin) {
            probes.push((bits, char_key(base)));
        }
        probes.into_iter().find_map(|probe| {
            map.table
                .iter()
                .find(|(entry, _)| *entry == probe)
                .map(|&(_, slot)| slot)
        })
    }

    const KEYS: [&str; 9] = ["a", "t", "/", "=", "1", "up", "enter", "kp_1", "f5"];
    const MODIFIERS: [&str; 6] = ["", "cmd+", "ctrl+", "shift+", "cmd+shift+", "alt+ctrl+"];
    const VALUES: [&str; 5] = ["new_tab", "none", "send_text:x", "unbound", "help"];

    fn key_of(index: usize) -> Key {
        [
            Key::Char('a'),
            Key::Char('t'),
            Key::Char('/'),
            Key::Char('='),
            Key::Char('1'),
            Key::Up,
            Key::Enter,
            Key::Kp1,
            Key::F(5),
            Key::Char('е'),
            Key::Char('ö'),
        ][index]
    }

    proptest::proptest! {
        /// The modifier mask and the sorted table find what a scan of
        /// every probe over the whole table finds.
        #[test]
        fn resolve_agrees_with_a_linear_scan(
            entries in proptest::collection::vec((0..6usize, 0..9usize, 0..5usize), 0..10),
            events in proptest::collection::vec(
                (0..11usize, 0..=255u8, proptest::option::of(0..9usize), proptest::option::of(0..9usize)),
                1..40,
            ),
        ) {
            let mut toml = String::from("[keys]\n");
            for (modifiers, key, value) in entries {
                writeln!(
                    toml,
                    "'{}{}' = '{}'",
                    MODIFIERS[modifiers], KEYS[key], VALUES[value]
                )
                .unwrap();
            }
            // TOML refuses a key written twice; that is not under test.
            let Ok(loaded) = tessera_config::Config::from_toml_lenient(&toml) else {
                return Ok(());
            };
            let map = Keymap::compile(&loaded.config.keys);
            let chars = ['a', 't', '/', '=', '1', '?', '+', 'T', 'е'];
            for (key, bits, shifted, base) in events {
                let mut event = KeyEvent::new(key_of(key), Modifiers::from_bits(bits));
                event.shifted_key = shifted.map(|i| chars[i]);
                event.base_layout_key = base.map(|i| chars[i]);
                let fast = map.resolve(&event);
                let slow = resolve_slowly(&map, &event).map(|slot| map.binding(slot));
                if !may_match(&event, map.mask, |word| map.ascii[word]) {
                    proptest::prop_assert_eq!(slow, None, "filtered out: {:?}", event);
                }
                proptest::prop_assert_eq!(fast, slow, "{:?}", event);
            }
        }
    }

    /// #317: the default map is the registry's defaults, for every command
    /// (it used to have no chords at all, and `chords` panicked).
    #[test]
    fn the_default_map_is_the_registry_defaults() {
        let map = Keymap::default();
        let compiled = Keymap::compile(&Keys::default());
        for id in CommandId::all() {
            let expected: Vec<Chord> = id
                .spec()
                .defaults
                .iter()
                .map(|text| Chord::parse(text).unwrap())
                .collect();
            assert_eq!(map.chords(id), expected, "{id}");
            assert_eq!(map.chords(id), compiled.chords(id), "{id}");
            for &chord in &expected {
                assert!(map.is_default(chord), "{chord}");
            }
        }
        let new_tab = CommandId::find("new_tab", None).unwrap();
        let event = KeyEvent::new(Key::Char('t'), Modifiers::SUPER);
        assert_eq!(map.resolve(&event), Some(Binding::Command(new_tab)));
        // Plain typing has no binding, and the mask says so first.
        let plain = KeyEvent::new(Key::Char('t'), Modifiers::NONE);
        assert!(!admits(map.mask, plain.modifiers));
        assert_eq!(map.resolve(&plain), None);
    }

    /// #317 K2: ⌘⇧E is unbound while ⌘E is bound (Use Selection for
    /// Find), so the filter must refuse it: the shifted `E` is looked up
    /// as typed under ⌘ (not lowercased to the bound `e`), and the shifted
    /// lookup is not a pass by itself.
    #[test]
    fn the_filter_refuses_command_shift_and_an_unbound_letter() {
        let map = Keymap::default();
        let mut event = KeyEvent::new(Key::Char('e'), Modifiers::SUPER | Modifiers::SHIFT);
        event.shifted_key = Some('E');
        assert!(admits(map.mask, event.modifiers), "some ⌘⇧ chord is bound");
        assert!(
            map.resolve(&KeyEvent::new(Key::Char('e'), Modifiers::SUPER))
                .is_some()
        );
        assert_eq!(map.resolve(&event), None);
        assert!(!may_match(&event, map.mask, |word| map.ascii[word]));
    }

    /// #317 K1: an entry left out for its command claims no chord, so a
    /// later entry of the same chord binds it; of two valid entries the
    /// first wins and the later one is left out with a warning naming
    /// the first.
    #[test]
    fn only_kept_entries_claim_their_chord() {
        let split_down = CommandId::find("split_down", None).unwrap();
        let split_right = CommandId::find("split_right", None).unwrap();
        let event = KeyEvent::new(Key::Char('d'), Modifiers::SUPER | Modifiers::SHIFT);
        let loaded = tessera_config::Config::from_toml_lenient(
            "[keys]\n'cmd+shift+d' = 'bogus'\n'shift+cmd+d' = 'split_right'\n",
        )
        .unwrap();
        let map = Keymap::compile(&loaded.config.keys);
        assert_eq!(map.resolve(&event), Some(Binding::Command(split_right)));
        let warnings: Vec<String> = map.warnings().iter().map(ToString::to_string).collect();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("\"cmd+shift+d\"") && warnings[0].contains("bogus"),
            "{warnings:?}"
        );
        assert!(
            map.chords(split_down).is_empty(),
            "the entry took ⌘⇧D from split_down"
        );

        let loaded = tessera_config::Config::from_toml_lenient(
            "[keys]\n'cmd+shift+d' = 'split_right'\n'shift+cmd+d' = 'split_down'\n",
        )
        .unwrap();
        let map = Keymap::compile(&loaded.config.keys);
        assert_eq!(map.resolve(&event), Some(Binding::Command(split_right)));
        let warnings: Vec<String> = map.warnings().iter().map(ToString::to_string).collect();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("\"shift+cmd+d\"")
                && warnings[0]
                    .contains("same chord as `cmd+shift+d` (the first entry wins); ignored"),
            "{warnings:?}"
        );
    }

    /// The effect and warning of each entry of `toml`'s `[keys]`, by key.
    fn effects(toml: &str) -> Vec<(String, KeyEffect, Option<String>)> {
        let loaded = tessera_config::Config::from_toml_lenient(toml).unwrap();
        let map = Keymap::compile_quietly(&loaded.config.keys);
        map.entries()
            .map(|entry| {
                (
                    entry.key,
                    entry.effect,
                    entry.warning.map(ToString::to_string),
                )
            })
            .collect()
    }

    /// #430 m4: "shadows plain typing; honoured" is reported for an entry
    /// that binds its chord, and only for one: an entry left out (an
    /// unknown command, an earlier entry for the chord) or `unbound` gets
    /// its own warning or none, and the loader reports none of it.
    #[test]
    fn only_a_bound_entry_shadows_plain_typing() {
        let toml = "[keys]\na = 'none'\n'shift+a' = 'bogus'\nA = 'copy'\nb = 'copy'\n\
                    'opt+a' = 'paste'\nc = 'unbound'\n'shift+b' = 'send_text:x'\n";
        let typing = |key: &str| {
            Some(format!(
                "`keys.\"{key}\"`: shadows plain typing of this key; honoured"
            ))
        };
        assert_eq!(
            effects(toml),
            [
                ("a".to_owned(), KeyEffect::Bound, typing("a")),
                (
                    "shift+a".to_owned(),
                    KeyEffect::Refused,
                    Some("`keys.\"shift+a\"`: unknown command `bogus`; ignored".to_owned())
                ),
                (
                    "A".to_owned(),
                    KeyEffect::Refused,
                    Some(
                        "`keys.\"A\"`: uppercase key `A` (write shift+ and the lowercase key); ignored"
                            .to_owned()
                    )
                ),
                ("b".to_owned(), KeyEffect::Bound, typing("b")),
                ("opt+a".to_owned(), KeyEffect::Bound, typing("opt+a")),
                ("c".to_owned(), KeyEffect::Unbound, None),
                ("shift+b".to_owned(), KeyEffect::Bound, typing("shift+b")),
            ]
        );
        let loaded = tessera_config::Config::from_toml_lenient(
            "[keys]\na = 'copy'\n'shift+a' = 'bogus'\nb = 'unbound'\n",
        )
        .unwrap();
        let load: Vec<String> = loaded.warnings.iter().map(ToString::to_string).collect();
        assert!(
            load.is_empty(),
            "the loader reports no typing caveat: {load:?}"
        );
        let map: Vec<String> = Keymap::compile_quietly(&loaded.config.keys)
            .warnings()
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            map,
            [
                "`keys.\"a\"`: shadows plain typing of this key; honoured",
                "`keys.\"shift+a\"`: unknown command `bogus`; ignored",
            ]
        );
        // Shadowed by an earlier entry: the "same chord" warning only.
        assert_eq!(
            effects("[keys]\n'shift+a' = 'copy'\n'SHIFT+a' = 'paste'\n"),
            [
                ("shift+a".to_owned(), KeyEffect::Bound, typing("shift+a")),
                (
                    "SHIFT+a".to_owned(),
                    KeyEffect::Shadowed,
                    Some(
                        "`keys.\"SHIFT+a\"`: same chord as `shift+a` (the first entry wins); ignored"
                            .to_owned()
                    )
                ),
            ]
        );
    }

    /// #430 m6: an `unbound` entry after a kept entry for the same chord
    /// is left out with the "same chord" warning, as any later entry is
    /// (the first wins), never silently; and a kept `unbound` wins over a
    /// later binding of its chord.
    #[test]
    fn an_unbound_entry_after_a_binding_is_shadowed_with_a_warning() {
        let new_tab = CommandId::find("new_tab", None).unwrap();
        let event = KeyEvent::new(Key::Char('t'), Modifiers::SUPER);
        let loaded = tessera_config::Config::from_toml_lenient(
            "[keys]\n'cmd+t' = 'new_tab'\n'command+t' = 'unbound'\n",
        )
        .unwrap();
        let map = Keymap::compile_quietly(&loaded.config.keys);
        let entries: Vec<_> = map
            .entries()
            .map(|e| (e.effect, e.warning.cloned()))
            .collect();
        let same = Warning::KeyBinding {
            key: "command+t".into(),
            reason: "same chord as `cmd+t` (the first entry wins); ignored".into(),
        };
        assert_eq!(
            entries,
            [
                (KeyEffect::Bound, None),
                (KeyEffect::Shadowed, Some(same.clone()))
            ]
        );
        assert_eq!(map.warnings(), [same]);
        assert_eq!(map.resolve(&event), Some(Binding::Command(new_tab)));
        assert_eq!(map.chords(new_tab), [Chord::parse("cmd+t").unwrap()]);

        let loaded = tessera_config::Config::from_toml_lenient(
            "[keys]\n'cmd+t' = 'unbound'\n'command+t' = 'new_tab'\n",
        )
        .unwrap();
        let map = Keymap::compile_quietly(&loaded.config.keys);
        let entries: Vec<_> = map.entries().map(|e| e.effect).collect();
        assert_eq!(entries, [KeyEffect::Unbound, KeyEffect::Shadowed]);
        assert_eq!(map.resolve(&event), None, "the unbound entry came first");
        assert_eq!(map.chords(new_tab), []);
    }

    /// `is_default` against the rule it caches: no `[keys]` entry names
    /// the chord, and some command has it as a registry default.
    #[test]
    fn is_default_agrees_with_the_registry() {
        let loaded = tessera_config::Config::from_toml_lenient(
            "[keys]\n'cmd+up' = 'scroll_page_up'\n'cmd+t' = 'bogus'\n'cmd+n' = 'unbound'\n'cmd+j' = 'new_tab'\n",
        )
        .unwrap();
        let map = Keymap::compile(&loaded.config.keys);
        let mut chords: Vec<Chord> = COMMANDS
            .iter()
            .flat_map(|spec| spec.defaults.iter().map(|text| Chord::parse(text).unwrap()))
            .collect();
        chords.extend(["cmd+j", "cmd+shift+j", "ctrl+t", "f5"].map(|t| Chord::parse(t).unwrap()));
        for chord in chords {
            let slow = !map.keys().iter().any(|binding| binding.chord == chord)
                && COMMANDS.iter().any(|spec| {
                    spec.defaults
                        .iter()
                        .any(|text| Chord::parse(text).ok() == Some(chord))
                });
            assert_eq!(map.is_default(chord), slow, "{chord}");
        }
        assert!(!map.is_default(Chord::parse("cmd+up").unwrap()));
        assert!(map.is_default(Chord::parse("cmd+down").unwrap()));
    }
}
