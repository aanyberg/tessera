//! The embedded scripts: their version, and `materialise` on a temporary
//! directory.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Barrier};
use std::time::{Duration, SystemTime};

use tessera_shell_integration::scripts::{
    SCRIPTS, SCRIPTS_VERSION, digest, is_complete, materialise, version_dir,
};

/// The version and digest of the scripts as last released. A script change
/// must bump `SCRIPTS_VERSION` and record the new digest here.
const RECORDED: (u32, u64) = (3, 0xfc22_2169_1d5d_5246);

#[test]
fn scripts_version_matches_the_scripts() {
    let (version, recorded) = RECORDED;
    assert!(
        digest() == recorded || SCRIPTS_VERSION != version,
        "the embedded scripts changed: bump SCRIPTS_VERSION (now {SCRIPTS_VERSION}) and record \
         ({}, {:#018x}) in RECORDED",
        SCRIPTS_VERSION + 1,
        digest()
    );
    assert_eq!(
        (SCRIPTS_VERSION, digest()),
        RECORDED,
        "SCRIPTS_VERSION was bumped: record ({SCRIPTS_VERSION}, {:#018x}) in RECORDED",
        digest()
    );
}

#[test]
fn every_shell_has_its_scripts() {
    let paths: Vec<_> = SCRIPTS.iter().map(|script| script.path).collect();
    assert_eq!(
        paths,
        [
            "zsh/.zshenv",
            "zsh/tessera-integration",
            "bash/tessera.bash",
            "fish/vendor_conf.d/tessera-shell-integration.fish",
        ]
    );
    for script in SCRIPTS {
        assert!(
            script.contents.contains("ShellIntegrationVersion=1;shell=")
                || script.path == "zsh/.zshenv",
            "{} sends no handshake",
            script.path
        );
    }
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn writes_every_script_with_private_dirs_and_readable_files() {
    let root = tempfile::tempdir().unwrap();
    let dir = root
        .path()
        .join("Application Support/Tessera/shell-integration");
    let written = materialise(&dir).unwrap();
    assert_eq!(written, dir.join(SCRIPTS_VERSION.to_string()));
    assert_eq!(written, version_dir(&dir));
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(&written), 0o700);
    for script in SCRIPTS {
        let path = written.join(script.path);
        assert_eq!(fs::read_to_string(&path).unwrap(), script.contents);
        assert_eq!(mode(&path), 0o644, "{}", script.path);
        assert_eq!(mode(path.parent().unwrap()), 0o700, "{}", script.path);
    }
    // Nothing else is left behind: no temporary directory.
    let names: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, [SCRIPTS_VERSION.to_string()]);
}

#[test]
fn an_existing_version_dir_is_kept_as_is() {
    let root = tempfile::tempdir().unwrap();
    let written = materialise(root.path()).unwrap();
    let edited = written.join("bash/tessera.bash");
    fs::write(&edited, "# edited\n").unwrap();
    assert_eq!(materialise(root.path()).unwrap(), written);
    assert_eq!(fs::read_to_string(&edited).unwrap(), "# edited\n");
}

/// Sets `path`'s modification time `age` into the past.
fn age(path: &Path, age: Duration) {
    let file = fs::File::open(path).unwrap();
    file.set_modified(SystemTime::now() - age).unwrap();
}

#[test]
fn writing_prunes_older_versions_and_stale_temporaries_only() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path();
    let older = dir.join((SCRIPTS_VERSION - 1).to_string());
    let newer = dir.join((SCRIPTS_VERSION + 1).to_string());
    let stale = dir.join(".tmp-1-1-1-1");
    let fresh = dir.join(".tmp-1-2-2-2");
    let other = dir.join("notes");
    for path in [&older, &newer, &stale, &fresh, &other] {
        fs::create_dir(path).unwrap();
        fs::write(path.join("f"), "").unwrap();
    }
    // A symlink named like an old version is not followed or removed.
    let target = tempfile::tempdir().unwrap();
    fs::write(target.path().join("keep"), "").unwrap();
    std::os::unix::fs::symlink(target.path(), dir.join("00")).unwrap();
    age(&stale, Duration::from_hours(2));
    age(&fresh, Duration::from_secs(60));

    materialise(dir).unwrap();
    assert!(!older.exists(), "older version kept");
    assert!(!stale.exists(), "stale temporary kept");
    assert!(newer.exists(), "a newer version belongs to a newer Tessera");
    assert!(
        fresh.exists(),
        "a fresh temporary may belong to a running write"
    );
    assert!(other.exists());
    assert!(dir.join("00").symlink_metadata().is_ok());
    assert!(target.path().join("keep").exists());
}

#[test]
fn concurrent_writers_agree_on_one_complete_dir() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("si");
    let threads = 8;
    let barrier = Arc::new(Barrier::new(threads));
    let handles: Vec<_> = (0..threads)
        .map(|_| {
            let dir = dir.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                materialise(&dir)
            })
        })
        .collect();
    for handle in handles {
        assert_eq!(handle.join().unwrap().unwrap(), version_dir(&dir));
    }
    for script in SCRIPTS {
        let path = version_dir(&dir).join(script.path);
        assert_eq!(fs::read_to_string(path).unwrap(), script.contents);
    }
    let names: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, [SCRIPTS_VERSION.to_string()], "temporaries left");
}

#[test]
fn a_file_in_the_way_is_an_error_and_leaves_no_temporary() {
    let root = tempfile::tempdir().unwrap();
    fs::write(version_dir(root.path()), "not a directory").unwrap();
    assert!(materialise(root.path()).is_err());
    let names: Vec<_> = fs::read_dir(root.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, [SCRIPTS_VERSION.to_string()]);
}

#[test]
fn an_unwritable_dir_is_an_error() {
    let root = tempfile::tempdir().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o500)).unwrap();
    let result = materialise(&root.path().join("si"));
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    assert!(result.is_err());
}

#[test]
fn a_version_dir_missing_a_script_is_written_again() {
    let root = tempfile::tempdir().unwrap();
    let written = materialise(root.path()).unwrap();
    assert!(is_complete(&written));
    let edited = written.join("zsh/tessera-integration");
    fs::write(&edited, "# edited\n").unwrap();
    fs::remove_file(written.join("zsh/.zshenv")).unwrap();
    assert!(!is_complete(&written));

    assert_eq!(materialise(root.path()).unwrap(), written);
    assert!(is_complete(&written));
    // A fresh copy: every file as embedded, the edit gone too.
    for script in SCRIPTS {
        assert_eq!(
            fs::read_to_string(written.join(script.path)).unwrap(),
            script.contents
        );
    }
    let names: Vec<_> = fs::read_dir(root.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, [SCRIPTS_VERSION.to_string()], "temporaries left");
}
