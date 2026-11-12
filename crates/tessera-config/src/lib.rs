//! Configuration.
//!
//! Defines the TOML schema (`config.toml`, `profiles/*.toml`, `themes/*.toml`)
//! as plain Rust types with `serde`, supplies defaults for every field, loads
//! and validates files, watches them for changes, and writes edits back from
//! the preferences UI without clobbering user comments. Unknown keys warn and
//! are preserved; they never fail a load.
//!
//! Loading is resilient: a `config.toml` that cannot be read or does not
//! parse never prevents the store from opening (defaults apply, with
//! `terminal.clipboard_write` off until a non-empty version of the file
//! loads, and [`ConfigStore::last_error`] reports why), and an invalid edit
//! while running keeps the last good configuration until the file is
//! repaired. A `config.toml` symlink whose target is missing counts as a
//! file that does not load, not as a missing one, and so does a
//! `config.toml` missing because the configuration directory is such a
//! symlink. Each failure is logged
//! once, with the text it quotes from the file escaped.
//! Watching uses kernel file events where available and polling elsewhere;
//! see ADR 0006.
//!
//! Typical use from the application layer: **one watcher per process**, with
//! the application fanning each event out to its windows. Several watchers
//! on the same store also work (every one receives every event), but each
//! costs a thread, a kernel queue and about seven file descriptors (up to
//! eight more while the configured theme's file does not load, and one per
//! directory holding a symlink on the way to the configuration directory,
//! at most eight), so do not create one per window.
//!
//! A `config.toml` that is a symlink (into a dotfiles checkout, say) is
//! respected: defaults are never written over it, saves go through to its
//! target, and the watcher follows the target even when it is deleted and
//! recreated.
//!
//! Scope: this crate reads and watches `config.toml` and exposes the applied
//! values (font, size, opacity, scrollback cap, theme name). It also reads
//! colour themes: `themes/*.toml` files and ten bundled themes, resolved
//! from `appearance.theme` by [`ConfigStore::theme`] ([`Theme::resolve`]),
//! and imports iTerm2 `.itermcolors` and base16 schemes
//! ([`import_itermcolors`], [`import_base16`]); see ADR 0019. Routing the
//! applied configuration into the windows is the application layer's job
//! (`tessera-app-core` runs one watcher per application and applies each
//! change to its views) and is not done here.
//!
//! ```no_run
//! use tessera_config::{ConfigEvent, ConfigPaths, ConfigStore, WatchOptions};
//!
//! let paths = ConfigPaths::for_current_user().expect("home directory");
//! let store = ConfigStore::open(paths)?;          // creates the directory and default file if missing
//! let font_size = store.config().appearance.font_size;
//! if let Some(error) = store.last_error() {
//!     // show "config.toml could not be loaded; using defaults"
//! }
//! let watcher = store.watch(WatchOptions::default())?;
//! match watcher.try_recv() {
//!     Ok(ConfigEvent::Changed(changed)) => { /* apply changed.config to every window */ }
//!     Ok(ConfigEvent::Invalid(error)) => { /* keep running; show the error */ }
//!     Ok(ConfigEvent::ThemesChanged) => { /* apply store.theme() if it differs */ }
//!     _ => {}
//! }
//! # Ok::<(), tessera_config::Error>(())
//! ```

#![forbid(unsafe_code)]

pub mod edit;
mod import;
mod keys;
mod load;
mod paths;
mod profile;
mod selection;
mod store;
mod theme;
mod watch;

pub use import::{
    ImportError, MAX_IMPORT_LEN, import_base16, import_itermcolors, read_scheme_file,
};
pub use theme::{
    Color, DERIVED_SELECTION_ALPHA, InvalidColor, MAX_SLUG_LEN, MAX_THEME_DIR_ENTRIES,
    MAX_THEME_FILE_LEN, MAX_THEME_FILES, MAX_THEME_NAME_LEN, ResolvedTheme, Theme, ThemeError,
    ThemeSource, clip_theme_name, slug,
};

