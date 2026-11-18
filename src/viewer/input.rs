//! From what the window sees (the mouse, the keyboard) to what the stream takes ([`Command`]s). No window and no
//! device here: the app feeds these types with the events it gets and sends what they return, so the rules are testable.
//!
//! * The **mouse** is one finger, id 0: pressing the left button inside the picture is `touch down` at the mapped
//!   point of the upright picture, moving with it held is `touch move`, releasing is `touch up` (a drag that leaves the
//!   picture sticks to its edge, as the finger would when it slides off the screen's rim). Losing the pointer, the focus
//!   or the picture's orientation lifts the finger where it is, so that it can never stay down on the device.
//! * The **keyboard** is the device's keyboard: a key goes down and up on the device as it does on the host, by its
//!   position on the keyboard (a HID usage, so a US-layout device gets the US meaning of the key, whatever the host's
//!   layout), with auto-repeat left to the device. A key with no usage that yields text types that text.
//! * The **shortcuts** are the keys pressed while the *shortcut modifier* is held: Cmd on macOS, Alt elsewhere (as
//!   scrcpy's MOD is). They are the viewer's and are not sent to the device, and neither is the modifier key itself:
//!   `MOD+H` Home, `MOD+S` App Switcher, `MOD+Up` and `MOD+Down` volume, `MOD+G` the swipe up from the bottom edge (the
//!   gesture that goes Home or unlocks), `MOD+W` closes the window. `MOD+P` locks the device only when the user asked for
//!   it (`--lock-shortcut`): a lock from the keyboard while the screen is the only way in is a way to lock oneself out.
//! * The **margin** around the picture (see [`Layout`]) makes the device's edges easy to start a drag at: a press in it is
//!   on the device's edge, which is what the side and top edge swipes need.
//! * The **bottom swipe** is different: iOS ignores the home-indicator swipe when it is injected as touch, even at the
//!   exact edge. So (`--bottom-swipe keys`, the default) a drag that starts in the bottom margin or the picture's lowest
//!   strip and goes up is not sent as touch: when it ends it is the Home button, or the App Switcher if it paused before
//!   it ended, and a drag that is not that swipe is sent as the touch it was ([`Mouse`]).

use std::time::Duration;

use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey};

use crate::commands::{Command, Kb, Key as HwKey, Press, Touch, TouchPhase};
use crate::viewer::geometry::Layout;

/// Which host modifier is the viewer's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutModifier {
    /// Cmd on a Mac (the Windows key elsewhere).
    Super,
    /// Alt (Option on a Mac).
    Alt,
}

impl ShortcutModifier {
    /// The convention of this host: Cmd on macOS, Alt on Linux and Windows.
    #[must_use]
    pub const fn host_default() -> ShortcutModifier {
        if cfg!(target_os = "macos") { ShortcutModifier::Super } else { ShortcutModifier::Alt }
    }

    /// How the documentation and the log name it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            ShortcutModifier::Super => "Cmd",
            ShortcutModifier::Alt => "Alt",
        }
    }

    fn held(self, modifiers: ModifiersState) -> bool {
        match self {
            ShortcutModifier::Super => modifiers.super_key(),
            ShortcutModifier::Alt => modifiers.alt_key(),
        }
    }

    /// Whether `code` is a key of this modifier itself.
    fn is_key(self, code: KeyCode) -> bool {
        match self {
            ShortcutModifier::Super => matches!(code, KeyCode::SuperLeft | KeyCode::SuperRight),
            ShortcutModifier::Alt => matches!(code, KeyCode::AltLeft | KeyCode::AltRight),
        }
    }
}

/// What the app is to do about a key, besides sending what it returns.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Send this to the stream.
    Send(Command),
    /// Close the window (and with it stop the stream).
    Close,
    /// The bottom swipe, from the keyboard (what `--bottom-swipe` says it is: the Home button, or the touch of
    /// [`swipe_up`]).
    SwipeUp,
    /// Commands to send, each after its wait, from a thread of their own (a touch replayed with its timing).
    Play(Vec<(Duration, Command)>),
    /// What was just done, to say in the window ("Home").
    Note(&'static str),
}

/// A key event, as much of a winit `KeyEvent` as the rules need (a `KeyEvent` cannot be built outside winit, so the
/// tests could not use it).
#[derive(Debug, Clone)]
pub struct KeyPress<'a> {
    /// The key by its position.
    pub physical: PhysicalKey,
    /// The key by what it is on the host's layout, with no modifier applied: `h`, not `H` or `˙`.
    pub logical: &'a Key,
    /// The text the press produces, if any.
    pub text: Option<&'a str>,
    pub pressed: bool,
    /// An auto-repeat of a key that is held (the host's: the device repeats a held key itself).
    pub repeat: bool,
    pub modifiers: ModifiersState,
}

/// The keyboard's state: which keys are down on the device, and which presses were shortcuts.
#[derive(Debug)]
pub struct Keyboard {
    modifier: ShortcutModifier,
    lock_shortcut: bool,
    /// The keys down on the device: the host key and the usage that went down for it.
    down: Vec<(KeyCode, u8)>,
    /// The keys whose press was the viewer's (a shortcut, or the modifier itself): their release is too.
    swallowed: Vec<KeyCode>,
}

impl Keyboard {
    #[must_use]
    pub fn new(modifier: ShortcutModifier, lock_shortcut: bool) -> Keyboard {
        Keyboard { modifier, lock_shortcut, down: Vec::new(), swallowed: Vec::new() }
    }

    /// One key event. Returns what to do about it, in order.
    pub fn key(&mut self, press: &KeyPress<'_>) -> Vec<Action> {
        let code = match press.physical {
            PhysicalKey::Code(code) => Some(code),
            PhysicalKey::Unidentified(_) => None,
        };
        if !press.pressed {
            return self.released(code);
        }
        // The modifier's own key is the viewer's.
        if let Some(code) = code
            && self.modifier.is_key(code)
        {
            self.swallow(code);
            return Vec::new();
        }
        if self.modifier.held(press.modifiers) {
            if let Some(code) = code {
                self.swallow(code);
            }
            return self.shortcut(press).into_iter().collect();
        }
        if press.repeat {
            return Vec::new();
        }
        if let Some(code) = code
            && let Some(usage) = hid_usage(code)
        {
            if self.down.iter().any(|(c, _)| *c == code) {
                return Vec::new();
            }
            self.down.push((code, usage));
            return vec![Action::Send(Command::Kb(Kb::Down(usage)))];
        }
        // A key with no place on the keyboard that makes text (an input method's, a key of a layout the table does not
        // know): the text itself.
        match press.text {
            Some(text) if !text.is_empty() && !text.chars().any(char::is_control) => {
                vec![Action::Send(Command::Kb(Kb::Type(text.to_owned())))]
            }
            _ => Vec::new(),
        }
    }

    fn swallow(&mut self, code: KeyCode) {
        if !self.swallowed.contains(&code) {
            self.swallowed.push(code);
        }
    }

    fn released(&mut self, code: Option<KeyCode>) -> Vec<Action> {
        let Some(code) = code else { return Vec::new() };
        // What went down on the device comes up, whatever the modifiers are now.
        if let Some(i) = self.down.iter().position(|(c, _)| *c == code) {
            let (_, usage) = self.down.remove(i);
            return vec![Action::Send(Command::Kb(Kb::Up(usage)))];
        }
        self.swallowed.retain(|c| *c != code);
        Vec::new()
    }

    /// A key press with the shortcut modifier held.
    fn shortcut(&self, press: &KeyPress<'_>) -> Option<Action> {
        let shortcut = Shortcut::of(press.logical, self.lock_shortcut)?;
        if press.repeat && !shortcut.repeats() {
            return None;
        }
        Some(match shortcut {
            Shortcut::Home => Action::Send(Command::Key(HwKey::Home, Press::Short)),
            Shortcut::AppSwitcher => Action::Send(Command::Key(HwKey::AppSwitcher, Press::Short)),
            Shortcut::VolumeUp => Action::Send(Command::Key(HwKey::VolumeUp, Press::Short)),
            Shortcut::VolumeDown => Action::Send(Command::Key(HwKey::VolumeDown, Press::Short)),
            Shortcut::Lock => Action::Send(Command::Key(HwKey::Lock, Press::Short)),
            Shortcut::Close => Action::Close,
            Shortcut::SwipeUp => Action::SwipeUp,
        })
    }

