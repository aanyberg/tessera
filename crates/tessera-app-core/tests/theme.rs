//! Live re-theming: a store-backed [`App`] re-themed from a
//! `ConfigEvent::Changed` draws the new background, and the `App`'s own
//! configuration watch applies edits of `config.toml` and `themes/`.
//!
//! Views need a Core Animation layer, so the frame is drawn offscreen by
//! one renderer that is handed [`App::view_theme`] through
//! `Renderer::set_theme`, as [`App::apply_theme`] hands it to every view's
//! renderer. That attached views really receive it is tested with real
//! layers in `tessera-ffi/tests/surface.rs`.
//!
//! Needs a GPU; `TESSERA_SKIP_GPU_TESTS=1` opts out on a machine without
//! one, as in `tessera-render`'s headless tests.

use std::time::{Duration, Instant};

use tessera_app_core::App;
use tessera_config::{ConfigEvent, ConfigPaths, ConfigStore, Theme, WatchOptions, write_atomic};
use tessera_render::{
    FontConfig, FrameInput, Gpu, OffscreenTarget, PhysicalSize, Renderer, Shaper, wgpu,
};

const SKIP_VAR: &str = "TESSERA_SKIP_GPU_TESTS";

fn gpu() -> Option<Gpu> {
    if std::env::var_os(SKIP_VAR).is_some() {
        eprintln!("skipping: {SKIP_VAR} is set");
        return None;
    }
    Some(Gpu::headless().unwrap_or_else(|e| {
        panic!("no GPU adapter available ({e}); set {SKIP_VAR}=1 to skip the GPU tests")
    }))
}

/// A renderer like a view's, drawing with the renderer's default theme.
fn renderer(gpu: &Gpu) -> Renderer {
    let theme = tessera_render::Theme::default();
    let shaper = Shaper::new(&FontConfig {
        family: theme.font_family.clone(),
        size_pt: theme.font_size,
        scale: 1.0,
        fallback: Vec::new(),
    })
    .expect("fonts");
    Renderer::new(gpu, theme, shaper).expect("renderer")
}

/// The top-left pixel of an empty frame drawn into an sRGB target after
/// handing `renderer` `app`'s view theme the way [`App::apply_theme`] hands
/// it to each view (`Renderer::set_theme`): the clear colour, as sRGB
/// bytes.
fn clear_colour(gpu: &Gpu, renderer: &mut Renderer, app: &App) -> [u8; 4] {
    renderer.set_theme(app.view_theme());
    let target = OffscreenTarget::new(
        gpu,
        PhysicalSize::new(32, 32),
        wgpu::TextureFormat::Rgba8UnormSrgb,
    );
    target
        .render(gpu, renderer, &FrameInput::new(&[]))
        .expect("render");
    let pixels = target.read_back(gpu).expect("readback");
    [pixels[0], pixels[1], pixels[2], pixels[3]]
}

fn assert_bytes(got: [u8; 4], want: [u8; 4]) {
    let close = got.iter().zip(want).all(|(g, w)| g.abs_diff(w) <= 1);
    assert!(close, "clear colour {got:02x?}, expected {want:02x?} (±1)");
}

/// Waits for the watcher's next `Changed` event.
fn next_change(watcher: &tessera_config::Watcher) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match watcher.recv_timeout(left) {
            Ok(ConfigEvent::Changed(_)) => return,
            Ok(other) => eprintln!("ignoring {other:?}"),
            Err(e) => panic!("no Changed event within 10 s: {e}"),
        }
    }
}

