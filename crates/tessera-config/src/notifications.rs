//! `[notifications]`: a system notification when a long command finishes in
//! a pane that does not have the keyboard (#470, ADR 0048).

use serde::{Deserialize, Serialize};

use crate::Warning;

/// System notifications (ADR 0048). App-wide: no profile changes them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(default)]
pub struct Notifications {
    /// When a command that ran at least `command_finished_after` seconds
    /// finishes, `"unfocused"` posts a system notification and marks its
    /// tab, unless its pane has the keyboard (the app is active, its window
    /// is key, its tab selected and the pane focused); `"off"` does
    /// neither. Needs shell integration's prompt marks (OSC 133). The
    /// first notification asks macOS for permission. At most 10 are posted
    /// a minute; the tab is marked all the same. Set here only: no
    /// profile changes it. Any other value is reported with a warning and
    /// `"off"` is used.
    #[serde(deserialize_with = "crate::load::lenient_command_finished")]
    #[cfg_attr(
        feature = "schema",
        schemars(schema_with = "CommandFinishedMode::schema")
    )]
    pub command_finished: CommandFinishedMode,
    /// How long a command must run, in whole seconds (1 to 86400), before
    /// its finish notifies. A value outside the range is clamped to it
    /// with a warning. Set here only: no profile changes it.
    pub command_finished_after: u32,
}

impl Notifications {
    /// The shortest `command_finished_after`, in seconds.
    pub const MIN_COMMAND_FINISHED_AFTER: u32 = 1;
    /// The longest `command_finished_after`, in seconds (a day).
    pub const MAX_COMMAND_FINISHED_AFTER: u32 = 86_400;

    /// Clamps `command_finished_after` into its range, with one warning.
    pub(crate) fn normalize(&mut self, warnings: &mut Vec<Warning>) {
        let after = self.command_finished_after;
        let clamped = after.clamp(
            Self::MIN_COMMAND_FINISHED_AFTER,
            Self::MAX_COMMAND_FINISHED_AFTER,
        );
        if clamped != after {
            warnings.push(Warning::OutOfRange {
                path: "notifications.command_finished_after".into(),
                detail: format!("{after} clamped to {clamped}"),
            });
            self.command_finished_after = clamped;
        }
    }
}

impl Default for Notifications {
    fn default() -> Self {
        Self {
            command_finished: CommandFinishedMode::Off,
            command_finished_after: 10,
        }
    }
}

/// `[notifications] command_finished`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CommandFinishedMode {
    /// Notify for a pane that does not have the keyboard.
    Unfocused,
    /// Never notify, never mark: the default (ADR 0048).
    #[default]
    Off,
}

