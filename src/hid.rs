//! Touch and hardware keys for the device, from the `touch` and `key` commands on stdin.
//!
//! The logic here is sans-IO like the engine: [`Hid`] turns commands into [`Action`]s (a touch
//! report, a button state change) and the session loop performs them through a [`HidBackend`], which
//! is the device's HID service in production and a fake in the tests.
//!
//! Touch coordinates arrive in pixels of the *upright* picture, the one the viewer shows after the
//! framebuffer has been turned by the `config` event's `rotation`. [`map_touch`] undoes that turn and
//! normalizes to the digitizer's 0..=65535 space: pixel 0 of an axis is 0 and its last pixel (size - 1)
//! is 65535, so that a touch on the picture's outermost row or column is on the digitizer's very edge.

use std::collections::{BTreeSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{debug, info};

use crate::commands::{Kb, Key, MAX_TYPE_CHARS, Press, Touch, TouchPhase};
use crate::frame::{Event, Frame, HidEvent};
use crate::outqueue::OutQueue;

/// The largest value of the digitizer's normalized axes.
pub const AXIS_MAX: f64 = 65535.0;
/// Touch positions are sent at most this often while a finger moves (the newest wins).
pub const MOVE_INTERVAL: Duration = Duration::from_millis(8);
/// Each HID report must go out within this, or it is given up on: a stalled service must not keep the
/// reports behind it waiting for long (the stream loop does not wait for it at all, see
/// [`ActionQueue`]).
pub const SEND_TIMEOUT: Duration = Duration::from_millis(500);
/// Key presses waiting for their release (held down for the button's hold time) at most.
const MAX_KEYS_HELD: usize = 8;
/// While typing, a key is down this long, and the next character follows this long after its release.
pub const TYPE_KEY_HOLD: Duration = Duration::from_millis(8);
pub const TYPE_CHAR_GAP: Duration = Duration::from_millis(4);
/// Keys held without any keyboard command for this long are let go (a stuck key must not outlive the viewer).
pub const KEYBOARD_IDLE_RELEASE: Duration = Duration::from_secs(30);
/// Typing that has fallen this far behind its schedule (a busy loop) shifts the rest of the schedule instead of bursting.
const TYPE_LAG: Duration = Duration::from_millis(50);
/// Left Shift in the HID keyboard page.
pub const LEFT_SHIFT: u8 = 0xE1;
/// The App Switcher is the Home button pressed twice: each press is held for the short Home hold
/// (80 ms), and the second goes down this long after the first came up, so that the two downs are
/// 200 ms apart (the device wants them within 250 ms; 400 ms was measured to be too slow).
pub const APP_SWITCHER_GAP: Duration = Duration::from_millis(120);
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
        Axes { swap_xy: net[0].0 == 1, flip_x: net[0].1, flip_y: net[1].1 }
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
        HidMapping { space: HidSpace::Auto, flip_x: false, flip_y: false, swap_xy: false }
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

/// The fraction (0.0..=1.0) of the way along an axis of `size` pixels that pixel position `p` is:
/// `p / (size - 1)`, so pixel 0 is exactly 0.0 and the last pixel, `size - 1`, exactly 1.0 (the edge
/// recognisers of iPadOS want the first contact at the very edge of the digitizer, which a row
/// of pixels `size - 1` of `size` would stop short of by one pixel's worth, 27 units on a 2388-pixel
/// axis). A position at or beyond the last pixel is 1.0, and one before the first is 0.0. This is a
/// slight rescale of the earlier `p / size`: a touch moves by `p / (size * (size - 1))` of the axis,
/// which is nothing at pixel 0, half a pixel mid-picture and one pixel at the far edge.
/// A picture one pixel wide has no span: 0.0 for pixel 0 and before, 1.0 beyond.
fn axis_fraction(p: f64, size: u32) -> f64 {
    let span = f64::from(size.max(1) - 1);
    if span == 0.0 {
        return if p > 0.0 { 1.0 } else { 0.0 };
    }
    p.clamp(0.0, span) / span
}

/// The normalized position (0.0..=1.0 on both axes) of an upright picture position in the
/// framebuffer's own axes. `rotation` is the clockwise angle (0, 90, 180, 270) the framebuffer is
/// turned to be upright; `upright_w` x `upright_h` is the upright picture. The first pixel of an
/// axis is 0.0 and the last (size - 1) is 1.0; positions outside the picture are clamped to its edge;
/// a rotation that is not a right angle counts as 0.
pub fn panel_position(x: f64, y: f64, upright_w: u32, upright_h: u32, rotation: u16) -> (f64, f64) {
    let (ux, uy) = (axis_fraction(x, upright_w), axis_fraction(y, upright_h));
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

/// The sides of the upright picture that an upright position touches (its outermost column or
/// row, or beyond), such as `left`, `bottom` or `right+bottom`; `None` for a position inside.
fn picture_edges(px: f64, py: f64, upright_w: u32, upright_h: u32) -> Option<String> {
    let mut sides = Vec::new();
    if px <= 0.0 {
        sides.push("left");
    } else if px >= f64::from(upright_w.max(1) - 1) {
        sides.push("right");
    }
    if py <= 0.0 {
        sides.push("top");
    } else if py >= f64::from(upright_h.max(1) - 1) {
        sides.push("bottom");
    }
    (!sides.is_empty()).then(|| sides.join("+"))
}

/// The log line for a touch `down` that lands on the very edge of the digitizer (its mapped x or y
/// is 0 or 65535), or `None` for a touch that does not. `px` and `py` are the upright picture
/// position as it came; the edge named is the picture's own (a corner names both sides).
pub fn edge_down_message(x: u16, y: u16, px: f64, py: f64, upright_w: u32, upright_h: u32) -> Option<String> {
    let at_limit = |v: u16| v == 0 || f64::from(v) == AXIS_MAX;
    if !at_limit(x) && !at_limit(y) {
        return None;
    }
    let edge = picture_edges(px, py, upright_w, upright_h).unwrap_or_else(|| "?".into());
    Some(format!("edge touch down at HID ({x}, {y}) from upright pixel ({px}, {py}), edge: {edge}"))
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
        let user = Axes { swap_xy: self.swap_xy, flip_x: self.flip_x, flip_y: self.flip_y };
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
    /// A key of the keyboard page (by usage) goes down or up in the virtual keyboard's pressed set.
    Key { usage: u8, down: bool },
    /// Every key of the virtual keyboard up (an empty report, then the device's own reset).
    KeyboardRelease,
    /// The virtual keyboard is taken down (the end of the session).
    KeyboardRemove,
}

impl Action {
    /// How long the backend may take to perform it.
    pub fn timeout(self) -> Duration {
        match self {
            // Taking the keyboard down asks the device to list its surfaces to confirm it is gone.
            Action::KeyboardRemove => Duration::from_secs(2),
            _ => SEND_TIMEOUT,
        }
    }

    /// Whether it lets go of something (a lift, a button or key up, the keyboard released or removed). These
    /// are never refused by a full [`ActionQueue`]: what is held down must come up.
    pub fn is_release(self) -> bool {
        match self {
            Action::Touch { down, .. } | Action::Button { down, .. } | Action::Key { down, .. } => !down,
            Action::KeyboardRelease | Action::KeyboardRemove => true,
        }
    }
}

/// The most actions that wait for the HID service (not counting releases, see [`ActionQueue::push`]).
pub const ACTION_QUEUE_CAP: usize = 64;

/// The actions waiting to be performed, in the order they were made: what stands between the controller
/// and a HID service that is slow or stuck, so that the stream loop never waits for the service.
///
/// Order is kept: nothing is reordered, and a finger down, a lift, a button or a key is never dropped
/// to make room (a refused press is reported to the caller, a release is always taken). The only
/// thing that may be lost is a touch *move*, a mere sample of where the finger is: a move that is
/// waiting while the next one arrives is replaced by it, and when the queue is full the oldest move
/// makes room.
#[derive(Debug, Default)]
pub struct ActionQueue {
    items: VecDeque<Queued>,
    /// Whether the finger is down as of the last action queued: the next contact sample is then a
    /// move, not a down.
    finger_down: bool,
}

#[derive(Debug, Clone, Copy)]
struct Queued {
    action: Action,
    /// A contact sample of a finger that was down already.
    is_move: bool,
}

impl ActionQueue {
    /// Queues `action`. Returns false if it was refused: the queue is full of presses and the action
    /// is another one (never a release, which is always taken, and never a move, which replaces the
    /// pending move or the oldest one).
    pub fn push(&mut self, action: Action) -> bool {
        let is_move = self.finger_down && matches!(action, Action::Touch { down: true, .. });
        if is_move && let Some(tail) = self.items.back_mut().filter(|q| q.is_move) {
            tail.action = action; // the newest position wins; the order does not change
            return true;
        }
        if self.items.len() >= ACTION_QUEUE_CAP {
            match self.items.iter().position(|q| q.is_move) {
                Some(oldest) => {
                    self.items.remove(oldest);
                }
                None if action.is_release() => {}
                None => return false,
            }
        }
        if let Action::Touch { down, .. } = action {
            self.finger_down = down;
        }
        self.items.push_back(Queued { action, is_move });
        true
    }

    /// The next action to perform.
    pub fn pop(&mut self) -> Option<Action> {
        self.items.pop_front().map(|q| q.action)
    }

    /// Forgets the moves that wait (the finger is about to be lifted anyway); how many there were.
    pub fn drop_moves(&mut self) -> usize {
        let before = self.items.len();
        self.items.retain(|q| !q.is_move);
        before - self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }
}

/// The US keyboard layout: the usage of the key that types `c`, and whether Shift is needed. Letters,
/// digits, the printable ASCII punctuation, space, newline (Return) and tab; nothing else.
pub fn us_layout(c: char) -> Option<(u8, bool)> {
    let n = c as u32;
    Some(match c {
        'a'..='z' => (0x04 + (n - 'a' as u32) as u8, false),
        'A'..='Z' => (0x04 + (n - 'A' as u32) as u8, true),
        '1'..='9' => (0x1E + (n - '1' as u32) as u8, false),
        '0' => (0x27, false),
        '\n' => (0x28, false), // Return
        '\t' => (0x2B, false), // Tab
        ' ' => (0x2C, false),
        '!' => (0x1E, true),
        '@' => (0x1F, true),
        '#' => (0x20, true),
        '$' => (0x21, true),
        '%' => (0x22, true),
        '^' => (0x23, true),
        '&' => (0x24, true),
        '*' => (0x25, true),
        '(' => (0x26, true),
        ')' => (0x27, true),
        '-' => (0x2D, false),
        '_' => (0x2D, true),
        '=' => (0x2E, false),
        '+' => (0x2E, true),
        '[' => (0x2F, false),
        '{' => (0x2F, true),
        ']' => (0x30, false),
        '}' => (0x30, true),
        '\\' => (0x31, false),
        '|' => (0x31, true),
        ';' => (0x33, false),
        ':' => (0x33, true),
        '\'' => (0x34, false),
        '"' => (0x34, true),
        '`' => (0x35, false),
        '~' => (0x35, true),
        ',' => (0x36, false),
        '<' => (0x36, true),
        '.' => (0x37, false),
        '>' => (0x37, true),
        '/' => (0x38, false),
        '?' => (0x38, true),
        _ => return None,
    })
}

/// Where actions go: the device's HID services, or a fake.
pub trait HidBackend {
    fn perform(&mut self, action: Action) -> Pin<Box<dyn Future<Output = Result<(), String>> + '_>>;
}

/// The backend when control is off: every action fails with a message that says why.
#[derive(Debug)]
pub struct NoHid;

impl HidBackend for NoHid {
    fn perform(&mut self, action: Action) -> Pin<Box<dyn Future<Output = Result<(), String>> + '_>> {
        Box::pin(async move {
            // Letting go of what was never pressed is nothing to complain about.
            if matches!(action, Action::KeyboardRelease | Action::KeyboardRemove) {
                return Ok(());
            }
            Err("touch and keys are not enabled (control is off: see --no-control)".to_string())
        })
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

/// A later button state change of a double press (the App Switcher), at the time it is due.
#[derive(Debug, Clone, Copy)]
struct ButtonStep {
    at: Instant,
    page: u64,
    code: u64,
    down: bool,
}

/// One step of a string being typed, at the time it is due.
#[derive(Debug, Clone, Copy)]
struct TypeStep {
    at: Instant,
    op: TypeOp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TypeOp {
    ShiftDown,
    KeyDown(u8),
    KeyUp(u8),
    ShiftUp,
}

/// Lets a repeating message through once a second.
#[derive(Debug)]
struct ReplyLimit {
    last: Option<Instant>,
    held: u64,
}

#[derive(Debug)]
pub struct Hid {
    mapping: HidMapping,
    queue: Arc<OutQueue>,
    /// The finger on the screen: where the last report left it.
    finger: Option<(u16, u16)>,
    /// A move waiting for the 8 ms spacing, and when the last touch report went out.
    pending_move: Option<(u16, u16)>,
    last_touch_sent: Option<Instant>,
    keys_down: VecDeque<KeyUp>,
    /// What is still to come of a double press, in time order (empty when none is in progress).
    /// The button is down while the first step is a release.
    button_steps: VecDeque<ButtonStep>,
    tokens: f64,
    tokens_at: Option<Instant>,
    replies: ReplyLimit,
    warnings: ReplyLimit,
    actions: Vec<Action>,
    /// The mapping the client was last told about (None until announced).
    announced: Option<Effective>,
    /// The keys of the virtual keyboard that are down, as the device's pressed set has them.
    kb_pressed: BTreeSet<u8>,
    /// When the last keyboard command (or typing step) was.
    kb_last: Option<Instant>,
    /// The string being typed: the steps still to come, and how many characters they hold.
    typing: VecDeque<TypeStep>,
    typing_chars: usize,
    /// Whether the typing owns the Shift and the key it has pressed (it does not let go of what the
    /// viewer is holding).
    owns_shift: bool,
    owns_key: bool,
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
            button_steps: VecDeque::new(),
            tokens: BURST,
            tokens_at: None,
            replies: ReplyLimit { last: None, held: 0 },
            warnings: ReplyLimit { last: None, held: 0 },
            actions: Vec::new(),
            announced: None,
            kb_pressed: BTreeSet::new(),
            kb_last: None,
            typing: VecDeque::new(),
            typing_chars: 0,
            owns_shift: false,
            owns_key: false,
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
        let move_due = self.pending_move.and(self.last_touch_sent).map(|t| t + MOVE_INTERVAL);
        let key_due = self.keys_down.iter().map(|k| k.at).chain(self.button_steps.front().map(|s| s.at)).min();
        let type_due = self.typing.front().map(|s| s.at);
        let idle_due = (!self.kb_pressed.is_empty()).then(|| self.kb_last.map(|t| t + KEYBOARD_IDLE_RELEASE)).flatten();
        [move_due, key_due, type_due, idle_due].into_iter().flatten().min()
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
        let quiet = self.replies.last.is_some_and(|t| now.saturating_duration_since(t) < Duration::from_secs(1));
        if quiet {
            self.replies.held += 1;
            return;
        }
        let held = std::mem::take(&mut self.replies.held);
        self.replies.last = Some(now);
        let message = if held > 0 { format!("{message} ({held} more failures)") } else { message.to_string() };
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

    /// An action the backend could not perform. A keyboard action that failed lets go of every key
    /// (the device and this side may no longer agree on what is pressed).
    pub fn on_action_failed(&mut self, now: Instant, message: &str, action: Action) {
        self.fail(now, message);
        if matches!(action, Action::Key { .. }) {
            self.release_keyboard();
        }
    }

    /// Writes `{"event":"hid","warning":...}`, at most once a second.
    fn warn(&mut self, now: Instant, message: String) {
        let quiet = self.warnings.last.is_some_and(|t| now.saturating_duration_since(t) < Duration::from_secs(1));
        if quiet {
            self.warnings.held += 1;
            return;
        }
        let held = std::mem::take(&mut self.warnings.held);
        self.warnings.last = Some(now);
        let message = if held > 0 { format!("{message} ({held} more warnings)") } else { message };
        tracing::warn!("hid: {message}");
        self.queue.push_droppable_control(Frame::control(&Event::Hid(HidEvent {
            warning: Some(message),
            ..HidEvent::default()
        })));
    }

    // ---- the keyboard ----------------------------------------------------------------------

    /// A `kb` command.
    pub fn on_kb(&mut self, now: Instant, kb: Kb) {
        match kb {
            Kb::Down(usage) => {
                if !self.take_token(now) {
                    return self.fail(now, "too many commands: key dropped");
                }
                self.kb_last = Some(now);
                // A key held down repeats its `down` (as a browser does): the device knows already.
                if self.kb_pressed.insert(usage) {
                    self.actions.push(Action::Key { usage, down: true });
                }
            }
            Kb::Up(usage) => {
                self.kb_last = Some(now);
                if self.kb_pressed.remove(&usage) {
                    self.actions.push(Action::Key { usage, down: false });
                }
            }
            Kb::Type(text) => self.start_typing(now, &text),
        }
    }

    /// Schedules a string: for each character that the layout has, the key goes down (with Shift if
    /// needed), 8 ms later up, and 4 ms after that the next one follows.
    fn start_typing(&mut self, now: Instant, text: &str) {
        if !self.take_token(now) {
            return self.fail(now, "too many commands: kb type dropped");
        }
        let mut steps = Vec::new();
        let mut skipped = 0usize;
        let mut chars = 0usize;
        let mut at = self.typing.back().map_or(now, |s| s.at.max(now) + TYPE_CHAR_GAP);
        for c in text.chars() {
            let Some((usage, shift)) = us_layout(c) else {
                skipped += 1;
                continue;
            };
            if shift {
                steps.push(TypeStep { at, op: TypeOp::ShiftDown });
            }
            steps.push(TypeStep { at, op: TypeOp::KeyDown(usage) });
            steps.push(TypeStep { at: at + TYPE_KEY_HOLD, op: TypeOp::KeyUp(usage) });
            if shift {
                steps.push(TypeStep { at: at + TYPE_KEY_HOLD, op: TypeOp::ShiftUp });
            }
            at += TYPE_KEY_HOLD + TYPE_CHAR_GAP;
            chars += 1;
        }
        if self.typing_chars + chars > MAX_TYPE_CHARS {
            return self.fail(
                now,
                &format!("kb type: {} characters are waiting to be typed, at most {MAX_TYPE_CHARS}", self.typing_chars),
            );
        }
        if skipped > 0 {
            self.warn(now, format!("kb type: {skipped} characters are not in the US layout table and were skipped"));
        }
        self.kb_last = Some(now);
        self.typing_chars += chars;
        self.typing.extend(steps);
    }

    /// Runs the typing steps that are due.
    fn run_typing(&mut self, now: Instant) {
        // Behind schedule (the loop was busy): move the rest of the schedule, do not burst.
        if let Some(front) = self.typing.front() {
            let lag = now.saturating_duration_since(front.at);
            if lag > TYPE_LAG {
                self.typing.iter_mut().for_each(|s| s.at += lag);
            }
        }
        while self.typing.front().is_some_and(|s| s.at <= now) {
            let step = self.typing.pop_front().expect("checked");
            self.kb_last = Some(now);
            match step.op {
                TypeOp::ShiftDown => {
                    self.owns_shift = self.kb_pressed.insert(LEFT_SHIFT);
                    if self.owns_shift {
                        self.actions.push(Action::Key { usage: LEFT_SHIFT, down: true });
                    }
                }
                TypeOp::KeyDown(usage) => {
                    self.typing_chars = self.typing_chars.saturating_sub(1);
                    self.owns_key = self.kb_pressed.insert(usage);
                    if self.owns_key {
                        self.actions.push(Action::Key { usage, down: true });
                    }
                }
                TypeOp::KeyUp(usage) => {
                    if std::mem::take(&mut self.owns_key) && self.kb_pressed.remove(&usage) {
                        self.actions.push(Action::Key { usage, down: false });
                    }
                }
                TypeOp::ShiftUp => {
                    if std::mem::take(&mut self.owns_shift) && self.kb_pressed.remove(&LEFT_SHIFT) {
                        self.actions.push(Action::Key { usage: LEFT_SHIFT, down: false });
                    }
                }
            }
        }
    }

    /// Drops the typing in progress and every key that is down, and has the device let go too.
    fn release_keyboard(&mut self) {
        let was_busy = !self.kb_pressed.is_empty() || !self.typing.is_empty();
        self.kb_pressed.clear();
        self.typing.clear();
        self.typing_chars = 0;
        self.owns_shift = false;
        self.owns_key = false;
        if was_busy {
            self.actions.push(Action::KeyboardRelease);
        }
    }

    /// Lets go of everything and takes the virtual keyboard down: for the end of the session.
    pub fn shutdown(&mut self) {
        self.release_all();
        self.actions.push(Action::KeyboardRemove);
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
            self.queue.push_control(Frame::control(&mapping_event(None, None, &effective)));
            self.announced = Some(effective);
        }
        match touch.phase {
            TouchPhase::Down => {
                if !self.take_token(now) {
                    return self.fail(now, "too many commands: touch down dropped");
                }
                self.pending_move = None;
                if let Some(line) = edge_down_message(x, y, touch.x, touch.y, w, h) {
                    self.queue.count_edge_down();
                    info!("{line}");
                }
                self.send_touch(now, true, x, y);
            }
            TouchPhase::Move => {
                if self.finger.is_none() {
                    debug!("hid: move without a finger down ignored");
                    return;
                }
                let due = self.last_touch_sent.is_none_or(|t| now.saturating_duration_since(t) >= MOVE_INTERVAL);
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
        if key == Key::AppSwitcher {
            return self.on_app_switcher(now);
        }
        if self.keys_down.len() >= MAX_KEYS_HELD {
            return self.fail(now, "too many keys pressed at once: key ignored");
        }
        if !self.take_token(now) {
            return self.fail(now, "too many commands: key dropped");
        }
        let (page, code) = key.usage();
        let hold_ms = key.hold_ms(press);
        debug!("hid: key {key:?} {press:?} (usage page {page:#x}, code {code:#x}, held {hold_ms} ms)");
        self.actions.push(Action::Button { page, code, down: true });
        self.keys_down.push_back(KeyUp { at: now + Duration::from_millis(hold_ms), page, code });
    }

    /// `key app-switcher`: the Home button, pressed and released, then after a gap pressed and
    /// released again. The first press goes out now and the rest is scheduled (see `on_tick`), so the
    /// loop is never held up by it: a touch, a key or a stop arriving meanwhile is handled as it
    /// comes, and `release_all` lets go of the button if it is down. One at a time: a second one
    /// while the first is going on is refused (it would make a triple press).
    fn on_app_switcher(&mut self, now: Instant) {
        if !self.button_steps.is_empty() {
            return self.fail(now, "the app switcher is already being opened: key ignored");
        }
        if !self.take_token(now) {
            return self.fail(now, "too many commands: key dropped");
        }
        let (page, code) = Key::AppSwitcher.usage();
        let hold = Duration::from_millis(Key::AppSwitcher.hold_ms(Press::Short));
        debug!("hid: key app-switcher (the home button twice: held {hold:?}, {APP_SWITCHER_GAP:?} apart)");
        self.actions.push(Action::Button { page, code, down: true });
        let up1 = now + hold;
        let down2 = up1 + APP_SWITCHER_GAP;
        let up2 = down2 + hold;
        self.button_steps.extend([
            ButtonStep { at: up1, page, code, down: false },
            ButtonStep { at: down2, page, code, down: true },
            ButtonStep { at: up2, page, code, down: false },
        ]);
    }

    /// The clock reached [`next_deadline`](Self::next_deadline).
    pub fn on_tick(&mut self, now: Instant) {
        self.run_typing(now);
        if !self.kb_pressed.is_empty()
            && self.kb_last.is_some_and(|t| now.saturating_duration_since(t) >= KEYBOARD_IDLE_RELEASE)
        {
            self.release_keyboard();
            self.warn(
                now,
                format!("keys held for {} s without a keyboard command were released", KEYBOARD_IDLE_RELEASE.as_secs()),
            );
        }
        if let Some((x, y)) = self.pending_move {
            let due = self.last_touch_sent.is_none_or(|t| now.saturating_duration_since(t) >= MOVE_INTERVAL);
            if due && self.finger.is_some() {
                self.pending_move = None;
                self.send_touch(now, true, x, y);
            }
        }
        while let Some(step) = self.button_steps.front().filter(|s| s.at <= now).copied() {
            self.button_steps.pop_front();
            self.actions.push(Action::Button { page: step.page, code: step.code, down: step.down });
        }
        while let Some(i) = self.keys_down.iter().position(|k| k.at <= now) {
            let k = self.keys_down.remove(i).expect("position is in range");
            self.actions.push(Action::Button { page: k.page, code: k.code, down: false });
        }
    }

    /// Lets go of everything: a finger still down is lifted where it is, and held keys are released.
    /// For a stop, the end of stdin, or a failure.
    pub fn release_all(&mut self) {
        self.release_keyboard();
        self.pending_move = None;
        if let Some((x, y)) = self.finger.take() {
            self.actions.push(Action::Touch { down: false, x, y });
        }
        // A double press under way: if the button is down (the next step is its release), let go of
        // it; the rest of the sequence is dropped.
        if let Some(step) = self.button_steps.front().filter(|s| !s.down).copied() {
            self.actions.push(Action::Button { page: step.page, code: step.code, down: false });
        }
        self.button_steps.clear();
        for k in self.keys_down.drain(..) {
            self.actions.push(Action::Button { page: k.page, code: k.code, down: false });
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

    /// The pixel at fraction `f` of the way from the first to the last pixel of an axis of `size`.
    fn along(f: f64, size: u32) -> f64 {
        f * f64::from(size - 1)
    }

    /// All four corners (the first and the last pixel of each axis) and the centre of an upright
    /// picture, as (x, y) -> (digitizer x, digitizer y).
    fn corners(w: u32, h: u32, rotation: u16) -> [(u16, u16); 5] {
        let (lx, ly) = (f64::from(w - 1), f64::from(h - 1));
        [
            map_touch(0.0, 0.0, w, h, rotation),
            map_touch(lx, 0.0, w, h, rotation),
            map_touch(0.0, ly, w, h, rotation),
            map_touch(lx, ly, w, h, rotation),
            map_touch(lx / 2.0, ly / 2.0, w, h, rotation),
        ]
    }

    #[test]
    fn rotation_0_maps_straight() {
        let m = 65535;
        // top-left, top-right, bottom-left, bottom-right, centre
        assert_eq!(corners(FB_W, FB_H, 0), [(0, 0), (m, 0), (0, m), (m, m), (32768, 32768)]);
    }

    #[test]
    fn rotation_180_maps_through_the_centre() {
        let m = 65535;
        assert_eq!(corners(FB_W, FB_H, 180), [(m, m), (0, m), (m, 0), (0, 0), (32768, 32768)]);
    }

    #[test]
    fn rotation_90_turns_the_framebuffer_clockwise_to_be_upright() {
        // The upright picture is portrait, 1668x2388: the framebuffer (landscape) turned 90 clockwise.
        let m = 65535;
        assert_eq!(
            corners(FB_H, FB_W, 90),
            [
                (0, m), // upright top-left is the framebuffer's bottom-left
                (0, 0), // upright top-right is its top-left
                (m, m), // upright bottom-left is its bottom-right
                (m, 0), // upright bottom-right is its top-right
                (32768, 32768),
            ]
        );
    }

    #[test]
    fn rotation_270_turns_the_framebuffer_counter_clockwise_to_be_upright() {
        let m = 65535;
        assert_eq!(corners(FB_H, FB_W, 270), [(m, 0), (m, m), (0, 0), (0, m), (32768, 32768)]);
    }

    #[test]
    fn a_point_off_centre_follows_the_turn() {
        // A quarter of the way along the top of a 1000x2000 upright picture, framebuffer turned 90.
        let (x, y) = map_touch(250.0, 0.0, 1001, 2001, 90);
        assert_eq!((x, y), (0, to_axis(0.75)));
        // Halfway down the left edge, turned 270.
        let (x, y) = map_touch(0.0, 1000.0, 1001, 2001, 270);
        assert_eq!((x, y), (to_axis(0.5), 0));
        // Turning the picture by r and mapping back by r is the identity for every angle.
        for r in [0u16, 90, 180, 270] {
            let (uw, uh) = if r % 180 == 90 { (FB_H, FB_W) } else { (FB_W, FB_H) };
            let (x, y) = (along(0.3, uw), along(0.8, uh));
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
        // At the size itself (one past the last pixel) and at the last pixel: the far limit.
        assert_eq!(map_touch(100.0, 200.0, 100, 200, 0), (65535, 65535));
        assert_eq!(map_touch(99.0, 199.0, 100, 200, 0), (65535, 65535));
        // A rotation that is not a right angle counts as 0; a zero-sized picture does not divide by zero.
        assert_eq!(map_touch(50.0, 100.0, 101, 201, 45), (32768, 32768));
        assert_eq!(map_touch(5.0, 5.0, 0, 0, 0), (65535, 65535));
        assert_eq!(map_touch(0.0, 0.0, 0, 0, 0), (0, 0));
        // A picture one pixel across has no span: its pixel is 0, anything beyond is the far limit.
        assert_eq!(map_touch(0.0, 0.0, 1, 1, 0), (0, 0));
        assert_eq!(map_touch(1.0, 0.5, 1, 1, 0), (65535, 65535));
        assert_eq!(map_touch(f64::NAN, 1.0, 10, 10, 0).1, to_axis(1.0 / 9.0));
        assert_eq!(map_touch(1.0, 1.0, 10, 10, 360), map_touch(1.0, 1.0, 10, 10, 0));
    }

    #[test]
    fn the_ui_space_ignores_the_rotation_and_the_escape_hatches_apply_in_order() {
        let ui = HidMapping { space: HidSpace::Ui, ..HidMapping::default() };
        assert_eq!(ui.map(250.0, 500.0, 1001, 2001, 90, None), (to_axis(0.25), to_axis(0.25)));
        assert_eq!(ui.map(250.0, 500.0, 1001, 2001, 270, None), ui.map(250.0, 500.0, 1001, 2001, 0, None));
        // Panel space is map_touch, whatever the native orientation.
        let panel = HidMapping { space: HidSpace::Panel, ..HidMapping::default() };
        assert_eq!(panel.map(250.0, 500.0, 1001, 2001, 90, None), map_touch(250.0, 500.0, 1001, 2001, 90));
        assert_eq!(panel.map(250.0, 500.0, 1001, 2001, 90, Some(270)), map_touch(250.0, 500.0, 1001, 2001, 90));

        // x = 0.25, y = 0.5 of the picture, upright (rotation 0).
        let base = |flip_x, flip_y, swap_xy| {
            HidMapping { space: HidSpace::Panel, flip_x, flip_y, swap_xy }.map(250.0, 1000.0, 1001, 2001, 0, None)
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

    /// The framebuffer-normalized point (fx, fy) as an upright position of a 1001x2001 picture (1000 and
    /// 2000 pixels between the first and the last) at rotation 0 (where the upright picture is the framebuffer), mapped with `native`.
    fn auto(native: Option<u16>, fx: f64, fy: f64) -> (u16, u16) {
        HidMapping::default().map(fx * 1000.0, fy * 2000.0, 1001, 2001, 0, native)
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
        assert_eq!(at(0.25, 0.2), flags.map(250.0, 400.0, 1001, 2001, 0, Some(270)));
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
        // Rotation 0: the picture is the framebuffer, 2388x1668. Upright (0.25, 0.5) of the way is pixel (596.75, 833.5);
        // the digitizer point is (1 - 0.5, 0.25).
        assert_eq!(m.map(along(0.25, 2388), along(0.5, 1668), 2388, 1668, 0, Some(270)), (to_axis(0.5), to_axis(0.25)));
        // The same as the flags that landed on the device.
        let landed = HidMapping { space: HidSpace::Panel, swap_xy: true, flip_x: true, flip_y: false };
        for (x, y) in [(0.0, 0.0), (597.0, 834.0), (2388.0, 1668.0), (1000.0, 100.0), (5.0, 1600.0)] {
            assert_eq!(m.map(x, y, 2388, 1668, 0, Some(270)), landed.map(x, y, 2388, 1668, 0, None), "({x}, {y})");
        }
        // Rotation 90: the upright picture is portrait, 1668x2388. Upright (0.25, 0.5) of the way is pixel (416.75, 1193.5);
        // undoing the rotation gives the framebuffer point (fx, fy) = (0.5, 0.75); the digitizer point
        // is (1 - 0.75, 0.5).
        assert_eq!(
            m.map(along(0.25, 1668), along(0.5, 2388), 1668, 2388, 90, Some(270)),
            (to_axis(0.25), to_axis(0.5))
        );
        // Corners of the portrait picture: upright top-left is the framebuffer's bottom-left (0, 1),
        // which the digitizer sees as (1 - 1, 0) = (0, 0).
        assert_eq!(m.map(0.0, 0.0, 1668, 2388, 90, Some(270)), (0, 0));
        assert_eq!(m.map(1667.0, 0.0, 1668, 2388, 90, Some(270)), (MAX, 0)); // fb (0,0) -> (1, 0)
        assert_eq!(m.map(0.0, 2387.0, 1668, 2388, 90, Some(270)), (0, MAX)); // fb (1,1) -> (0, 1)
        assert_eq!(m.map(1667.0, 2387.0, 1668, 2388, 90, Some(270)), (MAX, MAX)); // fb (1,0) -> (1, 1)
    }

    #[test]
    fn without_display_info_auto_falls_back_to_the_identity_and_says_so() {
        let m = HidMapping::default();
        let eff = m.effective(None);
        assert_eq!(eff.label, "identity (no display info)");
        assert_eq!(eff.axes, Axes::default());
        assert_eq!(m.map(250.0, 500.0, 1001, 2001, 0, None), map_touch(250.0, 500.0, 1001, 2001, 0));
        assert_eq!(m.map(250.0, 500.0, 1001, 2001, 90, None), map_touch(250.0, 500.0, 1001, 2001, 90));
        // A native orientation that is not a right angle is no information either.
        assert_eq!(m.effective(Some(45)).label, "identity (no display info)");
        // The overrides still apply in the fallback.
        let flipped = HidMapping { flip_x: true, ..HidMapping::default() };
        let eff = flipped.effective(None);
        assert_eq!(eff.label, "identity (no display info)");
        assert_eq!(eff.axes, Axes { flip_x: true, ..Axes::default() });
        assert_eq!(flipped.map(250.0, 500.0, 1001, 2001, 0, None), (to_axis(0.75), to_axis(0.25)));
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
        assert_eq!(ui.map(250.0, 500.0, 1001, 2001, 90, Some(270)), (to_axis(0.25), to_axis(0.25)));
    }

    #[test]
    fn the_overrides_apply_after_the_automatic_mapping() {
        // The iPad's point: fx = 0.25, fy = 0.2, native 270: automatic gives (0.8, 0.25).
        let at = |m: HidMapping| m.map(250.0, 400.0, 1001, 2001, 0, Some(270));
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
                    assert!(
                        (stepwise.0 - net.0).abs() < 1e-12 && (stepwise.1 - net.1).abs() < 1e-12,
                        "native {native:?} overrides {bits:03b} at ({fx}, {fy})"
                    );
                    let digitizer = m.map(fx * 1000.0, fy * 2000.0, 1001, 2001, 0, native);
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
        h.on_touch(t0, touch(TouchPhase::Down, 596.75, 833.5), ipad(0));
        h.on_touch(ms(t0, 20), touch(TouchPhase::Up, 596.75, 833.5), ipad(0));
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
        assert_eq!(
            (c[0]["swap_xy"].as_bool(), c[0]["flip_x"].as_bool(), c[0]["flip_y"].as_bool()),
            (Some(true), Some(true), Some(false))
        );
        assert!(c[0].get("ready").is_none());
        // The picture turned (rotation 90): the same upright point is another digitizer point, and
        // the mapping did not change, so nothing is announced.
        h.on_touch(ms(t0, 40), touch(TouchPhase::Down, 416.75, 1193.5), ipad(90));
        assert_eq!(h.take_actions(), vec![Action::Touch { down: true, x: to_axis(0.25), y: to_axis(0.5) }]);
        assert!(controls(&q).is_empty());
    }

    #[test]
    fn nothing_is_announced_before_the_ready_event_names_a_mapping() {
        let (mut h, q, t0) = rig();
        h.on_touch(
            t0,
            touch(TouchPhase::Down, 5.0, 5.0),
            Some(View { width: 100, height: 200, rotation: 0, native: Some(270) }),
        );
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

    const VIEW: Option<View> = Some(View { width: 1001, height: 2001, rotation: 0, native: None });

    fn touch(phase: TouchPhase, x: f64, y: f64) -> Touch {
        Touch { phase, x, y, id: 0 }
    }

    fn controls(q: &OutQueue) -> Vec<serde_json::Value> {
        q.drain().iter().filter(|f| f.is_control()).map(|f| serde_json::from_slice(&f.payload).unwrap()).collect()
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
        assert_eq!(h.take_actions(), vec![Action::Touch { down: true, x: to_axis(0.07), y: 0 }]);
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
        assert_eq!(h.take_actions(), vec![Action::Touch { down: false, x: to_axis(0.3), y: to_axis(0.3) }]);
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

    const HOME_DOWN: Action = Action::Button { page: 0x0C, code: 0x40, down: true };
    const HOME_UP: Action = Action::Button { page: 0x0C, code: 0x40, down: false };

    #[test]
    fn the_app_switcher_is_the_home_button_twice_80_ms_held_and_120_ms_apart() {
        // short and long are the same for it.
        for press in [Press::Short, Press::Long] {
            let (mut h, q, t0) = rig();
            h.on_key(t0, Key::AppSwitcher, press);
            // The first press is down at once; nothing else is sent yet.
            assert_eq!(h.take_actions(), vec![HOME_DOWN]);
            assert_eq!(h.next_deadline(), Some(ms(t0, 80)));
            h.on_tick(ms(t0, 79));
            assert!(h.take_actions().is_empty());
            // 80 ms: up.
            h.on_tick(ms(t0, 80));
            assert_eq!(h.take_actions(), vec![HOME_UP]);
            assert_eq!(h.next_deadline(), Some(ms(t0, 200)), "the second press, 120 ms after the release");
            h.on_tick(ms(t0, 199));
            assert!(h.take_actions().is_empty());
            // 200 ms: down again (the two downs are 200 ms apart); then held 80 ms.
            h.on_tick(ms(t0, 200));
            assert_eq!(h.take_actions(), vec![HOME_DOWN]);
            assert_eq!(h.next_deadline(), Some(ms(t0, 280)));
            h.on_tick(ms(t0, 279));
            assert!(h.take_actions().is_empty());
            h.on_tick(ms(t0, 280));
            assert_eq!(h.take_actions(), vec![HOME_UP]);
            assert_eq!(h.next_deadline(), None);
            h.on_tick(ms(t0, 10_000));
            assert!(h.take_actions().is_empty(), "nothing more");
            assert!(controls(&q).is_empty(), "no reply: it worked");
        }
    }

    #[test]
    fn a_late_tick_does_the_due_steps_in_order() {
        let (mut h, _q, t0) = rig();
        h.on_key(t0, Key::AppSwitcher, Press::Short);
        h.take_actions();
        h.on_tick(ms(t0, 1000));
        assert_eq!(h.take_actions(), vec![HOME_UP, HOME_DOWN, HOME_UP]);
        assert_eq!(h.next_deadline(), None);
    }

    #[test]
    fn a_touch_or_another_key_in_the_middle_of_an_app_switcher_is_handled_at_once() {
        let (mut h, _q, t0) = rig();
        h.on_key(t0, Key::AppSwitcher, Press::Short);
        assert_eq!(h.take_actions(), vec![HOME_DOWN]);
        // In the gap (between 80 and 200 ms) a finger goes down; the sequence is not held up.
        h.on_tick(ms(t0, 80));
        assert_eq!(h.take_actions(), vec![HOME_UP]);
        h.on_touch(ms(t0, 100), touch(TouchPhase::Down, 500.0, 1000.0), VIEW);
        h.on_key(ms(t0, 110), Key::VolumeUp, Press::Short);
        assert_eq!(
            h.take_actions(),
            vec![
                Action::Touch { down: true, x: 32768, y: 32768 },
                Action::Button { page: 0x0C, code: 0xE9, down: true },
            ]
        );
        // The deadline is the earliest of the volume key's release (190 ms) and the second press (200 ms).
        assert_eq!(h.next_deadline(), Some(ms(t0, 190)));
        h.on_tick(ms(t0, 190));
        assert_eq!(h.take_actions(), vec![Action::Button { page: 0x0C, code: 0xE9, down: false }]);
        assert_eq!(h.next_deadline(), Some(ms(t0, 200)));
        h.on_tick(ms(t0, 200));
        assert_eq!(h.take_actions(), vec![HOME_DOWN]);
        h.on_tick(ms(t0, 280));
        assert_eq!(h.take_actions(), vec![HOME_UP]);
    }

    #[test]
    fn a_stop_in_the_middle_of_an_app_switcher_lets_go_of_the_button_and_drops_the_rest() {
        // Each stop time, with everything sent after the first press (the loop ticks at every deadline
        // up to the stop, then lets go of everything): the button is up at the end, whenever the stop is.
        let cases: [(u64, Vec<Action>); 9] = [
            (0, vec![HOME_UP]),  // as the first press goes down
            (40, vec![HOME_UP]), // during the first press
            (79, vec![HOME_UP]),
            (80, vec![HOME_UP]),  // as it comes up: once, not twice
            (150, vec![HOME_UP]), // in the gap: it is up; the second press never happens
            (199, vec![HOME_UP]),
            (200, vec![HOME_UP, HOME_DOWN, HOME_UP]), // as the second goes down
            (250, vec![HOME_UP, HOME_DOWN, HOME_UP]), // during the second press
            (280, vec![HOME_UP, HOME_DOWN, HOME_UP]), // as the sequence ends: nothing extra
        ];
        for (stop_at, want) in cases {
            let (mut h, _q, t0) = rig();
            h.on_key(t0, Key::AppSwitcher, Press::Short);
            assert_eq!(h.take_actions(), vec![HOME_DOWN]);
            let stop = ms(t0, stop_at);
            while let Some(d) = h.next_deadline().filter(|d| *d <= stop) {
                h.on_tick(d);
            }
            h.release_all();
            assert_eq!(h.take_actions(), want, "stop at {stop_at} ms");
            // Nothing is left to happen, however long it is waited for.
            assert_eq!(h.next_deadline(), None, "stop at {stop_at} ms");
            h.on_tick(ms(t0, 10_000));
            assert!(h.take_actions().is_empty(), "stop at {stop_at} ms");
        }
    }

    #[test]
    fn a_second_app_switcher_while_one_is_going_on_is_refused_and_one_after_it_works() {
        let (mut h, q, t0) = rig();
        h.on_key(t0, Key::AppSwitcher, Press::Short);
        h.take_actions();
        h.on_key(ms(t0, 50), Key::AppSwitcher, Press::Short);
        assert!(h.take_actions().is_empty());
        let c = controls(&q);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0]["ok"], false);
        assert!(c[0]["message"].as_str().unwrap().contains("app switcher"), "{c:?}");
        h.on_tick(ms(t0, 280));
        assert_eq!(h.take_actions(), vec![HOME_UP, HOME_DOWN, HOME_UP]);
        // Done: the next one runs.
        h.on_key(ms(t0, 400), Key::AppSwitcher, Press::Short);
        assert_eq!(h.take_actions(), vec![HOME_DOWN]);
        assert_eq!(h.next_deadline(), Some(ms(t0, 480)));
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
            h.on_action_failed(ms(t0, i * 10), "the service is gone", Action::Touch { down: true, x: 0, y: 0 });
        }
        h.on_action_failed(ms(t0, 1500), "the service is gone", Action::Touch { down: true, x: 0, y: 0 });
        let c = controls(&q);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0]["message"], "the service is gone");
        assert_eq!(c[1]["message"], "the service is gone (9 more failures)");
        h.on_bad_command(ms(t0, 5000), "bad touch command");
        assert_eq!(controls(&q)[0]["message"], "bad touch command");
    }

    // ---- the keyboard -----------------------------------------------------------------------

    fn kb_down(u: u8) -> Action {
        Action::Key { usage: u, down: true }
    }

    fn kb_up(u: u8) -> Action {
        Action::Key { usage: u, down: false }
    }

    #[test]
    fn the_us_layout_has_every_printable_ascii_character_once() {
        let mut seen = std::collections::BTreeSet::new();
        for b in 0x20u8..=0x7E {
            let c = b as char;
            let key = us_layout(c).unwrap_or_else(|| panic!("{c:?} has no key"));
            assert!(seen.insert(key), "{c:?} shares its key with another character");
            assert!((0x04..=0x38).contains(&key.0), "{c:?}: usage {:#x}", key.0);
        }
        assert_eq!(seen.len(), 95);
        // Newline is Return, tab is Tab: keys of their own.
        assert_eq!(us_layout('\n'), Some((0x28, false)));
        assert_eq!(us_layout('\t'), Some((0x2B, false)));
        assert!(!seen.contains(&(0x28, false)) && !seen.contains(&(0x2B, false)));
    }

    #[test]
    fn letters_digits_and_punctuation_follow_the_us_layout() {
        for (i, c) in ('a'..='z').enumerate() {
            assert_eq!(us_layout(c), Some((0x04 + i as u8, false)), "{c}");
            assert_eq!(us_layout(c.to_ascii_uppercase()), Some((0x04 + i as u8, true)), "{c}");
        }
        for (i, c) in ('1'..='9').enumerate() {
            assert_eq!(us_layout(c), Some((0x1E + i as u8, false)), "{c}");
        }
        assert_eq!(us_layout('0'), Some((0x27, false)));
        // The digit row with Shift.
        for (c, usage) in "!@#$%^&*()".chars().zip(0x1Eu8..) {
            assert_eq!(us_layout(c), Some((usage, true)), "{c}");
        }
        for (c, key) in [
            (' ', (0x2C, false)),
            ('-', (0x2D, false)),
            ('_', (0x2D, true)),
            ('=', (0x2E, false)),
            ('+', (0x2E, true)),
            ('[', (0x2F, false)),
            ('{', (0x2F, true)),
            (']', (0x30, false)),
            ('}', (0x30, true)),
            ('\\', (0x31, false)),
            ('|', (0x31, true)),
            (';', (0x33, false)),
            (':', (0x33, true)),
            ('\'', (0x34, false)),
            ('"', (0x34, true)),
            ('`', (0x35, false)),
            ('~', (0x35, true)),
            (',', (0x36, false)),
            ('<', (0x36, true)),
            ('.', (0x37, false)),
            ('>', (0x37, true)),
            ('/', (0x38, false)),
            ('?', (0x38, true)),
        ] {
            assert_eq!(us_layout(c), Some(key), "{c:?}");
        }
    }

    #[test]
    fn characters_outside_the_table_have_no_key() {
        for c in ['\r', '\0', '\u{7f}', '\u{1b}', 'é', 'ß', '€', '£', '日', '\u{1F600}', '\u{a0}'] {
            assert_eq!(us_layout(c), None, "{c:?}");
        }
    }

    #[test]
    fn kb_down_and_up_accumulate_like_a_physical_keyboard() {
        let (mut h, _q, t0) = rig();
        h.on_kb(t0, Kb::Down(0xE1)); // Left Shift
        h.on_kb(ms(t0, 5), Kb::Down(0xE0)); // Left Control
        h.on_kb(ms(t0, 10), Kb::Down(0x04)); // A
        assert_eq!(h.take_actions(), vec![kb_down(0xE1), kb_down(0xE0), kb_down(0x04)]);
        // Let go in another order.
        h.on_kb(ms(t0, 20), Kb::Up(0x04));
        h.on_kb(ms(t0, 25), Kb::Up(0xE1));
        h.on_kb(ms(t0, 30), Kb::Up(0xE0));
        assert_eq!(h.take_actions(), vec![kb_up(0x04), kb_up(0xE1), kb_up(0xE0)]);
        assert_eq!(h.next_deadline(), None, "nothing is held");
    }

    #[test]
    fn a_repeated_down_and_an_up_without_a_down_are_no_ops() {
        let (mut h, _q, t0) = rig();
        h.on_kb(t0, Kb::Down(0x04));
        // A browser repeats keydown while a key is held.
        for i in 1..20u64 {
            h.on_kb(ms(t0, i * 30), Kb::Down(0x04));
        }
        assert_eq!(h.take_actions(), vec![kb_down(0x04)]);
        h.on_kb(ms(t0, 700), Kb::Up(0x05));
        assert!(h.take_actions().is_empty());
        h.on_kb(ms(t0, 710), Kb::Up(0x04));
        h.on_kb(ms(t0, 720), Kb::Up(0x04));
        assert_eq!(h.take_actions(), vec![kb_up(0x04)]);
    }

    #[test]
    fn typing_follows_the_8_and_4_ms_schedule_with_shift_where_needed() {
        let (mut h, _q, t0) = rig();
        h.on_kb(t0, Kb::Type("Hi!".into()));
        assert!(h.take_actions().is_empty(), "nothing happens until the clock ticks");
        assert_eq!(h.next_deadline(), Some(t0));
        // 'H': Shift and h down together.
        h.on_tick(t0);
        assert_eq!(h.take_actions(), vec![kb_down(0xE1), kb_down(0x0B)]);
        assert_eq!(h.next_deadline(), Some(ms(t0, 8)));
        h.on_tick(ms(t0, 7));
        assert!(h.take_actions().is_empty());
        // 8 ms later both up.
        h.on_tick(ms(t0, 8));
        assert_eq!(h.take_actions(), vec![kb_up(0x0B), kb_up(0xE1)]);
        // 4 ms after that, 'i' (no Shift).
        assert_eq!(h.next_deadline(), Some(ms(t0, 12)));
        h.on_tick(ms(t0, 12));
        assert_eq!(h.take_actions(), vec![kb_down(0x0C)]);
        h.on_tick(ms(t0, 20));
        assert_eq!(h.take_actions(), vec![kb_up(0x0C)]);
        // '!' is Shift 1.
        assert_eq!(h.next_deadline(), Some(ms(t0, 24)));
        h.on_tick(ms(t0, 24));
        assert_eq!(h.take_actions(), vec![kb_down(0xE1), kb_down(0x1E)]);
        h.on_tick(ms(t0, 32));
        assert_eq!(h.take_actions(), vec![kb_up(0x1E), kb_up(0xE1)]);
        assert_eq!(h.next_deadline(), None);
        // Newline and tab are Return and Tab.
        h.on_kb(ms(t0, 100), Kb::Type("\n\t".into()));
        h.on_tick(ms(t0, 100));
        h.on_tick(ms(t0, 108));
        h.on_tick(ms(t0, 112));
        h.on_tick(ms(t0, 120));
        assert_eq!(h.take_actions(), vec![kb_down(0x28), kb_up(0x28), kb_down(0x2B), kb_up(0x2B)]);
    }

    #[test]
    fn typing_does_not_let_go_of_a_shift_the_viewer_is_holding() {
        let (mut h, _q, t0) = rig();
        h.on_kb(t0, Kb::Down(0xE1));
        h.take_actions();
        h.on_kb(ms(t0, 10), Kb::Type("Ab".into()));
        for t in [10, 18, 22, 30] {
            h.on_tick(ms(t0, t));
        }
        // The key presses, with no second Shift down and no Shift up between them.
        assert_eq!(h.take_actions(), vec![kb_down(0x04), kb_up(0x04), kb_down(0x05), kb_up(0x05)]);
        // Shift is still down: the 30 s idle release has a deadline.
        assert!(h.next_deadline().is_some());
    }

    #[test]
    fn characters_that_cannot_be_typed_are_skipped_and_counted_in_one_warning() {
        let (mut h, q, t0) = rig();
        h.on_kb(t0, Kb::Type("aé€b\r😀".into()));
        let c = controls(&q);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0]["event"], "hid");
        assert!(c[0]["warning"].as_str().unwrap().contains("4 characters"), "{c:?}");
        assert!(c[0].get("ok").is_none(), "a warning, not an error");
        // The two that can be typed are.
        for t in [0, 8, 12, 20] {
            h.on_tick(ms(t0, t));
        }
        assert_eq!(h.take_actions(), vec![kb_down(0x04), kb_up(0x04), kb_down(0x05), kb_up(0x05)]);
        // Nothing to skip: no warning.
        h.on_kb(ms(t0, 100), Kb::Type("ok".into()));
        assert!(controls(&q).is_empty());
    }

    #[test]
    fn a_long_string_is_typed_in_steps_not_in_one_go() {
        let (mut h, _q, t0) = rig();
        let text: String = "a".repeat(MAX_TYPE_CHARS);
        h.on_kb(t0, Kb::Type(text));
        assert!(h.take_actions().is_empty());
        // One tick does one character's worth (and only what is due).
        h.on_tick(t0);
        assert_eq!(h.take_actions().len(), 1);
        assert_eq!(h.next_deadline(), Some(ms(t0, 8)));
        // 4096 characters at 12 ms each: the last one is done at 4095 * 12 + 8 ms.
        let mut now = t0;
        let mut actions = 1;
        while let Some(d) = h.next_deadline() {
            now = d;
            h.on_tick(now);
            actions += h.take_actions().len();
        }
        assert_eq!(actions, 2 * MAX_TYPE_CHARS - 1 + 1);
        assert_eq!(now - t0, Duration::from_millis(4095 * 12 + 8));
    }

    #[test]
    fn the_typing_queue_is_bounded() {
        let (mut h, q, t0) = rig();
        h.on_kb(t0, Kb::Type("a".repeat(MAX_TYPE_CHARS)));
        assert!(controls(&q).is_empty());
        h.on_kb(ms(t0, 1), Kb::Type("b".into()));
        let c = controls(&q);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0]["ok"], false);
        assert!(c[0]["message"].as_str().unwrap().contains("4096"));
        // The first string is not disturbed: it ends with an 'a'.
        let mut last = None;
        let mut now = t0;
        while let Some(d) = h.next_deadline() {
            now = d;
            h.on_tick(now);
            for a in h.take_actions() {
                last = Some(a);
            }
        }
        assert_eq!(last, Some(kb_up(0x04)));
        // Once it is typed there is room again.
        h.on_kb(now, Kb::Type("b".into()));
        assert!(controls(&q).is_empty());
    }

    #[test]
    fn a_second_string_waits_behind_the_first() {
        let (mut h, _q, t0) = rig();
        h.on_kb(t0, Kb::Type("a".into()));
        h.on_kb(ms(t0, 1), Kb::Type("b".into()));
        let mut actions = Vec::new();
        while let Some(d) = h.next_deadline() {
            h.on_tick(d);
            actions.extend(h.take_actions());
        }
        assert_eq!(actions, vec![kb_down(0x04), kb_up(0x04), kb_down(0x05), kb_up(0x05)]);
    }

    #[test]
    fn a_loop_that_fell_behind_shifts_the_typing_instead_of_bursting() {
        let (mut h, _q, t0) = rig();
        h.on_kb(t0, Kb::Type("abc".into()));
        // The first tick comes a whole second late.
        h.on_tick(ms(t0, 1000));
        assert_eq!(h.take_actions(), vec![kb_down(0x04)], "one step, then the schedule moves on");
        assert_eq!(h.next_deadline(), Some(ms(t0, 1008)));
    }

    #[test]
    fn release_all_aborts_the_typing_and_lets_go_of_every_key() {
        let (mut h, _q, t0) = rig();
        h.on_kb(t0, Kb::Type("abcdef".into()));
        h.on_tick(t0);
        assert_eq!(h.take_actions(), vec![kb_down(0x04)]);
        h.on_kb(ms(t0, 3), Kb::Down(0xE0));
        h.take_actions();
        h.release_all();
        assert_eq!(h.take_actions(), vec![Action::KeyboardRelease]);
        assert_eq!(h.next_deadline(), None);
        h.on_tick(ms(t0, 500));
        assert!(h.take_actions().is_empty(), "the rest of the string is not typed");
        // Nothing down, nothing to release.
        h.release_all();
        assert!(h.take_actions().is_empty());
    }

    #[test]
    fn shutdown_releases_and_takes_the_keyboard_down() {
        let (mut h, _q, t0) = rig();
        h.on_kb(t0, Kb::Down(0x04));
        h.on_touch(t0, touch(TouchPhase::Down, 5.0, 5.0), VIEW);
        h.take_actions();
        h.shutdown();
        assert_eq!(
            h.take_actions(),
            vec![
                Action::KeyboardRelease,
                Action::Touch { down: false, x: to_axis(0.005), y: to_axis(0.0025) },
                Action::KeyboardRemove
            ]
        );
        // With nothing held it is only the removal.
        h.shutdown();
        assert_eq!(h.take_actions(), vec![Action::KeyboardRemove]);
    }

    #[test]
    fn keys_held_for_30_seconds_without_a_keyboard_command_are_released() {
        let (mut h, q, t0) = rig();
        h.on_kb(t0, Kb::Down(0xE1));
        h.take_actions();
        assert_eq!(h.next_deadline(), Some(ms(t0, 30_000)));
        // Another keyboard command moves the deadline (the repeat of a held key counts).
        h.on_kb(ms(t0, 20_000), Kb::Down(0x04));
        h.take_actions();
        assert_eq!(h.next_deadline(), Some(ms(t0, 50_000)));
        h.on_tick(ms(t0, 49_999));
        assert!(h.take_actions().is_empty());
        h.on_tick(ms(t0, 50_000));
        assert_eq!(h.take_actions(), vec![Action::KeyboardRelease]);
        let c = controls(&q);
        assert_eq!(c.len(), 1);
        assert!(c[0]["warning"].as_str().unwrap().contains("30 s"), "{c:?}");
        assert_eq!(h.next_deadline(), None);
        // Touch and buttons do not count as keyboard activity, and nothing is pressed now.
        h.on_tick(ms(t0, 100_000));
        assert!(h.take_actions().is_empty());
    }

    #[test]
    fn a_failed_key_action_releases_everything_once() {
        let (mut h, q, t0) = rig();
        h.on_kb(t0, Kb::Down(0xE1));
        h.on_kb(t0, Kb::Down(0x04));
        h.take_actions();
        h.on_action_failed(ms(t0, 5), "keyboard: transport", kb_down(0x04));
        assert_eq!(h.take_actions(), vec![Action::KeyboardRelease]);
        assert_eq!(h.next_deadline(), None);
        let c = controls(&q);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0]["ok"], false);
        // The release itself failing does not queue another release.
        h.on_action_failed(ms(t0, 2000), "keyboard: transport", Action::KeyboardRelease);
        assert!(h.take_actions().is_empty());
        // Touch failures do not touch the keyboard.
        h.on_kb(ms(t0, 3000), Kb::Down(0x05));
        h.take_actions();
        h.on_action_failed(ms(t0, 4000), "x", Action::Touch { down: true, x: 0, y: 0 });
        assert!(h.take_actions().is_empty());
        assert!(h.next_deadline().is_some());
    }

    #[test]
    fn a_flood_of_key_downs_is_rate_limited_but_ups_are_not() {
        let (mut h, q, t0) = rig();
        for i in 0..200u8 {
            h.on_kb(t0, Kb::Down(0x04 + (i % 100)));
        }
        let downs = h.take_actions().len();
        assert!(downs <= BURST as usize, "{downs}");
        for i in 0..100u8 {
            h.on_kb(t0, Kb::Up(0x04 + i));
        }
        assert_eq!(h.take_actions().len(), downs, "every key that went down comes up");
        assert!(!controls(&q).is_empty());
    }

    #[test]
    fn the_ready_event_carries_the_keyboard_flag() {
        let eff = HidMapping::default().effective(Some(270));
        let mut ev = mapping_event(Some(true), None, &eff);
        if let Event::Hid(h) = &mut ev {
            h.keyboard = Some(true);
        }
        assert_eq!(
            Frame::control(&ev).payload,
            br#"{"event":"hid","ready":true,"keyboard":true,"mapping":"auto","swap_xy":true,"flip_x":true,"flip_y":false}"#
        );
        let mut ev = mapping_event(Some(true), Some("keyboard: no".into()), &eff);
        if let Event::Hid(h) = &mut ev {
            h.keyboard = Some(false);
        }
        let json: serde_json::Value = serde_json::from_slice(&Frame::control(&ev).payload).unwrap();
        assert_eq!((json["ready"].as_bool(), json["keyboard"].as_bool()), (Some(true), Some(false)));
        assert_eq!(json["message"], "keyboard: no");
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
        let failed =
            Frame::control(&Event::Hid(HidEvent { ok: Some(false), message: Some("x".into()), ..HidEvent::default() }));
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

    // ---- the very edge of the screen ---------------------------------------------------------

    const MID: u16 = 32768;

    /// The middle of the left, right, top and bottom edges, then the top-left, top-right, bottom-left
    /// and bottom-right corners of an upright `w` x `h` picture, at its outermost columns and rows.
    fn edge_points(w: u32, h: u32) -> [(f64, f64); 8] {
        let (lx, ly) = (f64::from(w - 1), f64::from(h - 1));
        [(0.0, ly / 2.0), (lx, ly / 2.0), (lx / 2.0, 0.0), (lx / 2.0, ly), (0.0, 0.0), (lx, 0.0), (0.0, ly), (lx, ly)]
    }

    fn mapped(m: &HidMapping, w: u32, h: u32, rotation: u16, native: Option<u16>) -> [(u16, u16); 8] {
        edge_points(w, h).map(|(x, y)| m.map(x, y, w, h, rotation, native))
    }

    #[test]
    fn the_outermost_pixels_are_the_digitizers_limits_at_every_rotation() {
        let (m, mid) = (MAX, MID);
        let panel = HidMapping { space: HidSpace::Panel, ..HidMapping::default() };
        // (left, right, top, bottom, top-left, top-right, bottom-left, bottom-right) in the digitizer's
        // axes, for the framebuffer turned clockwise by the rotation to be upright.
        let expected: [(u16, [(u16, u16); 8]); 4] = [
            (0, [(0, mid), (m, mid), (mid, 0), (mid, m), (0, 0), (m, 0), (0, m), (m, m)]),
            (90, [(mid, m), (mid, 0), (0, mid), (m, mid), (0, m), (0, 0), (m, m), (m, 0)]),
            (180, [(m, mid), (0, mid), (mid, m), (mid, 0), (m, m), (0, m), (m, 0), (0, 0)]),
            (270, [(mid, 0), (mid, m), (m, mid), (0, mid), (m, 0), (m, m), (0, 0), (0, m)]),
        ];
        for (rotation, want) in expected {
            // The iPad's framebuffer is 2388x1668; upright it is that, or turned a quarter, 1668x2388.
            let (w, h) = if rotation % 180 == 90 { (FB_H, FB_W) } else { (FB_W, FB_H) };
            assert_eq!(mapped(&panel, w, h, rotation, None), want, "rotation {rotation}");
            assert_eq!(
                map_touch(0.0, 0.0, w, h, rotation),
                want[4],
                "map_touch and the panel space are the same thing"
            );
        }
    }

    #[test]
    fn the_ipads_automatic_mapping_lands_exactly_on_the_limits_at_the_edges_and_corners() {
        let (m, mid) = (MAX, MID);
        let auto = HidMapping::default();
        // Native 270 is swap + flip x. With the framebuffer upright (2388x1668) the point (fx, fy)
        // becomes (1 - fy, fx); with it turned 90 (the picture portrait, 1668x2388) the two turns
        // cancel and the digitizer's axes are the upright ones.
        assert_eq!(
            mapped(&auto, 2388, 1668, 0, Some(270)),
            [(mid, 0), (mid, m), (m, mid), (0, mid), (m, 0), (m, m), (0, 0), (0, m)]
        );
        assert_eq!(
            mapped(&auto, 1668, 2388, 90, Some(270)),
            [(0, mid), (m, mid), (mid, 0), (mid, m), (0, 0), (m, 0), (0, m), (m, m)]
        );
        // Whatever the rotation: an edge is a limit on exactly one axis (the other is the middle), a
        // corner is a limit on both.
        for rotation in [0, 90, 180, 270] {
            let (w, h) = if rotation % 180 == 90 { (FB_H, FB_W) } else { (FB_W, FB_H) };
            let got = mapped(&auto, w, h, rotation, Some(270));
            let limit = |v: u16| v == 0 || v == m;
            for (i, (x, y)) in got.into_iter().enumerate() {
                if i < 4 {
                    assert!(
                        limit(x) != limit(y) && (x == mid || y == mid),
                        "edge {i} at rotation {rotation}: ({x}, {y})"
                    );
                } else {
                    assert!(limit(x) && limit(y), "corner {i} at rotation {rotation}: ({x}, {y})");
                }
            }
        }
        // The home bar: the bottom edge of the portrait picture is the digitizer's far limit on the axis
        // that runs down the screen, whichever way the screen is held, so the same on the flags that landed.
        let landed = HidMapping { space: HidSpace::Panel, swap_xy: true, flip_x: true, flip_y: false };
        for x in [0.0, 100.0, 834.0, 1667.0] {
            assert_eq!(auto.map(x, 2387.0, 1668, 2388, 90, Some(270)), (to_axis(x / 1667.0), m));
            assert_eq!(auto.map(x, 2387.0, 1668, 2388, 90, Some(270)), landed.map(x, 2387.0, 1668, 2388, 90, None));
        }
    }

    #[test]
    fn the_last_pixel_row_reaches_the_limit_which_it_used_to_stop_short_of() {
        // The page sends the last pixel row, 2387 of the iPad's 2388. It used to be 2387/2388 of the way:
        // HID 65508, 27 units short of the limit.
        assert_eq!(to_axis(2387.0 / 2388.0), 65508);
        let auto = HidMapping::default();
        assert_eq!(auto.map(600.0, 2387.0, 1668, 2388, 90, Some(270)).1, MAX);
        assert_eq!(map_touch(600.0, 2387.0, 1668, 2388, 0).1, MAX);
        assert_eq!(auto.map(1667.0, 1200.0, 1668, 2388, 90, Some(270)).0, MAX);
        assert_eq!(auto.map(0.0, 1200.0, 1668, 2388, 90, Some(270)).0, 0);
        // The row before it is still short of it, by one pixel's step.
        let before = auto.map(600.0, 2386.0, 1668, 2388, 90, Some(270)).1;
        assert!(before < MAX && MAX - before <= 28, "{before}");
        // At the size itself, or beyond, it is the limit too.
        assert_eq!(auto.map(600.0, 2388.0, 1668, 2388, 90, Some(270)).1, MAX);
        assert_eq!(auto.map(600.0, 5000.0, 1668, 2388, 90, Some(270)).1, MAX);
        // And the first row is 0.
        assert_eq!(auto.map(600.0, 0.0, 1668, 2388, 90, Some(270)).1, 0);
    }

    #[test]
    fn the_rescale_is_below_one_pixel_and_nothing_at_the_first_pixel() {
        // p / (size - 1) instead of p / size: a touch moves by p / (size * (size - 1)) of the axis,
        // nothing at pixel 0, half a pixel in the middle, one pixel at the last pixel.
        for size in [100u32, 1668, 2388, 2656] {
            let pixel = AXIS_MAX / f64::from(size); // one pixel of the picture, in digitizer units
            let moved_at = |p: u32| {
                let old = f64::from(p) / f64::from(size) * AXIS_MAX;
                f64::from(map_touch(f64::from(p), 0.0, size, 10, 0).0) - old
            };
            for p in 0..size {
                let moved = moved_at(p);
                assert!(moved >= -0.5 && moved <= pixel + 0.5, "size {size}, pixel {p}: moved {moved}");
            }
            // It grows along the axis (give or take the rounding of the integer axis) to one pixel.
            assert!((moved_at(size - 1) - pixel).abs() <= 0.5, "size {size}: {}", moved_at(size - 1));
            assert!(moved_at(size / 4) < moved_at(size / 2) && moved_at(size / 2) < moved_at(size - 1));
            assert_eq!(map_touch(0.0, 0.0, size, 10, 0).0, 0);
            let mid = size / 2;
            let moved =
                f64::from(map_touch(f64::from(mid), 0.0, size, 10, 0).0) - f64::from(mid) / f64::from(size) * AXIS_MAX;
            assert!(moved > 0.0 && moved < pixel, "size {size}: mid-screen moved {moved} of one pixel ({pixel})");
        }
        // The iPad's long axis: mid-screen moves by about 14 units (half a pixel), the far end by 27.
        let at = |p: f64| f64::from(map_touch(0.0, p, 1668, 2388, 0).1) - p / 2388.0 * AXIS_MAX;
        assert!((at(1194.0) - 13.7).abs() < 0.6, "{}", at(1194.0));
        assert!((at(2387.0) - 27.4).abs() < 0.6, "{}", at(2387.0));
    }

    #[test]
    fn the_edge_down_message_names_the_edge_and_only_edges() {
        let say = |x, y, px, py| edge_down_message(x, y, px, py, 1668, 2388);
        assert_eq!(
            say(30000, 65535, 800.0, 2387.0).as_deref(),
            Some("edge touch down at HID (30000, 65535) from upright pixel (800, 2387), edge: bottom")
        );
        assert_eq!(
            say(0, 30000, 0.0, 1000.5).as_deref(),
            Some("edge touch down at HID (0, 30000) from upright pixel (0, 1000.5), edge: left")
        );
        assert!(say(30000, 0, 800.0, 0.0).unwrap().ends_with("edge: top"));
        assert!(say(65535, 30000, 1667.0, 900.0).unwrap().ends_with("edge: right"));
        // A corner names both sides.
        assert!(say(65535, 65535, 1667.0, 2387.0).unwrap().ends_with("edge: right+bottom"));
        assert!(say(0, 0, 0.0, 0.0).unwrap().ends_with("edge: left+top"));
        // Inside, there is nothing to say; so one pixel in from the edge, unless the digitizer says so.
        assert_eq!(say(30000, 30000, 800.0, 1000.0), None);
        assert_eq!(say(1, 65534, 0.0, 2387.0), None);
        // The condition is the digitizer's limit, the name the picture's edge: an override that mirrors
        // the axis puts the picture's right edge at HID 0.
        let flipped = HidMapping { flip_x: true, ..HidMapping::default() };
        let (x, y) = flipped.map(1667.0, 1000.0, 1668, 2388, 0, None);
        assert_eq!(x, 0);
        assert!(edge_down_message(x, y, 1667.0, 1000.0, 1668, 2388).unwrap().ends_with("edge: right"));
    }

    #[derive(Clone, Default)]
    struct LogBuf(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for LogBuf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuf {
        type Writer = LogBuf;
        fn make_writer(&'a self) -> LogBuf {
            self.clone()
        }
    }

    #[test]
    fn every_touch_down_on_the_edge_is_logged_at_info_and_counted_and_nothing_else_is() {
        let (mut h, q, t0) = rig();
        let portrait = Some(View { width: 1668, height: 2388, rotation: 0, native: None });
        let log = LogBuf::default();
        let subscriber = tracing_subscriber::fmt().with_writer(log.clone()).with_ansi(false).finish();
        tracing::subscriber::with_default(subscriber, || {
            // The home bar swipe: down on the very last row, up the screen, up.
            h.on_touch(t0, touch(TouchPhase::Down, 800.0, 2387.0), portrait);
            h.on_touch(ms(t0, 20), touch(TouchPhase::Move, 800.0, 2000.0), portrait);
            h.on_touch(ms(t0, 40), touch(TouchPhase::Up, 800.0, 1500.0), portrait);
            // A touch inside, one on the left edge, a move and a lift onto the edge, one in a corner.
            h.on_touch(ms(t0, 60), touch(TouchPhase::Down, 800.0, 1000.0), portrait);
            h.on_touch(ms(t0, 80), touch(TouchPhase::Move, 0.0, 1000.0), portrait);
            h.on_touch(ms(t0, 100), touch(TouchPhase::Up, 0.0, 1000.0), portrait);
            h.on_touch(ms(t0, 120), touch(TouchPhase::Down, 0.0, 1000.0), portrait);
            h.on_touch(ms(t0, 140), touch(TouchPhase::Up, 5.0, 1000.0), portrait);
            h.on_touch(ms(t0, 160), touch(TouchPhase::Down, 1667.0, 0.0), portrait);
            // A finger that is not the first one is ignored, edge or not.
            h.on_touch(ms(t0, 180), Touch { phase: TouchPhase::Down, x: 0.0, y: 0.0, id: 1 }, portrait);
        });
        assert_eq!(q.edge_downs(), 3);
        let text = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
        let lines: Vec<&str> = text.lines().filter(|l| l.contains("edge touch down")).collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert!(lines.iter().all(|l| l.contains(" INFO ")), "{text}");
        let hid_at = |x, y| map_touch(x, y, 1668, 2388, 0);
        let (x0, y0) = hid_at(800.0, 2387.0);
        assert_eq!(y0, MAX);
        assert!(
            lines[0].ends_with(&format!(
                "edge touch down at HID ({x0}, {y0}) from upright pixel (800, 2387), edge: bottom"
            )),
            "{text}"
        );
        let (x1, y1) = hid_at(0.0, 1000.0);
        assert_eq!(x1, 0);
        assert!(
            lines[1]
                .ends_with(&format!("edge touch down at HID ({x1}, {y1}) from upright pixel (0, 1000), edge: left")),
            "{text}"
        );
        assert!(
            lines[2].ends_with("edge touch down at HID (65535, 0) from upright pixel (1667, 0), edge: right+top"),
            "{text}"
        );
        // The stats event of the engine carries the count (the queue is where the two meet).
        let cfg = crate::engine::EngineConfig::new(0x1234_5678);
        let e = crate::engine::Engine::new(cfg, q.clone(), Instant::now());
        assert_eq!(e.stats().edge_downs, 3);
    }

    // ---- the queue between the controller and the service ------------------------------------

    fn contact(x: u16) -> Action {
        Action::Touch { down: true, x, y: 0 }
    }

    fn lift(x: u16) -> Action {
        Action::Touch { down: false, x, y: 0 }
    }

    fn home(down: bool) -> Action {
        Action::Button { page: 0x0C, code: 0x40, down }
    }

    fn drained(q: &mut ActionQueue) -> Vec<Action> {
        std::iter::from_fn(|| q.pop()).collect()
    }

    /// What waits in the queue, without taking it.
    fn waiting(q: &ActionQueue) -> Vec<Action> {
        q.items.iter().map(|i| i.action).collect()
    }

    #[test]
    fn a_pending_move_is_replaced_by_the_next_one_and_the_order_is_kept() {
        let mut q = ActionQueue::default();
        for a in [contact(1), contact(2), contact(3), contact(4)] {
            assert!(q.push(a));
        }
        // The first is the down; the three after it are moves of which only the newest is kept.
        assert_eq!(q.len(), 2);
        assert_eq!(drained(&mut q), vec![contact(1), contact(4)]);
        assert!(q.is_empty());
    }

    #[test]
    fn a_down_is_never_replaced_by_a_move_and_a_lift_or_a_key_in_between_keeps_moves_apart() {
        let mut q = ActionQueue::default();
        let key = Action::Key { usage: 4, down: true };
        for a in [
            contact(1), // down
            contact(2), // move
            key,        // a key in between: the next move does not reach back over it
            contact(3), // move
            contact(4), // replaces 3
            lift(5),
            contact(6), // a new down: not a move, so not merged with anything
            contact(7), // move
            home(true),
            home(false),
            lift(8),
        ] {
            assert!(q.push(a), "{a:?}");
        }
        assert_eq!(
            drained(&mut q),
            vec![
                contact(1),
                contact(2),
                key,
                contact(4),
                lift(5),
                contact(6),
                contact(7),
                home(true),
                home(false),
                lift(8)
            ]
        );
    }

    #[test]
    fn a_full_queue_makes_room_with_its_oldest_move_and_refuses_a_press_but_never_a_release() {
        let mut q = ActionQueue::default();
        assert!(q.push(contact(0))); // the down
        let key = |n: u8| Action::Key { usage: n, down: true };
        // Presses (and a move among them) up to the cap.
        assert!(q.push(key(0)));
        assert!(q.push(contact(1))); // a move, after a key: its own entry
        for n in 1..(ACTION_QUEUE_CAP as u8 - 2) {
            assert!(q.push(key(n)));
        }
        assert_eq!(q.len(), ACTION_QUEUE_CAP);
        // Another press: the pending move is the one to go.
        assert!(q.push(key(100)));
        assert_eq!(q.len(), ACTION_QUEUE_CAP);
        assert!(!waiting(&q).contains(&contact(1)));
        // Nothing to give up now: a press is refused, and so is a down after a lift ...
        assert!(!q.push(key(101)));
        assert!(!q.push(home(true)));
        assert_eq!(q.len(), ACTION_QUEUE_CAP);
        // ... but a lift, a button up, a key up and a keyboard release are always taken, in order.
        for a in [
            lift(9),
            home(false),
            Action::Key { usage: 4, down: false },
            Action::KeyboardRelease,
            Action::KeyboardRemove,
        ] {
            assert!(q.push(a), "{a:?}");
        }
        let all = drained(&mut q);
        assert_eq!(all.len(), ACTION_QUEUE_CAP + 5);
        assert_eq!(all[0], contact(0));
        assert_eq!(all[ACTION_QUEUE_CAP - 1], key(100));
        assert_eq!(
            all[ACTION_QUEUE_CAP..],
            [
                lift(9),
                home(false),
                Action::Key { usage: 4, down: false },
                Action::KeyboardRelease,
                Action::KeyboardRemove
            ]
        );
    }

    #[test]
    fn a_move_when_the_queue_is_full_of_presses_is_the_one_that_replaces_the_oldest_move() {
        let mut q = ActionQueue::default();
        assert!(q.push(contact(0)));
        assert!(q.push(Action::Key { usage: 1, down: true }));
        assert!(q.push(contact(1))); // a move, behind the key
        assert!(q.push(Action::Key { usage: 2, down: true }));
        while q.len() < ACTION_QUEUE_CAP {
            assert!(q.push(Action::Key { usage: 3, down: true }));
        }
        // A new move cannot merge (a key is last); the oldest move is dropped for it.
        assert!(q.push(contact(2)));
        let all = waiting(&q);
        assert_eq!(all.len(), ACTION_QUEUE_CAP);
        assert!(!all.contains(&contact(1)));
        assert_eq!(all.last(), Some(&contact(2)));
    }

    #[test]
    fn at_the_end_the_waiting_moves_are_dropped_and_the_rest_stays() {
        let mut q = ActionQueue::default();
        let key = Action::Key { usage: 4, down: true };
        for a in [contact(1), contact(2), key, contact(3), home(true)] {
            q.push(a);
        }
        assert_eq!(q.drop_moves(), 2);
        assert_eq!(drained(&mut q), vec![contact(1), key, home(true)]);
    }

    #[test]
    fn what_lets_go_is_a_release() {
        assert!(lift(0).is_release());
        assert!(home(false).is_release());
        assert!(Action::Key { usage: 4, down: false }.is_release());
        assert!(Action::KeyboardRelease.is_release());
        assert!(Action::KeyboardRemove.is_release());
        assert!(!contact(0).is_release());
        assert!(!home(true).is_release());
        assert!(!Action::Key { usage: 4, down: true }.is_release());
    }
}
