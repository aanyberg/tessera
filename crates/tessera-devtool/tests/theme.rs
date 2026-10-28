//! End-to-end tests of `tessera-devtool theme import`, the fixture's
//! `[[devtool_case]]` rows (section 8 of
//! `crates/tessera-config/tests/fixtures/theme-spec-cases.toml`).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tessera_config::{Color, Theme};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../tessera-config/tests/fixtures")
        .join(name)
}

fn import(args: &[&std::ffi::OsStr]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tessera-devtool"))
        .args(["theme", "import"])
        .args(args)
        .output()
        .expect("the devtool binary runs")
}

fn import_into(file: &Path, out: &Path, extra: &[&str]) -> Output {
    let mut args: Vec<&std::ffi::OsStr> = vec![file.as_os_str(), "--out".as_ref(), out.as_os_str()];
    args.extend(extra.iter().map(std::ffi::OsStr::new));
    import(&args)
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn iterm_happy_path() {
    let out = tempfile::tempdir().unwrap();
    let output = import_into(&fixture("dracula.itermcolors"), out.path(), &[]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let path = out.path().join("dracula.toml");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("{}\n", path.display()),
        "stdout is the path and nothing else"
    );
    let theme = Theme::load(&path).unwrap();
    assert_eq!(theme.name, "dracula", "named after the file stem");
    let mut bundled = Theme::bundled("Dracula").unwrap().clone();
    bundled.name = "dracula".into();
    assert_eq!(theme, bundled, "the section 2 dracula row");
}

#[test]
fn base16_happy_path() {
    let out = tempfile::tempdir().unwrap();
    let output = import_into(&fixture("base16-nord-legacy.yaml"), out.path(), &[]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let theme = Theme::load(&out.path().join("nord.toml")).unwrap();
    assert_eq!(theme.name, "Nord", "from `scheme:`");
    assert_eq!(theme.background, Color::rgb(0x2e, 0x34, 0x40));
    assert_eq!(theme.selection, Color::rgb(0x43, 0x4c, 0x5e));
    assert_eq!(theme.ansi[15], Color::rgb(0x8f, 0xbc, 0xbb));
    // The other two fixtures import as well.
    for (file, slug) in [
        ("base16-dracula-nested.yaml", "dracula"),
        ("base16-solarized-dark-legacy.yaml", "solarized-dark"),
        ("nord.itermcolors", "nord-1"),
    ] {
        let extra: &[&str] = if slug == "nord-1" {
            &["--name", "Nord 1"]
        } else {
            &[]
        };
        let output = import_into(&fixture(file), out.path(), extra);
        assert_eq!(output.status.code(), Some(0), "{file}: {}", stderr(&output));
        Theme::load(&out.path().join(format!("{slug}.toml"))).unwrap();
    }
}

#[test]
fn name_override() {
    let out = tempfile::tempdir().unwrap();
    let output = import_into(
        &fixture("dracula.itermcolors"),
        out.path(),
        &["--name", "My Dracula"],
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = std::fs::read_to_string(out.path().join("my-dracula.toml")).unwrap();
    assert!(text.contains("name = \"My Dracula\""), "{text}");
    assert_eq!(entries(out.path()), ["my-dracula.toml"]);
}

#[test]
fn refuse_then_force_overwrite() {
    let out = tempfile::tempdir().unwrap();
    let file = fixture("dracula.itermcolors");
    assert_eq!(import_into(&file, out.path(), &[]).status.code(), Some(0));
    let path = out.path().join("dracula.toml");
    // Make the existing file recognisable.
    std::fs::write(&path, "sentinel").unwrap();

    let second = import_into(&file, out.path(), &[]);
    assert_eq!(second.status.code(), Some(125));
    assert!(stderr(&second).contains("--force"), "{}", stderr(&second));
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "sentinel",
        "untouched"
    );
    assert_eq!(second.stdout, [] as [u8; 0]);

    let forced = import_into(&file, out.path(), &["--force"]);
    assert_eq!(forced.status.code(), Some(0), "{}", stderr(&forced));
    Theme::load(&path).unwrap();
    assert_eq!(
        entries(out.path()),
        ["dracula.toml"],
        "no temporary file left"
    );
}

#[test]
fn failures_exit_125_and_write_nothing() {
    let out = tempfile::tempdir().unwrap();
    let inputs = tempfile::tempdir().unwrap();

    // unknown-extension
    let txt = inputs.path().join("theme.txt");
    std::fs::write(&txt, "x").unwrap();
    let output = import_into(&txt, out.path(), &[]);
    assert_eq!(output.status.code(), Some(125));
    assert!(
        stderr(&output).contains("unrecognised theme format"),
        "{}",
        stderr(&output)
    );

    // missing-input
    let missing = inputs.path().join("nonexistent.itermcolors");
    let output = import_into(&missing, out.path(), &[]);
    assert_eq!(output.status.code(), Some(125));
    assert!(
        stderr(&output).contains("nonexistent.itermcolors"),
        "{}",
        stderr(&output)
    );

    // garbage-input
    let garbage = inputs.path().join("garbage.itermcolors");
    std::fs::write(&garbage, "hello").unwrap();
    let output = import_into(&garbage, out.path(), &[]);
    assert_eq!(output.status.code(), Some(125));
    assert!(
        stderr(&output).contains("property list"),
        "{}",
        stderr(&output)
    );

    // A name with nothing to slug.
    let output = import_into(
        &fixture("dracula.itermcolors"),
        out.path(),
        &["--name", "!!!"],
    );
    assert_eq!(output.status.code(), Some(125));
    assert!(stderr(&output).contains("--name"), "{}", stderr(&output));

    // A missing output directory is not created.
    let nowhere = out.path().join("missing");
    let output = import_into(&fixture("dracula.itermcolors"), &nowhere, &[]);
    assert_eq!(output.status.code(), Some(125));
    assert!(!nowhere.exists());

    // A directory and a FIFO as input are refused without blocking.
    let output = import_into(&inputs.path().join("d.yaml"), out.path(), &[]);
    assert_eq!(output.status.code(), Some(125));
    std::fs::create_dir(inputs.path().join("dir.yaml")).unwrap();
    let output = import_into(&inputs.path().join("dir.yaml"), out.path(), &[]);
    assert_eq!(output.status.code(), Some(125));
    let fifo = inputs.path().join("fifo.yaml");
    assert!(
        Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    let output = import_into(&fifo, out.path(), &[]);
    assert_eq!(output.status.code(), Some(125));

    // An oversized input is refused after reading only past the limit.
    let huge = inputs.path().join("huge.yaml");
    std::fs::File::create(&huge)
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let output = import_into(&huge, out.path(), &[]);
    assert_eq!(output.status.code(), Some(125));
    assert!(stderr(&output).contains("limit"), "{}", stderr(&output));

    assert!(entries(out.path()).is_empty(), "{:?}", entries(out.path()));
}

#[test]
fn long_names_are_clipped_so_the_file_stays_loadable() {
    use tessera_config::{MAX_THEME_FILE_LEN, MAX_THEME_NAME_LEN};

    let out = tempfile::tempdir().unwrap();
    // Control characters are escaped six bytes each in TOML; a long name
    // of them must still give a small, loadable file.
    let name = format!("a{}", "\u{1}".repeat(20_000));
    let output = import_into(
        &fixture("dracula.itermcolors"),
        out.path(),
        &["--name", &name],
    );
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let path = out.path().join("a.toml");
    let len = std::fs::metadata(&path).unwrap().len();
    assert!(len < MAX_THEME_FILE_LEN / 4, "{len} bytes");
    let theme = Theme::load(&path).unwrap();
    assert!(theme.name.len() <= MAX_THEME_NAME_LEN);

    // A base16 scheme with a huge `name:`.
    let inputs = tempfile::tempdir().unwrap();
    let scheme = std::fs::read_to_string(fixture("base16-nord-legacy.yaml"))
        .unwrap()
        .replace(
            "scheme: \"Nord\"",
            &format!("scheme: \"{}\"", "\u{7f}n".repeat(200_000)),
        );
    let file = inputs.path().join("long.yaml");
    std::fs::write(&file, scheme).unwrap();
    let output = import_into(&file, out.path(), &[]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let written = String::from_utf8(output.stdout).unwrap();
    let path = PathBuf::from(written.trim_end());
    assert!(std::fs::metadata(&path).unwrap().len() < MAX_THEME_FILE_LEN / 4);
    assert!(Theme::load(&path).unwrap().name.len() <= MAX_THEME_NAME_LEN);
}

#[test]
fn concurrent_imports_without_force_never_overwrite() {
    use tessera_devtool::theme::{ImportOptions, ThemeImportError, import};

    let out = tempfile::tempdir().unwrap();
    let results: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let out = out.path().to_path_buf();
                scope.spawn(move || {
                    import(&ImportOptions {
                        input: fixture("dracula.itermcolors"),
                        name: Some(format!("Dracula{}", " ".repeat(i))),
                        out: Some(out),
                        force: false,
                    })
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let ok = results.iter().filter(|r| r.is_ok()).count();
    let exists = results
        .iter()
        .filter(|r| matches!(r, Err(ThemeImportError::Exists(_))))
        .count();
    assert_eq!((ok, exists), (1, 7), "{results:?}");
    assert_eq!(
        entries(out.path()),
        ["dracula.toml"],
        "no temporary file left"
    );

    // A dangling symlink in the way counts as a file.
    let link = out.path().join("nord.toml");
    std::os::unix::fs::symlink(out.path().join("missing"), &link).unwrap();
    let result = import(&ImportOptions {
        input: fixture("nord.itermcolors"),
        name: None,
        out: Some(out.path().to_path_buf()),
        force: false,
    });
    assert!(
        matches!(result, Err(ThemeImportError::Exists(_))),
        "{result:?}"
    );
    assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
}

/// Removes the disk image and detaches its volume, even when the test
/// fails.
#[cfg(target_os = "macos")]
struct FatVolume {
    image: PathBuf,
    mount: PathBuf,
}

#[cfg(target_os = "macos")]
impl Drop for FatVolume {
    fn drop(&mut self) {
        let _ = Command::new("hdiutil")
            .args(["detach", "-force"])
            .arg(&self.mount)
            .output();
        let _ = std::fs::remove_file(&self.image);
    }
}

/// A non-force import onto a real FAT32 volume, where `link(2)` fails
/// with `ENOTSUP`. Needs `hdiutil` (no privileges) and takes a few
/// seconds, so it is not part of the gate: run it with
/// `cargo nextest run -p tessera-devtool --run-ignored only fat32`.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "creates and mounts a disk image with hdiutil"]
fn import_onto_a_fat32_volume() {
    let scratch = tempfile::tempdir().unwrap();
    let image = scratch.path().join("fat.dmg");
    let mount = scratch.path().join("mnt");
    std::fs::create_dir(&mount).unwrap();
    let created = Command::new("hdiutil")
        .args([
            "create",
            "-size",
            "40m",
            "-fs",
            "MS-DOS FAT32",
            "-volname",
            "T41",
            "-o",
        ])
        .arg(&image)
        .output()
        .unwrap();
    assert!(created.status.success(), "{}", stderr(&created));
    let attached = Command::new("hdiutil")
        .args(["attach", "-nobrowse", "-mountpoint"])
        .arg(&mount)
        .arg(&image)
        .output()
        .unwrap();
    assert!(attached.status.success(), "{}", stderr(&attached));
    let volume = FatVolume {
        image: image.clone(),
        mount: mount.clone(),
    };
    // The volume really has no hard links.
    std::fs::write(mount.join("a"), "a").unwrap();
    let link = std::fs::hard_link(mount.join("a"), mount.join("b")).unwrap_err();
    eprintln!(
        "link(2) on FAT32: {link} ({:?}, errno {:?})",
        link.kind(),
        link.raw_os_error()
    );
    std::fs::remove_file(mount.join("a")).unwrap();

    let output = import_into(&fixture("dracula.itermcolors"), &mount, &[]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let theme = Theme::load(&mount.join("dracula.toml")).unwrap();
    assert_eq!(theme.background, Color::rgb(0x28, 0x2a, 0x36));
    let again = import_into(&fixture("dracula.itermcolors"), &mount, &[]);
    assert_eq!(again.status.code(), Some(125));
    assert!(stderr(&again).contains("--force"), "{}", stderr(&again));
    let forced = import_into(&fixture("dracula.itermcolors"), &mount, &["--force"]);
    assert_eq!(forced.status.code(), Some(0), "{}", stderr(&forced));
    let mut left: Vec<String> = entries(&mount)
        .into_iter()
        .filter(|n| {
            !n.starts_with("._")
                && !n.starts_with(".fseventsd")
                && n != ".Trashes"
                && n != ".Spotlight-V100"
        })
        .collect();
    left.sort();
    assert_eq!(left, ["dracula.toml"], "no temporary file left");
    drop(volume);
}

#[test]
fn unicode_and_plus_names_get_their_own_files() {
    let out = tempfile::tempdir().unwrap();
    for (name, file) in [
        ("Rosé Pine", "rosé-pine.toml"),
        ("日本語", "日本語.toml"),
        ("Dracula+", "dracula-plus.toml"),
        ("Dracula", "dracula.toml"),
    ] {
        let output = import_into(
            &fixture("dracula.itermcolors"),
            out.path(),
            &["--name", name],
        );
        assert_eq!(output.status.code(), Some(0), "{name}: {}", stderr(&output));
        assert_eq!(Theme::load(&out.path().join(file)).unwrap().name, name);
    }
    assert_eq!(entries(out.path()).len(), 4);
}
