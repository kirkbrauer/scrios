//! The window: winit's event loop, the renderer, and what connects them to the stream (see `pipeline`).

use std::sync::Arc;
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

use tracing::{debug, error, info, warn};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::ModifiersState;
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
use winit::window::{Window, WindowId};

use crate::commands::Command;
use crate::frame::Event;
use crate::stream::{EXIT_FAILED, EXIT_START_FAILED};
use crate::viewer::geometry::{
    Fitter, Layout, PictureGeometry, Refit, Rotation, fit_size, refit_size, scaled_pixels, whole_pixels,
};
use crate::viewer::input::{Action, KeyPress, Keyboard, Pointer, ShortcutModifier, swipe_up};
use crate::viewer::pipeline::{Control, Feed, FrameSlot, Wake};
use crate::viewer::render::Renderer;
use crate::viewer::stats::{Meter, Status, stats_line, title};

/// How long closing the window waits for the stream to stop on the device (a stop can take about 8 seconds).
const STOP_GRACE: Duration = Duration::from_secs(12);
/// The picture the first window is shaped for, until the stream says what the device's screen is: a phone, upright.
const FIRST_PICTURE: (u32, u32) = (1206, 2622);
/// What of the screen the first window may take: the width and the height it has to leave for the menu bar and the
/// dock (or the task bar) and the window's own title bar.
const SCREEN_SHARE: (f64, f64) = (0.9, 0.85);
/// A screen whose size is not known.
const FALLBACK_SCREEN: (f64, f64) = (1280.0, 720.0);

/// What the window is told to do, from the command line.
#[derive(Debug, Clone)]
pub struct WindowOptions {
    pub title: String,
    /// The largest the first window may be, if the user asked (logical pixels).
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub show_stats: bool,
    pub control: bool,
    pub lock_shortcut: bool,
    /// The margin around the picture, in logical pixels (0: none).
    pub edge_margin: u32,
    /// How the decoder is named in the log (it is on another thread).
    pub decoder: &'static str,
}

/// Runs the window on this thread until it is closed or the stream ends, then stops the stream and returns the exit
/// status: that of the stream's [`Outcome`](crate::stream::Outcome), or [`EXIT_START_FAILED`] if there was no window.
pub fn run(
    options: WindowOptions,
    feed: Arc<dyn Feed>,
    slot: Arc<FrameSlot>,
    control: Control,
    events: EventLoop<Wake>,
) -> i32 {
    let mut app = App::new(options, feed, slot, control);
    events.set_control_flow(ControlFlow::Wait);
    if let Err(e) = events.run_app(&mut app) {
        error!("the window's event loop failed: {e}");
        app.exit = app.exit.or(Some(EXIT_FAILED));
    }
    app.finish()
}

struct App {
    options: WindowOptions,
    feed: Arc<dyn Feed>,
    slot: Arc<FrameSlot>,
    control: Control,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    /// What the last `config` said (or, until one does, what the first picture is).
    geometry: Option<PictureGeometry>,
    /// What the window was sized for.
    fitter: Fitter,
    keyboard: Keyboard,
    pointer: Pointer,
    modifiers: ModifiersState,
    /// The pointer's last position in the window, physical pixels.
    cursor: PhysicalPosition<f64>,
    /// The pointer is in the margin below the picture and the hint of the edge swipe is shown.
    hint: bool,
    status: Status,
    stats: Option<String>,
    note: Option<String>,
    meter: Meter,
    /// Access units the stream dropped because the viewer was too slow (from its `stats`).
    stream_dropped: u64,
    /// The size of the screen in logical pixels, less what the window must leave.
    screen: (u32, u32),
    /// The window was closed (or the stream ended): it is being stopped.
    closing: bool,
    /// The exit status, once it is known.
    exit: Option<i32>,
}