    /// The window lost the keyboard: every key that is down on the device comes up, so that none stays down. Keys
    /// before modifiers.
    pub fn release_all(&mut self) -> Vec<Action> {
        self.swallowed.clear();
        let mut usages: Vec<u8> = self.down.drain(..).map(|(_, usage)| usage).collect();
        usages.sort_unstable();
        usages.into_iter().map(|usage| Action::Send(Command::Kb(Kb::Up(usage)))).collect()
    }

    /// How many keys are down on the device.
    #[must_use]
    pub fn held(&self) -> usize {
        self.down.len()
    }
}

/// What a shortcut does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shortcut {
    Home,
    AppSwitcher,
    VolumeUp,
    VolumeDown,
    Lock,
    Close,
    SwipeUp,
}

impl Shortcut {
    fn of(key: &Key, lock_enabled: bool) -> Option<Shortcut> {
        match key {
            Key::Named(NamedKey::ArrowUp) => Some(Shortcut::VolumeUp),
            Key::Named(NamedKey::ArrowDown) => Some(Shortcut::VolumeDown),
            Key::Character(c) if c.eq_ignore_ascii_case("h") => Some(Shortcut::Home),
            Key::Character(c) if c.eq_ignore_ascii_case("s") => Some(Shortcut::AppSwitcher),
            Key::Character(c) if c.eq_ignore_ascii_case("w") => Some(Shortcut::Close),
            Key::Character(c) if c.eq_ignore_ascii_case("g") => Some(Shortcut::SwipeUp),
            Key::Character(c) if lock_enabled && c.eq_ignore_ascii_case("p") => Some(Shortcut::Lock),
            _ => None,
        }
    }

    /// Whether holding the keys repeats the shortcut (volume does, as it does on the device's own buttons).
    fn repeats(self) -> bool {
        matches!(self, Shortcut::VolumeUp | Shortcut::VolumeDown)
    }
}

/// The HID keyboard-page usage of a key by its position: the letter keys, the digits, the editing and function keys,
/// the keypad and the modifiers. `None` for a key the device's keyboard has no usage for.
#[must_use]
#[rustfmt::skip]
pub fn hid_usage(code: KeyCode) -> Option<u8> {
    use KeyCode as K;
    Some(match code {
        K::KeyA => 0x04, K::KeyB => 0x05, K::KeyC => 0x06, K::KeyD => 0x07, K::KeyE => 0x08, K::KeyF => 0x09,
        K::KeyG => 0x0A, K::KeyH => 0x0B, K::KeyI => 0x0C, K::KeyJ => 0x0D, K::KeyK => 0x0E, K::KeyL => 0x0F,
        K::KeyM => 0x10, K::KeyN => 0x11, K::KeyO => 0x12, K::KeyP => 0x13, K::KeyQ => 0x14, K::KeyR => 0x15,
        K::KeyS => 0x16, K::KeyT => 0x17, K::KeyU => 0x18, K::KeyV => 0x19, K::KeyW => 0x1A, K::KeyX => 0x1B,
        K::KeyY => 0x1C, K::KeyZ => 0x1D,
        K::Digit1 => 0x1E, K::Digit2 => 0x1F, K::Digit3 => 0x20, K::Digit4 => 0x21, K::Digit5 => 0x22,
        K::Digit6 => 0x23, K::Digit7 => 0x24, K::Digit8 => 0x25, K::Digit9 => 0x26, K::Digit0 => 0x27,
        K::Enter => 0x28, K::Escape => 0x29, K::Backspace => 0x2A, K::Tab => 0x2B, K::Space => 0x2C,
        K::Minus => 0x2D, K::Equal => 0x2E, K::BracketLeft => 0x2F, K::BracketRight => 0x30, K::Backslash => 0x31,
        K::Semicolon => 0x33, K::Quote => 0x34, K::Backquote => 0x35, K::Comma => 0x36, K::Period => 0x37,
        K::Slash => 0x38, K::CapsLock => 0x39,
        K::F1 => 0x3A, K::F2 => 0x3B, K::F3 => 0x3C, K::F4 => 0x3D, K::F5 => 0x3E, K::F6 => 0x3F,
        K::F7 => 0x40, K::F8 => 0x41, K::F9 => 0x42, K::F10 => 0x43, K::F11 => 0x44, K::F12 => 0x45,
        K::PrintScreen => 0x46, K::ScrollLock => 0x47, K::Pause => 0x48, K::Insert => 0x49, K::Home => 0x4A,
        K::PageUp => 0x4B, K::Delete => 0x4C, K::End => 0x4D, K::PageDown => 0x4E,
        K::ArrowRight => 0x4F, K::ArrowLeft => 0x50, K::ArrowDown => 0x51, K::ArrowUp => 0x52,
        K::NumLock => 0x53, K::NumpadDivide => 0x54, K::NumpadMultiply => 0x55, K::NumpadSubtract => 0x56,
        K::NumpadAdd => 0x57, K::NumpadEnter => 0x58,
        K::Numpad1 => 0x59, K::Numpad2 => 0x5A, K::Numpad3 => 0x5B, K::Numpad4 => 0x5C, K::Numpad5 => 0x5D,
        K::Numpad6 => 0x5E, K::Numpad7 => 0x5F, K::Numpad8 => 0x60, K::Numpad9 => 0x61, K::Numpad0 => 0x62,
        K::NumpadDecimal => 0x63, K::IntlBackslash => 0x64, K::ContextMenu => 0x65, K::NumpadEqual => 0x67,
        K::F13 => 0x68, K::F14 => 0x69, K::F15 => 0x6A, K::F16 => 0x6B, K::F17 => 0x6C, K::F18 => 0x6D,
        K::F19 => 0x6E, K::F20 => 0x6F, K::F21 => 0x70, K::F22 => 0x71, K::F23 => 0x72, K::F24 => 0x73,
        K::ControlLeft => 0xE0, K::ShiftLeft => 0xE1, K::AltLeft => 0xE2, K::SuperLeft => 0xE3,
        K::ControlRight => 0xE4, K::ShiftRight => 0xE5, K::AltRight => 0xE6, K::SuperRight => 0xE7,
        _ => return None,
    })
}

/// The mouse as one finger.
#[derive(Debug, Default)]
pub struct Pointer {
    /// Where the finger is down, in pixels of the upright picture (`None`: it is up).
    finger: Option<(f64, f64)>,
}

fn touch(phase: TouchPhase, at: (f64, f64)) -> Command {
    Command::Touch(Touch { phase, x: at.0, y: at.1, id: 0 })
}

impl Pointer {
    /// The left button went down at `at` (physical pixels of the window). A press in the bars is no touch; a press in the
    /// margin is on the device's edge ([`Layout::press`]).
    pub fn pressed(&mut self, layout: Layout, at: (f64, f64)) -> Option<Command> {
        if self.finger.is_some() {
            return None;
        }
        let point = layout.press(at)?;
        self.finger = Some(point);
        Some(touch(TouchPhase::Down, point))
    }

    /// The pointer moved. With the finger down this is a move, kept on the picture.
    pub fn moved(&mut self, layout: Layout, at: (f64, f64)) -> Option<Command> {
        let last = self.finger?;
        let point = layout.follow(at)?;
        if point == last {
            return None;
        }
        self.finger = Some(point);
        Some(touch(TouchPhase::Move, point))
    }

    /// The left button went up: the finger lifts where the pointer is (kept on the picture).
    pub fn released(&mut self, layout: Layout, at: (f64, f64)) -> Option<Command> {
        let last = self.finger.take()?;
        let point = layout.follow(at).unwrap_or(last);
        Some(touch(TouchPhase::Up, point))
    }