#[test]
fn a_theme_change_through_the_watcher_rethemes_the_app() {
    let Some(gpu) = gpu() else { return };
    let root = tempfile::tempdir().unwrap();
    let paths = ConfigPaths::new(root.path());
    let store = ConfigStore::open(paths.clone()).unwrap();
    let watcher = store.watch(WatchOptions::default()).unwrap();
    let app = App::new((*store.config()).clone(), paths.clone());
    let mut renderer = renderer(&gpu);
    assert_eq!(app.view_theme(), tessera_render::Theme::default());
    assert_bytes(clear_colour(&gpu, &mut renderer, &app), [0, 0, 0, 0xff]);

    write_atomic(&paths.config_file(), "[appearance]\ntheme = \"Dracula\"\n").unwrap();
    next_change(&watcher);
    assert_eq!(store.config().appearance.theme, "Dracula");
    // Nothing changes until the application applies the new theme.
    assert_bytes(clear_colour(&gpu, &mut renderer, &app), [0, 0, 0, 0xff]);
    app.apply_theme(&store.theme());
    assert_bytes(
        clear_colour(&gpu, &mut renderer, &app),
        [0x28, 0x2a, 0x36, 0xff],
    );

    // A user theme file shadows the bundled one, and a change back to an
    // unknown name falls back to the default.
    std::fs::write(
        paths.themes_dir().join("dracula.toml"),
        Theme::bundled("Dracula")
            .unwrap()
            .to_toml()
            .replace("#282a36", "#102030"),
    )
    .unwrap();
    app.apply_theme(&store.theme());
    assert_bytes(
        clear_colour(&gpu, &mut renderer, &app),
        [0x10, 0x20, 0x30, 0xff],
    );

    write_atomic(&paths.config_file(), "[appearance]\ntheme = \"Nope\"\n").unwrap();
    next_change(&watcher);
    app.apply_theme(&store.theme());
    assert_bytes(clear_colour(&gpu, &mut renderer, &app), [0, 0, 0, 0xff]);
}

#[test]
fn a_new_app_starts_with_the_configured_theme() {
    let root = tempfile::tempdir().unwrap();
    let paths = ConfigPaths::new(root.path());
    std::fs::create_dir_all(paths.themes_dir()).unwrap();
    std::fs::write(
        paths.themes_dir().join("mine.toml"),
        Theme::bundled("Nord").unwrap().to_toml(),
    )
    .unwrap();
    let mut config = tessera_config::Config::default();
    config.appearance.theme = "mine".into();
    config.appearance.opacity = 0.5;
    let app = App::new(config, paths);
    let theme = app.view_theme();
    assert_eq!(
        theme.background.with_alpha(1.0),
        tessera_render::Rgba::from_srgb8(0x2e, 0x34, 0x40)
    );
    assert!((theme.background.a - 0.5).abs() < f32::EPSILON);
}

/// Records each `on_appearance_changed` on a channel.
struct Appearance(std::sync::mpsc::Sender<bool>);

impl tessera_app_core::SessionObserver for Appearance {
    fn on_session_event(
        &self,
        _id: tessera_app_core::SessionId,
        _event: tessera_app_core::SessionEvent,
    ) {
    }
    fn on_appearance_changed(&self, opaque: bool) {
        let _ = self.0.send(opaque);
    }
}

fn theme_doc(name: &str, background: &str) -> String {
    let mut theme = Theme::bundled("Dracula").unwrap().clone();
    theme.name = name.into();
    theme.background = background.parse().unwrap();
    theme.to_toml()
}

fn background(app: &App) -> [u8; 3] {
    app.view_theme().background.to_srgb8()
}

/// The bound the issue sets for a theme-file edit (a debounce plus 500 ms),
/// with headroom for a loaded machine: `recv_timeout` returns as soon as
/// the change arrives.
const BOUND: Duration = Duration::from_millis(1500);

/// Writes of the flood in the watch test: a fixed count, so a slow machine
/// (`taskpolicy -b`) floods as hard, just for longer.
const WRITES: u32 = 100;