pub use keys::{
    Chord, ChordError, ChordKey, ChordModifiers, KeyAction, KeyActionError, KeyBinding, Keys,
    MAX_CHORD_LEN, MAX_COMMAND_NAME_LEN, MAX_KEY_BINDINGS, MAX_SEND_TEXT_LEN,
};
pub use load::{
    DEFAULT_CONFIG_TOML, Loaded, MAX_CONFIG_FILE_LEN, MAX_PARSE_ERROR_LEN,
    MAX_UNKNOWN_KEY_WARNINGS, MAX_WARNING_PATH_LEN, ParseError, Warning, escaped, load_file,
    write_atomic, write_default_if_missing,
};
pub use paths::{APP_SUPPORT_DIR_NAME, CONFIG_FILE_NAME, ConfigPaths, SHELL_INTEGRATION_DIR_NAME};
pub use profile::{
    BUILT_IN_PROFILE_NAME, MAX_PROFILE_NAME_LEN, MAX_PROFILES, ProfileAppearance, ResolvedProfile,
};
pub use selection::{
    CompiledRule, DEFAULT_SMART_RULES, MAX_SMART_RULE_PATTERN_LEN, MAX_SMART_RULES, Precision,
    RuleCheck, SMART_RULE_NEST_LIMIT, SMART_RULE_SIZE_LIMIT, Selection, SmartRule, SmartRules,
    default_rules,
};
pub use store::ConfigStore;
pub use watch::{
    ConfigChanged, ConfigEvent, MAX_WATCH_DURATION, MIN_POLL_INTERVAL, WatchBackend, WatchGuard,
    WatchOptions, Watcher,
};

