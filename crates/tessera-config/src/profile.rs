//! Profiles (`[[profiles]]`): a named command, starting directory and
//! appearance for new sessions, and how a session's profile is resolved
//! against `[appearance]` and `[general] default_profile`.
//!
//! A profile's `appearance` holds only the keys it changes: every other key
//! comes from `[appearance]`, so editing `[appearance]` restyles the
//! sessions of every profile that does not set that key ([`ProfileAppearance::over`]).

use std::collections::HashMap;
use std::collections::hash_map::Entry;

use serde::{Deserialize, Serialize};

use crate::load::{self, MAX_WARNING_PATH_LEN, Warning, escaped};
use crate::{Appearance, Config, CursorShape};

/// Most `[[profiles]]` entries read; later ones are left out with a
/// [`Warning::TooManyProfiles`] (docs/security.md S5).
pub const MAX_PROFILES: usize = 256;

/// Longest profile `name` kept, in bytes; a longer one is cut to this
/// length on a character boundary with a warning (docs/security.md S5).
pub const MAX_PROFILE_NAME_LEN: usize = 128;

/// The name of the profile used when `[general] default_profile` names no
/// `[[profiles]]` entry and nothing else was asked for: the built-in one,
/// `[appearance]` and the login shell. Not finding it is not worth a
/// warning, since it is the default value of `default_profile`.
pub const BUILT_IN_PROFILE_NAME: &str = "Default";

/// A profile's appearance: each key that is set replaces the one in
/// `[appearance]`, each key left out is taken from it, also after
/// `[appearance]` changes.
///
/// `dim_inactive_panes` and `blur_radius` are not here: the window draws
/// them, not the session, so a profile that sets one gets a warning and
/// `[appearance]`'s value.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(default)]
pub struct ProfileAppearance {
    /// Colour theme, by name, looked up as for `[appearance] theme`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// Font family. Not honoured per profile yet: the font is shared by
    /// every session, so only `[appearance] font_family` changes it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font_family: Option<String>,
    /// Font size in points, clamped on load as for `[appearance]
    /// font_size`. A value that is not a finite number (`nan`, `inf`) is
    /// ignored with a warning, so `[appearance]`'s applies.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font_size: Option<f32>,
    /// Background opacity of this profile's sessions, 0.0..=1.0. A window
    /// is translucent while any pane of its tab is, and stays opaque only
    /// while every one is; an opaque session still draws its own opaque
    /// background. While the window is translucent its title bar and tab
    /// bar are always blurred, whatever `blur` says. The system's Reduce
    /// Transparency setting draws every session opaque. A value that is not a finite number is ignored with
    /// a warning, so `[appearance]`'s applies.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opacity: Option<f32>,
    /// Background blur behind each of this profile's panes whose background
    /// is translucent (`opacity` below 1.0, or a theme background with
    /// alpha). The title bar and tab bar of a translucent window are always
    /// blurred, whatever this says.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blur: Option<bool>,
    /// The cursor's shape, `"block"`, `"underline"` or `"beam"`, as for
    /// `[appearance] cursor_shape`. Any other value is reported with a
    /// warning and `[appearance]`'s shape is used.
    #[serde(
        deserialize_with = "load::lenient_optional_cursor_shape",
        skip_serializing_if = "Option::is_none"
    )]
    #[cfg_attr(feature = "schema", schemars(schema_with = "CursorShape::schema"))]
    pub cursor_shape: Option<CursorShape>,
    /// Whether the cursor blinks, as for `[appearance] cursor_blink`. A
    /// value that is not `true` or `false` is reported with a warning and
    /// `[appearance]`'s value is used.
    #[serde(
        deserialize_with = "load::lenient_optional_bool",
        skip_serializing_if = "Option::is_none"
    )]
    pub cursor_blink: Option<bool>,
}

