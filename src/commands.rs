//! The stdin command channel: one command per line.
//!
//! * `keyframe` asks the device for a fresh keyframe (rate-limited elsewhere).
//! * `stop` ends the stream cleanly.
//! * `touch down|move|up <x> <y> <id>`: a finger, in pixels of the upright picture (see `hid`).
//! * `key <name> [short|long]`: a hardware button, one of [`Key::NAMES`], pressed briefly (the
//!   default) or held.
//! * End of input is `stop`.
//! * Anything else is logged and ignored; a `touch` or `key` that is malformed is reported.

use std::future::Future;

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::sync::mpsc;
use tracing::{debug, error, warn};

/// A longer line is not a command (and is not buffered whole).
const MAX_LINE: usize = 1024;

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Keyframe,
    Stop,
    Touch(Touch),
    Key(Key, Press),
    /// A `touch` or `key` command that did not parse; the message says why.
    Invalid(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchPhase {
    Down,
    Move,
    Up,
}

/// A finger event: `x` and `y` in pixels of the upright picture.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Touch {
    pub phase: TouchPhase,
    pub x: f64,
    pub y: f64,
    pub id: u32,
}

/// The hardware buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Home,
    Lock,
    Power,
    Sleep,
    VolumeUp,
    VolumeDown,
    Mute,
    Siri,
}

/// How long a button is held: a short press, or a long one (about twice the short hold or more).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Press {
    Short,
    Long,
}

impl Key {
    pub const NAMES: &'static str = "home, lock, power, sleep, volume-up, volume-down, mute, siri";

    pub fn from_name(name: &str) -> Option<Key> {
        Some(match name {
            "home" => Key::Home,
            "lock" => Key::Lock,
            "power" => Key::Power,
            "sleep" => Key::Sleep,
            "volume-up" => Key::VolumeUp,
            "volume-down" => Key::VolumeDown,
            "mute" => Key::Mute,
            "siri" => Key::Siri,
            _ => return None,
        })
    }

    /// (HID usage page, usage code) of the button, from the reference tool's named buttons. Power,
    /// lock and sleep are the same button.
    pub fn usage(self) -> (u64, u64) {
        match self {
            Key::Home => (0x0C, 0x40),
            Key::Power | Key::Lock | Key::Sleep => (0x0C, 0x30),
            Key::VolumeUp => (0x0C, 0xE9),
            Key::VolumeDown => (0x0C, 0xEA),
            Key::Mute => (0x0C, 0xE2),
            Key::Siri => (0x0C, 0xCF),
        }
    }