use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// Errors from loading or saving configuration.
///
/// Cloneable so one error can be both reported through a
/// [`ConfigEvent`] and remembered by the store ([`ConfigStore::last_error`]).
#[derive(Debug, Clone, thiserror::Error)]
pub enum Error {
    /// The file could not be read or written.
    #[error("io: {0}")]
    Io(#[source] Arc<std::io::Error>),
    /// The file is not valid TOML or does not match the schema. The message
    /// is bounded in length (see [`ParseError`]), so a syntax error on a
    /// very long line cannot produce a megabyte-sized error.
    #[error("parse: {0}")]
    Parse(#[from] ParseError),
    /// The file is not valid UTF-8 (TOML documents must be).
    #[error("encoding: {0}")]
    Utf8(#[from] std::str::Utf8Error),
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::Io(Arc::new(error))
    }
}

impl From<toml::de::Error> for Error {
    fn from(error: toml::de::Error) -> Self {
        Self::Parse(error.into())
    }
}

/// Top-level configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(default)]
pub struct Config {
    /// Application-wide settings.
    pub general: General,
    /// Default appearance; profiles may override.
    pub appearance: Appearance,
    /// Terminal emulation limits and shell integration.
    pub terminal: Terminal,
    /// Named profiles.
    pub profiles: Vec<Profile>,
    /// Plugin enablement, keyed by plugin id.
    pub plugins: Plugins,
    /// Smart selection on a double-click, and Copy's trailing blanks.
    pub selection: Selection,
    /// Key bindings: each entry maps a chord to a command, so
    /// `"cmd+shift+d" = "split_down"` (always quote the chord). An entry
    /// adds its chord to the command, ahead of its defaults: the menu item
    /// shows the first, the others stay as hidden shortcuts. An entry on
    /// another command's default chord takes it from that command; write
    /// `"cmd+t" = "unbound"` to drop a default.
    ///
    /// A chord is zero or more modifiers and one key joined by `+`, with
    /// no spaces: modifiers `cmd` (`command`, `super`), `ctrl`
    /// (`control`), `alt` (`opt`, `option`) and `shift`, in any order and
    /// any case; the key is one lowercase character (`t`, `1`, `,`, `ö`)
    /// or a name: `escape`, `enter`, `tab`, `space`, `backspace`,
    /// `delete`, `insert`, `left`, `right`, `up`, `down`, `home`, `end`,
    /// `page_up`, `page_down`, `f1` to `f35`, `kp_0` to `kp_9`,
    /// `kp_decimal`, `kp_divide`, `kp_multiply`, `kp_subtract`, `kp_add`,
    /// `kp_enter`, `kp_equal`, and `plus`, `minus`, `equal`, `comma`,
    /// `period`, `slash`, `backslash`, `semicolon`, `apostrophe`,
    /// `grave_accent`, `left_bracket`, `right_bracket`. A shifted symbol is
    /// shorthand for Shift and its US key: `cmd+?` is `cmd+shift+/`,
    /// `cmd+plus` is `cmd+shift+=`. Uppercase keys are an error (write
    /// `shift+t`). At most 64 bytes.
    ///
    /// A value is a command name, `send_text:<text>` (bytes written to
    /// the shell, with the escapes `\e` `\n` `\r` `\t` `\a`
    /// `\b` `\f` `\v` `\\` `\"` `\'`, `\xHH` for a raw
    /// byte and `\u{H..}` for a character; at most 4096 bytes, written
    /// as is), `unbound` (remove the chord's default) or `none` (swallow
    /// the key). Commands: `settings`, `hide`, `hide_others`, `quit`,
    /// `new_window`, `new_tab`, `close_tab`, `close_window`,
    /// `clear_scrollback`, `copy`, `copy_with_formatting` (Copy as RTF and
    /// HTML too), `paste`, `paste_plain`, `select_all`,
    /// `font_bigger`, `font_smaller`, `font_reset`, `scroll_to_top`,
    /// `scroll_to_bottom`, `scroll_page_up`, `scroll_page_down`,
    /// `minimize`, `zoom`, `show_tab_bar`, `tab_previous`, `tab_next`,
    /// `move_tab_to_new_window`, `merge_all_windows`, `select_tab:1` to
    /// `select_tab:9`, `bring_all_to_front`, `help`, `about`, `show_all`,
    /// `split_right`, `split_down`, `focus_pane_left`, `focus_pane_right`,
    /// `focus_pane_up`, `focus_pane_down`, `focus_pane_previous`,
    /// `focus_pane_next`, `zoom_pane` (show the focused pane alone in its
    /// tab, or the whole layout again), `equalize_panes` (give every pane
    /// of the tab an equal share), `find`, `find_next` (the match toward
    /// older output), `find_previous` (toward newer output),
    /// `use_selection_for_find`, `copy_find_matches`, `prompt_previous` and
    /// `prompt_next` (the shell's previous or next prompt, by its OSC 133
    /// marks; Page Up or Down when the history holds none),
    /// `select_last_output` (select the output of the last finished
    /// command), and, for features still to come, `copy_mode`.
    ///
    /// Only chords with `cmd` appear in menus; others work in the
    /// terminal only. A malformed entry, an unknown command or a chord
    /// already bound by an earlier entry is ignored with a warning (the
    /// first entry wins); at most 512 entries are read.
    ///
    /// The Settings window's Keys pane writes chords in the canonical form
    /// (`cmd+ctrl+alt+shift+key`). An entry you write by hand in another
    /// form (modifiers in another order, an alias such as `opt`, a shifted
    /// symbol such as `cmd+?`) still binds, but the pane does not recognise
    /// the entry as its chord: such an entry that sends text or is `none`
    /// is not shown as a conflict, and replacing or recording again a chord
    /// such an entry holds shows the refusal ("same chord as …") and writes
    /// nothing. Write such entries canonically, or edit them in `config.toml`
    /// (#355).
    pub keys: Keys,
}

/// Application-wide settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(default)]
pub struct General {
    /// The `[[profiles]]` entry new tabs, windows and panes use when none
    /// is chosen (a pane split off another uses that pane's). When no
    /// profile has this name, the built-in one is used: `[appearance]` and
    /// the login shell (with a warning, unless the name is `"Default"`).
    pub default_profile: String,
    /// Confirm before closing a window with running jobs.
    pub confirm_close: bool,
    /// How a tab whose shell runs on another machine is marked: `"title"`
    /// puts `⇢ <host>: ` before its title, the host taken from the shell's
    /// working-directory report (OSC 7), or `⇢ ssh` when `ssh` runs with
    /// no such report; `"off"` shows titles unchanged. Applies to open
    /// tabs when the file changes. Any other value is reported with a
    /// warning and `"title"` is used.
    #[serde(deserialize_with = "load::lenient_remote_indicator")]
    #[cfg_attr(feature = "schema", schemars(schema_with = "RemoteIndicator::schema"))]
    pub remote_indicator: RemoteIndicator,
}

impl Default for General {
    fn default() -> Self {
        Self {
            default_profile: "Default".into(),
            confirm_close: true,
            remote_indicator: RemoteIndicator::Title,
        }
    }
}

/// How a remote session is marked (`[general] remote_indicator`, #205).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RemoteIndicator {
    /// `⇢ <host>: ` before the tab's title.
    #[default]
    Title,
    /// No mark.
    Off,
}

impl RemoteIndicator {
    /// Every value, in the order the documentation lists them.
    pub const ALL: [Self; 2] = [Self::Title, Self::Off];