    /// The finger lifts where it is: the pointer or the focus was lost, or the picture turned under it.
    pub fn cancel(&mut self) -> Option<Command> {
        let last = self.finger.take()?;
        Some(touch(TouchPhase::Up, last))
    }

    /// The finger is down at `at` already (its `down` was sent by the caller).
    fn adopt(&mut self, at: (f64, f64)) {
        self.finger = Some(at);
    }

    #[must_use]
    pub fn is_down(&self) -> bool {
        self.finger.is_some()
    }
}

/// What a drag from the bottom edge is sent as (`--bottom-swipe`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BottomSwipe {
    /// The Home button (or, held, the App Switcher): what iOS accepts.
    #[default]
    Keys,
    /// Touch, as any other drag: iOS ignores it at the bottom edge today.
    Touch,
}

/// The lowest part of the picture, as a fraction of its height, where a press can be the start of a bottom swipe.
pub const BOTTOM_STRIP: f64 = 0.03;
/// How far a drag must rise to be a swipe, as a fraction of the picture's height (and how far sideways one must go to be
/// a drag that is not).
const SWIPE_THRESHOLD: f64 = 0.08;
/// A pause this long, while the drag is risen, makes the swipe the App Switcher's.
pub const HOLD: Duration = Duration::from_millis(300);
/// The press of a tap that is replayed is at least this long, and at most the next.
const REPLAY_MIN: Duration = Duration::from_millis(60);
const REPLAY_MAX: Duration = Duration::from_millis(400);

/// What a drag from the bottom edge is, from where it started to where it ended: `Some(button)` if it is a swipe up (it
/// rose by 8% of the height and went no further sideways than up), else `None`. `paused` says the
/// pointer stayed still, risen, for [`HOLD`]: the App Switcher; otherwise Home.
#[must_use]
pub fn bottom_swipe(start: (f64, f64), end: (f64, f64), paused: bool, upright: (u32, u32)) -> Option<HwKey> {
    let rise = start.1 - end.1;
    let run = (end.0 - start.0).abs();
    if rise < f64::from(upright.1) * SWIPE_THRESHOLD || run > rise {
        return None;
    }
    Some(if paused { HwKey::AppSwitcher } else { HwKey::Home })
}

/// Whether a drag from the bottom edge has gone sideways for good: further across than the threshold and than it rose, so
/// it is no swipe up and is sent as the touch it is.
fn sideways(start: (f64, f64), end: (f64, f64), upright: (u32, u32)) -> bool {
    let run = (end.0 - start.0).abs();
    run > f64::from(upright.1) * SWIPE_THRESHOLD && run > start.1 - end.1
}

/// A press at the bottom edge that has not been decided yet: nothing was sent for it.
#[derive(Debug, Clone, Copy)]
struct Pending {
    start: (f64, f64),
    pressed_at: Duration,
    last: (f64, f64),
    moved_at: Duration,
    /// The pointer stayed still, risen, for [`HOLD`] at some point.
    paused: bool,
    /// The pointer moved at all since the press.
    moved: bool,
}

/// The mouse: the finger ([`Pointer`]) and, with `--bottom-swipe keys`, the bottom swipe that is made into buttons.
///
/// A press that starts where a bottom swipe starts is held back: nothing is sent for it until it is decided. If it rises
/// far enough without going sideways, it is that swipe and, when the button is released, it is **Home** (a pause before the
/// release makes it the **App Switcher**) and no touch was ever sent. If it goes sideways it is a touch from its start,
/// sent at once and followed live. If it ends without being either (a tap, a short drag) it is replayed as the touch it was.
/// Every other press is the finger's, as it was. Time is given by the caller (a monotonic clock) so that this is testable.
#[derive(Debug, Default)]
pub struct Mouse {
    mode: BottomSwipe,
    finger: Pointer,
    pending: Option<Pending>,
}

impl Mouse {
    #[must_use]
    pub fn new(mode: BottomSwipe) -> Mouse {
        Mouse { mode, finger: Pointer::default(), pending: None }
    }

    /// The left button went down at `at`, at time `now`.
    pub fn pressed(&mut self, layout: Layout, at: (f64, f64), now: Duration) -> Vec<Action> {
        if self.pending.is_some() {
            return Vec::new();
        }
        if self.mode == BottomSwipe::Keys
            && !self.finger.is_down()
            && layout.starts_bottom_swipe(at, BOTTOM_STRIP)
            && let Some(start) = layout.press(at)
        {
            self.pending =
                Some(Pending { start, pressed_at: now, last: start, moved_at: now, paused: false, moved: false });
            return Vec::new();
        }
        self.finger.pressed(layout, at).map(|c| vec![Action::Send(c)]).unwrap_or_default()
    }

    /// The pointer moved to `at`, at time `now`.
    pub fn moved(&mut self, layout: Layout, at: (f64, f64), now: Duration) -> Vec<Action> {
        let Some(mut pending) = self.pending else {
            return self.finger.moved(layout, at).map(|c| vec![Action::Send(c)]).unwrap_or_default();
        };
        let Some(point) = layout.follow(at) else { return Vec::new() };
        // A pause while risen: the pointer was still for HOLD before this move.
        let risen = pending.start.1 - pending.last.1 >= f64::from(layout.upright.1) * SWIPE_THRESHOLD;
        if risen && now.saturating_sub(pending.moved_at) >= HOLD {
            pending.paused = true;
        }
        pending.last = point;
        pending.moved_at = now;
        pending.moved = true;
        if sideways(pending.start, point, layout.upright) {
            // Not a swipe up: the touch it is, from where it started, and the finger follows from here.
            self.pending = None;
            let down = touch(TouchPhase::Down, pending.start);
            let to = touch(TouchPhase::Move, point);
            self.finger.adopt(point);
            return vec![Action::Send(down), Action::Send(to)];
        }
        self.pending = Some(pending);
        Vec::new()
    }

    /// The left button went up at `at`, at time `now`.
    pub fn released(&mut self, layout: Layout, at: (f64, f64), now: Duration) -> Vec<Action> {
        let Some(pending) = self.pending.take() else {
            return self.finger.released(layout, at).map(|c| vec![Action::Send(c)]).unwrap_or_default();
        };
        // A press that never moved ends where it began (its position is the edge's, not the pointer's).
        let end = if pending.moved { layout.follow(at).unwrap_or(pending.last) } else { pending.start };
        let risen = pending.start.1 - pending.last.1 >= f64::from(layout.upright.1) * SWIPE_THRESHOLD;
        let paused = pending.paused || (risen && now.saturating_sub(pending.moved_at) >= HOLD);
        if let Some(button) = bottom_swipe(pending.start, end, paused, layout.upright) {
            let name = if button == HwKey::AppSwitcher { "App Switcher" } else { "Home" };
            return vec![Action::Send(Command::Key(button, Press::Short)), Action::Note(name)];
        }
        // Not a swipe: the touch it was, sent now, as long as the press was (so that a tap is a tap).
        let held = now.saturating_sub(pending.pressed_at).clamp(REPLAY_MIN, REPLAY_MAX);
        let mut plan = vec![(Duration::ZERO, touch(TouchPhase::Down, pending.start))];
        if end == pending.start {
            plan.push((held, touch(TouchPhase::Up, end)));
        } else {
            plan.push((held / 2, touch(TouchPhase::Move, end)));
            plan.push((held / 2, touch(TouchPhase::Up, end)));
        }
        vec![Action::Play(plan)]
    }

    /// The finger lifts where it is, or the press that was held back is dropped (nothing was sent for it): the pointer or the
    /// focus was lost, or the picture turned under it.
    pub fn cancel(&mut self) -> Vec<Action> {
        self.pending = None;
        self.finger.cancel().map(|c| vec![Action::Send(c)]).unwrap_or_default()
    }

    /// Whether a button is down: a finger on the device, or a press that is not decided yet.
    #[must_use]
    pub fn is_down(&self) -> bool {
        self.pending.is_some() || self.finger.is_down()
    }
}