    /// How long the button is held down in ms. The short hold is the reference tool's; a long
    /// press is held about twice as long or more (a held press, not a repeated one).
    pub fn hold_ms(self, press: Press) -> u64 {
        match (self, press) {
            (Key::Home | Key::VolumeUp | Key::VolumeDown | Key::Mute, Press::Short) => 80,
            (Key::Home | Key::VolumeUp | Key::VolumeDown | Key::Mute, Press::Long) => 600,
            (Key::Power | Key::Lock | Key::Sleep, Press::Short) => 200,
            (Key::Power | Key::Lock | Key::Sleep, Press::Long) => 1200,
            (Key::Siri, Press::Short) => 1200,
            (Key::Siri, Press::Long) => 2500,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Parsed {
    Command(Command),
    /// A line that is no command (kept for the log, lossily decoded and shortened).
    Unknown(String),
    /// A `touch` or `key` command with bad arguments.
    Bad(String),
    Empty,
}

/// Coordinates beyond this are not pixels of any screen.
const MAX_COORDINATE: f64 = 1_000_000.0;

fn parse_coordinate(what: &str, s: &str) -> Result<f64, String> {
    let v: f64 = s
        .parse()
        .map_err(|_| format!("{what} {s:?} is not a number"))?;
    if !v.is_finite() || v.abs() > MAX_COORDINATE {
        return Err(format!("{what} {s:?} is out of range"));
    }
    Ok(v)
}

fn parse_touch(args: &[&str]) -> Result<Touch, String> {
    let [phase, x, y, id] = args else {
        return Err("usage: touch down|move|up <x> <y> <id>".to_string());
    };
    let phase = match *phase {
        "down" => TouchPhase::Down,
        "move" => TouchPhase::Move,
        "up" => TouchPhase::Up,
        other => return Err(format!("touch phase {other:?} is not down, move or up")),
    };
    let id: u32 = id
        .parse()
        .map_err(|_| format!("finger id {id:?} is not a number"))?;
    Ok(Touch {
        phase,
        x: parse_coordinate("x", x)?,
        y: parse_coordinate("y", y)?,
        id,
    })
}

fn parse_key(args: &[&str]) -> Result<(Key, Press), String> {
    let (name, press) = match args {
        [name] => (name, Press::Short),
        [name, "short"] => (name, Press::Short),
        [name, "long"] => (name, Press::Long),
        [_, other] => return Err(format!("key press {other:?} is not short or long")),
        _ => {
            return Err(format!(
                "usage: key <name> [short|long] (name is one of {})",
                Key::NAMES
            ));
        }
    };
    let key = Key::from_name(name)
        .ok_or_else(|| format!("unknown key {name:?} (one of {})", Key::NAMES))?;
    Ok((key, press))
}

pub fn parse_line(line: &[u8]) -> Parsed {
    let text = String::from_utf8_lossy(line);
    let text = text.trim();
    let mut words = text.split_whitespace();
    match words.next() {
        None => Parsed::Empty,
        Some("keyframe") if words.next().is_none() => Parsed::Command(Command::Keyframe),
        Some("stop") if words.next().is_none() => Parsed::Command(Command::Stop),
        Some("touch") => {
            let args: Vec<&str> = words.collect();
            match parse_touch(&args) {
                Ok(t) => Parsed::Command(Command::Touch(t)),
                Err(e) => Parsed::Bad(e),
            }
        }
        Some("key") => {
            let args: Vec<&str> = words.collect();
            match parse_key(&args) {
                Ok((k, p)) => Parsed::Command(Command::Key(k, p)),
                Err(e) => Parsed::Bad(e),
            }
        }
        Some(_) => Parsed::Unknown(text.chars().take(64).collect()),
    }
}

/// Reads lines from `input` and sends the commands to `tx`; at end of input sends [`Command::Stop`].
/// Returns when the input ends or `tx` is closed.
pub async fn read_commands<R: AsyncRead + Unpin>(input: R, tx: mpsc::Sender<Command>) {
    let mut reader = BufReader::new(input);
    let mut line: Vec<u8> = Vec::with_capacity(64);
    let mut too_long = false;

    loop {
        let buf = match reader.fill_buf().await {
            Ok(b) => b,
            Err(e) => {
                warn!("reading commands from stdin failed: {e}");
                break;
            }
        };
        if buf.is_empty() {
            break; // end of input
        }
        let (chunk, found_newline) = match buf.iter().position(|b| *b == b'\n') {
            Some(i) => (&buf[..i], true),
            None => (buf, false),
        };
        let consumed = chunk.len() + usize::from(found_newline);
        if line.len() + chunk.len() > MAX_LINE {
            too_long = true;
        } else if !too_long {
            line.extend_from_slice(chunk);
        }
        reader.consume(consumed);

        if found_newline {
            if !dispatch(&line, too_long, &tx).await {
                return;
            }
            line.clear();
            too_long = false;
        }
    }

    // A last line without a newline still counts.
    if (!line.is_empty() || too_long) && !dispatch(&line, too_long, &tx).await {
        return;
    }
    debug!("stdin closed: stopping");
    let _ = tx.send(Command::Stop).await;
}

/// Runs the command reader as its own task. If it dies (a panic), commands can no longer arrive and
/// the end of stdin would go unnoticed, so that counts as a `stop`.
pub async fn supervised<F>(reader: F, tx: mpsc::Sender<Command>)
where
    F: Future<Output = ()> + Send + 'static,
{
    if let Err(e) = tokio::spawn(reader).await {
        error!("the stdin reader failed ({e}): stopping");
        let _ = tx.send(Command::Stop).await;
    }
}

/// Handles one line; false if the receiving side is gone.
async fn dispatch(line: &[u8], too_long: bool, tx: &mpsc::Sender<Command>) -> bool {
    if too_long {
        warn!("ignoring a stdin line longer than {MAX_LINE} bytes");
        return true;
    }
    match parse_line(line) {
        Parsed::Command(c) => tx.send(c).await.is_ok(),
        Parsed::Unknown(s) => {
            warn!("unknown command on stdin ignored: {s:?}");
            true
        }
        // Reported to the client by the HID side.
        Parsed::Bad(why) => tx.send(Command::Invalid(why)).await.is_ok(),
        Parsed::Empty => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn run(input: &[u8]) -> Vec<Command> {
        let (tx, mut rx) = mpsc::channel(64);
        read_commands(input, tx).await;
        let mut v = Vec::new();
        while let Ok(c) = rx.try_recv() {
            v.push(c);
        }
        v
    }

    #[test]
    fn lines_parse_to_commands() {
        assert_eq!(parse_line(b"keyframe"), Parsed::Command(Command::Keyframe));
        assert_eq!(parse_line(b"stop"), Parsed::Command(Command::Stop));
        // Whitespace and a CR from a CRLF writer are tolerated.
        assert_eq!(parse_line(b"  keyframe \r"), Parsed::Command(Command::Keyframe));
        assert_eq!(parse_line(b""), Parsed::Empty);
        assert_eq!(parse_line(b"   "), Parsed::Empty);
        assert_eq!(parse_line(b"restart"), Parsed::Unknown("restart".into()));
        // Case matters: the protocol is exact.
        assert_eq!(parse_line(b"STOP"), Parsed::Unknown("STOP".into()));
        // Bytes that are not UTF-8 are an unknown command, not a panic.
        assert!(matches!(parse_line(&[0xFF, 0xFE, b'x']), Parsed::Unknown(_)));
    }

    fn touch(line: &str) -> Parsed {
        parse_line(line.as_bytes())
    }

    #[test]
    fn touch_commands_parse() {
        assert_eq!(
            touch("touch down 100 200 0"),
            Parsed::Command(Command::Touch(Touch { phase: TouchPhase::Down, x: 100.0, y: 200.0, id: 0 }))
        );
        assert_eq!(
            touch("  touch   move  12.5   -3  0 \r"),
            Parsed::Command(Command::Touch(Touch { phase: TouchPhase::Move, x: 12.5, y: -3.0, id: 0 }))
        );
        assert_eq!(
            touch("touch up 0 0 7"),
            Parsed::Command(Command::Touch(Touch { phase: TouchPhase::Up, x: 0.0, y: 0.0, id: 7 }))
        );
        // Decimals and exponents are numbers too.
        assert_eq!(
            touch("touch down 1e2 .5 0"),
            Parsed::Command(Command::Touch(Touch { phase: TouchPhase::Down, x: 100.0, y: 0.5, id: 0 }))
        );
    }

    #[test]
    fn bad_touch_commands_are_reported_not_ignored() {
        let bad = |line: &str| match touch(line) {
            Parsed::Bad(why) => why,
            other => panic!("{line:?} parsed as {other:?}"),
        };
        assert!(bad("touch").contains("usage"));
        assert!(bad("touch down 1 2").contains("usage"));
        assert!(bad("touch down 1 2 0 extra").contains("usage"));
        assert!(bad("touch press 1 2 0").contains("down, move or up"));
        assert!(bad("touch down abc 2 0").contains("not a number"));
        assert!(bad("touch down 1 2x 0").contains("not a number"));
        assert!(bad("touch down 1 2 zero").contains("finger id"));
        assert!(bad("touch down 1 2 -1").contains("finger id"));
        assert!(bad("touch down 1 2 4294967296").contains("finger id"), "a finger id over u32");
        // Huge, infinite and not-a-number coordinates.
        assert!(bad("touch down 1e300 2 0").contains("out of range"));
        assert!(bad("touch down 1 -1000001 0").contains("out of range"));
        assert!(bad("touch down inf 2 0").contains("out of range"));
        assert!(bad("touch down -inf 2 0").contains("out of range"));
        assert!(bad("touch down NaN 2 0").contains("out of range"));
        // Case matters here too.
        assert!(matches!(touch("Touch down 1 2 0"), Parsed::Unknown(_)));
        // Negative coordinates are fine (a drag may leave the picture); the mapping clamps them.
        assert!(matches!(touch("touch move -5 -5 0"), Parsed::Command(Command::Touch(_))));
        assert!(matches!(touch("touch move 1000000 -1000000 0"), Parsed::Command(Command::Touch(_))));
    }

    #[test]
    fn key_commands_parse_every_named_button() {
        for (name, key) in [
            ("home", Key::Home),
            ("lock", Key::Lock),
            ("power", Key::Power),
            ("sleep", Key::Sleep),
            ("volume-up", Key::VolumeUp),
            ("volume-down", Key::VolumeDown),
            ("mute", Key::Mute),
            ("siri", Key::Siri),
        ] {
            // The press kind defaults to short.
            assert_eq!(touch(&format!("key {name}")), Parsed::Command(Command::Key(key, Press::Short)), "{name}");
            assert_eq!(touch(&format!("key {name} short")), Parsed::Command(Command::Key(key, Press::Short)), "{name}");
            assert_eq!(touch(&format!("key {name} long")), Parsed::Command(Command::Key(key, Press::Long)), "{name}");
        }
        for bad in [
            "key",
            "key volume_up",
            "key Home",
            "key home extra",
            "key home long extra",
            "key home Long",
            "key home 600",
            "key long home",
            "key raw 12 34",
            "key 0x40",
        ] {
            assert!(matches!(touch(bad), Parsed::Bad(_)), "{bad}");
        }
        assert!(matches!(touch("key home forever"), Parsed::Bad(m) if m.contains("short or long")));
        assert!(matches!(touch("key what"), Parsed::Bad(m) if m.contains("home") && m.contains("siri")));
    }

    #[test]
    fn the_buttons_have_the_reference_tools_usages_and_hold_times() {
        assert_eq!(Key::Home.usage(), (0x0C, 0x40));
        assert_eq!(Key::Power.usage(), (0x0C, 0x30));
        assert_eq!(Key::Lock.usage(), Key::Power.usage());
        assert_eq!(Key::Sleep.usage(), Key::Power.usage());
        assert_eq!(Key::VolumeUp.usage(), (0x0C, 0xE9));
        assert_eq!(Key::VolumeDown.usage(), (0x0C, 0xEA));
        assert_eq!(Key::Mute.usage(), (0x0C, 0xE2));
        assert_eq!(Key::Siri.usage(), (0x0C, 0xCF));
        // (short, long) in ms.
        for (key, short, long) in [
            (Key::Home, 80, 600),
            (Key::Power, 200, 1200),
            (Key::Lock, 200, 1200),
            (Key::Sleep, 200, 1200),
            (Key::Siri, 1200, 2500),
            (Key::VolumeUp, 80, 600),
            (Key::VolumeDown, 80, 600),
            (Key::Mute, 80, 600),
        ] {
            assert_eq!(key.hold_ms(Press::Short), short, "{key:?}");
            assert_eq!(key.hold_ms(Press::Long), long, "{key:?}");
        }
    }

    #[tokio::test]
    async fn bad_hid_commands_reach_the_receiver_as_invalid() {
        let v = run(b"touch down 1 2 0\ntouch bogus\nkey home\nkey nope\n").await;
        assert_eq!(v.len(), 5);
        assert!(matches!(v[0], Command::Touch(_)));
        assert!(matches!(&v[1], Command::Invalid(m) if m.contains("usage")));
        assert_eq!(v[2], Command::Key(Key::Home, Press::Short));
        assert!(matches!(&v[3], Command::Invalid(m) if m.contains("unknown key")));
        assert_eq!(v[4], Command::Stop);
    }

    #[tokio::test]
    async fn commands_in_order_then_eof_is_stop() {
        let v = run(b"keyframe\nkeyframe\n").await;
        assert_eq!(v, vec![Command::Keyframe, Command::Keyframe, Command::Stop]);
    }

    #[tokio::test]
    async fn an_explicit_stop_then_more_input_still_ends_with_stop() {
        let v = run(b"keyframe\nstop\nkeyframe\n").await;
        assert_eq!(
            v,
            vec![Command::Keyframe, Command::Stop, Command::Keyframe, Command::Stop]
        );
    }

    #[tokio::test]
    async fn unknown_and_empty_lines_are_ignored() {
        let v = run(b"\nfoo\n  \nkeyframe\nbar baz\n").await;
        assert_eq!(v, vec![Command::Keyframe, Command::Stop]);
    }

    #[tokio::test]
    async fn empty_input_is_stop() {
        assert_eq!(run(b"").await, vec![Command::Stop]);
    }

    #[tokio::test]
    async fn a_last_line_without_a_newline_counts() {
        assert_eq!(run(b"keyframe\nstop").await, vec![Command::Keyframe, Command::Stop, Command::Stop]);
        assert_eq!(run(b"keyframe").await, vec![Command::Keyframe, Command::Stop]);
    }

    #[tokio::test]
    async fn crlf_lines_work() {
        assert_eq!(run(b"keyframe\r\nstop\r\n").await[..2], [Command::Keyframe, Command::Stop]);
    }

    #[tokio::test]
    async fn an_overlong_line_is_discarded_without_buffering_it_all() {
        let mut input = vec![b'k'; 100_000];
        input.extend_from_slice(b"\nkeyframe\n");
        assert_eq!(run(&input).await, vec![Command::Keyframe, Command::Stop]);
        // An overlong line that happens to start with a command word is still no command.
        let mut input = b"keyframe".to_vec();
        input.extend(std::iter::repeat(b' ').take(5000));
        input.extend_from_slice(b"\nstop\n");
        assert_eq!(run(&input).await, vec![Command::Stop, Command::Stop]);
    }

    #[tokio::test]
    async fn a_reader_that_panics_is_a_stop() {
        let (tx, mut rx) = mpsc::channel(4);
        supervised(async { panic!("the reader broke") }, tx).await;
        assert_eq!(rx.recv().await, Some(Command::Stop));
    }

    #[tokio::test]
    async fn a_reader_that_ends_normally_adds_nothing() {
        let (tx, mut rx) = mpsc::channel(4);
        let tx2 = tx.clone();
        supervised(async move { read_commands(&b"keyframe\n"[..], tx2).await }, tx).await;
        assert_eq!(rx.recv().await, Some(Command::Keyframe));
        assert_eq!(rx.recv().await, Some(Command::Stop)); // the reader's own, at end of input
        assert_eq!(rx.recv().await, None, "and no second one from the supervisor");
    }

    #[tokio::test]
    async fn a_closed_receiver_ends_the_reader() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        // Must return rather than wait for more input.
        read_commands(&b"keyframe\nkeyframe\n"[..], tx).await;
    }
}
