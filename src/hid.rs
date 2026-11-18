//! Touch and hardware keys for the device, from the `touch` and `key` commands on stdin.
//!
//! The logic here is sans-IO like the engine: [`Hid`] turns commands into [`Action`]s (a touch
//! report, a button state change) and the session loop performs them through a [`HidBackend`], which
//! is the device's HID service in production and a fake in the tests.
//!
//! Touch coordinates arrive in pixels of the *upright* picture, the one the viewer shows after the
//! framebuffer has been turned by the `config` event's `rotation`. [`map_touch`] undoes that turn and
//! normalizes to the digitizer's 0..=65535 space.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::debug;

use crate::commands::{Key, Press, Touch, TouchPhase};
use crate::frame::{Event, Frame, HidEvent};
use crate::outqueue::OutQueue;

/// The largest value of the digitizer's normalized axes.
pub const AXIS_MAX: f64 = 65535.0;
/// Touch positions are sent at most this often while a finger moves (the newest wins).
pub const MOVE_INTERVAL: Duration = Duration::from_millis(8);
/// Each HID report must go out within this, or it is given up on: a stalled service must not hold
/// the stream loop.
pub const SEND_TIMEOUT: Duration = Duration::from_millis(500);
/// Key presses waiting for their release (held down for the button's hold time) at most.
const MAX_KEYS_HELD: usize = 8;
/// Rate limit on commands that send a report: a burst, then this many a second.
const BURST: f64 = 100.0;
const REFILL_PER_SEC: f64 = 250.0;

// ---- mapping ----------------------------------------------------------------------------------

/// Where the digitizer's axes lie.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HidSpace {
    /// The panel's native axes, worked out from the display's reported native orientation: the
    /// framebuffer point is rotated clockwise by `(360 - native degrees) % 360`. Without display info
    /// it is the identity (and says so). Checked on one device only, an iPad14,3 on iOS 27.0.1.
    Auto,
    /// The framebuffer's own axes: the upright position is turned back by the rotation, nothing more.
    Panel,
    /// The UI's orientation: the upright position is used as it is.
    Ui,
}

/// A flip/swap of the two axes of a normalized point: first the axes are exchanged (`swap_xy`), then
/// x is mirrored (`flip_x`), then y (`flip_y`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Axes {
    pub swap_xy: bool,
    pub flip_x: bool,
    pub flip_y: bool,
}

impl Axes {
    pub fn apply(self, (a, b): (f64, f64)) -> (f64, f64) {
        let (mut x, mut y) = if self.swap_xy { (b, a) } else { (a, b) };
        if self.flip_x {
            x = 1.0 - x;
        }
        if self.flip_y {
            y = 1.0 - y;
        }
        (x, y)
    }

    /// Which input each output axis takes (0 = x, 1 = y) and whether it is mirrored.
    fn terms(self) -> [(usize, bool); 2] {
        let (sx, sy) = if self.swap_xy { (1, 0) } else { (0, 1) };
        [(sx, self.flip_x), (sy, self.flip_y)]
    }

    /// `self` first, then `other`, as one transform in the same form.
    pub fn then(self, other: Axes) -> Axes {
        let first = self.terms();
        let second = other.terms();
        let mut net = [(0usize, false); 2];
        for (out, (src, inv)) in second.iter().enumerate() {
            let (inner_src, inner_inv) = first[*src];
            net[out] = (inner_src, inner_inv ^ inv);
        }
        Axes {
            swap_xy: net[0].0 == 1,
            flip_x: net[0].1,
            flip_y: net[1].1,
        }
    }

    fn is_identity(self) -> bool {
        self == Axes::default()
    }
}

/// The automatic mapping: the framebuffer point turned clockwise by `(360 - native) % 360` to the
/// digitizer's point, as swaps and flips. `None` if the native orientation is unknown or not a right
/// angle.
pub fn auto_axes(native_degrees: Option<u16>) -> Option<Axes> {
    Some(match native_degrees? {
        0 => Axes::default(),
        // A quarter turn clockwise: (fx, fy) -> (1 - fy, fx).
        270 => Axes { swap_xy: true, flip_x: true, flip_y: false },
        // Half a turn: (1 - fx, 1 - fy).
        180 => Axes { swap_xy: false, flip_x: true, flip_y: true },
        // Three quarters clockwise: (fy, 1 - fx).
        90 => Axes { swap_xy: true, flip_x: false, flip_y: true },
        _ => return None,
    })
}

/// How an upright picture position becomes a digitizer position: the space, and swaps and flips
/// applied after it (on the normalized position, after the automatic mapping if there is one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HidMapping {
    pub space: HidSpace,
    pub flip_x: bool,
    pub flip_y: bool,
    pub swap_xy: bool,
}

impl Default for HidMapping {
    fn default() -> Self {
        HidMapping {
            space: HidSpace::Auto,
            flip_x: false,
            flip_y: false,
            swap_xy: false,
        }
    }
}

/// The mapping in effect once the display's native orientation is known (or not): what to show in
/// the `hid` events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effective {
    /// `auto`, `identity (no display info)`, `panel` or `ui`.
    pub label: &'static str,
    /// The net swap and flips from the framebuffer-normalized point (the upright point for `ui`).
    pub axes: Axes,
}

/// The normalized position (0.0..=1.0 on both axes) of an upright picture position in the
/// framebuffer's own axes. `rotation` is the clockwise angle (0, 90, 180, 270) the framebuffer is
/// turned to be upright; `upright_w` x `upright_h` is the upright picture. Positions outside the
/// picture are clamped to its edge; a rotation that is not a right angle counts as 0.
pub fn panel_position(x: f64, y: f64, upright_w: u32, upright_h: u32, rotation: u16) -> (f64, f64) {
    let (w, h) = (f64::from(upright_w.max(1)), f64::from(upright_h.max(1)));
    let (ux, uy) = (x.clamp(0.0, w) / w, y.clamp(0.0, h) / h);
    // Turning the framebuffer clockwise by r gives the upright picture; this is the inverse.
    match rotation % 360 {
        90 => (uy, 1.0 - ux),
        180 => (1.0 - ux, 1.0 - uy),
        270 => (1.0 - uy, ux),
        _ => (ux, uy),
    }
}

fn to_axis(f: f64) -> u16 {
    (f * AXIS_MAX).round().clamp(0.0, AXIS_MAX) as u16
}