    /// The value as written in `config.toml`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Title => "title",
            Self::Off => "off",
        }
    }

    /// The value `name` is written as: exactly
    /// [`RemoteIndicator::as_str`], no other spelling or case.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|value| value.as_str() == name)
    }

    /// A plain string `enum` schema, as for [`CursorShape`].
    #[cfg(feature = "schema")]
    fn schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let values: Vec<&str> = Self::ALL.iter().map(|value| value.as_str()).collect();
        schemars::json_schema!({ "type": "string", "enum": values })
    }
}

/// Visual settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(default)]
pub struct Appearance {
    /// Colour theme, by name. Looked up first among the `*.toml` files in
    /// `themes/` next to this file (by file stem, then by the `name` key
    /// inside each file), then among the bundled themes: Tessera Dark,
    /// Tessera Light, Solarized Dark, Solarized Light, Dracula, Nord,
    /// Gruvbox Dark, One Dark, Tokyo Night and Catppuccin Mocha. Names
    /// match ignoring case, and any run of characters other than letters
    /// and digits counts as one separator, with `+` read as the word
    /// `plus`: `solarized-dark` and `SOLARIZED  DARK` are Solarized Dark,
    /// `Dracula+` is `dracula-plus`, not Dracula, but `solarizeddark` is not
    /// Solarized Dark. A user file shadows a bundled theme of the same name.
    /// Unicode is not normalised, so an accented name matches only when
    /// typed in the same form (precomposed or combining) as the theme's. An
    /// unknown name falls back to Tessera Dark with a warning.
    pub theme: String,
    /// Font family.
    pub font_family: String,
    /// Font size in points, clamped on load to
    /// [`Appearance::MIN_FONT_SIZE`]..=[`Appearance::MAX_FONT_SIZE`].
    pub font_size: f32,
    /// Background opacity, 0.0..=1.0. A window is translucent while any
    /// pane of its tab is. While the window is translucent its title bar
    /// and tab bar are always blurred, whatever `blur` says, and tinted
    /// with the theme's background at this opacity. The system's Reduce
    /// Transparency setting draws every session opaque.
    pub opacity: f32,
    /// Background blur behind each pane whose background is translucent
    /// (`opacity` below 1.0, or a theme background with alpha). The title
    /// bar and tab bar of a translucent window are always blurred, whatever
    /// this says. Off while the system's Reduce Transparency setting is on.
    pub blur: bool,
    /// How strongly a window blurs what is behind it, from 0 to 64
    /// ([`Appearance::MAX_BLUR_RADIUS`]). 0 keeps the system's standard
    /// blur behind each pane that asks for `blur`; 1 to 64 blurs the whole
    /// window at that radius instead, but only while every translucent
    /// pane of the tab asks for `blur` (otherwise each pane keeps the
    /// standard blur). The title bar and tab bar keep the standard blur
    /// either way. A value above 64 is lowered to 64 with a warning; a
    /// negative or fractional one (`-1`, `20.0`) makes the file invalid,
    /// as for `scrollback_lines`. Applies to open windows when the file
    /// changes. Set it here only: in a profile's `appearance` it is
    /// ignored with a warning, since the window, not the session, is
    /// blurred.
    pub blur_radius: u32,
    /// The cursor's shape: `"block"`, `"underline"` or `"beam"` (a
    /// vertical bar), exactly as written here. A program may change it
    /// (DECSCUSR, `CSI Ps SP q`, as vim and fish do); it returns to this
    /// one when the program asks for the default or resets the terminal.
    /// Any other value is reported with a warning and the default is used.
    #[serde(deserialize_with = "load::lenient_cursor_shape")]
    #[cfg_attr(feature = "schema", schemars(schema_with = "CursorShape::schema"))]
    pub cursor_shape: CursorShape,
    /// Whether the cursor blinks, unless a program says otherwise (as for
    /// `cursor_shape`). tmux and vim turn blinking off on their own
    /// (`CSI ? 12 l`), so inside a running tmux a blinking cursor stays off
    /// until this is next changed. Changing it applies to open tabs, also
    /// where a program turned blinking on or off, but a program still
    /// running, such as tmux, may turn it off again. Recorded only for now:
    /// the cursor is drawn steady. A value that is not `true` or `false` is
    /// reported with a warning and the default is used.
    #[serde(deserialize_with = "load::lenient_cursor_blink")]
    pub cursor_blink: bool,
    /// How much to darken the panes of a split tab that do not have the
    /// keyboard focus, from 0.0 (off) to 0.8, drawn as a black overlay of
    /// that opacity. A tab with one pane is never dimmed. A value outside
    /// the range is clamped (not a number means 0.0) with a warning.
    /// Applies to open windows when the file changes. Set it here only:
    /// in a profile's `appearance` it is ignored with a warning, since the
    /// window, not the session, draws it.
    pub dim_inactive_panes: f32,
}

