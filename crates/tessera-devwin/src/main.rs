//! `tessera-devwin`: a development window that runs a login shell through
//! the application core, without Xcode or the macOS app.
//!
//! One winit window shows one session. The window's `wgpu` surface is
//! handed to [`App::attach_view`], so what it draws is exactly what the
//! app's terminal view draws; keyboard, mouse and scroll events go through
//! [`App::send_input`] (mapped by the [`input`] module), so the real
//! keyboard encoder runs. Session events reach the event loop through a
//! coalescing [`mailbox`], so a flooding child cannot grow winit's queue.
//! A paste reads the clipboard on a helper thread ([`clipboard`]), so a hung
//! clipboard tool cannot freeze the window.
//!
//! The window installs no signal handlers: `SIGINT` or `SIGTERM` to the
//! window process ends it without closing the session, so the shell gets
//! only the kernel's hangup when the PTY closes, and a child that ignores
//! `SIGHUP` is orphaned. Close the window instead.
//!
//! A frame the core skips (`FrameStatus::Skipped`) has three causes since
//! #21: nothing changed, the frame was paced (more than 240 frames per
//! second), or the surface was not ready. They are not told apart yet
//! (#101), so the window retries a skipped frame about one refresh later,
//! at most [`MAX_SKIP_RETRIES`] times in a row: enough to draw a paced
//! frame (a retry that late is never paced) without waking 60 times a
//! second while nothing changes. A presented frame and any new output,
//! input, resize, occlusion change or re-attached surface start the count
//! over.
//!
//! Dev-only: never published, and nothing in the app or in `tessera-ffi`
//! depends on it (a test checks). Run it with `just devwin`.

mod clipboard;
mod input;
mod mailbox;

use std::cell::Cell;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tessera_app_core::input::MouseButton;
use tessera_app_core::session::PENDING_INPUT_BYTES;
use tessera_app_core::view::{FrameStatus, PhysicalSize, RenderError};
use tessera_app_core::{
    App, Config, ConfigPaths, Error, InputEvent, InputOutcome, SessionId, SpawnOptions,
};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::ModifiersKeyState;
use winit::window::{Window, WindowId};

use crate::input::{CellGeometry, InputHold, KeyAction, KeyInput, KeyMods, Platform};
use crate::mailbox::{Mailbox, PasteDelivery};

/// How long to wait before drawing again after a frame was skipped (the
/// swapchain timed out) while the window is visible: about one refresh.
const SKIPPED_FRAME_RETRY: Duration = Duration::from_millis(16);

/// Most skipped frames in a row that are retried (see the module docs).
/// One retry draws a paced frame: [`SKIPPED_FRAME_RETRY`] is longer than
/// the core's frame interval, so the retry is due. The second covers a
/// surface that was not ready once.
const MAX_SKIP_RETRIES: u32 = 2;

/// How long a paste waits for the clipboard tool before killing it.
const CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(2);

/// Consecutive frames that may need recovery (a resize after a suboptimal
/// frame, a re-attach after a lost surface) before the window gives up:
/// one that keeps failing would otherwise redraw in a tight loop.
const MAX_RECOVERIES: u32 = 3;

/// The window's initial title, until the shell sets one.
const TITLE: &str = "Tessera (devwin)";