/// Maps an upright picture position (pixels) to the digitizer's normalized 0..=65535 space, in the
/// framebuffer's own axes: the inverse of turning the framebuffer clockwise by `rotation`.
pub fn map_touch(x: f64, y: f64, upright_w: u32, upright_h: u32, rotation: u16) -> (u16, u16) {
    let (fx, fy) = panel_position(x, y, upright_w, upright_h, rotation);
    (to_axis(fx), to_axis(fy))
}

impl HidMapping {
    /// The mapping in effect, given the display's native orientation in degrees (if known).
    pub fn effective(&self, native_degrees: Option<u16>) -> Effective {
        let user = Axes {
            swap_xy: self.swap_xy,
            flip_x: self.flip_x,
            flip_y: self.flip_y,
        };
        match self.space {
            HidSpace::Auto => match auto_axes(native_degrees) {
                Some(auto) => Effective { label: "auto", axes: auto.then(user) },
                None => Effective { label: "identity (no display info)", axes: user },
            },
            HidSpace::Panel => Effective { label: "panel", axes: user },
            HidSpace::Ui => Effective { label: "ui", axes: user },
        }
    }

    /// An upright picture position (pixels) to the digitizer's 0..=65535 axes.
    /// `native_degrees` is the primary display's native orientation, if the device said.
    pub fn map(
        &self,
        x: f64,
        y: f64,
        upright_w: u32,
        upright_h: u32,
        rotation: u16,
        native_degrees: Option<u16>,
    ) -> (u16, u16) {
        let axes = self.effective(native_degrees).axes;
        if self.space != HidSpace::Ui && axes.is_identity() {
            return map_touch(x, y, upright_w, upright_h, rotation);
        }
        let rotation = if self.space == HidSpace::Ui { 0 } else { rotation };
        let (a, b) = axes.apply(panel_position(x, y, upright_w, upright_h, rotation));
        (to_axis(a), to_axis(b))
    }
}

/// The picture as the last `config` event described it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct View {
    /// The config's `width` and `height`: the framebuffer's visible size.
    pub width: u32,
    pub height: u32,
    pub rotation: u16,
    /// The display's native orientation in degrees, if the device said.
    pub native: Option<u16>,
}

impl View {
    /// The size of the upright picture: the framebuffer's, with the sides swapped for 90 and 270.
    pub fn upright_size(&self) -> (u32, u32) {
        match self.rotation % 360 {
            90 | 270 => (self.height, self.width),
            _ => (self.width, self.height),
        }
    }
}

/// A `hid` event that carries the mapping in effect (and, for `ready`, the state of the service).
pub fn mapping_event(ready: Option<bool>, message: Option<String>, effective: &Effective) -> Event {
    Event::Hid(HidEvent {
        ready,
        message,
        mapping: Some(effective.label.to_string()),
        swap_xy: Some(effective.axes.swap_xy),
        flip_x: Some(effective.axes.flip_x),
        flip_y: Some(effective.axes.flip_y),
        ..HidEvent::default()
    })
}

// ---- actions and the backend ------------------------------------------------------------------

/// One thing to send to the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// A touchscreen report: a contact sample (`down`) or the release.
    Touch { down: bool, x: u16, y: u16 },
    /// A hardware button state change.
    Button { page: u64, code: u64, down: bool },
}

/// Where actions go: the device's HID services, or a fake.
pub trait HidBackend {
    fn perform(&mut self, action: Action) -> Pin<Box<dyn Future<Output = Result<(), String>> + '_>>;
}

/// The backend when `--hid` is off: every action fails with a message that says why.
pub struct NoHid;

impl HidBackend for NoHid {
    fn perform(&mut self, _: Action) -> Pin<Box<dyn Future<Output = Result<(), String>> + '_>> {
        Box::pin(async { Err("HID is not enabled (start with --hid)".to_string()) })
    }
}

// ---- the controller ---------------------------------------------------------------------------

/// A released-later key: the button is up again at `at`.
#[derive(Debug, Clone, Copy)]
struct KeyUp {
    at: Instant,
    page: u64,
    code: u64,
}

/// Lets a repeating message through once a second.
#[derive(Debug)]
struct ReplyLimit {
    last: Option<Instant>,
    held: u64,
}

pub struct Hid {
    mapping: HidMapping,
    queue: Arc<OutQueue>,
    /// The finger on the screen: where the last report left it.
    finger: Option<(u16, u16)>,
    /// A move waiting for the 8 ms spacing, and when the last touch report went out.
    pending_move: Option<(u16, u16)>,
    last_touch_sent: Option<Instant>,
    keys_down: VecDeque<KeyUp>,
    tokens: f64,
    tokens_at: Option<Instant>,
    replies: ReplyLimit,
    actions: Vec<Action>,
    /// The mapping the client was last told about (None until announced).
    announced: Option<Effective>,
}

impl Hid {
    pub fn new(mapping: HidMapping, queue: Arc<OutQueue>) -> Self {
        Hid {
            mapping,
            queue,
            finger: None,
            pending_move: None,
            last_touch_sent: None,
            keys_down: VecDeque::new(),
            tokens: BURST,
            tokens_at: None,
            replies: ReplyLimit {
                last: None,
                held: 0,
            },
            actions: Vec::new(),
            announced: None,
        }
    }

    /// The mapping in effect as the `ready` event tells it; if it changes later (the display info came
    /// in), the next touch announces the new one.
    pub fn announce(&mut self, effective: Effective) {
        self.announced = Some(effective);
    }

    /// The mapping this controller uses for a display with the given native orientation.
    pub fn effective(&self, native_degrees: Option<u16>) -> Effective {
        self.mapping.effective(native_degrees)
    }
    /// The actions to perform, in order.
    pub fn take_actions(&mut self) -> Vec<Action> {
        std::mem::take(&mut self.actions)
    }

    /// When a held move or a key release is due.
    pub fn next_deadline(&self) -> Option<Instant> {
        let move_due = self
            .pending_move
            .and(self.last_touch_sent)
            .map(|t| t + MOVE_INTERVAL);
        let key_due = self.keys_down.iter().map(|k| k.at).min();
        [move_due, key_due].into_iter().flatten().min()
    }

    #[cfg(test)]
    pub fn finger_down(&self) -> bool {
        self.finger.is_some()
    }

