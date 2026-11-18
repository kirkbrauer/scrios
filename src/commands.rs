//! The stdin command channel: one command per line.
//!
//! * `keyframe` asks the device for a fresh keyframe (rate-limited elsewhere).
//! * `stop` ends the stream cleanly.
//! * `touch down|move|up <x> <y> <id>`: a finger, in pixels of the upright picture (see `hid`).
//! * `key <name> [short|long]`: a hardware button, one of [`Key::NAMES`], pressed briefly (the
//!   default) or held. `key app-switcher` (or `switcher`) is the Home button pressed twice, and
//!   ignores `short|long`.
//! * `kb down|up <usage>`: a key of the HID keyboard page, by its usage in hex (`04` is A, `e1` is
//!   Left Shift); `kb type <base64>`: type a UTF-8 string.
//! * End of input is `stop`.
//! * Anything else is logged and ignored; a `touch` or `key` that is malformed is reported.

use std::future::Future;

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::sync::mpsc;
use tracing::{debug, error, warn};

/// A longer line is not a command (and is not buffered whole). The longest command is `kb type` with
/// 4096 characters of up to 4 bytes each in base64 (about 22 KB).
const MAX_LINE: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Keyframe,
    Stop,
    Touch(Touch),
    Key(Key, Press),
    Kb(Kb),
    /// A `touch` or `key` command that did not parse; the message says why.
    Invalid(String),
}

/// The most characters one `kb type` may carry.
pub const MAX_TYPE_CHARS: usize = 4096;
/// The HID keyboard page's usages the device's keyboard takes: 0x01 to 0xE7.
pub const MAX_KEY_USAGE: u8 = 0xE7;