fn main() -> ExitCode {
    let Some(paths) = ConfigPaths::for_current_user() else {
        eprintln!("tessera-devwin: cannot determine the home directory");
        return ExitCode::FAILURE;
    };
    let event_loop = match EventLoop::<Wake>::with_user_event().build() {
        Ok(event_loop) => event_loop,
        Err(error) => {
            eprintln!("tessera-devwin: cannot create the event loop: {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut devwin = DevWin::new(
        App::new(Config::default(), paths),
        event_loop.create_proxy(),
    );
    if let Err(error) = event_loop.run_app(&mut devwin) {
        eprintln!("tessera-devwin: event loop failed: {error}");
        return ExitCode::FAILURE;
    }
    devwin.exit_code
}

/// The user event the observer sends when the [`Mailbox`] has news.
#[derive(Debug, Clone, Copy)]
struct Wake;

/// The window, its session and the input state winit does not keep.
struct DevWin {
    app: App,
    mailbox: Arc<Mailbox>,
    proxy: EventLoopProxy<Wake>,
    /// The renderer's padding in points, from the configuration (which
    /// does not change while the window runs).
    padding_pt: f32,
    window: Option<Arc<Window>>,
    session: Option<SessionId>,
    /// The modifiers held, as keys and the mouse count them
    /// ([`input::effective_modifiers`]: a right Option that composes is
    /// not Alt).
    modifiers: KeyMods,
    /// Last pointer position, physical pixels.
    cursor: (f64, f64),
    /// The button held for drags.
    held: MouseButton,
    occluded: bool,
    /// When to draw again after a skipped frame.
    retry_at: Option<Instant>,
    /// Skipped frames in a row since the last presented frame or event;
    /// see [`retry_skipped`]. A `Cell` so the `&self` event paths reset it.
    skips: Cell<u32>,
    /// Frames in a row that needed recovery; see [`MAX_RECOVERIES`].
    recoveries: u32,
    /// A clipboard read for a paste is running; further pastes are
    /// dropped and other input waits in `hold`.
    paste_in_flight: bool,
    /// Input that arrived while a paste was being read.
    hold: InputHold,
    /// A paste dropped during the current read was reported (once per
    /// read, however long Cmd-V auto-repeats).
    paste_drop_reported: bool,
    exit_code: ExitCode,
}

impl DevWin {
    /// A window-less handler whose session events wake the loop through
    /// `proxy`.
    fn new(app: App, proxy: EventLoopProxy<Wake>) -> Self {
        let mailbox = Arc::new(Mailbox::default());
        let posting = Arc::clone(&mailbox);
        let waking = proxy.clone();
        app.set_observer(Some(Box::new(move |_id, event| {
            if posting.post(event) {
                // Fails only once the loop is gone; nothing to wake then.
                let _ = waking.send_event(Wake);
            }
        })));
        let padding_pt = app.view_theme().padding;
        Self {
            app,
            mailbox,
            proxy,
            padding_pt,
            window: None,
            session: None,
            modifiers: KeyMods::default(),
            cursor: (0.0, 0.0),
            held: MouseButton::None,
            occluded: false,
            retry_at: None,
            skips: Cell::new(0),
            recoveries: 0,
            paste_in_flight: false,
            hold: InputHold::default(),
            paste_drop_reported: false,
            exit_code: ExitCode::SUCCESS,
        }
    }

    /// Opens the window, spawns the shell and attaches the view.
    fn start(&mut self, event_loop: &ActiveEventLoop) -> Result<(), String> {
        let attributes = Window::default_attributes()
            .with_title(TITLE)
            .with_inner_size(LogicalSize::new(900.0, 560.0));
        // The left Option key is Alt: winit then reports its chords with
        // Shift applied and no Option composition (`<` for Option+Shift+`,`,
        // not `¯`). The right one composes, for layouts that type `@`, `[`
        // or `|` with it (see the `input` module's policies).
        #[cfg(target_os = "macos")]
        let attributes = {
            use winit::platform::macos::{OptionAsAlt, WindowAttributesExtMacOS as _};
            attributes.with_option_as_alt(OptionAsAlt::OnlyLeft)
        };
        let window = Arc::new(
            event_loop
                .create_window(attributes)
                .map_err(|e| format!("cannot open a window: {e}"))?,
        );
        self.window = Some(Arc::clone(&window));
        let id = self
            .app
            .spawn_session(SpawnOptions::login_shell(""))
            .map_err(|e| format!("cannot start the shell: {e}"))?;
        self.session = Some(id);
        self.attach(&window, id)?;
        window.request_redraw();
        Ok(())
    }

    /// Creates a surface on `window` and makes it session `id`'s view.
    fn attach(&self, window: &Arc<Window>, id: SessionId) -> Result<(), String> {
        let instance = self.app.view_instance();
        let surface = instance
            .create_surface(Arc::clone(window))
            .map_err(|e| format!("cannot create a surface: {e}"))?;
        let size = window.inner_size();
        self.app
            .attach_view(
                id,
                instance,
                surface,
                PhysicalSize::new(size.width, size.height),
                scale(window),
            )
            .map_err(|e| format!("cannot attach the view: {e}"))
    }

    /// Prints `message`, closes the session and stops the loop.
    fn fail(&mut self, event_loop: &ActiveEventLoop, message: &str) {
        eprintln!("tessera-devwin: {message}");
        self.exit_code = ExitCode::FAILURE;
        self.shut_down(event_loop);
    }

    /// Closes the session (hanging up the shell) and stops the loop.
    fn shut_down(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(id) = self.session.take()
            && let Err(error) = self.app.close_session(id)
        {
            eprintln!("tessera-devwin: closing the session failed: {error}");
        }
        self.window = None;
        event_loop.exit();
    }

    /// Reconfigures the view for the window's current size and scale.
    fn follow_window(&self) {
        let (Some(window), Some(id)) = (&self.window, self.session) else {
            return;
        };
        let size = window.inner_size();
        let result = self.app.resize_view(
            id,
            PhysicalSize::new(size.width, size.height),
            scale(window),
        );
        if let Err(error) = result {
            eprintln!("tessera-devwin: resizing the view failed: {error}");
        }
        self.skips.set(0);
        window.request_redraw();
    }

    /// Draws a frame and handles what the swapchain reports.
    fn render(&mut self, event_loop: &ActiveEventLoop) {
        let (Some(window), Some(id)) = (self.window.clone(), self.session) else {
            return;
        };
        let status = self.app.render_view(id);
        if gives_up(&mut self.recoveries, &status) {
            let message = format!(
                "the surface still needs recovery after {MAX_RECOVERIES} attempts ({status:?})"
            );
            self.fail(event_loop, &message);
            return;
        }
        let mut skips = self.skips.get();
        let retry = retry_skipped(&mut skips, &status);
        self.skips.set(skips);
        match status {
            Ok(FrameStatus::Presented) => {}
            // Shown, but the swapchain no longer matches the window: the
            // core's contract is to resize the view before the next frame.
            // Metal never reports it, so on macOS this is untested; #65
            // (Vulkan/Wayland, where it is common during a resize) must
            // check that the resize clears it.
            Ok(FrameStatus::Suboptimal) => self.follow_window(),
            Ok(FrameStatus::Skipped) => {
                let size = window.inner_size();
                if retry && !self.occluded && size.width > 0 && size.height > 0 {
                    self.retry_at = Some(Instant::now() + SKIPPED_FRAME_RETRY);
                }
            }
            // The core detached the view; give it a fresh surface.
            Err(Error::Gpu(RenderError::SurfaceLost)) => match self.attach(&window, id) {
                Ok(()) => {
                    self.skips.set(0);
                    window.request_redraw();
                }
                Err(message) => self.fail(event_loop, &message),
            },
            Err(error) => eprintln!("tessera-devwin: drawing failed: {error}"),
        }
    }

    /// Where the grid's cells are in the window right now.
    fn geometry(&self) -> CellGeometry {
        let Some(id) = self.session else {
            return CellGeometry::default();
        };
        match (self.app.view_cell_metrics(id), self.app.view_size(id)) {
            (Ok(metrics), Ok(size)) => CellGeometry::new(metrics, size.scale, self.padding_pt),
            _ => CellGeometry::default(),
        }
    }

    /// Sends `event` to the session, or holds it while a paste is being
    /// read so it cannot overtake the pasted text.
    fn send(&mut self, event: InputEvent) {
        if self.paste_in_flight && InputHold::holds(&event) {
            self.hold.push(event);
        } else {
            self.send_now(event);
        }
    }

    /// Sends `event` to the session and redraws when the core used it.
    fn send_now(&self, event: InputEvent) {
        let (Some(window), Some(id)) = (&self.window, self.session) else {
            return;
        };
        self.skips.set(0);
        match self.app.send_input(id, event) {
            Ok(InputOutcome::Consumed) => window.request_redraw(),
            Ok(InputOutcome::Ignored) => {}
            Err(error) => eprintln!("tessera-devwin: input dropped: {error}"),
        }
    }

    /// Starts reading the clipboard on a helper thread; the result arrives
    /// through the mailbox. A paste while one is running is dropped.
    fn start_paste(&mut self) {
        if self.paste_in_flight {
            if !std::mem::replace(&mut self.paste_drop_reported, true) {
                eprintln!(
                    "tessera-devwin: paste: still reading the clipboard; pastes are dropped until it finishes"
                );
            }
            return;
        }
        let proxy = self.proxy.clone();
        // Reports even if the read panics, so `paste_in_flight` is cleared.
        let delivery = PasteDelivery::new(Arc::clone(&self.mailbox), move || {
            let _ = proxy.send_event(Wake);
        });
        let spawned = std::thread::Builder::new()
            .name("paste".into())
            .spawn(move || {
                delivery.deliver(clipboard::read(
                    &clipboard::native_commands(),
                    PENDING_INPUT_BYTES,
                    CLIPBOARD_TIMEOUT,
                ));
            });
        match spawned {
            Ok(_) => self.paste_in_flight = true,
            Err(error) => eprintln!("tessera-devwin: paste: cannot start a thread: {error}"),
        }
    }

    /// Applies what the session reported since the last wake-up.
    fn drain_mailbox(&mut self, event_loop: &ActiveEventLoop) {
        let pending = self.mailbox.take();
        if let Some(window) = &self.window {
            if pending.output {
                self.skips.set(0);
                window.request_redraw();
            }
            if let Some(title) = &pending.title {
                window.set_title(title);
            }
        }
        if let Some(result) = pending.paste {
            self.paste_in_flight = false;
            self.paste_drop_reported = false;
            match result {
                Ok(text) => self.send_now(InputEvent::Paste(text)),
                Err(error) => eprintln!("tessera-devwin: paste: {error}"),
            }
            let (held, dropped) = self.hold.release();
            if dropped > 0 {
                eprintln!("tessera-devwin: {dropped} input events dropped during a paste");
            }
            for event in held {
                self.send_now(event);
            }
        }
        if pending.bells > 0 {
            eprintln!("tessera-devwin: bell (x{})", pending.bells);
        }
        if let Some(status) = pending.exit {
            eprintln!("tessera-devwin: shell exited ({status})");
            self.shut_down(event_loop);
        }
    }
}

/// Whether a frame that came back `status` is retried, counting skipped
/// frames in a row in `skips`: a skip adds one and is retried while the
/// streak is at most [`MAX_SKIP_RETRIES`]; a drawn frame ends the streak;
/// an error leaves it. The event paths reset `skips` themselves.
fn retry_skipped(skips: &mut u32, status: &Result<FrameStatus, Error>) -> bool {
    match status {
        Ok(FrameStatus::Skipped) => {
            *skips = skips.saturating_add(1);
            *skips <= MAX_SKIP_RETRIES
        }
        Ok(FrameStatus::Presented | FrameStatus::Suboptimal) => {
            *skips = 0;
            false
        }
        Err(_) => false,
    }
}

/// Counts `status` against [`MAX_RECOVERIES`] in `recoveries`: a frame
/// that needs recovery (suboptimal, surface lost) adds one, a presented
/// frame resets the count, a skipped one leaves it. `true` once more than
/// [`MAX_RECOVERIES`] frames in a row needed recovery.
fn gives_up(recoveries: &mut u32, status: &Result<FrameStatus, Error>) -> bool {
    match status {
        Ok(FrameStatus::Suboptimal) | Err(Error::Gpu(RenderError::SurfaceLost)) => {
            *recoveries = recoveries.saturating_add(1);
            *recoveries > MAX_RECOVERIES
        }
        Ok(FrameStatus::Presented) => {
            *recoveries = 0;
            false
        }
        _ => false,
    }
}

/// The window's backing scale as the core takes it.
#[expect(
    clippy::cast_possible_truncation,
    reason = "scale factors are small positive numbers"
)]
fn scale(window: &Window) -> f32 {
    window.scale_factor() as f32
}