    fn take_token(&mut self, now: Instant) -> bool {
        if let Some(prev) = self.tokens_at {
            let dt = now.saturating_duration_since(prev).as_secs_f64();
            self.tokens = (self.tokens + dt * REFILL_PER_SEC).min(BURST);
        }
        self.tokens_at = Some(now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Writes `{"event":"hid","ok":false,"message":...}`, at most once a second (with a count of
    /// what was held back).
    fn fail(&mut self, now: Instant, message: &str) {
        let quiet = self
            .replies
            .last
            .is_some_and(|t| now.saturating_duration_since(t) < Duration::from_secs(1));
        if quiet {
            self.replies.held += 1;
            return;
        }
        let held = std::mem::take(&mut self.replies.held);
        self.replies.last = Some(now);
        let message = if held > 0 {
            format!("{message} ({held} more failures)")
        } else {
            message.to_string()
        };
        tracing::warn!("hid: {message}");
        self.queue.push_droppable_control(Frame::control(&Event::Hid(HidEvent {
            ok: Some(false),
            message: Some(message),
            ..HidEvent::default()
        })));
    }

    /// A command that could not be parsed.
    pub fn on_bad_command(&mut self, now: Instant, message: &str) {
        self.fail(now, message);
    }

    /// An action the backend could not perform.
    pub fn on_action_failed(&mut self, now: Instant, message: &str) {
        self.fail(now, message);
    }

    /// A `touch` command. Only finger 0 is handled.
    pub fn on_touch(&mut self, now: Instant, touch: Touch, view: Option<View>) {
        if touch.id != 0 {
            debug!("hid: touch with finger id {} ignored (only 0 is handled)", touch.id);
            return;
        }
        let Some(view) = view else {
            self.fail(now, "no picture yet: touch ignored");
            return;
        };
        let (w, h) = view.upright_size();
        let (x, y) = self.mapping.map(touch.x, touch.y, w, h, view.rotation, view.native);
        let effective = self.mapping.effective(view.native);
        debug!(
            "hid: touch {:?} ({}, {}) in the {w}x{h} upright picture, rotation {}, native {:?}, mapping {} {:?} -> digitizer ({x}, {y})",
            touch.phase, touch.x, touch.y, view.rotation, view.native, effective.label, effective.axes
        );
        // The display info arrived (or changed) after the mapping was announced: say so.
        if self.announced.as_ref().is_some_and(|a| *a != effective) {
            self.queue
                .push_control(Frame::control(&mapping_event(None, None, &effective)));
            self.announced = Some(effective);
        }
        match touch.phase {
            TouchPhase::Down => {
                if !self.take_token(now) {
                    return self.fail(now, "too many commands: touch down dropped");
                }
                self.pending_move = None;
                self.send_touch(now, true, x, y);
            }
            TouchPhase::Move => {
                if self.finger.is_none() {
                    debug!("hid: move without a finger down ignored");
                    return;
                }
                let due = self
                    .last_touch_sent
                    .is_none_or(|t| now.saturating_duration_since(t) >= MOVE_INTERVAL);
                if due {
                    self.pending_move = None;
                    self.send_touch(now, true, x, y);
                } else {
                    // Too soon after the last report: only the newest position is kept.
                    self.pending_move = Some((x, y));
                }
            }
            TouchPhase::Up => {
                if self.finger.is_none() {
                    debug!("hid: up without a finger down ignored");
                    return;
                }
                // A lift is never dropped by the rate limit: a finger stuck down is worse.
                self.pending_move = None;
                self.send_touch(now, false, x, y);
            }
        }
    }

    fn send_touch(&mut self, now: Instant, down: bool, x: u16, y: u16) {
        self.actions.push(Action::Touch { down, x, y });
        self.last_touch_sent = Some(now);
        self.finger = down.then_some((x, y));
    }

    /// A `key` command: the button goes down now and up after its hold time (a long press is held
    /// longer; it is one held press, not a repeated one).
    pub fn on_key(&mut self, now: Instant, key: Key, press: Press) {
        if self.keys_down.len() >= MAX_KEYS_HELD {
            return self.fail(now, "too many keys pressed at once: key ignored");
        }
        if !self.take_token(now) {
            return self.fail(now, "too many commands: key dropped");
        }
        let (page, code) = key.usage();
        let hold_ms = key.hold_ms(press);
        debug!("hid: key {key:?} {press:?} (usage page {page:#x}, code {code:#x}, held {hold_ms} ms)");
        self.actions.push(Action::Button {
            page,
            code,
            down: true,
        });
        self.keys_down.push_back(KeyUp {
            at: now + Duration::from_millis(hold_ms),
            page,
            code,
        });
    }

    /// The clock reached [`next_deadline`](Self::next_deadline).
    pub fn on_tick(&mut self, now: Instant) {
        if let Some((x, y)) = self.pending_move {
            let due = self
                .last_touch_sent
                .is_none_or(|t| now.saturating_duration_since(t) >= MOVE_INTERVAL);
            if due && self.finger.is_some() {
                self.pending_move = None;
                self.send_touch(now, true, x, y);
            }
        }
        while let Some(i) = self.keys_down.iter().position(|k| k.at <= now) {
            let k = self.keys_down.remove(i).expect("position is in range");
            self.actions.push(Action::Button {
                page: k.page,
                code: k.code,
                down: false,
            });
        }
    }

    /// Lets go of everything: a finger still down is lifted where it is, and held keys are released.
    /// For a stop, the end of stdin, or a failure.
    pub fn release_all(&mut self) {
        self.pending_move = None;
        if let Some((x, y)) = self.finger.take() {
            self.actions.push(Action::Touch {
                down: false,
                x,
                y,
            });
        }
        for k in self.keys_down.drain(..) {
            self.actions.push(Action::Button {
                page: k.page,
                code: k.code,
                down: false,
            });
        }
    }
}

// ---- the tests --------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(t0: Instant, n: u64) -> Instant {
        t0 + Duration::from_millis(n)
    }

    // The iPad of the measurements: a landscape framebuffer of 2388x1668.
    const FB_W: u32 = 2388;
    const FB_H: u32 = 1668;

    /// All four corners and the centre of an upright picture, as (x, y) -> (digitizer x, digitizer y).
    fn corners(w: u32, h: u32, rotation: u16) -> [(u16, u16); 5] {
        let (w, h) = (f64::from(w), f64::from(h));
        [
            map_touch(0.0, 0.0, w as u32, h as u32, rotation),
            map_touch(w, 0.0, w as u32, h as u32, rotation),
            map_touch(0.0, h, w as u32, h as u32, rotation),
            map_touch(w, h, w as u32, h as u32, rotation),
            map_touch(w / 2.0, h / 2.0, w as u32, h as u32, rotation),
        ]
    }

    #[test]
    fn rotation_0_maps_straight() {
        let m = 65535;
        // top-left, top-right, bottom-left, bottom-right, centre
        assert_eq!(
            corners(FB_W, FB_H, 0),
            [(0, 0), (m, 0), (0, m), (m, m), (32768, 32768)]
        );
    }

    #[test]
    fn rotation_180_maps_through_the_centre() {
        let m = 65535;
        assert_eq!(
            corners(FB_W, FB_H, 180),
            [(m, m), (0, m), (m, 0), (0, 0), (32768, 32768)]
        );
    }

    #[test]
    fn rotation_90_turns_the_framebuffer_clockwise_to_be_upright() {
        // The upright picture is portrait, 1668x2388: the framebuffer (landscape) turned 90 clockwise.
        let m = 65535;
        assert_eq!(
            corners(FB_H, FB_W, 90),
            [
                (0, m),  // upright top-left is the framebuffer's bottom-left
                (0, 0),  // upright top-right is its top-left
                (m, m),  // upright bottom-left is its bottom-right
                (m, 0),  // upright bottom-right is its top-right
                (32768, 32768),
            ]
        );
    }

    #[test]
    fn rotation_270_turns_the_framebuffer_counter_clockwise_to_be_upright() {
        let m = 65535;
        assert_eq!(
            corners(FB_H, FB_W, 270),
            [(m, 0), (m, m), (0, 0), (0, m), (32768, 32768)]
        );
    }

    #[test]
    fn a_point_off_centre_follows_the_turn() {
        // A quarter of the way along the top of a 1000x2000 upright picture, framebuffer turned 90.
        let (x, y) = map_touch(250.0, 0.0, 1000, 2000, 90);
        assert_eq!((x, y), (0, to_axis(0.75)));
        // Halfway down the left edge, turned 270.
        let (x, y) = map_touch(0.0, 1000.0, 1000, 2000, 270);
        assert_eq!((x, y), (to_axis(0.5), 0));
        // Turning the picture by r and mapping back by r is the identity for every angle.
        for r in [0u16, 90, 180, 270] {
            let (uw, uh) = if r % 180 == 90 { (FB_H, FB_W) } else { (FB_W, FB_H) };
            let (x, y) = (uw as f64 * 0.3, uh as f64 * 0.8);
            let (fx, fy) = panel_position(x, y, uw, uh, r);
            // Forward: framebuffer position -> upright, by turning clockwise by r.
            let (ux, uy) = match r {
                0 => (fx, fy),
                90 => (1.0 - fy, fx),
                180 => (1.0 - fx, 1.0 - fy),
                _ => (fy, 1.0 - fx),
            };
            assert!((ux - 0.3).abs() < 1e-9 && (uy - 0.8).abs() < 1e-9, "rotation {r}");
        }
    }

    #[test]
    fn positions_outside_the_picture_are_clamped_and_odd_input_is_safe() {
        assert_eq!(map_touch(-50.0, -1.0, 100, 200, 0), (0, 0));
        assert_eq!(map_touch(500.0, 900.0, 100, 200, 0), (65535, 65535));
        // A rotation that is not a right angle counts as 0; a zero-sized picture does not divide by zero.
        assert_eq!(map_touch(50.0, 100.0, 100, 200, 45), (32768, 32768));
        assert_eq!(map_touch(5.0, 5.0, 0, 0, 0), (65535, 65535));
        assert_eq!(map_touch(f64::NAN, 1.0, 10, 10, 0).1, to_axis(0.1));
        assert_eq!(map_touch(1.0, 1.0, 10, 10, 360), map_touch(1.0, 1.0, 10, 10, 0));
    }

    #[test]
    fn the_ui_space_ignores_the_rotation_and_the_escape_hatches_apply_in_order() {
        let ui = HidMapping {
            space: HidSpace::Ui,
            ..HidMapping::default()
        };
        assert_eq!(ui.map(250.0, 500.0, 1000, 2000, 90, None), (to_axis(0.25), to_axis(0.25)));
        assert_eq!(ui.map(250.0, 500.0, 1000, 2000, 270, None), ui.map(250.0, 500.0, 1000, 2000, 0, None));
        // Panel space is map_touch, whatever the native orientation.
        let panel = HidMapping { space: HidSpace::Panel, ..HidMapping::default() };
        assert_eq!(panel.map(250.0, 500.0, 1000, 2000, 90, None), map_touch(250.0, 500.0, 1000, 2000, 90));
        assert_eq!(panel.map(250.0, 500.0, 1000, 2000, 90, Some(270)), map_touch(250.0, 500.0, 1000, 2000, 90));

        // x = 0.25, y = 0.5 of the picture, upright (rotation 0).
        let base = |flip_x, flip_y, swap_xy| {
            HidMapping { space: HidSpace::Panel, flip_x, flip_y, swap_xy }.map(250.0, 1000.0, 1000, 2000, 0, None)
        };
        assert_eq!(base(false, false, false), (to_axis(0.25), to_axis(0.5)));
        assert_eq!(base(true, false, false), (to_axis(0.75), to_axis(0.5)));
        assert_eq!(base(false, true, false), (to_axis(0.25), to_axis(0.5)));
        assert_eq!(base(false, false, true), (to_axis(0.5), to_axis(0.25)));
        // Swap first, then flip x (which is now the old y): 0.5 -> 0.5, flip y: 0.25 -> 0.75.
        assert_eq!(base(true, true, true), (to_axis(0.5), to_axis(0.75)));
    }

    // ---- the automatic mapping ---------------------------------------------------------------

    const MAX: u16 = 65535;

    /// The framebuffer-normalized point (fx, fy) as an upright position of a 1000x2000 picture at
    /// rotation 0 (where the upright picture is the framebuffer), mapped with `native`.
    fn auto(native: Option<u16>, fx: f64, fy: f64) -> (u16, u16) {
        HidMapping::default().map(fx * 1000.0, fy * 2000.0, 1000, 2000, 0, native)
    }

    #[test]
    fn native_0_is_the_identity() {
        let at = |fx, fy| auto(Some(0), fx, fy);
        assert_eq!(
            [at(0.0, 0.0), at(1.0, 0.0), at(0.0, 1.0), at(1.0, 1.0), at(0.5, 0.5)],
            [(0, 0), (MAX, 0), (0, MAX), (MAX, MAX), (32768, 32768)]
        );
        assert_eq!(at(0.25, 0.5), (to_axis(0.25), to_axis(0.5)));
    }

    #[test]
    fn native_270_turns_the_point_a_quarter_clockwise() {
        // (fx, fy) -> (1 - fy, fx): the iPad's case.
        let at = |fx, fy| auto(Some(270), fx, fy);
        assert_eq!(
            [at(0.0, 0.0), at(1.0, 0.0), at(0.0, 1.0), at(1.0, 1.0), at(0.5, 0.5)],
            [(MAX, 0), (MAX, MAX), (0, 0), (0, MAX), (32768, 32768)]
        );
        assert_eq!(at(0.25, 0.5), (to_axis(0.5), to_axis(0.25)));
        // That is "swap, then flip the first coordinate": what landed on the device as flags.
        let flags = HidMapping { space: HidSpace::Panel, swap_xy: true, flip_x: true, flip_y: false };
        assert_eq!(at(0.25, 0.2), flags.map(250.0, 400.0, 1000, 2000, 0, Some(270)));
    }

    #[test]
    fn native_180_turns_the_point_half_way() {
        let at = |fx, fy| auto(Some(180), fx, fy);
        assert_eq!(
            [at(0.0, 0.0), at(1.0, 0.0), at(0.0, 1.0), at(1.0, 1.0), at(0.5, 0.5)],
            [(MAX, MAX), (0, MAX), (MAX, 0), (0, 0), (32768, 32768)]
        );
        assert_eq!(at(0.25, 0.5), (to_axis(0.75), to_axis(0.5)));
    }

    #[test]
    fn native_90_turns_the_point_three_quarters_clockwise() {
        // (fx, fy) -> (fy, 1 - fx)
        let at = |fx, fy| auto(Some(90), fx, fy);
        assert_eq!(
            [at(0.0, 0.0), at(1.0, 0.0), at(0.0, 1.0), at(1.0, 1.0), at(0.5, 0.5)],
            [(0, MAX), (0, 0), (MAX, MAX), (MAX, 0), (32768, 32768)]
        );
        assert_eq!(at(0.25, 0.5), (to_axis(0.5), to_axis(0.75)));
    }

    #[test]
    fn the_ipad_case_native_270_with_the_framebuffer_upright_or_turned() {
        let m = HidMapping::default();
        // Rotation 0: the picture is the framebuffer, 2388x1668. Upright (597, 834) is (0.25, 0.5);
        // the digitizer point is (1 - 0.5, 0.25).
        assert_eq!(
            m.map(597.0, 834.0, 2388, 1668, 0, Some(270)),
            (to_axis(0.5), to_axis(0.25))
        );
        // The same as the flags that landed on the device.
        let landed = HidMapping { space: HidSpace::Panel, swap_xy: true, flip_x: true, flip_y: false };
        for (x, y) in [(0.0, 0.0), (597.0, 834.0), (2388.0, 1668.0), (1000.0, 100.0), (5.0, 1600.0)] {
            assert_eq!(m.map(x, y, 2388, 1668, 0, Some(270)), landed.map(x, y, 2388, 1668, 0, None), "({x}, {y})");
        }
        // Rotation 90: the upright picture is portrait, 1668x2388. Upright (417, 1194) is (0.25, 0.5);
        // undoing the rotation gives the framebuffer point (fx, fy) = (0.5, 0.75); the digitizer point
        // is (1 - 0.75, 0.5).
        assert_eq!(
            m.map(417.0, 1194.0, 1668, 2388, 90, Some(270)),
            (to_axis(0.25), to_axis(0.5))
        );
        // Corners of the portrait picture: upright top-left is the framebuffer's bottom-left (0, 1),
        // which the digitizer sees as (1 - 1, 0) = (0, 0).
        assert_eq!(m.map(0.0, 0.0, 1668, 2388, 90, Some(270)), (0, 0));
        assert_eq!(m.map(1668.0, 0.0, 1668, 2388, 90, Some(270)), (MAX, 0)); // fb (0,0) -> (1, 0)
        assert_eq!(m.map(0.0, 2388.0, 1668, 2388, 90, Some(270)), (0, MAX)); // fb (1,1) -> (0, 1)
        assert_eq!(m.map(1668.0, 2388.0, 1668, 2388, 90, Some(270)), (MAX, MAX)); // fb (1,0) -> (1, 1)
    }

    #[test]
    fn without_display_info_auto_falls_back_to_the_identity_and_says_so() {
        let m = HidMapping::default();
        let eff = m.effective(None);
        assert_eq!(eff.label, "identity (no display info)");
        assert_eq!(eff.axes, Axes::default());
        assert_eq!(m.map(250.0, 500.0, 1000, 2000, 0, None), map_touch(250.0, 500.0, 1000, 2000, 0));
        assert_eq!(m.map(250.0, 500.0, 1000, 2000, 90, None), map_touch(250.0, 500.0, 1000, 2000, 90));
        // A native orientation that is not a right angle is no information either.
        assert_eq!(m.effective(Some(45)).label, "identity (no display info)");
        // The overrides still apply in the fallback.
        let flipped = HidMapping { flip_x: true, ..HidMapping::default() };
        let eff = flipped.effective(None);
        assert_eq!(eff.label, "identity (no display info)");
        assert_eq!(eff.axes, Axes { flip_x: true, ..Axes::default() });
        assert_eq!(flipped.map(250.0, 500.0, 1000, 2000, 0, None), (to_axis(0.75), to_axis(0.25)));
    }

    #[test]
    fn the_effective_mapping_of_every_space() {
        let auto = HidMapping::default();
        let e = auto.effective(Some(270));
        assert_eq!((e.label, e.axes), ("auto", Axes { swap_xy: true, flip_x: true, flip_y: false }));
        assert_eq!(auto.effective(Some(0)).axes, Axes::default());
        assert_eq!(auto.effective(Some(180)).axes, Axes { swap_xy: false, flip_x: true, flip_y: true });
        assert_eq!(auto.effective(Some(90)).axes, Axes { swap_xy: true, flip_x: false, flip_y: true });
        // Panel and ui ignore the native orientation.
        let panel = HidMapping { space: HidSpace::Panel, ..auto };
        assert_eq!((panel.effective(Some(270)).label, panel.effective(Some(270)).axes), ("panel", Axes::default()));
        let ui = HidMapping { space: HidSpace::Ui, ..auto };
        assert_eq!(ui.effective(Some(270)).label, "ui");
        // The ui space does not undo the rotation either.
        assert_eq!(ui.map(250.0, 500.0, 1000, 2000, 90, Some(270)), (to_axis(0.25), to_axis(0.25)));
    }

    #[test]
    fn the_overrides_apply_after_the_automatic_mapping() {
        // The iPad's point: fx = 0.25, fy = 0.2, native 270: automatic gives (0.8, 0.25).
        let at = |m: HidMapping| m.map(250.0, 400.0, 1000, 2000, 0, Some(270));
        let with = |swap_xy, flip_x, flip_y| HidMapping { space: HidSpace::Auto, flip_x, flip_y, swap_xy };
        assert_eq!(at(with(false, false, false)), (to_axis(0.8), to_axis(0.25)));
        assert_eq!(at(with(false, true, false)), (to_axis(0.2), to_axis(0.25)));
        assert_eq!(at(with(false, false, true)), (to_axis(0.8), to_axis(0.75)));
        assert_eq!(at(with(true, false, false)), (to_axis(0.25), to_axis(0.8)));
        assert_eq!(at(with(true, true, true)), (to_axis(0.75), to_axis(0.2)));
    }

    #[test]
    fn the_net_axes_always_describe_what_map_does() {
        // For every native orientation and every combination of overrides, the effective swap and
        // flips reproduce the mapping on the framebuffer-normalized point.
        let points = [(0.25, 0.2), (0.9, 0.35), (0.0, 1.0), (0.5, 0.5)];
        for native in [None, Some(0), Some(90), Some(180), Some(270)] {
            for bits in 0..8u8 {
                let m = HidMapping {
                    space: HidSpace::Auto,
                    swap_xy: bits & 1 != 0,
                    flip_x: bits & 2 != 0,
                    flip_y: bits & 4 != 0,
                };
                let eff = m.effective(native);
                let user = Axes { swap_xy: m.swap_xy, flip_x: m.flip_x, flip_y: m.flip_y };
                for (fx, fy) in points {
                    let stepwise = user.apply(auto_axes(native).unwrap_or_default().apply((fx, fy)));
                    let net = eff.axes.apply((fx, fy));
                    assert!((stepwise.0 - net.0).abs() < 1e-12 && (stepwise.1 - net.1).abs() < 1e-12,
                        "native {native:?} overrides {bits:03b} at ({fx}, {fy})");
                    let digitizer = m.map(fx * 1000.0, fy * 2000.0, 1000, 2000, 0, native);
                    assert_eq!(digitizer, (to_axis(net.0), to_axis(net.1)));
                }
            }
        }
    }

    #[test]
    fn a_touch_follows_the_automatic_mapping_and_a_changed_mapping_is_announced_once() {
        let (mut h, q, t0) = rig();
        // The ready event said "identity (no display info)".
        h.announce(HidMapping::default().effective(None));
        let ipad = |rotation| Some(View { width: 2388, height: 1668, rotation, native: Some(270) });
        h.on_touch(t0, touch(TouchPhase::Down, 597.0, 834.0), ipad(0));
        h.on_touch(ms(t0, 20), touch(TouchPhase::Up, 597.0, 834.0), ipad(0));
        assert_eq!(
            h.take_actions(),
            vec![
                Action::Touch { down: true, x: to_axis(0.5), y: to_axis(0.25) },
                Action::Touch { down: false, x: to_axis(0.5), y: to_axis(0.25) },
            ]
        );
        // The display info came in: one hid event with the mapping now in effect, not one per touch.
        let c = controls(&q);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0]["event"], "hid");
        assert_eq!(c[0]["mapping"], "auto");
        assert_eq!((c[0]["swap_xy"].as_bool(), c[0]["flip_x"].as_bool(), c[0]["flip_y"].as_bool()), (Some(true), Some(true), Some(false)));
        assert!(c[0].get("ready").is_none());
        // The picture turned (rotation 90): the same upright point is another digitizer point, and
        // the mapping did not change, so nothing is announced.
        h.on_touch(ms(t0, 40), touch(TouchPhase::Down, 417.0, 1194.0), ipad(90));
        assert_eq!(
            h.take_actions(),
            vec![Action::Touch { down: true, x: to_axis(0.25), y: to_axis(0.5) }]
        );
        assert!(controls(&q).is_empty());
    }

    #[test]
    fn nothing_is_announced_before_the_ready_event_names_a_mapping() {
        let (mut h, q, t0) = rig();
        h.on_touch(t0, touch(TouchPhase::Down, 5.0, 5.0), Some(View { width: 100, height: 200, rotation: 0, native: Some(270) }));
        assert!(controls(&q).is_empty());
    }

    #[test]
    fn the_upright_size_swaps_for_quarter_turns() {
        let v = |r| View { width: 2388, height: 1668, rotation: r, native: None };
        assert_eq!(v(0).upright_size(), (2388, 1668));
        assert_eq!(v(90).upright_size(), (1668, 2388));
        assert_eq!(v(180).upright_size(), (2388, 1668));
        assert_eq!(v(270).upright_size(), (1668, 2388));
    }

    // ---- the controller --------------------------------------------------------------------

    fn rig() -> (Hid, Arc<OutQueue>, Instant) {
        let q = Arc::new(OutQueue::new());
        (Hid::new(HidMapping::default(), q.clone()), q, Instant::now())
    }

    const VIEW: Option<View> = Some(View {
        width: 1000,
        height: 2000,
        rotation: 0,
        native: None,
    });

    fn touch(phase: TouchPhase, x: f64, y: f64) -> Touch {
        Touch { phase, x, y, id: 0 }
    }

    fn controls(q: &OutQueue) -> Vec<serde_json::Value> {
        q.drain()
            .iter()
            .filter(|f| f.is_control())
            .map(|f| serde_json::from_slice(&f.payload).unwrap())
            .collect()
    }

    #[test]
    fn down_move_up_become_contact_contact_release() {
        let (mut h, _q, t0) = rig();
        h.on_touch(t0, touch(TouchPhase::Down, 500.0, 1000.0), VIEW);
        assert!(h.finger_down());
        h.on_touch(ms(t0, 20), touch(TouchPhase::Move, 600.0, 1000.0), VIEW);
        h.on_touch(ms(t0, 40), touch(TouchPhase::Up, 600.0, 1000.0), VIEW);
        assert!(!h.finger_down());
        assert_eq!(
            h.take_actions(),
            vec![
                Action::Touch { down: true, x: 32768, y: 32768 },
                Action::Touch { down: true, x: to_axis(0.6), y: 32768 },
                Action::Touch { down: false, x: to_axis(0.6), y: 32768 },
            ]
        );
    }

    #[test]
    fn moves_are_coalesced_to_the_latest_position_every_8_ms() {
        let (mut h, _q, t0) = rig();
        h.on_touch(t0, touch(TouchPhase::Down, 0.0, 0.0), VIEW);
        h.take_actions();
        // A flood of moves inside 8 ms: nothing goes out, the newest is kept.
        for i in 1..=7u64 {
            h.on_touch(ms(t0, i), touch(TouchPhase::Move, i as f64 * 10.0, 0.0), VIEW);
        }
        assert!(h.take_actions().is_empty());
        assert_eq!(h.next_deadline(), Some(ms(t0, 8)));
        // The timer releases the newest one only.
        h.on_tick(ms(t0, 7));
        assert!(h.take_actions().is_empty());
        h.on_tick(ms(t0, 8));
        assert_eq!(
            h.take_actions(),
            vec![Action::Touch { down: true, x: to_axis(0.07), y: 0 }]
        );
        assert_eq!(h.next_deadline(), None);
        // A move after a quiet 8 ms goes out at once.
        h.on_touch(ms(t0, 100), touch(TouchPhase::Move, 500.0, 0.0), VIEW);
        assert_eq!(h.take_actions().len(), 1);
        // And the next within the window waits again.
        h.on_touch(ms(t0, 103), touch(TouchPhase::Move, 510.0, 0.0), VIEW);
        assert!(h.take_actions().is_empty());
    }

    #[test]
    fn an_up_supersedes_a_held_move_and_is_never_delayed() {
        let (mut h, _q, t0) = rig();
        h.on_touch(t0, touch(TouchPhase::Down, 0.0, 0.0), VIEW);
        h.on_touch(ms(t0, 2), touch(TouchPhase::Move, 100.0, 0.0), VIEW);
        h.on_touch(ms(t0, 3), touch(TouchPhase::Up, 200.0, 0.0), VIEW);
        let a = h.take_actions();
        assert_eq!(a.len(), 2, "down, then the release; the held move is moot");
        assert_eq!(a[1], Action::Touch { down: false, x: to_axis(0.2), y: 0 });
        assert_eq!(h.next_deadline(), None);
    }

    #[test]
    fn a_finger_still_down_is_lifted_where_it_is_on_release_all() {
        let (mut h, _q, t0) = rig();
        h.on_touch(t0, touch(TouchPhase::Down, 250.0, 500.0), VIEW);
        h.take_actions();
        h.on_touch(ms(t0, 20), touch(TouchPhase::Move, 300.0, 600.0), VIEW);
        h.take_actions();
        h.release_all(); // stop, or end of stdin
        assert_eq!(
            h.take_actions(),
            vec![Action::Touch { down: false, x: to_axis(0.3), y: to_axis(0.3) }]
        );
        assert!(!h.finger_down());
        // Nothing to lift the second time.
        h.release_all();
        assert!(h.take_actions().is_empty());
    }

    #[test]
    fn a_move_or_up_without_a_finger_and_other_finger_ids_are_ignored() {
        let (mut h, q, t0) = rig();
        h.on_touch(t0, touch(TouchPhase::Move, 1.0, 1.0), VIEW);
        h.on_touch(t0, touch(TouchPhase::Up, 1.0, 1.0), VIEW);
        h.on_touch(t0, Touch { phase: TouchPhase::Down, x: 1.0, y: 1.0, id: 1 }, VIEW);
        h.on_touch(t0, Touch { phase: TouchPhase::Down, x: 1.0, y: 1.0, id: 4_000_000_000 }, VIEW);
        assert!(h.take_actions().is_empty());
        assert!(controls(&q).is_empty(), "ignored quietly");
        assert!(!h.finger_down());
    }

    #[test]
    fn a_second_down_moves_the_finger() {
        let (mut h, _q, t0) = rig();
        h.on_touch(t0, touch(TouchPhase::Down, 100.0, 100.0), VIEW);
        h.on_touch(ms(t0, 1), touch(TouchPhase::Down, 200.0, 200.0), VIEW);
        assert_eq!(h.take_actions().len(), 2);
        assert!(h.finger_down());
    }

    #[test]
    fn the_mapping_follows_the_view_at_the_moment_of_each_touch() {
        let (mut h, _q, t0) = rig();
        let v0 = Some(View { width: 2388, height: 1668, rotation: 0, native: None });
        let v90 = Some(View { width: 2388, height: 1668, rotation: 90, native: None });
        h.on_touch(t0, touch(TouchPhase::Down, 0.0, 0.0), v0);
        h.on_touch(ms(t0, 10), touch(TouchPhase::Up, 0.0, 0.0), v0);
        // The iPad turned: the same pixel of the (now portrait) picture is another digitizer position.
        h.on_touch(ms(t0, 20), touch(TouchPhase::Down, 0.0, 0.0), v90);
        assert_eq!(
            h.take_actions(),
            vec![
                Action::Touch { down: true, x: 0, y: 0 },
                Action::Touch { down: false, x: 0, y: 0 },
                Action::Touch { down: true, x: 0, y: 65535 },
            ]
        );
    }

    #[test]
    fn no_picture_yet_is_an_error_reply_not_a_touch() {
        let (mut h, q, t0) = rig();
        h.on_touch(t0, touch(TouchPhase::Down, 1.0, 1.0), None);
        assert!(h.take_actions().is_empty());
        let c = controls(&q);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0]["event"], "hid");
        assert_eq!(c[0]["ok"], false);
        assert!(c[0]["message"].as_str().unwrap().contains("no picture yet"));
    }

    #[test]
    fn keys_go_down_now_and_up_after_their_hold_time() {
        let (mut h, _q, t0) = rig();
        h.on_key(t0, Key::Home, Press::Short); // 80 ms
        h.on_key(ms(t0, 10), Key::Power, Press::Short); // 200 ms
        assert_eq!(
            h.take_actions(),
            vec![
                Action::Button { page: 0x0C, code: 0x40, down: true },
                Action::Button { page: 0x0C, code: 0x30, down: true },
            ]
        );
        assert_eq!(h.next_deadline(), Some(ms(t0, 80)));
        h.on_tick(ms(t0, 79));
        assert!(h.take_actions().is_empty());
        h.on_tick(ms(t0, 80));
        assert_eq!(h.take_actions(), vec![Action::Button { page: 0x0C, code: 0x40, down: false }]);
        h.on_tick(ms(t0, 500));
        assert_eq!(h.take_actions(), vec![Action::Button { page: 0x0C, code: 0x30, down: false }]);
        assert_eq!(h.next_deadline(), None);
    }

    #[test]
    fn a_long_press_is_one_longer_hold_not_a_repeat() {
        let (mut h, _q, t0) = rig();
        // (key, short hold, long hold) in ms, each pressed long on its own.
        for (key, long_ms) in [
            (Key::Home, 600u64),
            (Key::Power, 1200),
            (Key::Lock, 1200),
            (Key::Siri, 2500),
            (Key::VolumeUp, 600),
            (Key::VolumeDown, 600),
            (Key::Mute, 600),
        ] {
            let (page, code) = key.usage();
            h.on_key(t0, key, Press::Long);
            assert_eq!(h.take_actions(), vec![Action::Button { page, code, down: true }], "{key:?} down");
            assert_eq!(h.next_deadline(), Some(ms(t0, long_ms)), "{key:?} deadline");
            h.on_tick(ms(t0, long_ms - 1));
            assert!(h.take_actions().is_empty(), "{key:?} still held");
            h.on_tick(ms(t0, long_ms));
            assert_eq!(h.take_actions(), vec![Action::Button { page, code, down: false }], "{key:?} up");
            assert_eq!(h.next_deadline(), None);
        }
    }

    #[test]
    fn short_and_long_presses_of_the_same_key_use_their_own_hold_times() {
        let (mut h, _q, t0) = rig();
        h.on_key(t0, Key::Home, Press::Short);
        h.on_key(t0, Key::Home, Press::Long);
        h.take_actions();
        h.on_tick(ms(t0, 80));
        assert_eq!(h.take_actions().len(), 1, "only the short one is up at 80 ms");
        h.on_tick(ms(t0, 599));
        assert!(h.take_actions().is_empty());
        h.on_tick(ms(t0, 600));
        assert_eq!(h.take_actions().len(), 1);
    }

    #[test]
    fn held_keys_are_released_by_release_all() {
        let (mut h, _q, t0) = rig();
        h.on_key(t0, Key::Siri, Press::Short); // 1200 ms
        h.take_actions();
        h.release_all();
        assert_eq!(h.take_actions(), vec![Action::Button { page: 0x0C, code: 0xCF, down: false }]);
        assert_eq!(h.next_deadline(), None);
    }

    #[test]
    fn a_flood_of_key_presses_is_limited_and_reported_once() {
        let (mut h, q, t0) = rig();
        for i in 0..50u64 {
            h.on_key(ms(t0, i), Key::Siri, Press::Short);
        }
        // Only the ones that fit were sent: MAX_KEYS_HELD of them.
        assert_eq!(h.take_actions().len(), MAX_KEYS_HELD);
        let c = controls(&q);
        assert_eq!(c.len(), 1, "one reply for the flood, not 42");
        assert_eq!(c[0]["ok"], false);
    }

    #[test]
    fn the_rate_limit_drops_downs_but_never_lifts() {
        let (mut h, q, t0) = rig();
        // 400 down commands in the same instant: the burst gets through, the rest does not.
        for _ in 0..400 {
            h.on_touch(t0, touch(TouchPhase::Down, 1.0, 1.0), VIEW);
        }
        let sent = h.take_actions().len();
        assert_eq!(sent, BURST as usize);
        // The finger is down, and the lift still goes out.
        h.on_touch(t0, touch(TouchPhase::Up, 1.0, 1.0), VIEW);
        assert_eq!(h.take_actions(), vec![Action::Touch { down: false, x: to_axis(0.001), y: to_axis(0.0005) }]);
        // The limit refills with time.
        h.on_touch(ms(t0, 1000), touch(TouchPhase::Down, 1.0, 1.0), VIEW);
        assert_eq!(h.take_actions().len(), 1);
        assert!(!controls(&q).is_empty());
    }

    #[test]
    fn failures_are_reported_at_most_once_a_second() {
        let (mut h, q, t0) = rig();
        for i in 0..10u64 {
            h.on_action_failed(ms(t0, i * 10), "the service is gone");
        }
        h.on_action_failed(ms(t0, 1500), "the service is gone");
        let c = controls(&q);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0]["message"], "the service is gone");
        assert_eq!(c[1]["message"], "the service is gone (9 more failures)");
        h.on_bad_command(ms(t0, 5000), "bad touch command");
        assert_eq!(controls(&q)[0]["message"], "bad touch command");
    }

    #[test]
    fn the_hid_event_json() {
        let ready = Frame::control(&Event::Hid(HidEvent { ready: Some(true), ..HidEvent::default() }));
        assert_eq!(ready.payload, br#"{"event":"hid","ready":true}"#);
        let not_ready = Frame::control(&Event::Hid(HidEvent {
            ready: Some(false),
            message: Some("no service".into()),
            ..HidEvent::default()
        }));
        assert_eq!(not_ready.payload, br#"{"event":"hid","ready":false,"message":"no service"}"#);
        let failed = Frame::control(&Event::Hid(HidEvent {
            ok: Some(false),
            message: Some("x".into()),
            ..HidEvent::default()
        }));
        assert_eq!(failed.payload, br#"{"event":"hid","ok":false,"message":"x"}"#);
        let warned = Frame::control(&Event::Hid(HidEvent { warning: Some("w".into()), ..HidEvent::default() }));
        assert_eq!(warned.payload, br#"{"event":"hid","warning":"w"}"#);
        // The ready event with the mapping (the iPad's: native rot270).
        let eff = HidMapping::default().effective(Some(270));
        let ready = Frame::control(&mapping_event(Some(true), None, &eff));
        assert_eq!(
            ready.payload,
            br#"{"event":"hid","ready":true,"mapping":"auto","swap_xy":true,"flip_x":true,"flip_y":false}"#
        );
        let eff = HidMapping::default().effective(None);
        let ready = Frame::control(&mapping_event(Some(true), Some("no buttons".into()), &eff));
        assert_eq!(
            ready.payload,
            br#"{"event":"hid","ready":true,"message":"no buttons","mapping":"identity (no display info)","swap_xy":false,"flip_x":false,"flip_y":false}"#
        );
    }
}