/// The app's own watch thread applies `config.toml` and theme-file edits:
/// the theme in use, a translucent opacity (reported as not opaque), an
/// unrelated theme file (nothing applied, nothing reported), an invalid
/// file (ignored) and a flood of writes (coalesced).
#[test]
fn the_config_watch_applies_edits_and_reports_appearance_changes() {
    let root = tempfile::tempdir().unwrap();
    let paths = ConfigPaths::new(root.path());
    std::fs::create_dir_all(paths.themes_dir()).unwrap();
    std::fs::write(
        paths.themes_dir().join("mine.toml"),
        theme_doc("Mine", "#101010"),
    )
    .unwrap();
    std::fs::write(
        paths.themes_dir().join("other.toml"),
        theme_doc("Other", "#202020"),
    )
    .unwrap();
    std::fs::write(paths.config_file(), "[appearance]\ntheme = \"mine\"\n").unwrap();
    let store = ConfigStore::open(paths.clone()).unwrap();
    let app = App::new((*store.config()).clone(), paths.clone());
    assert_eq!(background(&app), [0x10, 0x10, 0x10]);
    let (tx, changes) = std::sync::mpsc::channel();
    app.set_observer(Some(Box::new(Appearance(tx))));
    app.start_config_watch(store, WatchOptions::default())
        .unwrap();
    let quiet = |what: &str| {
        let got = changes.recv_timeout(Duration::from_millis(800));
        assert!(got.is_err(), "{what}: unexpected appearance change {got:?}");
    };
    quiet("start");

    // The theme in use, edited in place.
    std::fs::write(
        paths.themes_dir().join("mine.toml"),
        theme_doc("Mine", "#111111"),
    )
    .unwrap();
    assert_eq!(changes.recv_timeout(BOUND), Ok(true), "theme file edit");
    assert_eq!(background(&app), [0x11, 0x11, 0x11]);
    quiet("after the theme edit");

    // An unrelated theme file: nothing changes on screen (D3).
    std::fs::write(
        paths.themes_dir().join("other.toml"),
        theme_doc("Other", "#212121"),
    )
    .unwrap();
    std::fs::write(
        paths.themes_dir().join("new.toml"),
        theme_doc("New", "#303030"),
    )
    .unwrap();
    quiet("unrelated theme files");
    assert_eq!(background(&app), [0x11, 0x11, 0x11]);

    // The configuration: opacity applies with the theme, and the App's
    // configuration follows.
    write_atomic(
        &paths.config_file(),
        "[appearance]\ntheme = \"other\"\nopacity = 0.5\n",
    )
    .unwrap();
    assert_eq!(changes.recv_timeout(BOUND), Ok(false), "translucent");
    assert_eq!(background(&app), [0x21, 0x21, 0x21]);
    assert!((app.view_theme().background.a - 0.5).abs() < 1e-6);
    assert_eq!(app.config().appearance.opacity, 0.5);

    // An invalid file keeps the last good configuration.
    std::fs::write(paths.config_file(), "[appearance\n").unwrap();
    quiet("invalid config");
    assert_eq!(app.config().appearance.theme, "other");

    // A flood of edits is coalesced: far fewer changes than writes, and
    // the last write wins.
    let started = Instant::now();
    let mut previous = Instant::now();
    let mut largest_gap = Duration::ZERO;
    for n in 0..WRITES {
        let shade = 0x40 + n % 0x40;
        std::fs::write(
            paths.themes_dir().join("other.toml"),
            theme_doc("Other", &format!("#{shade:02x}{shade:02x}{shade:02x}")),
        )
        .unwrap();
        largest_gap = largest_gap.max(previous.elapsed());
        previous = Instant::now();
        std::thread::sleep(Duration::from_millis(2));
    }
    let elapsed = started.elapsed();
    let last_write = Instant::now();
    let shade = u8::try_from(0x40 + (WRITES - 1) % 0x40).unwrap();
    // The latency is to the change that carries the last value.
    let deadline = last_write + BOUND;
    let mut reported = 0_u32;
    let mut latency = None;
    while let Ok(_opaque) = changes.recv_timeout(deadline.saturating_duration_since(Instant::now()))
    {
        reported += 1;
        if latency.is_none() && background(&app) == [shade, shade, shade] {
            latency = Some(last_write.elapsed());
        }
    }
    eprintln!(
        "flood: {WRITES} writes in {elapsed:?}, {reported} appearance changes, \
         the last value applied {latency:?} after the last write"
    );
    // At most one per `max_wait` (1 s) while the writes go on, plus the
    // final one; unless the machine is so slow (`taskpolicy -b`) that a
    // gap between two writes outlasted the debounce, which ends a burst.
    if largest_gap < WatchOptions::default().debounce {
        assert!(
            reported <= 1 + u32::try_from(elapsed.as_secs()).unwrap() + 1,
            "{reported} changes for {WRITES} writes in {elapsed:?}"
        );
    } else {
        eprintln!(
            "flood: a {largest_gap:?} gap between writes; the coalescing bound does not apply"
        );
    }
    assert!(latency.is_some(), "the last write was never applied");
}