impl App {
    fn new(options: WindowOptions, feed: Arc<dyn Feed>, slot: Arc<FrameSlot>, control: Control) -> App {
        let note = (!options.control).then(|| "view only".to_owned());
        let keyboard = Keyboard::new(ShortcutModifier::host_default(), options.lock_shortcut);
        App {
            options,
            feed,
            slot,
            control,
            window: None,
            renderer: None,
            geometry: None,
            fitter: Fitter::default(),
            keyboard,
            pointer: Pointer::default(),
            modifiers: ModifiersState::empty(),
            cursor: PhysicalPosition::new(0.0, 0.0),
            hint: false,
            status: Status::Connecting,
            stats: None,
            note,
            meter: Meter::new(Instant::now()),
            stream_dropped: 0,
            screen: (1152, 612),
            closing: false,
            exit: None,
        }
    }

    fn set_title(&self) {
        if let Some(window) = &self.window {
            let status = match &self.status {
                Status::Live { .. } => Status::Live { stats: self.stats.clone(), note: self.note.clone() },
                other => other.clone(),
            };
            window.set_title(&title(&self.options.title, &status));
        }
    }

    /// The margin round the picture in physical pixels: `--edge-margin` is in logical ones.
    fn margin(&self) -> u32 {
        let scale = self.window.as_ref().map_or(1.0, |w| w.scale_factor());
        scaled_pixels(self.options.edge_margin, scale)
    }

    /// How the window is laid out, for the mouse and the drawing.
    fn layout(&self) -> Option<Layout> {
        let renderer = self.renderer.as_ref()?;
        Some(Layout { window: renderer.size(), upright: self.geometry?.upright(), margin: self.margin() })
    }

    /// Shows the hint while the pointer is in the margin below the picture and no finger is down.
    fn update_hint(&mut self) {
        let hint = !self.pointer.is_down()
            && self.layout().is_some_and(|l| l.in_bottom_margin((self.cursor.x, self.cursor.y)));
        if hint != self.hint {
            self.hint = hint;
            if let Some(window) = &self.window {
                window.request_redraw();
            }
        }
    }