/// The cursor's shape (`[appearance] cursor_shape`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CursorShape {
    /// A filled cell.
    #[default]
    Block,
    /// A bar along the bottom of the cell.
    Underline,
    /// A vertical bar at the left edge of the cell (xterm's "bar").
    Beam,
}

impl CursorShape {
    /// Every value, in the order the documentation lists them.
    pub const ALL: [Self; 3] = [Self::Block, Self::Underline, Self::Beam];

    /// The value as written in `config.toml`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Block => "block",
            Self::Underline => "underline",
            Self::Beam => "beam",
        }
    }

    /// The shape `name` is written as: exactly [`CursorShape::as_str`],
    /// no other spelling or case.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|shape| shape.as_str() == name)
    }

    /// A plain string `enum` schema, as for [`ShellIntegration`].
    #[cfg(feature = "schema")]
    fn schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let values: Vec<&str> = Self::ALL.iter().map(|value| value.as_str()).collect();
        schemars::json_schema!({ "type": "string", "enum": values })
    }
}

impl Appearance {
    /// Smallest `font_size` accepted; smaller values are raised on load.
    pub const MIN_FONT_SIZE: f32 = 4.0;
    /// Largest `font_size` accepted; larger values are lowered on load.
    pub const MAX_FONT_SIZE: f32 = 512.0;
    /// Largest `dim_inactive_panes` accepted, so an unfocused pane stays
    /// readable; larger values are lowered on load.
    pub const MAX_DIM_INACTIVE_PANES: f32 = 0.8;
    /// Largest `blur_radius` accepted, where the window server's blur stops
    /// growing (ADR 0045); larger values are lowered on load.
    pub const MAX_BLUR_RADIUS: u32 = 64;
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            theme: Theme::DEFAULT_NAME.into(),
            font_family: "Menlo".into(),
            font_size: 13.0,
            opacity: 1.0,
            blur: false,
            blur_radius: 0,
            cursor_shape: CursorShape::Block,
            cursor_blink: false,
            dim_inactive_panes: 0.0,
        }
    }
}

