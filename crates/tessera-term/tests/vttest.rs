//! vttest screens drivable with CSI/ESC support and the terminal modes.
//!
//! Part 2 of `tests/fixtures/csi_spec.txt` holds the byte streams vttest
//! 2.7 (20251205) sends for each screen, captured under a pty at 80x24,
//! with the expected screen text. In the screens added before the modes
//! existed, `ESC[?7h/l` and `ESC[4h/l` are translated into the harness's
//! `@set` steps; the screens lifted by #8 are fed as captured.
//!
//! In scope: menu 1 screens 1, 3, 5 and 6; menu 2 screens 1, 2, 4, 6 to
//! 12 and 14; menu 8 screens 1 to 5 and 7 (3 and 4 on the adapted screen
//! 2 of #6, with `ESC[?6h ESC[1;1H` written as `ESC[2;1H`). DECCOLM is
//! ignored without DECSET 40 (xterm's default), so the 132-column screens
//! stay listed as `#### DEFERRED` blocks in the fixture with the screens
//! that need SGR (#7), character sets (#112) or double-width lines;
//! [`deferred_screens_are_listed`] keeps that list honest.

#[allow(dead_code)] // Shared harness: not every helper is used here.
mod common;
#[allow(dead_code)] // Shared with tests/csi.rs.
mod spec;

use std::path::{Path, PathBuf};

use common::{Chunking, Script};

fn scripts_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scripts/vttest")
}

/// Script stem for a vttest fixture id: `V8S2_...` is `menu8_screen2`.
fn script_name(id: &str) -> String {
    let (menu, screen) = spec::vttest_screen(id).expect("a vttest case id");
    format!("menu{menu}_screen{screen}")
}

/// Runs `tests/scripts/vttest/<name>.txt` whole and byte by byte, requires
/// both to agree and snapshots the result as `vttest__<name>.snap`.
fn snapshot_script(name: &str) {
    let path = scripts_dir().join(format!("{name}.txt"));
    let script = Script::load(&path);
    let whole = script.run_rendered(Chunking::Whole);
    let bytewise = script.run_rendered(Chunking::Bytewise);
    assert_eq!(
        whole, bytewise,
        "{name}: feeding byte by byte changed the result"
    );
    insta::with_settings!({ input_file => &path, omit_expression => true }, {
        insta::assert_snapshot!(name, whole);
    });
}

macro_rules! script_tests {
    ($($name:ident),* $(,)?) => {
        /// Every script with a snapshot test, by file stem.
        const SCRIPTS: &[&str] = &[$(stringify!($name)),*];
        $(
            #[test]
            fn $name() {
                snapshot_script(stringify!($name));
            }
        )*
    };
}

script_tests! {
    menu1_screen1,
    menu1_screen3,
    menu1_screen5,
    menu1_screen6,
    menu2_screen1,
    menu2_screen2,
    menu2_screen4,
    menu2_screen6,
    menu2_screen7,
    menu2_screen8,
    menu2_screen9,
    menu2_screen10,
    menu2_screen11,
    menu2_screen12,
    menu2_screen14,
    menu8_screen1,
    menu8_screen2,
    menu8_screen3,
    menu8_screen4,
    menu8_screen5,
    menu8_screen7,
}

#[test]
fn every_script_has_a_test() {
    let mut on_disk: Vec<String> = std::fs::read_dir(scripts_dir())
        .expect("tests/scripts/vttest exists")
        .map(|entry| entry.expect("readable directory entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "txt"))
        .map(|path| {
            path.file_stem()
                .expect("script has a file name")
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    on_disk.sort();
    let mut listed: Vec<&str> = SCRIPTS.to_vec();
    listed.sort_unstable();
    assert_eq!(on_disk, listed, "add new scripts to `script_tests!`");
}

#[test]
fn fixture_screens_hold() {
    let checked = spec::check_all(|case| spec::is_vttest(&case.id));
    assert_eq!(checked, SCRIPTS.len(), "vttest case count");
}

#[test]
fn every_fixture_screen_has_a_script() {
    let cases = spec::parse_fixture(spec::FIXTURE);
    let mut names = Vec::new();
    for case in cases.iter().filter(|case| spec::is_vttest(&case.id)) {
        let name = script_name(&case.id);
        let path = scripts_dir().join(format!("{name}.txt"));
        assert_eq!(
            Script::load(&path),
            case.script,
            "{name}.txt does not match fixture case {}",
            case.id
        );
        names.push(name);
    }
    names.sort();
    let mut listed: Vec<&str> = SCRIPTS.to_vec();
    listed.sort_unstable();
    assert_eq!(names, listed, "one fixture case per snapshot script");
}

/// The in-scope screens (lead decision on #6) and the deferred ones
/// together cover every screen of menus 1, 2 and 8 at 80 columns, and no
/// screen is both.
#[test]
fn deferred_screens_are_listed() {
    let deferred: Vec<&str> = spec::FIXTURE
        .lines()
        .filter_map(|line| line.strip_prefix("#### DEFERRED "))
        .filter_map(|rest| rest.split_whitespace().next())
        .collect();
    let deferred_screen = |menu: u32, screen: u32| {
        deferred
            .iter()
            .any(|id| spec::vttest_screen(id) == Some((menu, screen)))
    };
    for (menu, screens) in [(1, 1..=6), (2, 1..=15), (8, 1..=7)] {
        for screen in screens {
            let name = format!("menu{menu}_screen{screen}");
            let in_scope = SCRIPTS.contains(&name.as_str());
            assert!(
                in_scope || deferred_screen(menu, screen),
                "{name} is neither tested nor listed as deferred"
            );
            assert!(
                !(in_scope && deferred_screen(menu, screen)),
                "{name} is tested and deferred"
            );
        }
    }
}