    /// Sends the actions a key made.
    fn act(&mut self, event_loop: &ActiveEventLoop, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::Send(command) => self.control.send(command),
                Action::Close => self.close(event_loop),
                Action::SwipeUp => {
                    // Not over a finger that is down, nor over another gesture; with no picture there is no edge.
                    if let Some(g) = self.geometry
                        && !self.pointer.is_down()
                    {
                        // Best effort: not started if one is playing already.
                        let _ = self.control.gesture(swipe_up(g.upright()));
                    }
                }
            }
        }
    }

    /// Lifts the finger and every key that is down on the device.
    fn release_inputs(&mut self) {
        if let Some(command) = self.pointer.cancel() {
            self.control.send(command);
        }
        for action in self.keyboard.release_all() {
            if let Action::Send(command) = action {
                self.control.send(command);
            }
        }
    }

    fn close(&mut self, event_loop: &ActiveEventLoop) {
        if self.closing {
            return;
        }
        self.closing = true;
        self.release_inputs();
        self.status = Status::Stopping;
        self.set_title();
        // The window goes at once: stopping the stream on the device takes a few seconds.
        if let Some(window) = &self.window {
            window.set_visible(false);
        }
        event_loop.exit();
    }

    /// Stops the stream, waits for it to end (a little), and gives the exit status. Safe to call again.
    fn finish(&mut self) -> i32 {
        if let Some(code) = self.exit {
            return code;
        }
        let feed = self.feed.clone();
        let (tx, rx) = channel();
        let spawned = std::thread::Builder::new().name("scrios-viewer-stop".into()).spawn(move || {
            // Nothing happens if the stream is over already.
            let _ = feed.send(Command::Stop);
            let _ = tx.send(feed.wait());
        });
        let code = match spawned {
            Ok(_) => rx.recv_timeout(STOP_GRACE).unwrap_or_else(|_| {
                warn!("the stream did not stop within {} s; leaving it", STOP_GRACE.as_secs());
                self.feed.hang_up();
                0
            }),
            Err(e) => {
                warn!("cannot wait for the stream to stop: {e}");
                self.feed.hang_up();
                0
            }
        };
        self.exit = Some(code);
        code
    }

    /// What the stream's `config` says: the picture's size and turn.
    fn on_config(&mut self, geometry: PictureGeometry) {
        let upright = geometry.upright();
        if self.geometry.map(|g| g.upright()) != Some(upright) {
            // The same point of the window is another point of the picture now.
            if let Some(command) = self.pointer.cancel() {
                self.control.send(command);
            }
        }
        self.geometry = Some(geometry);
        // The window is sized by the config, not by a picture that came first (which only shows what it is). Without a
        // window yet it will be sized from this geometry when it opens.
        let refit = self.fitter.shape(upright);
        if let (Some(refit), Some(window)) = (refit, &self.window) {
            let current = (refit == Refit::Turned).then(|| {
                let size = window.inner_size().to_logical::<f64>(window.scale_factor());
                (whole_pixels(size.width), whole_pixels(size.height))
            });
            let (w, h) = refit_size(
                upright,
                current,
                self.screen,
                self.options.width,
                self.options.height,
                self.options.edge_margin,
            );
            debug!("the picture is {upright:?} upright: window {w}x{h}");
            let _ = window.request_inner_size(LogicalSize::new(w, h));
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn on_event(&mut self, event: Event) {
        match event {
            Event::Config { .. } => {
                if let Some(geometry) = PictureGeometry::from_event(&event) {
                    self.on_config(geometry);
                }
            }
            Event::Stats(stats) => self.stream_dropped = stats.dropped_units,
            Event::Hid(hid) => {
                // The device could not be given touch or keys: the window still shows it.
                if hid.ready == Some(false) {
                    let why = hid.message.as_deref().unwrap_or("not available");
                    warn!("touch and keys are not available: {why}");
                    self.note = Some("view only (touch and keys unavailable)".to_owned());
                    self.set_title();
                } else if hid.keyboard == Some(false) {
                    self.note = Some("no keyboard".to_owned());
                    self.set_title();
                }
            }
            Event::Error { message } => {
                self.status = Status::Ended(message);
                self.set_title();
            }
            Event::Stopped => {}
        }
    }

    fn redraw(&mut self, event_loop: &ActiveEventLoop) {
        let margin = self.margin();
        let Some(renderer) = self.renderer.as_mut() else { return };
        let frame = self.slot.take();
        self.meter.stale(self.slot.take_stale());
        let mut timing = None;
        if let Some(frame) = frame {
            if self.geometry.is_none() {
                // A picture before the stream said what the screen is: show it as it is.
                self.geometry = Some(PictureGeometry {
                    width: frame.picture.width,
                    height: frame.picture.height,
                    rotation: Rotation::R0,
                });
            }
            renderer.upload(&frame.picture);
            timing = Some((frame.arrived, frame.decode));
        }
        if let Err(e) = renderer.render(self.geometry.as_ref(), margin, self.hint) {
            error!("cannot draw: {e}");
            self.exit = Some(EXIT_FAILED);
            event_loop.exit();
            return;
        }
        let now = Instant::now();
        if let Some((arrived, decode)) = timing {
            self.meter.shown(now, arrived, decode);
            if !matches!(self.status, Status::Live { .. }) && !self.closing {
                self.status = Status::Live { stats: None, note: None };
                self.set_title();
            }
        }
        if let Some(report) = self.meter.report(now) {
            let line = stats_line(&report, self.stream_dropped);
            debug!("{line}");
            if self.options.show_stats && matches!(self.status, Status::Live { .. }) {
                self.stats = Some(line);
                self.set_title();
            }
        }
    }

    fn key(&mut self, event_loop: &ActiveEventLoop, event: &winit::event::KeyEvent) {
        let logical = event.key_without_modifiers();
        let press = KeyPress {
            physical: event.physical_key,
            logical: &logical,
            text: event.text.as_deref(),
            pressed: event.state == ElementState::Pressed,
            repeat: event.repeat,
            modifiers: self.modifiers,
        };
        let actions = self.keyboard.key(&press);
        self.act(event_loop, actions);
    }

    fn mouse(&mut self, state: ElementState) {
        let Some(layout) = self.layout() else { return };
        let at = (self.cursor.x, self.cursor.y);
        let command = match state {
            // A gesture of the keyboard is being played: the mouse waits for it.
            ElementState::Pressed if self.control.gesture_active() => None,
            ElementState::Pressed => self.pointer.pressed(layout, at),
            ElementState::Released => self.pointer.released(layout, at),
        };
        if let Some(command) = command {
            self.control.send(command);
        }
        self.update_hint();
    }
}

impl ApplicationHandler<Wake> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() || self.closing {
            return;
        }
        let monitor = event_loop.primary_monitor().or_else(|| event_loop.available_monitors().next());
        let screen = monitor.map_or(FALLBACK_SCREEN, |m| {
            let size = m.size().to_logical::<f64>(m.scale_factor());
            (size.width, size.height)
        });
        self.screen = (whole_pixels(screen.0 * SCREEN_SHARE.0), whole_pixels(screen.1 * SCREEN_SHARE.1));
        // The stream may have said what the picture is before there was a window to size.
        let picture = self.geometry.map_or(FIRST_PICTURE, |g| g.upright());
        let (w, h) = fit_size(picture, self.screen, self.options.width, self.options.height, self.options.edge_margin);
        let attributes = Window::default_attributes()
            .with_title(title(&self.options.title, &Status::Connecting))
            .with_inner_size(LogicalSize::new(w, h))
            .with_min_inner_size(LogicalSize::new(80, 80));
        let window = match event_loop.create_window(attributes) {
            Ok(window) => Arc::new(window),
            Err(e) => {
                error!("cannot open the window: {e}");
                self.exit = Some(EXIT_START_FAILED);
                event_loop.exit();
                return;
            }
        };
        match Renderer::new(window.clone(), event_loop.owned_display_handle()) {
            Ok(renderer) => self.renderer = Some(renderer),
            Err(e) => {
                error!("{e}");
                self.exit = Some(EXIT_START_FAILED);
                event_loop.exit();
                return;
            }
        }
        info!("window {w}x{h} (logical), decoder: {}", self.options.decoder);
        self.window = Some(window);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, wake: Wake) {
        match wake {
            Wake::Frame => {
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            Wake::Event(event) => self.on_event(event),
            Wake::Failed(why) => {
                error!("{why}");
                self.exit = Some(EXIT_FAILED);
                self.close(event_loop);
            }
            Wake::Ended => self.close(event_loop),
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => self.close(event_loop),
            WindowEvent::Resized(size) => {
                if let Some(renderer) = self.renderer.as_mut() {
                    renderer.resize(size.width, size.height);
                }
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => self.redraw(event_loop),
            WindowEvent::ModifiersChanged(modifiers) => self.modifiers = modifiers.state(),
            WindowEvent::Focused(false) => self.release_inputs(),
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = position;
                if let Some(layout) = self.layout()
                    && let Some(command) = self.pointer.moved(layout, (position.x, position.y))
                {
                    self.control.send(command);
                }
                self.update_hint();
            }
            WindowEvent::CursorLeft { .. } => {
                // Far outside: nowhere to be in the margin.
                self.cursor = PhysicalPosition::new(-1.0, -1.0);
                self.update_hint();
            }
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => self.mouse(state),
            // Keys the window held when it got the focus are not presses of its own.
            WindowEvent::KeyboardInput { event, is_synthetic: false, .. } => self.key(event_loop, &event),
            _ => {}
        }
    }

    /// Reached by closing the window and by the system quitting the program (Cmd+Q): either way the stream is stopped
    /// on the device before the process goes.
    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        self.finish();
    }
}
