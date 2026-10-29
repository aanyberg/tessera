//! Configuration.
//!
//! Defines the TOML schema (`config.toml`, `profiles/*.toml`, `themes/*.toml`)
//! as plain Rust types with `serde`, supplies defaults for every field, loads
//! and validates files, watches them for changes, and writes edits back from
//! the preferences UI without clobbering user comments. Unknown keys warn and
//! are preserved; they never fail a load.
//!
//! Loading is resilient: a `config.toml` that cannot be read or does not
//! parse never prevents the store from opening (defaults apply and
//! [`ConfigStore::last_error`] reports why), and an invalid edit while
//! running keeps the last good configuration until the file is repaired.
//! Watching uses kernel file events where available and polling elsewhere;
//! see ADR 0006.
//!
//! Typical use from the application layer: **one watcher per process**, with
//! the application fanning each event out to its windows. Several watchers
//! on the same store also work (every one receives every event), but each
//! costs a thread, a kernel queue and about five file descriptors, so do
//! not create one per window.
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

mod import;
mod load;
mod paths;
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

pub use load::{
    DEFAULT_CONFIG_TOML, Loaded, MAX_CONFIG_FILE_LEN, MAX_PARSE_ERROR_LEN,
    MAX_UNKNOWN_KEY_WARNINGS, MAX_WARNING_PATH_LEN, ParseError, Warning, load_file, write_atomic,
    write_default_if_missing,
};
pub use paths::{APP_SUPPORT_DIR_NAME, CONFIG_FILE_NAME, ConfigPaths, SHELL_INTEGRATION_DIR_NAME};
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
}

/// Application-wide settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(default)]
pub struct General {
    /// Profile used for new sessions when none is specified.
    pub default_profile: String,
    /// Confirm before closing a window with running jobs.
    pub confirm_close: bool,
}

impl Default for General {
    fn default() -> Self {
        Self {
            default_profile: "Default".into(),
            confirm_close: true,
        }
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
    /// Window opacity, 0.0..=1.0.
    pub opacity: f32,
    /// Background blur when opacity is below 1.0.
    pub blur: bool,
}

impl Appearance {
    /// Smallest `font_size` accepted; smaller values are raised on load.
    pub const MIN_FONT_SIZE: f32 = 4.0;
    /// Largest `font_size` accepted; larger values are lowered on load.
    pub const MAX_FONT_SIZE: f32 = 512.0;
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            theme: Theme::DEFAULT_NAME.into(),
            font_family: "Menlo".into(),
            font_size: 13.0,
            opacity: 1.0,
            blur: false,
        }
    }
}

/// Terminal emulation limits and shell integration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(default)]
pub struct Terminal {
    /// Maximum number of scrollback lines kept per pane.
    pub scrollback_lines: u32,
    /// Shell integration for zsh and fish (bash support is to follow):
    /// `"auto"` makes the shells Tessera starts load its scripts, which mark
    /// prompts and commands and report the working directory; `"off"` starts
    /// them unchanged. A session started with `TESSERA_SHELL_INTEGRATION` in
    /// its environment is never changed.
    #[cfg_attr(feature = "schema", schemars(schema_with = "ShellIntegration::schema"))]
    pub shell_integration: ShellIntegration,
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
        }
    }
}

/// A named session profile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(default)]
pub struct Profile {
    /// Display name, unique.
    pub name: String,
    /// Command to run; `None` means the user's login shell.
    pub command: Option<String>,
    /// Starting directory; `None` means inherit.
    pub working_directory: Option<String>,
    /// Appearance overrides.
    pub appearance: Option<Appearance>,
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
        Ok(toml::from_str(text)?)
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