impl ApplicationHandler<Wake> for DevWin {
    fn new_events(&mut self, _event_loop: &ActiveEventLoop, cause: StartCause) {
        if let StartCause::ResumeTimeReached { .. } = cause
            && let Some(window) = &self.window
        {
            self.retry_at = None;
            window.request_redraw();
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        if let Err(message) = self.start(event_loop) {
            self.fail(event_loop, &message);
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, Wake: Wake) {
        self.drain_mailbox(event_loop);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested | WindowEvent::Destroyed => self.shut_down(event_loop),
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                // A new size is a fresh start for a swapchain that kept
                // needing recovery. (#65, the Linux frontend, inherits this
                // loop; Wayland reconfigures on every resize.)
                self.recoveries = 0;
                self.follow_window();
            }
            WindowEvent::RedrawRequested => self.render(event_loop),
            WindowEvent::Occluded(occluded) => {
                self.occluded = occluded;
                self.skips.set(0);
                if !occluded && let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::Focused(focused) => self.send(input::focus(focused)),
            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = input::effective_modifiers(
                    modifiers.state(),
                    modifiers.lalt_state() == ModifiersKeyState::Pressed,
                    modifiers.ralt_state() == ModifiersKeyState::Pressed,
                    Platform::CURRENT,
                );
            }
            WindowEvent::KeyboardInput {
                event,
                is_synthetic: false,
                ..
            } => match input::map_key(
                &KeyInput::from_event(&event),
                self.modifiers,
                Platform::CURRENT,
            ) {
                Some(KeyAction::Send(event)) => self.send(event),
                Some(KeyAction::Paste) => self.start_paste(),
                None => {}
            },
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = (position.x, position.y);
                self.send(input::mouse_motion(
                    self.cursor,
                    self.held,
                    self.geometry(),
                    self.modifiers.held,
                ));
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let mapped = input::mouse_button(button);
                match state {
                    ElementState::Pressed => self.held = mapped.unwrap_or(self.held),
                    ElementState::Released if mapped == Some(self.held) => {
                        self.held = MouseButton::None;
                    }
                    ElementState::Released => {}
                }
                if let Some(event) = input::mouse_input(
                    state,
                    button,
                    self.cursor,
                    self.geometry(),
                    self.modifiers.held,
                ) {
                    self.send(event);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                self.send(input::scroll(
                    delta,
                    self.cursor,
                    self.geometry(),
                    self.modifiers.held,
                ));
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        event_loop.set_control_flow(match self.retry_at {
            Some(at) => ControlFlow::WaitUntil(at),
            None => ControlFlow::Wait,
        });
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        // Reached without `shut_down` when the app is quit from its menu
        // (Cmd-Q on macOS), which may not return from the loop.
        if let Some(id) = self.session.take() {
            let _ = self.app.close_session(id);
        }
        self.window = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A paced skip is retried once and the retry is due: the retry
    /// interval is longer than the core's frame interval.
    #[test]
    fn a_paced_skip_is_retried_late_enough_to_draw() {
        assert!(SKIPPED_FRAME_RETRY >= tessera_app_core::view::MIN_FRAME_INTERVAL);
        let mut skips = 0;
        assert!(
            retry_skipped(&mut skips, &Ok(FrameStatus::Skipped)),
            "paced"
        );
        assert!(
            !retry_skipped(&mut skips, &Ok(FrameStatus::Presented)),
            "the retry drew"
        );
        assert_eq!(skips, 0);
    }

    /// Nothing changed: the window retries twice, then waits for an event
    /// instead of waking every refresh.
    #[test]
    fn an_unchanged_streak_stops_after_two_retries() {
        let mut skips = 0;
        let retried: Vec<bool> = (0..6)
            .map(|_| retry_skipped(&mut skips, &Ok(FrameStatus::Skipped)))
            .collect();
        assert_eq!(retried, [true, true, false, false, false, false]);
        // A drawn frame starts over.
        assert!(!retry_skipped(&mut skips, &Ok(FrameStatus::Suboptimal)));
        assert!(retry_skipped(&mut skips, &Ok(FrameStatus::Skipped)));
        // An error neither counts nor resets.
        let before = skips;
        assert!(!retry_skipped(
            &mut skips,
            &Err(Error::NoView(SessionId::from_raw(1)))
        ));
        assert_eq!(skips, before);
    }

    /// The event paths reset the streak: after output (or input, a
    /// resize, an occlusion change, a re-attach) a skip is retried again.
    #[test]
    fn a_new_event_starts_the_count_over() {
        let mut skips = 0;
        for _ in 0..3 {
            retry_skipped(&mut skips, &Ok(FrameStatus::Skipped));
        }
        assert!(!retry_skipped(&mut skips, &Ok(FrameStatus::Skipped)));
        skips = 0; // what `drain_mailbox`, `send_now`, `follow_window` do
        assert!(retry_skipped(&mut skips, &Ok(FrameStatus::Skipped)));
    }

    #[test]
    fn recovery_gives_up_after_three_frames_in_a_row() {
        let mut count = 0;
        let lost = || Err(Error::Gpu(RenderError::SurfaceLost));
        for _ in 0..MAX_RECOVERIES {
            assert!(!gives_up(&mut count, &Ok(FrameStatus::Suboptimal)));
        }
        assert!(gives_up(&mut count, &lost()));

        // A presented frame resets; a skipped one does not.
        let mut count = 0;
        assert!(!gives_up(&mut count, &lost()));
        assert!(!gives_up(&mut count, &lost()));
        assert!(!gives_up(&mut count, &Ok(FrameStatus::Skipped)));
        assert!(!gives_up(&mut count, &Ok(FrameStatus::Presented)));
        for _ in 0..MAX_RECOVERIES {
            assert!(!gives_up(&mut count, &Ok(FrameStatus::Suboptimal)));
        }
        assert!(!gives_up(&mut count, &Ok(FrameStatus::Skipped)));
        assert!(gives_up(&mut count, &Ok(FrameStatus::Suboptimal)));

        // Other errors neither count nor reset.
        let mut count = MAX_RECOVERIES;
        assert!(!gives_up(
            &mut count,
            &Err(Error::NoView(SessionId::from_raw(1)))
        ));
        assert_eq!(count, MAX_RECOVERIES);
    }
}
