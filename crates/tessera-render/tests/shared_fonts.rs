//! The test-only shared installed fonts (#660): fonts loaded while a test
//! shares them build one coverage index between them, and each still hears
//! it land; a load while the indexes are held, or after sharing stops,
//! builds its own.
//!
//! One test, so its process-wide counts see no other test's loads under
//! `cargo test`'s threads as well as under nextest's processes.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use tessera_render::{
    FontConfig, Fonts, fallback_index_builds, hold_fallback_index, share_installed_fonts,
};

/// Long enough for a debug index build on a loaded machine.
const LANDING: Duration = Duration::from_secs(120);

fn config(family: &str) -> FontConfig {
    FontConfig {
        family: family.into(),
        size_pt: 13.0,
        scale: 1.0,
        fallback: Vec::new(),
    }
}

/// Waits until `fonts`' index has landed.
fn wait_ready(fonts: &Fonts, what: &str) {
    let deadline = Instant::now() + LANDING;
    while !fonts.fallback_index_ready() {
        assert!(Instant::now() < deadline, "{what}: the index never landed");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn shared_fonts_build_one_index_that_every_load_hears_land() {
    share_installed_fonts(true);
    let before = fallback_index_builds();
    let (tx, rx) = mpsc::channel();
    // Each load names its own family, which it still resolves itself.
    let families = ["Menlo", "Monaco", "Courier New"];
    let loaded: Vec<Fonts> = families
        .iter()
        .enumerate()
        .map(|(i, family)| {
            let tx = tx.clone();
            Fonts::with_index_notice(&config(family), move || tx.send(i).expect("receiver"))
                .expect("the family or Menlo is installed")
        })
        .collect();
    let mut heard: Vec<usize> = (0..families.len())
        .map(|_| {
            rx.recv_timeout(LANDING)
                .expect("every load hears the index land")
        })
        .collect();
    heard.sort_unstable();
    assert_eq!(heard, [0, 1, 2], "each notice once");
    assert!(
        rx.recv_timeout(Duration::from_millis(200)).is_err(),
        "no notice twice"
    );
    assert!(loaded.iter().all(Fonts::fallback_index_ready));
    assert_eq!(loaded[0].family(), "Menlo");
    assert_eq!(loaded[1].family(), "Monaco");
    assert_eq!(
        fallback_index_builds() - before,
        1,
        "three shared loads build one index"
    );

    // A load after the index landed hears it land, too.
    let (late_tx, late_rx) = mpsc::channel();
    let late = Fonts::with_index_notice(&config("Menlo"), move || {
        late_tx.send(()).expect("receiver");
    })
    .expect("Menlo");
    late_rx
        .recv_timeout(LANDING)
        .expect("a late load hears the landed index");
    assert!(late.fallback_index_ready());
    assert_eq!(
        fallback_index_builds() - before,
        1,
        "a late shared load builds none"
    );

    // Held: a load builds an index of its own, which waits for the release.
    hold_fallback_index(true);
    let held = Fonts::new(&config("Menlo")).expect("Menlo");
    std::thread::sleep(Duration::from_millis(300));
    let held_ready = held.fallback_index_ready();
    hold_fallback_index(false);
    assert!(!held_ready, "a held load's index waits for the release");
    wait_ready(&held, "held");
    assert_eq!(
        fallback_index_builds() - before,
        2,
        "a held load builds its own"
    );

    // Sharing stopped: each load builds its own again, the second too
    // (#660 M6: an off switch that only forgot the shared index would let
    // the second share the first's).
    share_installed_fonts(false);
    let own = Fonts::new(&config("Menlo")).expect("Menlo");
    wait_ready(&own, "unshared");
    assert_eq!(
        fallback_index_builds() - before,
        3,
        "an unshared load builds its own"
    );
    let second = Fonts::new(&config("Menlo")).expect("Menlo");
    wait_ready(&second, "second unshared");
    assert_eq!(
        fallback_index_builds() - before,
        4,
        "a second unshared load builds its own too"
    );
}
