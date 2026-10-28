//! Live re-theming: a store-backed [`App`] re-themed from a
//! `ConfigEvent::Changed` draws the new background.
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