/// A keyboard command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kb {
    /// A key goes down (and stays down, like on a physical keyboard, until its `up`).
    Down(u8),
    Up(u8),
    /// A string to type through the US layout.
    Type(String),
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
    /// The App Switcher: the Home button pressed twice, quickly (see `hid`).
    AppSwitcher,
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
    pub const NAMES: &'static str =
        "home, app-switcher (or switcher), lock, power, sleep, volume-up, volume-down, mute, siri";

    pub fn from_name(name: &str) -> Option<Key> {
        Some(match name {
            "home" => Key::Home,
            "app-switcher" | "switcher" => Key::AppSwitcher,
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
            Key::Home | Key::AppSwitcher => (0x0C, 0x40),
            Key::Power | Key::Lock | Key::Sleep => (0x0C, 0x30),
            Key::VolumeUp => (0x0C, 0xE9),
            Key::VolumeDown => (0x0C, 0xEA),
            Key::Mute => (0x0C, 0xE2),
            Key::Siri => (0x0C, 0xCF),
        }
    }

    /// How long the button is held down in ms. The short hold is the reference tool's; a long
    /// press is held about twice as long or more (a held press, not a repeated one). The App
    /// Switcher is two short Home presses whatever `press` says.
    pub fn hold_ms(self, press: Press) -> u64 {
        match (self, press) {
            (Key::AppSwitcher, _) => 80,
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

/// How much of a token the client sent is quoted back in an error message.
const MAX_ECHO_CHARS: usize = 64;

/// A token of the client's line as an error message quotes it: in quotes, and at most
/// [`MAX_ECHO_CHARS`] characters of it (a line may be 64 KiB, and the message goes to the client's event
/// stream and the log), with `...` where it was cut.
fn quoted(token: &str) -> String {
    let mut chars = token.chars();
    let mut shown: String = chars.by_ref().take(MAX_ECHO_CHARS).collect();
    if chars.next().is_some() {
        shown.push_str("...");
    }
    format!("{shown:?}")
}

fn parse_coordinate(what: &str, s: &str) -> Result<f64, String> {
    let v: f64 = s.parse().map_err(|_| format!("{what} {} is not a number", quoted(s)))?;
    if !v.is_finite() || v.abs() > MAX_COORDINATE {
        return Err(format!("{what} {} is out of range", quoted(s)));
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
        other => return Err(format!("touch phase {} is not down, move or up", quoted(other))),
    };
    let id: u32 = id.parse().map_err(|_| format!("finger id {} is not a number", quoted(id)))?;
    Ok(Touch { phase, x: parse_coordinate("x", x)?, y: parse_coordinate("y", y)?, id })
}

fn parse_key(args: &[&str]) -> Result<(Key, Press), String> {
    let (name, press) = match args {
        [name] => (name, Press::Short),
        [name, "short"] => (name, Press::Short),
        [name, "long"] => (name, Press::Long),
        [_, other] => return Err(format!("key press {} is not short or long", quoted(other))),
        _ => {
            return Err(format!("usage: key <name> [short|long] (name is one of {})", Key::NAMES));
        }
    };
    let key = Key::from_name(name).ok_or_else(|| format!("unknown key {} (one of {})", quoted(name), Key::NAMES))?;
    Ok((key, press))
}

/// A HID keyboard usage in hex, `04` or `0x04`.
fn parse_usage(s: &str) -> Result<u8, String> {
    let hex = s.strip_prefix("0x").unwrap_or(s);
    if hex.is_empty() || hex.len() > 4 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("usage {} is not a hex number", quoted(s)));
    }
    match u16::from_str_radix(hex, 16) {
        Ok(v) if (1..=u16::from(MAX_KEY_USAGE)).contains(&v) => Ok(v as u8),
        _ => Err(format!("usage {} is out of range (01 to {MAX_KEY_USAGE:02x})", quoted(s))),
    }
}

fn parse_kb(args: &[&str]) -> Result<Kb, String> {
    use base64::Engine as _;
    match args {
        ["down", usage] => Ok(Kb::Down(parse_usage(usage)?)),
        ["up", usage] => Ok(Kb::Up(parse_usage(usage)?)),
        ["type", text] => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(text)
                .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(text))
                .map_err(|_| "kb type: the text is not valid base64".to_string())?;
            let text = String::from_utf8(bytes).map_err(|_| "kb type: the text is not valid UTF-8".to_string())?;
            let chars = text.chars().count();
            if chars > MAX_TYPE_CHARS {
                return Err(format!("kb type: {chars} characters, at most {MAX_TYPE_CHARS}"));
            }
            Ok(Kb::Type(text))
        }
        _ => Err("usage: kb down <usage>, kb up <usage> (hex), or kb type <base64>".to_string()),
    }
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
        Some("kb") => {
            let args: Vec<&str> = words.collect();
            match parse_kb(&args) {
                Ok(k) => Parsed::Command(Command::Kb(k)),
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
            ("app-switcher", Key::AppSwitcher),
            ("switcher", Key::AppSwitcher),
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
        assert!(
            matches!(touch("key what"), Parsed::Bad(m) if m.contains("home") && m.contains("siri") && m.contains("app-switcher"))
        );
    }

    #[test]
    fn the_buttons_have_the_reference_tools_usages_and_hold_times() {
        assert_eq!(Key::Home.usage(), (0x0C, 0x40));
        assert_eq!(Key::AppSwitcher.usage(), Key::Home.usage(), "the App Switcher is the Home button, twice");
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
            (Key::AppSwitcher, 80, 80), // `long` is ignored
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

    fn b64(s: &str) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(s)
    }

    #[test]
    fn kb_down_and_up_take_hex_usages() {
        let cmd = |l: &str| match touch(l) {
            Parsed::Command(Command::Kb(k)) => k,
            other => panic!("{l:?} parsed as {other:?}"),
        };
        assert_eq!(cmd("kb down 04"), Kb::Down(0x04));
        assert_eq!(cmd("kb up 04"), Kb::Up(0x04));
        assert_eq!(cmd("kb down 28"), Kb::Down(0x28)); // Enter
        assert_eq!(cmd("kb down 2a"), Kb::Down(0x2A)); // Backspace
        assert_eq!(cmd("kb down 2B"), Kb::Down(0x2B)); // Tab, upper-case hex
        assert_eq!(cmd("kb down 0x4f"), Kb::Down(0x4F)); // Right
        assert_eq!(cmd("kb up e0"), Kb::Up(0xE0)); // Left Control
        assert_eq!(cmd("kb down e7"), Kb::Down(0xE7)); // Right GUI, the last one
        assert_eq!(cmd("kb down 0001"), Kb::Down(1));
        assert_eq!(cmd("  kb   down   e1 \r"), Kb::Down(0xE1));
    }

    #[test]
    fn bad_kb_usages_are_reported() {
        let bad = |l: &str| match touch(l) {
            Parsed::Bad(why) => why,
            other => panic!("{l:?} parsed as {other:?}"),
        };
        assert!(bad("kb down").contains("usage"));
        assert!(bad("kb down 04 05").contains("usage"));
        assert!(bad("kb press 04").contains("usage"));
        assert!(bad("kb").contains("usage"));
        assert!(bad("kb down zz").contains("not a hex number"));
        assert!(bad("kb down 4g").contains("not a hex number"));
        assert!(bad("kb down -4").contains("not a hex number"));
        assert!(bad("kb down 0x").contains("not a hex number"));
        assert!(bad("kb down 00").contains("out of range"), "usage 0 is no key");
        assert!(bad("kb down e8").contains("out of range"));
        assert!(bad("kb up ff").contains("out of range"));
        assert!(bad("kb down 100").contains("out of range"));
        assert!(bad("kb down ffff").contains("out of range"));
        assert!(bad("kb down fffffffffff").contains("not a hex number"), "more than 4 digits");
        assert!(matches!(touch("KB down 04"), Parsed::Unknown(_)));
    }

    #[test]
    fn a_long_token_is_quoted_back_cut_short() {
        let bad = |line: &str| match touch(line) {
            Parsed::Bad(why) => why,
            other => panic!("{line:?} parsed as {other:?}"),
        };
        let long = "9".repeat(5000);
        let cut = format!("\"{}...\"", "9".repeat(64));
        let wide = "é".repeat(500); // characters, not bytes, are counted
        for (line, what) in [
            (format!("touch {long} 1 2 0"), "touch phase"),
            (format!("touch down {long}x 2 0"), "not a number"),
            (format!("touch down {long} 2 0"), "out of range"), // 10^5000 parses as infinity
            (format!("touch down 1 2 {long}"), "finger id"),
            (format!("key home {long}"), "key press"),
            (format!("key {long}"), "unknown key"),
            (format!("kb down {long}"), "not a hex number"),
        ] {
            let why = bad(&line);
            assert!(why.contains(what), "{why}");
            assert!(why.len() < 300, "{} bytes: {why}", why.len());
            assert!(why.contains(&cut), "{why}");
            assert!(!why.contains(&"9".repeat(65)), "{why}");
        }
        let why = bad(&format!("kb up 0x{long}"));
        assert!(why.contains(&format!("\"0x{}...\"", "9".repeat(62))), "{why}");
        let why = bad(&format!("key {wide}"));
        assert!(why.contains(&format!("\"{}...\"", "é".repeat(64))), "{why}");
        // A token that fits is quoted whole, with nothing added; at the limit too.
        assert!(bad("key what").contains("unknown key \"what\""));
        let exact = "k".repeat(64);
        assert!(bad(&format!("key {exact}")).contains(&format!("unknown key \"{exact}\" (one of")));
        let one_more = "k".repeat(65);
        assert!(bad(&format!("key {one_more}")).contains(&format!("unknown key \"{exact}...\" (one of")));
    }

    #[test]
    fn kb_type_decodes_base64_utf8() {
        let cmd = |l: &str| match touch(l) {
            Parsed::Command(Command::Kb(k)) => k,
            other => panic!("{l:?} parsed as {other:?}"),
        };
        assert_eq!(cmd(&format!("kb type {}", b64("Hello, World!\n"))), Kb::Type("Hello, World!\n".into()));
        assert_eq!(cmd(&format!("kb type {}", b64("grüße \u{1F600}"))), Kb::Type("grüße \u{1F600}".into()));
        // Padding may be left off.
        assert_eq!(cmd("kb type aGk"), Kb::Type("hi".into()));
        assert_eq!(cmd("kb type aGk="), Kb::Type("hi".into()));
    }

    #[test]
    fn bad_kb_type_input_is_reported() {
        let bad = |l: &str| match touch(l) {
            Parsed::Bad(why) => why,
            other => panic!("{l:?} parsed as {other:?}"),
        };
        assert!(bad("kb type !!!!").contains("base64"));
        assert!(bad("kb type aGk$").contains("base64"));
        assert!(bad("kb type a").contains("base64"), "one character is no base64");
        assert!(bad("kb type").contains("usage"));
        assert!(bad(&format!("kb type {}", b64(""))).contains("usage"), "an empty string is no text");
        assert!(bad("kb type aGk aGk").contains("usage"), "the text is one word of base64");
        // Valid base64 of bytes that are not UTF-8.
        use base64::Engine as _;
        let raw = base64::engine::general_purpose::STANDARD.encode([0xFF, 0xFE, 0x41]);
        assert!(bad(&format!("kb type {raw}")).contains("UTF-8"));
        // Exactly the limit passes; one more is too long.
        let ok = "a".repeat(MAX_TYPE_CHARS);
        assert!(matches!(touch(&format!("kb type {}", b64(&ok))), Parsed::Command(Command::Kb(Kb::Type(_)))));
        let long = "a".repeat(MAX_TYPE_CHARS + 1);
        let why = bad(&format!("kb type {}", b64(&long)));
        assert!(why.contains("4097 characters") && why.contains("4096"), "{why}");
        // The limit counts characters, not bytes: 4096 four-byte characters fit.
        let wide = "\u{1F600}".repeat(MAX_TYPE_CHARS);
        assert!(matches!(touch(&format!("kb type {}", b64(&wide))), Parsed::Command(Command::Kb(Kb::Type(_)))));
    }

    #[tokio::test]
    async fn the_longest_kb_type_line_is_read_whole() {
        let wide = "\u{1F600}".repeat(MAX_TYPE_CHARS);
        let line = format!("kb type {}\n", b64(&wide));
        assert!(line.len() < MAX_LINE, "{} bytes", line.len());
        let v = run(line.as_bytes()).await;
        assert_eq!(v.len(), 2);
        assert!(matches!(&v[0], Command::Kb(Kb::Type(t)) if t.chars().count() == MAX_TYPE_CHARS));
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
        assert_eq!(v, vec![Command::Keyframe, Command::Stop, Command::Keyframe, Command::Stop]);
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
        let mut input = vec![b'k'; 200_000];
        input.extend_from_slice(b"\nkeyframe\n");
        assert_eq!(run(&input).await, vec![Command::Keyframe, Command::Stop]);
        // An overlong line that happens to start with a command word is still no command.
        let mut input = b"keyframe".to_vec();
        input.extend(std::iter::repeat_n(b' ', 70_000));
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