impl ProfileAppearance {
    /// `base` with every key this profile sets replaced:
    /// the appearance a session of this profile is drawn with.
    /// `dim_inactive_panes` and `blur_radius` are always `base`'s, and so
    /// is a `font_size` or `opacity` that is not a finite number (the
    /// loader already turns those into unset keys; #333).
    #[must_use]
    pub fn over(&self, base: &Appearance) -> Appearance {
        Appearance {
            theme: self.theme.clone().unwrap_or_else(|| base.theme.clone()),
            font_family: self
                .font_family
                .clone()
                .unwrap_or_else(|| base.font_family.clone()),
            font_size: self
                .font_size
                .filter(|size| size.is_finite())
                .unwrap_or(base.font_size),
            opacity: self
                .opacity
                .filter(|opacity| opacity.is_finite())
                .unwrap_or(base.opacity),
            blur: self.blur.unwrap_or(base.blur),
            blur_radius: base.blur_radius,
            cursor_shape: self.cursor_shape.unwrap_or(base.cursor_shape),
            cursor_blink: self.cursor_blink.unwrap_or(base.cursor_blink),
            dim_inactive_panes: base.dim_inactive_panes,
        }
    }
}

/// What a new session runs and looks like, from [`Config::resolve_profile`].
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedProfile {
    /// The `[[profiles]]` entry used, by name; `None` for the built-in
    /// profile (no entry matched).
    pub name: Option<String>,
    /// The profile's command; `None` means the login shell.
    pub command: Option<String>,
    /// The profile's starting directory as written (`~` not expanded);
    /// `None` means the caller's choice.
    pub working_directory: Option<String>,
    /// `[appearance]` with the profile's keys over it.
    pub appearance: Appearance,
    /// Set when a name asked for (or `[general] default_profile`) names no
    /// profile: a [`Warning::ProfileNotFound`].
    pub warning: Option<Warning>,
}

impl Config {
    /// The profile a new session uses: `requested` if a `[[profiles]]`
    /// entry has that name (the first one, should two share it), else the
    /// one `[general] default_profile` names, else the built-in profile
    /// (`[appearance]` and the login shell).
    ///
    /// A `requested` name that names no profile, and a `default_profile`
    /// other than [`BUILT_IN_PROFILE_NAME`] that names none, give a
    /// [`Warning::ProfileNotFound`] in [`ResolvedProfile::warning`] (the
    /// first one that was missing).
    #[must_use]
    pub fn resolve_profile(&self, requested: Option<&str>) -> ResolvedProfile {
        let find = |name: &str| self.profiles.iter().find(|profile| profile.name == name);
        let default_name = self.general.default_profile.as_str();
        let mut missing = None;
        let mut profile = None;
        if let Some(name) = requested {
            profile = find(name);
            if profile.is_none() {
                missing = Some(name);
            }
        }
        if profile.is_none() {
            profile = find(default_name);
            if profile.is_none() && missing.is_none() && default_name != BUILT_IN_PROFILE_NAME {
                missing = Some(default_name);
            }
        }
        let warning = missing.map(|name| Warning::ProfileNotFound {
            name: escaped(name, MAX_WARNING_PATH_LEN),
            instead: profile.map(|profile| escaped(&profile.name, MAX_WARNING_PATH_LEN)),
        });
        ResolvedProfile {
            name: profile.map(|profile| profile.name.clone()),
            command: profile.and_then(|profile| profile.command.clone()),
            working_directory: profile.and_then(|profile| profile.working_directory.clone()),
            appearance: self.profile_appearance(profile.map(|profile| profile.name.as_str())),
            warning,
        }
    }

    /// The appearance of the profile named `name` as
    /// [`Config::resolve_profile`] finds it, without the rest and without
    /// a warning: what a session of that profile is drawn with now.
    #[must_use]
    pub fn profile_appearance(&self, name: Option<&str>) -> Appearance {
        let find = |name: &str| self.profiles.iter().find(|profile| profile.name == name);
        let profile = name
            .and_then(find)
            .or_else(|| find(&self.general.default_profile));
        match profile.and_then(|profile| profile.appearance.as_ref()) {
            Some(overrides) => overrides.over(&self.appearance),
            None => self.appearance.clone(),
        }
    }
}