crate::load::named_values!(CommandFinishedMode {
    Unfocused => "unfocused",
    Off => "off",
});

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, Error};

    fn load(text: &str) -> crate::Loaded {
        Config::from_toml_lenient(text).unwrap()
    }

    /// C01: off and 10 s by default, without a warning.
    #[test]
    fn defaults_are_off_and_ten_seconds() {
        let loaded = load("");
        assert_eq!(loaded.config.notifications, Notifications::default());
        assert_eq!(
            loaded.config.notifications.command_finished,
            CommandFinishedMode::Off
        );
        assert_eq!(loaded.config.notifications.command_finished_after, 10);
        assert_eq!(loaded.warnings, []);
    }

    /// C02-C05: both modes, and both ends of the range, without a warning.
    #[test]
    fn valid_values_load_without_a_warning() {
        for (text, mode, after) in [
            ("command_finished = \"off\"", CommandFinishedMode::Off, 10),
            (
                "command_finished = \"unfocused\"",
                CommandFinishedMode::Unfocused,
                10,
            ),
            ("command_finished_after = 1", CommandFinishedMode::Off, 1),
            (
                "command_finished_after = 86400",
                CommandFinishedMode::Off,
                86_400,
            ),
        ] {
            let loaded = load(&format!("[notifications]\n{text}\n"));
            assert_eq!(loaded.config.notifications.command_finished, mode, "{text}");
            assert_eq!(
                loaded.config.notifications.command_finished_after, after,
                "{text}"
            );
            assert!(loaded.warnings.is_empty(), "{text}: {:?}", loaded.warnings);
        }
    }

    /// C06-C08: 0 and above a day clamp, with exactly one warning.
    #[test]
    fn out_of_range_thresholds_clamp_with_one_warning() {
        for (written, clamped) in [(0u64, 1u32), (86_401, 86_400), (4_294_967_295, 86_400)] {
            let loaded = load(&format!(
                "[notifications]\ncommand_finished_after = {written}\n"
            ));
            assert_eq!(loaded.config.notifications.command_finished_after, clamped);
            assert_eq!(
                loaded.warnings,
                [Warning::OutOfRange {
                    path: "notifications.command_finished_after".into(),
                    detail: format!("{written} clamped to {clamped}"),
                }]
            );
        }
    }

    /// C09, C10: a value that is not a u32 fails the load, as for other
    /// u32 keys (`blur_radius`).
    #[test]
    fn a_threshold_that_is_not_a_u32_fails_the_load() {
        for value in ["-1", "10.5", "\"10\"", "true", "4294967296"] {
            let text = format!("[notifications]\ncommand_finished_after = {value}\n");
            assert!(
                matches!(Config::from_toml_lenient(&text), Err(Error::Parse(_))),
                "{value}"
            );
        }
    }

    /// C11-C13: an unknown or mis-cased mode warns and means off.
    #[test]
    fn an_unknown_mode_warns_and_means_off() {
        for value in ["always", "never", "Unfocused", "unfocused "] {
            let loaded = load(&format!(
                "[notifications]\ncommand_finished = \"{value}\"\n"
            ));
            assert_eq!(
                loaded.config.notifications.command_finished,
                CommandFinishedMode::Off,
                "{value}"
            );
            let [Warning::OutOfRange { path, detail }] = &loaded.warnings[..] else {
                panic!("{value}: {:?}", loaded.warnings);
            };
            assert_eq!(path, "notifications.command_finished");
            assert!(detail.contains("replaced by \"off\""), "{detail}");
        }
    }

    /// C14: a mode that is not a string fails the load.
    #[test]
    fn a_mode_that_is_not_a_string_fails_the_load() {
        for value in ["true", "1", "[]"] {
            let text = format!("[notifications]\ncommand_finished = {value}\n");
            assert!(
                matches!(Config::from_toml_lenient(&text), Err(Error::Parse(_))),
                "{value}"
            );
        }
    }

    /// C15, C16: an unknown key and a profile's `notifications` table are
    /// unknown keys; the known keys still apply.
    #[test]
    fn unknown_keys_and_profile_tables_warn() {
        let loaded = load(
            "[notifications]\ncommand_finished = \"unfocused\"\nunknown_key = 1\n\
             [[profiles]]\nname = \"p\"\n[profiles.notifications]\ncommand_finished = \"off\"\n",
        );
        assert_eq!(
            loaded.config.notifications.command_finished,
            CommandFinishedMode::Unfocused
        );
        let paths: Vec<_> = loaded
            .warnings
            .iter()
            .map(|warning| match warning {
                Warning::UnknownKey { path } => path.as_str(),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            paths,
            ["notifications.unknown_key", "profiles[0].notifications"]
        );
    }

    /// C17: a clamped value writes back in range and re-reads cleanly.
    #[test]
    fn a_clamped_value_writes_back_in_range() {
        let loaded =
            load("[notifications]\ncommand_finished = \"unfocused\"\ncommand_finished_after = 0\n");
        let text = loaded.config.to_toml().unwrap();
        assert!(text.contains("command_finished_after = 1\n"), "{text}");
        assert!(
            text.contains("command_finished = \"unfocused\"\n"),
            "{text}"
        );
        let again = load(&text);
        assert!(again.warnings.is_empty(), "{:?}", again.warnings);
        assert_eq!(again.config.notifications, loaded.config.notifications);
    }
}