/// How far the swipe from the bottom edge rises, as a fraction of the picture's height.
const SWIPE_RISE: f64 = 0.30;
/// How many moves it makes, and how long each step is: about a tenth of a second in all, a flick.
const SWIPE_STEPS: u32 = 12;
const SWIPE_STEP: Duration = Duration::from_millis(12);

/// The swipe up from the bottom edge, as the finger does it: down on the device's last row in the middle, a straight
/// run upwards, and up. Each command comes with how long to wait before it. It is the gesture that goes Home (and
/// unlocks), made on the device's own edge, with no pointer to aim.
#[must_use]
pub fn swipe_up(upright: (u32, u32)) -> Vec<(Duration, Command)> {
    let x = f64::from(upright.0) / 2.0;
    let bottom = f64::from(upright.1.saturating_sub(1));
    let rise = f64::from(upright.1) * SWIPE_RISE;
    let mut plan = vec![(Duration::ZERO, touch(TouchPhase::Down, (x, bottom)))];
    for step in 1..=SWIPE_STEPS {
        let y = bottom - rise * f64::from(step) / f64::from(SWIPE_STEPS);
        plan.push((SWIPE_STEP, touch(TouchPhase::Move, (x, y))));
    }
    plan.push((SWIPE_STEP, touch(TouchPhase::Up, (x, bottom - rise))));
    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::keyboard::NativeKeyCode;

    fn code(c: KeyCode) -> PhysicalKey {
        PhysicalKey::Code(c)
    }

    fn ch(s: &str) -> Key {
        Key::Character(s.into())
    }

    fn press(physical: PhysicalKey, logical: &Key, modifiers: ModifiersState) -> KeyPress<'_> {
        KeyPress { physical, logical, text: None, pressed: true, repeat: false, modifiers }
    }

    fn release(physical: PhysicalKey, logical: &Key, modifiers: ModifiersState) -> KeyPress<'_> {
        KeyPress { pressed: false, ..press(physical, logical, modifiers) }
    }

    fn sent(actions: &[Action]) -> Vec<Command> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Send(c) => Some(c.clone()),
                Action::Close | Action::SwipeUp | Action::Play(_) | Action::Note(_) => None,
            })
            .collect()
    }

    const NONE: ModifiersState = ModifiersState::empty();

    fn mac() -> Keyboard {
        Keyboard::new(ShortcutModifier::Super, false)
    }

    #[test]
    fn the_shortcut_modifier_is_cmd_on_a_mac_and_alt_elsewhere() {
        let expected = if cfg!(target_os = "macos") { ShortcutModifier::Super } else { ShortcutModifier::Alt };
        assert_eq!(ShortcutModifier::host_default(), expected);
        assert_eq!(ShortcutModifier::Super.name(), "Cmd");
        assert_eq!(ShortcutModifier::Alt.name(), "Alt");
        assert!(ShortcutModifier::Super.held(ModifiersState::SUPER));
        assert!(!ShortcutModifier::Super.held(ModifiersState::ALT));
        assert!(ShortcutModifier::Alt.held(ModifiersState::ALT));
        assert!(!ShortcutModifier::Alt.held(ModifiersState::SUPER | ModifiersState::CONTROL));
    }

    #[test]
    fn a_plain_key_goes_down_and_up_on_the_device_by_its_usage() {
        let mut kb = mac();
        let a = ch("a");
        assert_eq!(sent(&kb.key(&press(code(KeyCode::KeyA), &a, NONE))), vec![Command::Kb(Kb::Down(0x04))]);
        assert_eq!(kb.held(), 1);
        // The host's auto-repeat is not forwarded: the device repeats a key that is held.
        let repeat = KeyPress { repeat: true, ..press(code(KeyCode::KeyA), &a, NONE) };
        assert!(kb.key(&repeat).is_empty());
        assert_eq!(sent(&kb.key(&release(code(KeyCode::KeyA), &a, NONE))), vec![Command::Kb(Kb::Up(0x04))]);
        assert_eq!(kb.held(), 0);
        // A release of a key that was never down says nothing.
        assert!(kb.key(&release(code(KeyCode::KeyB), &ch("b"), NONE)).is_empty());
    }

    #[test]
    fn shift_is_a_key_of_its_own_and_goes_down_before_the_letter() {
        let mut kb = mac();
        let shift = Key::Named(NamedKey::Shift);
        let a = ch("a");
        let shifted = ModifiersState::SHIFT;
        assert_eq!(sent(&kb.key(&press(code(KeyCode::ShiftLeft), &shift, shifted))), vec![Command::Kb(Kb::Down(0xE1))]);
        assert_eq!(sent(&kb.key(&press(code(KeyCode::KeyA), &a, shifted))), vec![Command::Kb(Kb::Down(0x04))]);
        assert_eq!(sent(&kb.key(&release(code(KeyCode::KeyA), &a, shifted))), vec![Command::Kb(Kb::Up(0x04))]);
        assert_eq!(sent(&kb.key(&release(code(KeyCode::ShiftLeft), &shift, NONE))), vec![Command::Kb(Kb::Up(0xE1))]);
    }

    #[test]
    fn the_shortcuts_press_the_buttons_and_send_no_key() {
        let mut kb = mac();
        let cmd = ModifiersState::SUPER;
        // Cmd itself is not sent.
        assert!(kb.key(&press(code(KeyCode::SuperLeft), &Key::Named(NamedKey::Super), cmd)).is_empty());
        for (key, physical, want) in [
            (ch("h"), KeyCode::KeyH, Command::Key(HwKey::Home, Press::Short)),
            (ch("s"), KeyCode::KeyS, Command::Key(HwKey::AppSwitcher, Press::Short)),
            (Key::Named(NamedKey::ArrowUp), KeyCode::ArrowUp, Command::Key(HwKey::VolumeUp, Press::Short)),
            (Key::Named(NamedKey::ArrowDown), KeyCode::ArrowDown, Command::Key(HwKey::VolumeDown, Press::Short)),
        ] {
            assert_eq!(sent(&kb.key(&press(code(physical), &key, cmd))), vec![want], "{key:?}");
            // The release of a shortcut's key says nothing either.
            assert!(kb.key(&release(code(physical), &key, cmd)).is_empty());
        }
        assert_eq!(kb.held(), 0);
        // Upper case is the same shortcut (Shift held, or caps lock).
        assert_eq!(
            sent(&kb.key(&press(code(KeyCode::KeyH), &ch("H"), cmd | ModifiersState::SHIFT))),
            vec![Command::Key(HwKey::Home, Press::Short)]
        );
    }

    #[test]
    fn alt_is_the_modifier_elsewhere_and_cmd_is_then_an_ordinary_key() {
        let mut kb = Keyboard::new(ShortcutModifier::Alt, false);
        let alt = ModifiersState::ALT;
        assert!(kb.key(&press(code(KeyCode::AltLeft), &Key::Named(NamedKey::Alt), alt)).is_empty());
        assert_eq!(
            sent(&kb.key(&press(code(KeyCode::KeyH), &ch("h"), alt))),
            vec![Command::Key(HwKey::Home, Press::Short)]
        );
        // Alt+letter that is no shortcut is not typed on the device either.
        assert!(kb.key(&press(code(KeyCode::KeyX), &ch("x"), alt)).is_empty());
        assert!(kb.key(&release(code(KeyCode::KeyX), &ch("x"), alt)).is_empty());
        // Cmd (the Windows key) is an ordinary key to the device here.
        assert_eq!(
            sent(&kb.key(&press(code(KeyCode::SuperLeft), &Key::Named(NamedKey::Super), ModifiersState::SUPER))),
            vec![Command::Kb(Kb::Down(0xE3))]
        );
        // And on a Mac, with Cmd as the modifier, Option goes through.
        let mut kb = mac();
        assert_eq!(
            sent(&kb.key(&press(code(KeyCode::AltLeft), &Key::Named(NamedKey::Alt), ModifiersState::ALT))),
            vec![Command::Kb(Kb::Down(0xE2))]
        );
    }

    #[test]
    fn a_held_volume_shortcut_repeats_and_the_others_do_not() {
        let mut kb = mac();
        let cmd = ModifiersState::SUPER;
        let up = Key::Named(NamedKey::ArrowUp);
        let h = ch("h");
        let first = press(code(KeyCode::ArrowUp), &up, cmd);
        assert_eq!(sent(&kb.key(&first)).len(), 1);
        assert_eq!(
            sent(&kb.key(&KeyPress { repeat: true, ..first.clone() })),
            vec![Command::Key(HwKey::VolumeUp, Press::Short)]
        );
        let home = press(code(KeyCode::KeyH), &h, cmd);
        assert_eq!(sent(&kb.key(&home)).len(), 1);
        assert!(kb.key(&KeyPress { repeat: true, ..home }).is_empty(), "Home is pressed once per press of the key");
    }

    #[test]
    fn lock_is_a_shortcut_only_when_asked_for() {
        let cmd = ModifiersState::SUPER;
        let p = ch("p");
        let mut off = mac();
        assert!(off.key(&press(code(KeyCode::KeyP), &p, cmd)).is_empty(), "no lock by default");
        let mut on = Keyboard::new(ShortcutModifier::Super, true);
        assert_eq!(sent(&on.key(&press(code(KeyCode::KeyP), &p, cmd))), vec![Command::Key(HwKey::Lock, Press::Short)]);
        // Nothing else locks or powers: no shortcut makes Power, Sleep or Siri.
        for key in ["l", "o", "k", "q", "x"] {
            let k = ch(key);
            let acts = on.key(&press(code(KeyCode::KeyL), &k, cmd));
            assert!(sent(&acts).is_empty(), "{key}");
        }
    }

    #[test]
    fn mod_w_closes_the_window() {
        let mut kb = mac();
        let acts = kb.key(&press(code(KeyCode::KeyW), &ch("w"), ModifiersState::SUPER));
        assert_eq!(acts, vec![Action::Close]);
        // Without the modifier W is a letter.
        assert_eq!(sent(&kb.key(&press(code(KeyCode::KeyW), &ch("w"), NONE))), vec![Command::Kb(Kb::Down(0x1A))]);
    }

    #[test]
    fn a_key_that_went_down_comes_up_even_if_the_modifier_is_down_by_then() {
        let mut kb = mac();
        let h = ch("h");
        // H goes down on its own, then Cmd is pressed, then H is released with Cmd held: it comes up on the device.
        assert_eq!(sent(&kb.key(&press(code(KeyCode::KeyH), &h, NONE))), vec![Command::Kb(Kb::Down(0x0B))]);
        assert!(
            kb.key(&press(code(KeyCode::SuperLeft), &Key::Named(NamedKey::Super), ModifiersState::SUPER)).is_empty()
        );
        assert_eq!(
            sent(&kb.key(&release(code(KeyCode::KeyH), &h, ModifiersState::SUPER))),
            vec![Command::Kb(Kb::Up(0x0B))]
        );
        // The release of the modifier is the viewer's.
        assert!(kb.key(&release(code(KeyCode::SuperLeft), &Key::Named(NamedKey::Super), NONE)).is_empty());
        assert_eq!(kb.held(), 0);
    }

    #[test]
    fn losing_the_keyboard_lifts_every_key_keys_before_modifiers() {
        let mut kb = mac();
        let shift = Key::Named(NamedKey::Shift);
        kb.key(&press(code(KeyCode::ShiftLeft), &shift, ModifiersState::SHIFT));
        kb.key(&press(code(KeyCode::KeyA), &ch("a"), ModifiersState::SHIFT));
        kb.key(&press(code(KeyCode::KeyZ), &ch("z"), ModifiersState::SHIFT));
        assert_eq!(kb.held(), 3);
        assert_eq!(
            sent(&kb.release_all()),
            vec![Command::Kb(Kb::Up(0x04)), Command::Kb(Kb::Up(0x1D)), Command::Kb(Kb::Up(0xE1))]
        );
        assert_eq!(kb.held(), 0);
        assert!(kb.release_all().is_empty());
    }

    #[test]
    fn a_key_with_no_usage_types_its_text() {
        let mut kb = mac();
        let unidentified = PhysicalKey::Unidentified(NativeKeyCode::Unidentified);
        let key = ch("é");
        let with_text = KeyPress { text: Some("é"), ..press(unidentified, &key, NONE) };
        assert_eq!(sent(&kb.key(&with_text)), vec![Command::Kb(Kb::Type("é".into()))]);
        // A control character is not text; nor is nothing.
        let ctrl = KeyPress { text: Some("\u{7f}"), ..press(unidentified, &key, NONE) };
        assert!(kb.key(&ctrl).is_empty());
        assert!(kb.key(&press(unidentified, &key, NONE)).is_empty());
        // Its release says nothing.
        assert!(kb.key(&release(unidentified, &key, NONE)).is_empty());
        assert_eq!(kb.held(), 0);
    }

    #[test]
    fn the_usages_are_the_hid_keyboard_page() {
        let known = [
            (KeyCode::KeyA, 0x04),
            (KeyCode::KeyZ, 0x1D),
            (KeyCode::Digit1, 0x1E),
            (KeyCode::Digit0, 0x27),
            (KeyCode::Enter, 0x28),
            (KeyCode::Escape, 0x29),
            (KeyCode::Backspace, 0x2A),
            (KeyCode::Tab, 0x2B),
            (KeyCode::Space, 0x2C),
            (KeyCode::F1, 0x3A),
            (KeyCode::F12, 0x45),
            (KeyCode::ArrowRight, 0x4F),
            (KeyCode::ArrowLeft, 0x50),
            (KeyCode::ArrowDown, 0x51),
            (KeyCode::ArrowUp, 0x52),
            (KeyCode::Numpad0, 0x62),
            (KeyCode::ControlLeft, 0xE0),
            (KeyCode::SuperRight, 0xE7),
        ];
        for (key, usage) in known {
            assert_eq!(hid_usage(key), Some(usage), "{key:?}");
        }
        // Keys the keyboard page has no place for.
        assert_eq!(hid_usage(KeyCode::AudioVolumeUp), None);
        assert_eq!(hid_usage(KeyCode::Fn), None);
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "the list of every key the table knows")]
    fn every_usage_is_in_range_and_belongs_to_one_key() {
        use std::collections::HashMap;
        let all = [
            KeyCode::KeyA,
            KeyCode::KeyB,
            KeyCode::KeyC,
            KeyCode::KeyD,
            KeyCode::KeyE,
            KeyCode::KeyF,
            KeyCode::KeyG,
            KeyCode::KeyH,
            KeyCode::KeyI,
            KeyCode::KeyJ,
            KeyCode::KeyK,
            KeyCode::KeyL,
            KeyCode::KeyM,
            KeyCode::KeyN,
            KeyCode::KeyO,
            KeyCode::KeyP,
            KeyCode::KeyQ,
            KeyCode::KeyR,
            KeyCode::KeyS,
            KeyCode::KeyT,
            KeyCode::KeyU,
            KeyCode::KeyV,
            KeyCode::KeyW,
            KeyCode::KeyX,
            KeyCode::KeyY,
            KeyCode::KeyZ,
            KeyCode::Digit0,
            KeyCode::Digit1,
            KeyCode::Digit2,
            KeyCode::Digit3,
            KeyCode::Digit4,
            KeyCode::Digit5,
            KeyCode::Digit6,
            KeyCode::Digit7,
            KeyCode::Digit8,
            KeyCode::Digit9,
            KeyCode::Enter,
            KeyCode::Escape,
            KeyCode::Backspace,
            KeyCode::Tab,
            KeyCode::Space,
            KeyCode::Minus,
            KeyCode::Equal,
            KeyCode::BracketLeft,
            KeyCode::BracketRight,
            KeyCode::Backslash,
            KeyCode::Semicolon,
            KeyCode::Quote,
            KeyCode::Backquote,
            KeyCode::Comma,
            KeyCode::Period,
            KeyCode::Slash,
            KeyCode::CapsLock,
            KeyCode::F1,
            KeyCode::F2,
            KeyCode::F3,
            KeyCode::F4,
            KeyCode::F5,
            KeyCode::F6,
            KeyCode::F7,
            KeyCode::F8,
            KeyCode::F9,
            KeyCode::F10,
            KeyCode::F11,
            KeyCode::F12,
            KeyCode::PrintScreen,
            KeyCode::ScrollLock,
            KeyCode::Pause,
            KeyCode::Insert,
            KeyCode::Home,
            KeyCode::PageUp,
            KeyCode::Delete,
            KeyCode::End,
            KeyCode::PageDown,
            KeyCode::ArrowRight,
            KeyCode::ArrowLeft,
            KeyCode::ArrowDown,
            KeyCode::ArrowUp,
            KeyCode::NumLock,
            KeyCode::NumpadDivide,
            KeyCode::NumpadMultiply,
            KeyCode::NumpadSubtract,
            KeyCode::NumpadAdd,
            KeyCode::NumpadEnter,
            KeyCode::Numpad1,
            KeyCode::Numpad2,
            KeyCode::Numpad3,
            KeyCode::Numpad4,
            KeyCode::Numpad5,
            KeyCode::Numpad6,
            KeyCode::Numpad7,
            KeyCode::Numpad8,
            KeyCode::Numpad9,
            KeyCode::Numpad0,
            KeyCode::NumpadDecimal,
            KeyCode::IntlBackslash,
            KeyCode::ContextMenu,
            KeyCode::NumpadEqual,
            KeyCode::F13,
            KeyCode::F14,
            KeyCode::F15,
            KeyCode::F16,
            KeyCode::F17,
            KeyCode::F18,
            KeyCode::F19,
            KeyCode::F20,
            KeyCode::F21,
            KeyCode::F22,
            KeyCode::F23,
            KeyCode::F24,
            KeyCode::ControlLeft,
            KeyCode::ShiftLeft,
            KeyCode::AltLeft,
            KeyCode::SuperLeft,
            KeyCode::ControlRight,
            KeyCode::ShiftRight,
            KeyCode::AltRight,
            KeyCode::SuperRight,
        ];
        let mut seen: HashMap<u8, KeyCode> = HashMap::new();
        for key in all {
            let usage = hid_usage(key).unwrap_or_else(|| panic!("{key:?} has no usage"));
            assert!((0x01..=crate::commands::MAX_KEY_USAGE).contains(&usage), "{key:?} {usage:#x}");
            if let Some(other) = seen.insert(usage, key) {
                panic!("{key:?} and {other:?} share the usage {usage:#x}");
            }
        }
        assert_eq!(seen.len(), all.len());
    }

    fn view() -> Layout {
        Layout { window: (2001, 1311), upright: (1206, 2622), margin: 0 }
    }

    #[test]
    fn the_mouse_is_a_finger_down_moving_and_up() {
        let mut p = Pointer::default();
        // The window is wider than the picture: its centre is the picture's centre.
        let c = (1000.5, 655.5);
        assert_eq!(p.pressed(view(), c), Some(touch(TouchPhase::Down, (603.0, 1311.0))));
        assert!(p.is_down());
        // A move to the same device point says nothing; a move elsewhere is a move.
        assert_eq!(p.moved(view(), c), None);
        let moved = p.moved(view(), (1000.5, 755.5)).unwrap();
        match moved {
            Command::Touch(Touch { phase: TouchPhase::Move, x, y, id: 0 }) => {
                assert!((x - 603.0).abs() < 1e-6 && y > 1311.0, "{x} {y}");
            }
            other => panic!("{other:?}"),
        }
        let up = p.released(view(), (1000.5, 755.5)).unwrap();
        assert!(matches!(up, Command::Touch(Touch { phase: TouchPhase::Up, id: 0, .. })));
        assert!(!p.is_down());
        // Nothing moves a finger that is up.
        assert_eq!(p.moved(view(), c), None);
        assert_eq!(p.released(view(), c), None);
    }

    #[test]
    fn a_press_in_the_bars_is_no_touch_and_neither_is_its_release() {
        let mut p = Pointer::default();
        assert_eq!(p.pressed(view(), (5.0, 600.0)), None);
        assert!(!p.is_down());
        assert_eq!(p.moved(view(), (1000.5, 600.0)), None);
        assert_eq!(p.released(view(), (1000.5, 600.0)), None);
    }

    #[test]
    fn a_drag_out_of_the_picture_sticks_to_its_edge_and_lifts_there() {
        let mut p = Pointer::default();
        p.pressed(view(), (1000.5, 655.5)).unwrap();
        // Out through the left bar and the window's edge.
        assert_eq!(p.moved(view(), (-300.0, 655.5)), Some(touch(TouchPhase::Move, (0.0, 1311.0))));
        // Staying out is the same point: nothing to send.
        assert_eq!(p.moved(view(), (-900.0, 655.5)), None);
        assert_eq!(p.released(view(), (-900.0, 655.5)), Some(touch(TouchPhase::Up, (0.0, 1311.0))));
    }

    #[test]
    fn cancelling_lifts_the_finger_where_it_is_and_only_once() {
        let mut p = Pointer::default();
        assert_eq!(p.cancel(), None);
        p.pressed(view(), (1000.5, 655.5)).unwrap();
        assert_eq!(p.cancel(), Some(touch(TouchPhase::Up, (603.0, 1311.0))));
        assert_eq!(p.cancel(), None);
        // A second press while the first is down is not another finger.
        p.pressed(view(), (1000.5, 655.5)).unwrap();
        assert_eq!(p.pressed(view(), (1000.5, 700.0)), None);
    }

    #[test]
    fn a_window_resized_under_a_finger_keeps_the_finger_on_the_picture() {
        let mut p = Pointer::default();
        p.pressed(view(), (1000.5, 655.5)).unwrap();
        // The window is now half the size: the same pointer position is elsewhere on the picture, and still on it.
        let small = Layout { window: (1000, 655), upright: (1206, 2622), margin: 0 };
        let moved = p.moved(small, (500.0, 400.0)).unwrap();
        assert!(matches!(moved, Command::Touch(Touch { phase: TouchPhase::Move, .. })));
    }

    #[test]
    fn mod_g_is_the_swipe_up_from_the_bottom_edge() {
        let mut kb = mac();
        let g = ch("g");
        assert_eq!(kb.key(&press(code(KeyCode::KeyG), &g, ModifiersState::SUPER)), vec![Action::SwipeUp]);
        // Held, it is made once; the release says nothing.
        let first = press(code(KeyCode::KeyG), &g, ModifiersState::SUPER);
        assert!(kb.key(&KeyPress { repeat: true, ..first }).is_empty());
        assert!(kb.key(&release(code(KeyCode::KeyG), &g, ModifiersState::SUPER)).is_empty());
        // Without the modifier G is a letter.
        assert_eq!(sent(&kb.key(&press(code(KeyCode::KeyG), &g, NONE))), vec![Command::Kb(Kb::Down(0x0A))]);
        // And Cmd+Up is still the volume, Cmd+H the Home button, Cmd+S the App Switcher.
        let mut kb = mac();
        let up = Key::Named(NamedKey::ArrowUp);
        assert_eq!(
            sent(&kb.key(&press(code(KeyCode::ArrowUp), &up, ModifiersState::SUPER))),
            vec![Command::Key(HwKey::VolumeUp, Press::Short)]
        );
        assert_eq!(
            sent(&kb.key(&press(code(KeyCode::KeyH), &ch("h"), ModifiersState::SUPER))),
            vec![Command::Key(HwKey::Home, Press::Short)]
        );
    }

    #[test]
    fn the_swipe_starts_on_the_bottom_edge_and_runs_straight_up() {
        for upright in [(1206, 2622), (2622, 1206), (1668, 2388)] {
            let plan = swipe_up(upright);
            let points: Vec<(TouchPhase, f64, f64)> = plan
                .iter()
                .map(|(_, c)| match c {
                    Command::Touch(Touch { phase, x, y, id: 0 }) => (*phase, *x, *y),
                    other => panic!("not a touch: {other:?}"),
                })
                .collect();
            // Down on the last row (the digitizer's edge), in the middle; up where the run ended.
            let bottom = f64::from(upright.1 - 1);
            assert_eq!(points[0], (TouchPhase::Down, f64::from(upright.0) / 2.0, bottom));
            let last = points.last().expect("an end");
            assert_eq!(last.0, TouchPhase::Up);
            assert!(last.2 < bottom - f64::from(upright.1) * 0.25, "it rises a good part of the screen: {last:?}");
            // Every move is on the same column and above the one before it, and nothing leaves the picture.
            let moves = &points[1..points.len() - 1];
            assert!(moves.iter().all(|p| p.0 == TouchPhase::Move && (p.1 - points[0].1).abs() < 1e-9));
            assert!(moves.windows(2).all(|w| w[1].2 < w[0].2));
            assert!(points.iter().all(|p| p.2 >= 0.0 && p.2 <= bottom && p.1 >= 0.0 && p.1 <= f64::from(upright.0)));
            // A flick: well under half a second, and few enough commands for the touch rate limit (a burst of 100).
            let total: Duration = plan.iter().map(|(d, _)| *d).sum();
            assert!(total >= Duration::from_millis(100) && total < Duration::from_millis(400), "{total:?}");
            assert!(plan.len() < 40);
            assert_eq!(plan[0].0, Duration::ZERO);
        }
    }

    /// A window of 500x1000 with a 24-pixel margin, the 1206x2622 picture in it: the picture is 438x952 at (31, 24).
    fn margined() -> Layout {
        Layout { window: (500, 1000), upright: (1206, 2622), margin: 24 }
    }

    #[test]
    fn a_press_in_the_margin_is_on_the_devices_edge_and_a_drag_goes_on_from_there() {
        let mut p = Pointer::default();
        // In the bottom margin, near the middle: the last row of the device, the matching column.
        let down = p.pressed(margined(), (250.0, 990.0)).expect("a touch");
        match down {
            Command::Touch(Touch { phase: TouchPhase::Down, x, y, id: 0 }) => {
                assert!((x - 603.0 * (250.0 - 31.0) / 219.0).abs() < 1.0, "{x}");
                assert!((y - 2621.0).abs() < 1e-9, "{y}");
            }
            other => panic!("{other:?}"),
        }
        // Dragging up into the picture is the ordinary mapping: the finger rises.
        let Some(Command::Touch(Touch { phase: TouchPhase::Move, y: y1, .. })) = p.moved(margined(), (250.0, 900.0))
        else {
            panic!("a move")
        };
        assert!(y1 < 2621.0 - 100.0, "{y1}");
        // Dragging back below the picture sticks to the bottom edge; the release is there.
        let Some(Command::Touch(Touch { phase: TouchPhase::Move, y: y2, .. })) = p.moved(margined(), (250.0, 999.0))
        else {
            panic!("a move")
        };
        assert!(y2 >= 2621.0, "{y2}");
        assert!(matches!(
            p.released(margined(), (250.0, 999.0)),
            Some(Command::Touch(Touch { phase: TouchPhase::Up, .. }))
        ));
        // A press in the bars beyond the margin, at the window's own edge, is nothing.
        assert_eq!(p.pressed(margined(), (3.0, 500.0)), None);
        assert_eq!(p.pressed(margined(), (497.0, 500.0)), None);
    }

    // ---- the bottom swipe ---------------------------------------------------------------------------------------

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    const UP: (u32, u32) = (1206, 2622);

    #[test]
    fn a_quick_swipe_up_from_the_bottom_is_home_and_a_paused_one_is_the_app_switcher() {
        let start = (603.0, 2621.0);
        // Up by 10% of the height, straight: Home; paused first: the App Switcher.
        assert_eq!(bottom_swipe(start, (603.0, 2359.0), false, UP), Some(HwKey::Home));
        assert_eq!(bottom_swipe(start, (603.0, 2359.0), true, UP), Some(HwKey::AppSwitcher));
        // A long way up, a little aslant (less across than up) is still the swipe.
        assert_eq!(bottom_swipe(start, (803.0, 1500.0), false, UP), Some(HwKey::Home));
        assert_eq!(bottom_swipe(start, (403.0, 1500.0), false, UP), Some(HwKey::Home));
    }

    #[test]
    fn a_drag_that_is_too_short_or_sideways_or_downwards_is_no_swipe() {
        let start = (603.0, 2621.0);
        // The threshold is 8% of 2622: 209.76 pixels of rise.
        assert_eq!(bottom_swipe(start, (603.0, 2621.0 - 209.0), false, UP), None, "just short of it");
        assert!(bottom_swipe(start, (603.0, 2621.0 - 210.0), false, UP).is_some(), "just past it");
        assert_eq!(bottom_swipe(start, start, false, UP), None, "a tap");
        assert_eq!(bottom_swipe(start, (603.0, 2621.0 - 50.0), true, UP), None, "short, and paused: still short");
        // Sideways: across further than up.
        assert_eq!(bottom_swipe(start, (1100.0, 2621.0 - 300.0), false, UP), None);
        assert_eq!(bottom_swipe(start, (100.0, 2621.0 - 300.0), false, UP), None);
        // Downwards (a drag that left the picture below).
        assert_eq!(bottom_swipe((603.0, 2000.0), (603.0, 2621.0), false, UP), None);
        // The same rise on a landscape picture (its height is 1206): 8% of that.
        assert!(bottom_swipe((1311.0, 1205.0), (1311.0, 1205.0 - 100.0), false, (2622, 1206)).is_some());
        assert_eq!(bottom_swipe((1311.0, 1205.0), (1311.0, 1205.0 - 90.0), false, (2622, 1206)), None);
    }

    fn keys() -> Mouse {
        Mouse::new(BottomSwipe::Keys)
    }

    /// The commands that went to the device, whether sent at once or played.
    fn commands(actions: &[Action]) -> Vec<Command> {
        let mut out = Vec::new();
        for a in actions {
            match a {
                Action::Send(c) => out.push(c.clone()),
                Action::Play(plan) => out.extend(plan.iter().map(|(_, c)| c.clone())),
                Action::Close | Action::SwipeUp | Action::Note(_) => {}
            }
        }
        out
    }

    fn notes(actions: &[Action]) -> Vec<&'static str> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Note(n) => Some(*n),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_quick_swipe_from_the_bottom_margin_is_home_and_no_touch_is_ever_sent() {
        let mut m = keys();
        // The press, in the bottom margin: held back.
        assert!(m.pressed(margined(), (250.0, 990.0), ms(0)).is_empty());
        assert!(m.is_down());
        // The drag up: nothing sent while it goes.
        for (t, y) in [(10, 940.0), (20, 900.0), (30, 850.0), (40, 800.0)] {
            assert!(m.moved(margined(), (250.0, y), ms(t)).is_empty(), "y {y}");
        }
        // The release: the Home button, said in the title.
        let end = m.released(margined(), (250.0, 800.0), ms(50));
        assert_eq!(commands(&end), vec![Command::Key(HwKey::Home, Press::Short)]);
        assert_eq!(notes(&end), vec!["Home"]);
        assert!(!m.is_down());
    }

    #[test]
    fn a_swipe_that_stops_before_it_ends_is_the_app_switcher() {
        // Up, and still for 400 ms at the end, then released.
        let mut m = keys();
        m.pressed(margined(), (250.0, 990.0), ms(0));
        m.moved(margined(), (250.0, 850.0), ms(60));
        m.moved(margined(), (250.0, 760.0), ms(120));
        let end = m.released(margined(), (250.0, 760.0), ms(520));
        assert_eq!(commands(&end), vec![Command::Key(HwKey::AppSwitcher, Press::Short)]);
        assert_eq!(notes(&end), vec!["App Switcher"]);
        // A pause on the way up counts too, even if it moves on and lets go at once.
        let mut m = keys();
        m.pressed(margined(), (250.0, 990.0), ms(0));
        m.moved(margined(), (250.0, 850.0), ms(60));
        m.moved(margined(), (250.0, 760.0), ms(500));
        let end = m.released(margined(), (250.0, 760.0), ms(510));
        assert_eq!(commands(&end), vec![Command::Key(HwKey::AppSwitcher, Press::Short)]);
        // A swipe that does not stop is Home even when it is slow overall.
        let mut m = keys();
        m.pressed(margined(), (250.0, 990.0), ms(0));
        for i in 1..=20 {
            m.moved(margined(), (250.0, 990.0 - 12.0 * f64::from(i)), ms(i64_to_u64(i) * 100));
        }
        let end = m.released(margined(), (250.0, 750.0), ms(2050));
        assert_eq!(commands(&end), vec![Command::Key(HwKey::Home, Press::Short)]);
    }

    fn i64_to_u64(i: i32) -> u64 {
        u64::try_from(i).expect("positive")
    }

    #[test]
    fn a_pause_at_the_very_start_is_not_a_hold() {
        // Pressed, still for a second (not risen yet), then a quick swipe: Home.
        let mut m = keys();
        m.pressed(margined(), (250.0, 990.0), ms(0));
        m.moved(margined(), (250.0, 850.0), ms(1000));
        m.moved(margined(), (250.0, 760.0), ms(1030));
        let end = m.released(margined(), (250.0, 760.0), ms(1040));
        assert_eq!(commands(&end), vec![Command::Key(HwKey::Home, Press::Short)]);
    }

    #[test]
    fn a_swipe_from_the_pictures_lowest_strip_is_the_same() {
        // Inside the picture, 10 window pixels above its bottom edge (976): the lowest 3% (947 and below).
        let mut m = keys();
        assert!(m.pressed(margined(), (250.0, 966.0), ms(0)).is_empty());
        m.moved(margined(), (250.0, 800.0), ms(40));
        let end = m.released(margined(), (250.0, 800.0), ms(60));
        assert_eq!(commands(&end), vec![Command::Key(HwKey::Home, Press::Short)]);
        // Higher up, a press is a touch at once, and a swipe there is a drag (the app's own scrolling).
        let mut m = keys();
        let down = m.pressed(margined(), (250.0, 900.0), ms(0));
        assert!(matches!(commands(&down).as_slice(), [Command::Touch(Touch { phase: TouchPhase::Down, .. })]));
        let up = m.released(margined(), (250.0, 600.0), ms(100));
        assert!(matches!(commands(&up).as_slice(), [Command::Touch(Touch { phase: TouchPhase::Up, .. })]));
    }

    #[test]
    fn a_tap_near_the_bottom_still_taps() {
        let mut m = keys();
        assert!(m.pressed(margined(), (250.0, 966.0), ms(0)).is_empty(), "held back, not lost");
        let end = m.released(margined(), (250.0, 966.0), ms(90));
        assert!(notes(&end).is_empty());
        // Played, as long as the press was: down, then up at the same point.
        let [Action::Play(plan)] = end.as_slice() else { panic!("{end:?}") };
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].0, Duration::ZERO);
        assert_eq!(plan[1].0, ms(90));
        let (Command::Touch(down), Command::Touch(up)) = (&plan[0].1, &plan[1].1) else { panic!("{plan:?}") };
        assert_eq!((down.phase, up.phase), (TouchPhase::Down, TouchPhase::Up));
        assert!((down.x - up.x).abs() < 1e-9 && (down.y - up.y).abs() < 1e-9);
        // The tap in the margin is on the device's very bottom edge.
        let mut m = keys();
        m.pressed(margined(), (250.0, 990.0), ms(0));
        let end = m.released(margined(), (250.0, 990.0), ms(10));
        let [Action::Play(plan)] = end.as_slice() else { panic!("{end:?}") };
        let Command::Touch(down) = &plan[0].1 else { panic!("{plan:?}") };
        assert!((down.y - 2621.0).abs() < 1e-9, "{}", down.y);
        // A press that is very short is replayed for long enough to be one.
        assert!(plan[1].0 >= ms(60));
    }

    #[test]
    fn a_short_drag_from_the_bottom_is_replayed_as_the_touch_it_was() {
        let mut m = keys();
        m.pressed(margined(), (250.0, 966.0), ms(0));
        m.moved(margined(), (250.0, 935.0), ms(30));
        let end = m.released(margined(), (250.0, 935.0), ms(200));
        let cmds = commands(&end);
        assert_eq!(cmds.len(), 3, "{cmds:?}");
        assert!(matches!(cmds[0], Command::Touch(Touch { phase: TouchPhase::Down, .. })));
        assert!(matches!(cmds[1], Command::Touch(Touch { phase: TouchPhase::Move, .. })));
        assert!(matches!(cmds[2], Command::Touch(Touch { phase: TouchPhase::Up, .. })));
        assert!(notes(&end).is_empty());
    }

    #[test]
    fn a_drag_that_goes_sideways_is_a_touch_from_its_start_and_is_followed_live() {
        let mut m = keys();
        assert!(m.pressed(margined(), (60.0, 990.0), ms(0)).is_empty());
        // Across by 300 window pixels (about 830 device pixels, far past the threshold) and hardly up.
        let first = m.moved(margined(), (360.0, 985.0), ms(30));
        let cmds = commands(&first);
        assert_eq!(cmds.len(), 2, "{cmds:?}");
        let Command::Touch(down) = &cmds[0] else { panic!() };
        assert_eq!(down.phase, TouchPhase::Down);
        assert!((down.y - 2621.0).abs() < 1e-9, "from the very start: the bottom edge");
        assert!(matches!(cmds[1], Command::Touch(Touch { phase: TouchPhase::Move, .. })));
        // Then live: a move is a move, the release is the touch's up, and no key was pressed.
        let more = m.moved(margined(), (400.0, 985.0), ms(50));
        assert!(matches!(commands(&more).as_slice(), [Command::Touch(Touch { phase: TouchPhase::Move, .. })]));
        let end = m.released(margined(), (400.0, 985.0), ms(70));
        assert!(matches!(commands(&end).as_slice(), [Command::Touch(Touch { phase: TouchPhase::Up, .. })]));
        assert!(!m.is_down());
    }

    #[test]
    fn touch_mode_sends_the_bottom_drag_as_touch_from_the_first_press() {
        let mut m = Mouse::new(BottomSwipe::Touch);
        let down = m.pressed(margined(), (250.0, 990.0), ms(0));
        let Some(Command::Touch(t)) = commands(&down).into_iter().next() else { panic!("{down:?}") };
        assert_eq!(t.phase, TouchPhase::Down);
        assert!((t.y - 2621.0).abs() < 1e-9, "on the edge");
        assert!(m.is_down());
        assert!(!commands(&m.moved(margined(), (250.0, 800.0), ms(40))).is_empty());
        assert!(matches!(
            commands(&m.released(margined(), (250.0, 800.0), ms(80))).as_slice(),
            [Command::Touch(Touch { phase: TouchPhase::Up, .. })]
        ));
    }

    #[test]
    fn a_press_held_back_is_dropped_by_a_cancel_and_a_second_press_is_ignored() {
        let mut m = keys();
        m.pressed(margined(), (250.0, 990.0), ms(0));
        assert!(m.pressed(margined(), (250.0, 500.0), ms(5)).is_empty(), "no second finger");
        assert!(m.cancel().is_empty(), "nothing had been sent: nothing to lift");
        assert!(!m.is_down());
        // The release that follows a cancel is nothing.
        assert!(m.released(margined(), (250.0, 990.0), ms(40)).is_empty());
        // A finger that is down is lifted by a cancel.
        let mut m = keys();
        m.pressed(margined(), (250.0, 500.0), ms(0));
        assert!(matches!(commands(&m.cancel()).as_slice(), [Command::Touch(Touch { phase: TouchPhase::Up, .. })]));
    }
}