/// Caps `[[profiles]]` (docs/security.md S5): at most [`MAX_PROFILES`]
/// entries and names of at most [`MAX_PROFILE_NAME_LEN`] bytes, each cut
/// reported. Entries keep their places, so `profiles[i]` in a warning is
/// the `i`-th `[[profiles]]` of the file. A name an earlier entry has too
/// (byte for byte, after the cut) is reported: the earlier entry is the
/// one used. A NUL character in a name, which the frontends could not
/// pass as a C string, is replaced with U+FFFD and reported, before the
/// cut; the entry keeps its place (#333).
pub(crate) fn cap_profiles(config: &mut Config, warnings: &mut Vec<Warning>) {
    if config.profiles.len() > MAX_PROFILES {
        warnings.push(Warning::TooManyProfiles {
            dropped: config.profiles.len() - MAX_PROFILES,
        });
        config.profiles.truncate(MAX_PROFILES);
    }
    for (i, profile) in config.profiles.iter_mut().enumerate() {
        if profile.name.contains('\0') {
            warnings.push(Warning::OutOfRange {
                path: format!("profiles[{i}].name"),
                detail: "holds a NUL character, replaced with U+FFFD".into(),
            });
            profile.name = profile.name.replace('\0', "\u{fffd}");
        }
        if profile.name.len() > MAX_PROFILE_NAME_LEN {
            let mut end = MAX_PROFILE_NAME_LEN;
            while !profile.name.is_char_boundary(end) {
                end -= 1;
            }
            warnings.push(Warning::OutOfRange {
                path: format!("profiles[{i}].name"),
                detail: format!(
                    "{} bytes cut to {end} (at most {MAX_PROFILE_NAME_LEN})",
                    profile.name.len()
                ),
            });
            profile.name.truncate(end);
        }
    }
    let mut first = HashMap::new();
    for (i, profile) in config.profiles.iter().enumerate() {
        match first.entry(profile.name.as_str()) {
            Entry::Vacant(entry) => {
                entry.insert(i);
            }
            Entry::Occupied(entry) => warnings.push(Warning::OutOfRange {
                path: format!("profiles[{i}].name"),
                detail: format!(
                    "profiles[{}] has this name too and is the one used",
                    entry.get()
                ),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Profile;

    fn profile(name: &str, appearance: Option<ProfileAppearance>) -> Profile {
        Profile {
            name: name.into(),
            command: Some(format!("{name}-cmd")),
            working_directory: None,
            appearance,
        }
    }

    /// Done-when 1: a profile that sets only `font_size` takes the theme,
    /// opacity and cursor from `[appearance]`, also after `[appearance]`
    /// changes (it used to reset them to the built-in defaults).
    #[test]
    fn a_profile_with_only_a_font_size_inherits_the_rest_of_appearance() {
        let text = "\
            [appearance]\n\
            theme = \"Dracula\"\n\
            opacity = 0.8\n\
            cursor_shape = \"beam\"\n\
            cursor_blink = true\n\
            blur = true\n\
            font_family = \"Monaco\"\n\
            [[profiles]]\n\
            name = \"big\"\n\
            [profiles.appearance]\n\
            font_size = 20\n";
        let loaded = Config::from_toml_lenient(text).unwrap();
        assert_eq!(loaded.warnings, []);
        let resolved = loaded.config.resolve_profile(Some("big"));
        assert_eq!(resolved.name.as_deref(), Some("big"));
        assert!(resolved.warning.is_none());
        let expected = Appearance {
            font_size: 20.0,
            ..loaded.config.appearance.clone()
        };
        assert_eq!(resolved.appearance, expected);
        assert_eq!(resolved.appearance.theme, "Dracula");
        assert_eq!(resolved.appearance.cursor_shape, CursorShape::Beam);
        assert!(resolved.appearance.cursor_blink);
        // Editing `[appearance]` restyles the profile too.
        let mut config = loaded.config;
        config.appearance.theme = "Nord".into();
        config.appearance.font_size = 9.0;
        let appearance = config.profile_appearance(Some("big"));
        assert_eq!(appearance.theme, "Nord");
        assert!((appearance.font_size - 20.0).abs() < f32::EPSILON);
    }

    #[test]
    fn every_set_key_overrides_and_dimming_stays_the_windows() {
        let base = Appearance {
            dim_inactive_panes: 0.4,
            blur_radius: 20,
            ..Appearance::default()
        };
        let overrides = ProfileAppearance {
            theme: Some("Nord".into()),
            font_family: Some("Monaco".into()),
            font_size: Some(18.0),
            opacity: Some(0.5),
            blur: Some(true),
            cursor_shape: Some(CursorShape::Underline),
            cursor_blink: Some(true),
        };
        let over = overrides.over(&base);
        assert_eq!(
            over,
            Appearance {
                theme: "Nord".into(),
                font_family: "Monaco".into(),
                font_size: 18.0,
                opacity: 0.5,
                blur: true,
                blur_radius: 20,
                cursor_shape: CursorShape::Underline,
                cursor_blink: true,
                dim_inactive_panes: 0.4,
            }
        );
        assert_eq!(ProfileAppearance::default().over(&base), base);
    }

    #[test]
    fn resolution_falls_back_to_default_profile_then_built_in() {
        let mut config = Config {
            profiles: vec![
                profile("a", None),
                profile(
                    "dev",
                    Some(ProfileAppearance {
                        theme: Some("Nord".into()),
                        ..ProfileAppearance::default()
                    }),
                ),
            ],
            ..Config::default()
        };
        // No profile named "Default": the built-in one, silently.
        let built_in = config.resolve_profile(None);
        assert_eq!(built_in.name, None);
        assert_eq!(built_in.command, None);
        assert_eq!(built_in.appearance, config.appearance);
        assert_eq!(built_in.warning, None);
        // An unknown name falls back to it with a warning.
        let unknown = config.resolve_profile(Some("nope"));
        assert_eq!(unknown.name, None);
        assert_eq!(
            unknown.warning,
            Some(Warning::ProfileNotFound {
                name: "nope".into(),
                instead: None
            })
        );
        // With a default profile: None and unknown names use it.
        config.general.default_profile = "dev".into();
        assert_eq!(config.resolve_profile(None).name.as_deref(), Some("dev"));
        assert_eq!(config.resolve_profile(None).appearance.theme, "Nord");
        let unknown = config.resolve_profile(Some("nope"));
        assert_eq!(unknown.name.as_deref(), Some("dev"));
        assert_eq!(unknown.command.as_deref(), Some("dev-cmd"));
        assert!(matches!(
            unknown.warning,
            Some(Warning::ProfileNotFound { ref instead, .. }) if instead.as_deref() == Some("dev")
        ));
        assert_eq!(config.resolve_profile(Some("a")).name.as_deref(), Some("a"));
        // A default profile that names nothing warns too.
        config.general.default_profile = "gone".into();
        let resolved = config.resolve_profile(None);
        assert_eq!(resolved.name, None);
        assert!(matches!(
            resolved.warning,
            Some(Warning::ProfileNotFound { ref name, .. }) if name == "gone"
        ));
    }

    #[test]
    fn the_first_of_two_profiles_with_one_name_wins() {
        let mut second = profile("x", None);
        second.command = Some("second".into());
        let config = Config {
            profiles: vec![profile("x", None), second],
            ..Config::default()
        };
        assert_eq!(
            config.resolve_profile(Some("x")).command.as_deref(),
            Some("x-cmd")
        );
    }

    /// A name in a warning is escaped and capped (S8): it comes from the
    /// app (a menu item) or from the file.
    #[test]
    fn a_missing_name_is_escaped_in_its_warning() {
        let name = format!("\u{1b}[2J{}", "n".repeat(1000));
        let warning = Config::default()
            .resolve_profile(Some(&name))
            .warning
            .unwrap();
        let Warning::ProfileNotFound { name, .. } = &warning else {
            panic!("{warning:?}");
        };
        assert!(!name.contains('\u{1b}'));
        assert!(name.len() <= MAX_WARNING_PATH_LEN + 8, "{}", name.len());
        assert!(!warning.to_string().contains('\u{1b}'));
    }

    /// S5: a flood of profiles is cut at `MAX_PROFILES` with one warning,
    /// and long names are cut to `MAX_PROFILE_NAME_LEN` bytes.
    #[test]
    fn profiles_past_the_cap_and_long_names_are_cut_with_warnings() {
        use std::fmt::Write as _;

        let mut text = String::new();
        for i in 0..MAX_PROFILES + 44 {
            let _ = write!(
                text,
                "[[profiles]]\nname = \"p{i}\"\n[profiles.appearance]\ncursor_shape = \"bad\"\n\
                 dim_inactive_panes = true\n"
            );
        }
        let loaded = Config::from_toml_lenient(&text).unwrap();
        assert_eq!(loaded.config.profiles.len(), MAX_PROFILES);
        let too_many: Vec<_> = loaded
            .warnings
            .iter()
            .filter(|w| matches!(w, Warning::TooManyProfiles { dropped: 44 }))
            .collect();
        assert_eq!(too_many.len(), 1, "{:?}", loaded.warnings.last());
        // The lenient-value check stops at the cap too.
        let shapes = loaded
            .warnings
            .iter()
            .filter(
                |w| matches!(w, Warning::OutOfRange { path, .. } if path.ends_with("cursor_shape")),
            )
            .count();
        assert_eq!(shapes, MAX_PROFILES);
        // So does the per-profile `dim_inactive_panes` warning.
        let dims = loaded
            .warnings
            .iter()
            .filter(
                |w| matches!(w, Warning::OutOfRange { path, .. } if path.ends_with("dim_inactive_panes")),
            )
            .count();
        assert_eq!(dims, MAX_PROFILES);

        // 'é' is two bytes: a cut never splits it.
        let long = format!("a{}", "é".repeat(100));
        let text = format!("[[profiles]]\nname = \"{long}\"\n");
        let loaded = Config::from_toml_lenient(&text).unwrap();
        let name = &loaded.config.profiles[0].name;
        assert_eq!(name.len(), MAX_PROFILE_NAME_LEN - 1);
        assert!(long.starts_with(name.as_str()));
        assert!(
            matches!(&loaded.warnings[..], [Warning::OutOfRange { path, .. }] if path == "profiles[0].name"),
            "{:?}",
            loaded.warnings
        );
        let exact = "n".repeat(MAX_PROFILE_NAME_LEN);
        let loaded =
            Config::from_toml_lenient(&format!("[[profiles]]\nname = \"{exact}\"\n")).unwrap();
        assert_eq!(loaded.config.profiles[0].name, exact);
        assert_eq!(loaded.warnings, []);
    }

    /// A name an earlier profile has too warns; the earlier one is used.
    /// Names that differ only in their bytes are two profiles.
    #[test]
    fn a_duplicate_profile_name_warns_and_the_first_is_used() {
        let text = "[[profiles]]\nname = \"zsh\"\ncommand = \"zsh\"\n\
                    [[profiles]]\nname = \"caf\u{e9}\"\n\
                    [[profiles]]\nname = \"cafe\u{301}\"\n\
                    [[profiles]]\nname = \"zsh\"\ncommand = \"fish\"\n";
        let loaded = Config::from_toml_lenient(text).unwrap();
        assert!(
            matches!(
                &loaded.warnings[..],
                [Warning::OutOfRange { path, detail }]
                    if path == "profiles[3].name" && detail.contains("profiles[0]")
            ),
            "{:?}",
            loaded.warnings
        );
        let resolved = loaded.config.resolve_profile(Some("zsh"));
        assert_eq!(resolved.command.as_deref(), Some("zsh"));
    }

    /// A profile's `font_size = nan` and `opacity = inf` are warned about
    /// and unset, so the session takes `[appearance]`'s (#333 M-c); a
    /// finite value out of range is still clamped.
    #[test]
    fn a_non_finite_profile_size_or_opacity_inherits_appearance() {
        let text = "[appearance]\nfont_size = 15.0\nopacity = 0.8\n\
                    [[profiles]]\nname = \"p\"\nappearance.font_size = nan\nappearance.opacity = inf\n\
                    [[profiles]]\nname = \"q\"\nappearance.font_size = -inf\nappearance.opacity = 2.0\n";
        let loaded = Config::from_toml_lenient(text).unwrap();
        let paths: Vec<&str> = loaded
            .warnings
            .iter()
            .map(|w| match w {
                Warning::OutOfRange { path, .. } => path.as_str(),
                _ => "",
            })
            .collect();
        assert_eq!(
            paths,
            [
                "profiles[0].appearance.font_size",
                "profiles[0].appearance.opacity",
                "profiles[1].appearance.font_size",
                "profiles[1].appearance.opacity",
            ]
        );
        let p = loaded.config.resolve_profile(Some("p")).appearance;
        assert!((p.font_size - 15.0).abs() < f32::EPSILON, "{p:?}");
        assert!((p.opacity - 0.8).abs() < f32::EPSILON, "{p:?}");
        let q = loaded.config.resolve_profile(Some("q")).appearance;
        assert!((q.font_size - 15.0).abs() < f32::EPSILON, "{q:?}");
        assert!((q.opacity - 1.0).abs() < f32::EPSILON, "{q:?}");
        // A configuration built in code, never loaded: the same.
        let unchecked = ProfileAppearance {
            font_size: Some(f32::NAN),
            opacity: Some(f32::INFINITY),
            ..ProfileAppearance::default()
        };
        let base = Appearance::default();
        let drawn = unchecked.over(&base);
        assert_eq!(drawn.font_size.to_bits(), base.font_size.to_bits());
        assert_eq!(drawn.opacity.to_bits(), base.opacity.to_bits());
    }

    /// A NUL in a profile name is replaced with U+FFFD, with a load
    /// warning, and the profile keeps its place (#333): `profiles[i]` is
    /// still the file's `i`-th entry.
    #[test]
    fn a_nul_in_a_profile_name_is_replaced_and_the_profile_kept() {
        let text = "[[profiles]]\nname = \"a\\u0000b\"\ncommand = \"evil\"\n\
                    [[profiles]]\nname = \"ok\"\nappearance.opacity = 7.0\n\
                    [[profiles]]\nname = \"a\\u0000b\"\n";
        let loaded = Config::from_toml_lenient(text).unwrap();
        let names: Vec<&str> = loaded
            .config
            .profiles
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(names, ["a\u{fffd}b", "ok", "a\u{fffd}b"]);
        let warned: Vec<(&str, &str)> = loaded
            .warnings
            .iter()
            .map(|w| match w {
                Warning::OutOfRange { path, detail } => (path.as_str(), detail.as_str()),
                _ => ("", ""),
            })
            .collect();
        let at = |path: &str| -> Vec<&str> {
            warned
                .iter()
                .filter(|(p, _)| *p == path)
                .map(|(_, d)| *d)
                .collect()
        };
        assert_eq!(warned.len(), 4, "{warned:?}");
        assert!(at("profiles[0].name")[0].contains("NUL"), "{warned:?}");
        assert_eq!(at("profiles[1].appearance.opacity").len(), 1, "{warned:?}");
        let third = at("profiles[2].name");
        assert_eq!(third.len(), 2, "NUL and the duplicate: {warned:?}");
        assert!(third[0].contains("NUL"), "{warned:?}");
        assert!(third[1].contains("profiles[0]"), "{warned:?}");
        assert!(warned.iter().all(|(_, d)| !d.contains('\0')));
        let resolved = loaded.config.resolve_profile(Some("a\u{fffd}b"));
        assert_eq!(resolved.name.as_deref(), Some("a\u{fffd}b"));
        assert_eq!(resolved.command.as_deref(), Some("evil"));
        assert_eq!(loaded.config.resolve_profile(Some("a\0b")).name, None);
        // Replaced before the cut: the name stays within its bytes.
        let long = format!("[[profiles]]\nname = \"{}\\u0000\"\n", "n".repeat(127));
        let loaded = Config::from_toml_lenient(&long).unwrap();
        let name = &loaded.config.profiles[0].name;
        assert!(name.len() <= MAX_PROFILE_NAME_LEN, "{}", name.len());
        assert!(!name.contains('\0'));
    }

    /// Profile keys left out stay out when the configuration is written
    /// back, so they keep inheriting.
    #[test]
    fn unset_profile_keys_are_not_serialised() {
        let config = Config {
            profiles: vec![Profile {
                appearance: Some(ProfileAppearance {
                    font_size: Some(20.0),
                    ..ProfileAppearance::default()
                }),
                ..Profile::default()
            }],
            ..Config::default()
        };
        let text = config.to_toml().unwrap();
        let back = Config::from_toml_lenient(&text).unwrap();
        assert_eq!(back.warnings, []);
        assert_eq!(back.config, config);
        let table: toml::Table = toml::from_str(&text).unwrap();
        let appearance = table["profiles"].as_array().unwrap()[0]["appearance"]
            .as_table()
            .unwrap();
        assert_eq!(appearance.keys().collect::<Vec<_>>(), ["font_size"]);
    }
}