/// Terminal emulation limits and shell integration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(default)]
pub struct Terminal {
    /// Maximum number of scrollback lines kept per pane. Applies to sessions
    /// started after a change; running sessions keep the size they started with.
    /// Large values cost time on every width change: resizing a window
    /// re-wraps the whole history while the terminal is locked, about 0.2 ms
    /// per 1,000 lines (1-4 ms at 10,000 lines, 14-41 ms at 100,000), so at
    /// the 10,000,000 limit a full history takes about 18 GB of memory and
    /// each resize stalls the pane for 2-4 seconds.
    pub scrollback_lines: u32,
    /// Shell integration for zsh and fish (bash support is to follow):
    /// `"auto"` makes the shells Tessera starts load its scripts, which mark
    /// prompts and commands and report the working directory; `"off"` starts
    /// them unchanged. A session started with `TESSERA_SHELL_INTEGRATION` in
    /// its environment is never changed.
    #[cfg_attr(feature = "schema", schemars(schema_with = "ShellIntegration::schema"))]
    pub shell_integration: ShellIntegration,
    /// Whether programs may write the clipboard (OSC 52, as tmux and
    /// Neovim do over `ssh`): up to 1 MiB of text per write. Programs can
    /// never read the clipboard. A value that is not `true` or `false` is
    /// reported with a warning and counts as `false`: writes are refused
    /// until it is fixed. When Tessera starts with a `config.toml` that does
    /// not load at all (a syntax error anywhere in it, such as
    /// `clipboard_write = off` without quotes, a duplicate key, a file that
    /// is not UTF-8, a `config.toml` symlink whose target is missing, or a
    /// configuration directory that is such a symlink, as when a dotfiles
    /// volume is not mounted), writes are refused as well until the file is
    /// fixed (or the directory appears, also when the link is above the
    /// configuration directory), while the other defaults apply;
    /// emptying the file (or leaving only a byte order mark) does not turn
    /// them back on (a save that truncates first leaves it empty for a
    /// moment; one gap: an appeared directory opened with ⌘, within two
    /// seconds, before Tessera has noticed it, does), only a version
    /// of it with something in it that loads. Applies to every session at
    /// once when changed.
    #[serde(deserialize_with = "load::lenient_clipboard_write")]
    pub clipboard_write: bool,
}

/// Whether Tessera injects its shell integration scripts into the shells it
/// starts (`[terminal] shell_integration`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShellIntegration {
    /// Inject into zsh and fish; other programs (bash too, for now) are
    /// started unchanged.
    #[default]
    Auto,
    /// Never inject.
    Off,
}

impl ShellIntegration {
    /// Every value, in the order the documentation lists them.
    pub const ALL: [Self; 2] = [Self::Auto, Self::Off];

    /// The value as written in `config.toml`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Off => "off",
        }
    }

    /// A plain string `enum` schema: the derived one for documented
    /// variants (`oneOf` of `const`s) is a shape `cargo xtask config-docs`
    /// does not render.
    #[cfg(feature = "schema")]
    fn schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let values: Vec<&str> = Self::ALL.iter().map(|value| value.as_str()).collect();
        schemars::json_schema!({ "type": "string", "enum": values })
    }
}

impl Terminal {
    /// Upper bound for `scrollback_lines`; larger values are capped on load.
    pub const MAX_SCROLLBACK_LINES: u32 = 10_000_000;
}