/// A save of the theme in use that does not load (an editor's partial
/// write) keeps the colours in use instead of falling back to the default
/// theme, and the in-place save that fixes it applies; so does a
/// configuration that names a broken file, and a broken file read by the
/// watch's start-up catch-up.
#[test]
fn a_broken_theme_file_keeps_the_colours_in_use() {
    let root = tempfile::tempdir().unwrap();
    let paths = ConfigPaths::new(root.path());
    let mine = paths.themes_dir().join("mine.toml");
    std::fs::create_dir_all(paths.themes_dir()).unwrap();
    std::fs::write(&mine, theme_doc("Mine", "#101010")).unwrap();
    std::fs::write(paths.themes_dir().join("bad.toml"), "foreground = ").unwrap();
    std::fs::write(paths.config_file(), "[appearance]\ntheme = \"mine\"\n").unwrap();
    let app = App::with_config_dir(paths.clone(), WatchOptions::default());
    let (tx, changes) = std::sync::mpsc::channel();
    app.set_observer(Some(Box::new(Appearance(tx))));
    let quiet = |what: &str| {
        let got = changes.recv_timeout(Duration::from_millis(800));
        assert!(got.is_err(), "{what}: unexpected appearance change {got:?}");
    };
    quiet("start");

    std::fs::write(&mine, "name = \"Mine\"\nforeground = ").unwrap();
    quiet("broken save");
    assert_eq!(
        background(&app),
        [0x10, 0x10, 0x10],
        "not the default theme"
    );
    std::fs::write(&mine, theme_doc("Mine", "#1a1a1a")).unwrap();
    assert_eq!(changes.recv_timeout(BOUND), Ok(true), "the fix");
    assert_eq!(background(&app), [0x1a, 0x1a, 0x1a]);

    // A theme found by its `name` key (file stem `keyed`, name "Keyed
    // Theme"): its failure is not a lookup warning, yet the colours stay
    // and the fix applies.
    let keyed = paths.themes_dir().join("keyed.toml");
    std::fs::write(&keyed, theme_doc("Keyed Theme", "#1c1c1c")).unwrap();
    write_atomic(
        &paths.config_file(),
        "[appearance]\ntheme = \"Keyed Theme\"\n",
    )
    .unwrap();
    assert_eq!(
        changes.recv_timeout(BOUND),
        Ok(true),
        "keyed theme selected"
    );
    assert_eq!(background(&app), [0x1c, 0x1c, 0x1c]);
    std::fs::write(&keyed, "name = \"Keyed Theme\"\nforeground = ").unwrap();
    quiet("broken keyed save");
    assert_eq!(
        background(&app),
        [0x1c, 0x1c, 0x1c],
        "not the default theme"
    );
    std::fs::write(&keyed, theme_doc("Keyed Theme", "#1d1d1d")).unwrap();
    assert_eq!(changes.recv_timeout(BOUND), Ok(true), "the keyed fix");
    assert_eq!(background(&app), [0x1d, 0x1d, 0x1d]);

    // A configuration naming a broken file: applied, colours kept.
    write_atomic(
        &paths.config_file(),
        "[appearance]\ntheme = \"bad\"\nfont_size = 13\n",
    )
    .unwrap();
    quiet("config naming a broken theme");
    assert_eq!(app.config().appearance.theme, "bad");
    assert_eq!(background(&app), [0x1d, 0x1d, 0x1d]);
    drop(app);

    // The start-up catch-up reads a file that is broken for a moment and
    // fixed soon after: the colours go straight from the old to the new,
    // never through the default theme (one change, not two).
    std::fs::write(paths.config_file(), "[appearance]\ntheme = \"mine\"\n").unwrap();
    let app = App::new(
        tessera_config::Config {
            appearance: tessera_config::Appearance {
                theme: "mine".into(),
                ..Default::default()
            },
            ..Default::default()
        },
        paths.clone(),
    );
    let (tx, changes) = std::sync::mpsc::channel();
    app.set_observer(Some(Box::new(Appearance(tx))));
    std::fs::write(&mine, "foreground = ").unwrap();
    let store = ConfigStore::in_memory(paths.clone(), (*app.config()).clone());
    app.start_config_watch(store, WatchOptions::default())
        .unwrap();
    std::thread::sleep(Duration::from_millis(150));
    std::fs::write(&mine, theme_doc("Mine", "#1b1b1b")).unwrap();
    assert_eq!(changes.recv_timeout(BOUND), Ok(true), "the fixed file");
    assert_eq!(background(&app), [0x1b, 0x1b, 0x1b]);
    quiet("after the fix");
}