impl Default for Terminal {
    fn default() -> Self {
        Self {
            scrollback_lines: 10_000,
            shell_integration: ShellIntegration::Auto,
            clipboard_write: true,
        }
    }
}

/// A named session profile: what a new tab, window or pane runs and
/// looks like. Chosen from File > New Tab with Profile, or by
/// `[general] default_profile`. At most 256 profiles are read and a name
/// is at most 128 bytes (longer ones are cut, with a warning).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(default)]
pub struct Profile {
    /// Display name, shown in the menus. Should be unique: of two
    /// profiles with one name, the first is used and the second is
    /// reported with a warning. A NUL character in it is replaced with
    /// U+FFFD, with a warning.
    pub name: String,
    /// Command to run instead of the login shell: one program name or
    /// path runs directly (`"/opt/homebrew/bin/fish"`), anything with a
    /// space runs through `/bin/sh -c` (`"ssh build-box"`). Unset means the
    /// login shell. A command that cannot be executed (it does not
    /// exist, or is not executable) starts your login shell in your home
    /// directory instead, with a line in the new session saying so.
    pub command: Option<String>,
    /// Starting directory; `~` at the start is your home directory. A
    /// directory that does not exist falls back to your home directory
    /// with a warning; one that exists but cannot be entered starts your
    /// login shell in your home directory, with a line in the new session
    /// saying so. Used only when the session is not given a directory of
    /// its own; unset means your home directory.
    pub working_directory: Option<String>,
    /// Appearance for this profile's sessions: each key set here replaces
    /// the one in `[appearance]`, each key left out follows `[appearance]`
    /// (also when it changes). In 0.1.0 a key left out here was reset to
    /// its built-in default instead (a profile that set only `font_size`
    /// also reset `theme`, `opacity` and the cursor). `dim_inactive_panes` and
    /// `blur_radius` cannot be set here (the window draws them): they are
    /// ignored with a warning.
    pub appearance: Option<ProfileAppearance>,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            name: "Default".into(),
            command: None,
            working_directory: None,
            appearance: None,
        }
    }
}

/// Plugin enablement.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(default)]
pub struct Plugins {
    /// Plugin ids that are enabled.
    pub enabled: Vec<String>,
}

impl Config {
    /// Parses a TOML document without diagnostics: unknown keys are silently
    /// ignored and values are not range-checked. Prefer
    /// [`Config::from_toml_lenient`].
    pub fn from_toml(text: &str) -> Result<Self, Error> {
        let mut config: Self = toml::from_str(text)?;
        load::put_keys_in_document_order(&mut config, text);
        Ok(config)
    }

    /// Serializes to TOML.
    pub fn to_toml(&self) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_document_is_all_defaults() {
        assert_eq!(Config::from_toml("").unwrap(), Config::default());
    }

    #[test]
    fn round_trips() {
        let cfg = Config::default();
        assert_eq!(Config::from_toml(&cfg.to_toml().unwrap()).unwrap(), cfg);
    }

    #[test]
    fn shell_integration_is_auto_unless_turned_off() {
        assert_eq!(
            Config::default().terminal.shell_integration,
            ShellIntegration::Auto
        );
        for value in ShellIntegration::ALL {
            let text = format!("[terminal]\nshell_integration = \"{}\"\n", value.as_str());
            let loaded = Config::from_toml_lenient(&text).unwrap();
            assert_eq!(loaded.config.terminal.shell_integration, value);
            assert_eq!(loaded.warnings, []);
            // `as_str` is what serde writes.
            assert!(loaded.config.to_toml().unwrap().contains(&text[11..]));
        }
        // Any other value is a type mismatch, like a mistyped number.
        for bad in ["\"on\"", "\"Auto\"", "true", "1"] {
            let text = format!("[terminal]\nshell_integration = {bad}\n");
            assert!(Config::from_toml_lenient(&text).is_err(), "{bad}");
        }
    }
}
