//! `scrios`: mirrors an iOS device's screen as an H.264 or HEVC stream, with touch and keys.
//! This is the command line over the library (`scrios::stream`); see `docs/cli.md` and `docs/protocol.md`.

// A raw or framed stream may go to stdout: nothing else may be printed there while it does.
#![deny(clippy::print_stdout)]

use std::fs::File;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Args, Parser, Subcommand, ValueEnum};
use tokio::sync::mpsc;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use scrios::codec::CodecKind;
use scrios::commands::{Command, read_commands, supervised};
use scrios::frame::Event;
use scrios::hid::{HidMapping, HidSpace};
use scrios::list;
use scrios::sink::{Format, Sink};
use scrios::stream::{EXIT_START_FAILED, Input, Item, Probe, Stream, StreamOptions};

/// The longest interval accepted for the millisecond options (a day); longer ones are clamped.
const MAX_INTERVAL_MS: u64 = 24 * 60 * 60 * 1000;
/// How long the writer may take to finish at exit.
const WRITER_GRACE: Duration = Duration::from_secs(3);
/// The longest probe window accepted.
const MAX_PROBE_SECS: u64 = 600;

/// The video codec to ask for (see `--video-codec`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum CodecArg {
    /// An offer that names only H.264, which every browser and player decodes.
    H264,
    /// The stock offer, which names H.264 and HEVC; the device sends HEVC (needs hardware decoding in a browser).
    #[value(name = "h265", alias = "hevc")]
    H265,
}

/// The codec when `--video-codec` is not given. One line to change; the docs and the tests name it.
const DEFAULT_CODEC: CodecArg = CodecArg::H264;

impl CodecArg {
    fn kind(self) -> CodecKind {
        match self {
            CodecArg::H264 => CodecKind::H264,
            CodecArg::H265 => CodecKind::Hevc,
        }
    }
}

/// What is written to the output (see `--format`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum FormatArg {
    /// A plain Annex-B elementary stream, which ffplay, mpv and ffmpeg read as it is.
    Raw,
    /// The machine protocol of docs/protocol.md: length-framed access units and JSON events.
    Framed,
}

impl FormatArg {
    fn format(self) -> Format {
        match self {
            FormatArg::Raw => Format::Raw,
            FormatArg::Framed => Format::Framed,
        }
    }
}

/// Where the digitizer's axes lie (see `--hid-space`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum SpaceArg {
    /// Worked out from the display's native orientation (the identity without display info).
    Auto,
    /// The framebuffer's own axes: touches are turned back by the config's `rotation`.
    Panel,
    /// The UI's orientation: touches are used as they are.
    Ui,
}

const AFTER_HELP: &str = "\
Commands on stdin (one per line; end of input is `stop`):
  keyframe                     ask the device for a fresh keyframe
  stop                         stop cleanly (SIGINT and SIGTERM do too; it can take about 8 seconds)
  touch down|move|up X Y ID    a finger, in pixels of the upright picture (finger 0 only)
  key NAME [short|long]        a hardware button: home, app-switcher (switcher), lock, power, sleep,
                               volume-up, volume-down, mute, siri
  kb down|up USAGE             a key of the HID keyboard page, by its usage in hex (04 is A)
  kb type BASE64               type a UTF-8 string through the US layout
touch, key and kb need control (the default; see --no-control). docs/protocol.md has the details.

Exit status: 0 after a clean stop, 1 if the stream failed while running, 2 if it could not be
started (or the options do not go together), 101 after an internal error.

Examples:
  scrios list
  scrios | ffplay -f h264 -
  scrios --udid 00008110-001234567890401E --record screen.h264
  scrios --format framed --output - | my-program";

#[derive(Parser, Debug)]
#[command(
    name = "scrios",
    version,
    about = "Mirror and control an iOS device's screen: H.264 or HEVC video, touch and keys, over CoreDevice",
    long_about = "Mirror and control an iOS device's screen. Connects to the device through usbmuxd (with the pair \
record usbmuxd already has), opens the CoreDevice tunnel, starts the display stream and writes it as video: by \
default a raw Annex-B H.264 stream on stdout, which a player reads directly (`scrios | ffplay -f h264 -`). \
--record FILE records it as well, --output FILE writes it to a file instead of stdout, and --format framed is the \
length-framed machine protocol (access units and JSON events) for programs. Touch and keys are sent to the \
device through the commands on stdin (see below), unless --no-control. Logs go to stderr.\n\n\
Without a subcommand, `scrios` is `scrios stream`. `scrios list` (or --list-devices) lists the devices usbmuxd knows.",
    after_long_help = AFTER_HELP,
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,

    /// List the devices usbmuxd knows, then exit (the same as `scrios list`)
    #[arg(long)]
    list_devices: bool,

    #[command(flatten)]
    stream: StreamArgs,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Stream the device's screen (what `scrios` does without a subcommand)
    #[command(after_long_help = AFTER_HELP)]
    Stream(StreamArgs),
    /// List the devices usbmuxd knows: UDID and how each is connected
    List(ListArgs),
}

#[derive(Args, Debug)]
struct ListArgs {
    #[arg(
        long,
        value_name = "PATH|HOST:PORT",
        help = "usbmuxd address: a unix socket path or host:port (default: $USBMUXD_SOCKET_ADDRESS, else /var/run/usbmuxd)"
    )]
    usbmuxd: Option<String>,
}

/// What the command line asks for.
#[derive(Debug)]
enum Action {
    List { usbmuxd: Option<String> },
    Stream(Box<StreamArgs>),
}

impl Cli {
    fn action(self) -> Action {
        match self.command {
            Some(Cmd::List(l)) => Action::List { usbmuxd: l.usbmuxd },
            Some(Cmd::Stream(s)) => Action::Stream(Box::new(s)),
            None if self.list_devices => Action::List { usbmuxd: self.stream.usbmuxd },
            None => Action::Stream(Box::new(self.stream)),
        }
    }
}

#[derive(Args, Debug)]
struct StreamArgs {
    /// UDID of the device (default: the only device usbmuxd lists; see `scrios list`)
    #[arg(short = 's', long, value_name = "UDID")]
    udid: Option<String>,

    #[arg(
        long,
        value_name = "PATH|HOST:PORT",
        help = "usbmuxd address: a unix socket path or host:port (default: $USBMUXD_SOCKET_ADDRESS, else /var/run/usbmuxd)"
    )]
    usbmuxd: Option<String>,

    /// The display to stream (1 is the built-in display)
    #[arg(long, value_name = "N", default_value_t = 1)]
    display_id: i64,

    /// The video codec to ask for: h264 (an offer that leaves the HEVC bank out) or h265 (alias hevc: the stock
    /// offer, byte for byte)
    #[arg(long = "video-codec", alias = "codec", value_enum, value_name = "CODEC", default_value_t = DEFAULT_CODEC)]
    video_codec: CodecArg,

    /// Also record the video to FILE as a raw Annex-B stream (.h264, or .h265 for HEVC), flushed picture by picture.
    /// Given alone, it is all that is written: add --output or --format to stream as well
    #[arg(short = 'r', long, value_name = "FILE")]
    record: Option<PathBuf>,

    /// Where the stream goes: `-` for stdout or a file. Default: stdout, which is refused when it is a terminal
    /// (the video is binary); with --record alone, nowhere
    #[arg(short = 'o', long, value_name = "-|FILE")]
    output: Option<String>,

    /// What is written to the output: raw (a plain Annex-B stream: `scrios | ffplay -f h264 -`) or framed
    /// (the machine protocol of docs/protocol.md). Default: raw
    #[arg(long, value_enum, value_name = "FORMAT")]
    format: Option<FormatArg>,

    /// Do not connect the touch and key services: the touch, key and kb commands do nothing (a stream that
    /// only watches). Control is on by default; if it cannot be connected the stream runs without it and says so
    #[arg(short = 'n', long)]
    no_control: bool,

    /// Accepted for compatibility and ignored: control is on by default (see --no-control)
    #[arg(long, hide = true)]
    hid: bool,

    /// Accepted for compatibility and ignored: the display offer has no bitrate field
    #[arg(long, hide = true, value_name = "BPS")]
    max_bitrate: Option<u32>,

    #[command(flatten, next_help_heading = "Advanced")]
    advanced: AdvancedArgs,

    #[command(flatten, next_help_heading = "Diagnostics")]
    probe: ProbeArgs,
}

#[derive(Args, Debug)]
struct AdvancedArgs {
    /// Override of the picture's visible size, e.g. 1206x2622 (default: the primary display's size from
    /// the device, else the conformance window of the SPS, else its size)
    #[arg(long, value_name = "WxH", value_parser = parse_size)]
    visible_size: Option<(u32, u32)>,

    /// Send an RTCP receiver report this often, in ms (0 = never; the device wants one at least every 20 s)
    #[arg(long, value_name = "MS", default_value_t = 5000)]
    rtcp_interval_ms: u64,

    /// Never send two keyframe requests closer than this, in ms
    #[arg(long, value_name = "MS", default_value_t = 2000)]
    keyframe_min_interval_ms: u64,

    /// Emit a stats event this often, in ms (0 = never; the events are only written by --format framed)
    #[arg(long, value_name = "MS", default_value_t = 5000)]
    stats_interval_ms: u64,

    /// After this long without RTP (sender reports do not count), ask for a keyframe (ms, 0 = never)
    #[arg(long, value_name = "MS", default_value_t = 5000)]
    rtp_stall_keyframe_ms: u64,

    /// After this long without RTP, give up with an error (ms, 0 = never)
    #[arg(long, value_name = "MS", default_value_t = 15000)]
    rtp_stall_timeout_ms: u64,

    /// Ask the device for its display's size and orientation this often, in ms (0 = only at start
    /// and when the SPS changes). A change makes a new `config` event.
    #[arg(long, value_name = "MS", default_value_t = 1000)]
    display_poll_ms: u64,

    /// Where the digitizer's axes lie. auto: the panel's native axes, worked out from the display's
    /// reported native orientation (checked on one iPad only); panel: the framebuffer's own axes;
    /// ui: the upright picture's axes
    #[arg(long, value_enum, value_name = "SPACE", default_value_t = SpaceArg::Auto)]
    hid_space: SpaceArg,

    /// Mirror touches horizontally, after the chosen space and after --hid-swap-xy
    #[arg(long)]
    hid_flip_x: bool,

    /// Mirror touches vertically, after the chosen space and after --hid-swap-xy
    #[arg(long)]
    hid_flip_y: bool,

    /// Exchange the x and y axes, after the chosen space (these three apply on top of `auto` too)
    #[arg(long)]
    hid_swap_xy: bool,

    /// Do not read commands from stdin. Without it, the end of stdin is a `stop`, so a stream started with stdin
    /// closed (`< /dev/null`, nohup, a background job) would stop at once; stop it with SIGINT or SIGTERM instead
    #[arg(long)]
    no_stdin: bool,

    /// Offer allowRTCPFB = 1 (the stock offer has 0; a keyframe request is answered with the stock
    /// offer already, so this is only for comparing on a device)
    #[arg(long)]
    rtcp_fb: bool,

    /// Offer without long-term reference pictures (ltrpEnabled 0, no `LTR;` in the feature string)
    #[arg(long)]
    no_ltr: bool,

    /// Also send Device Hub's per-frame acknowledgements and RCTL reports (experimental, off by default)
    #[arg(long)]
    device_hub_feedback: bool,

    /// Log the device's answer, server status and media support info to stderr
    #[arg(long)]
    log_answer: bool,
}

#[derive(Args, Debug)]
struct ProbeArgs {
    /// Probe: start the stream (needs --video-codec h264, the default), count for this many seconds what arrives
    /// (RTP packets per payload type, the first payload bytes, H.264 NAL types, the first SPS), log it, stop cleanly
    /// and exit (0 if it was H.264, 1 with an `error` event if not). No video is written. 0 = off.
    #[arg(long, value_name = "SECS", default_value_t = 0)]
    probe_secs: u64,

    /// With the probe: write every datagram received during the window, RTP and RTCP, to this file, each as
    /// a 4-byte big-endian length followed by its bytes; every RTP packet whose payload kind is not
    /// 1, 5 or 28 is also logged in full (up to 400 payload bytes)
    #[arg(long, value_name = "PATH")]
    probe_raw: Option<PathBuf>,
}

fn parse_size(s: &str) -> Result<(u32, u32), String> {
    let (w, h) = s.split_once(['x', 'X']).ok_or_else(|| format!("expected WxH, got {s:?}"))?;
    let w: u32 = w.trim().parse().map_err(|_| format!("bad width in {s:?}"))?;
    let h: u32 = h.trim().parse().map_err(|_| format!("bad height in {s:?}"))?;
    if !(1..=32768).contains(&w) || !(1..=32768).contains(&h) {
        return Err(format!("size out of range: {s:?}"));
    }
    Ok((w, h))
}

/// An interval option in ms: 0 is "off", anything above a day is clamped (and says so).
fn interval(name: &str, ms: u64) -> Option<Duration> {
    if ms > MAX_INTERVAL_MS {
        warn!("{name} {ms} is more than a day: using a day");
    }
    (ms > 0).then(|| Duration::from_millis(ms.min(MAX_INTERVAL_MS)))
}

/// Where the stream is written.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    Stdout,
    File(PathBuf),
    /// Nowhere: only `--record` (or nothing at all, for a probe) is written.
    Nothing,
}

/// Containers that `--record` does not write (it writes a raw Annex-B stream).
const CONTAINERS: [&str; 6] = ["mp4", "m4v", "mov", "mkv", "webm", "avi"];

impl AdvancedArgs {
    fn hid_mapping(&self) -> HidMapping {
        HidMapping {
            space: match self.hid_space {
                SpaceArg::Auto => HidSpace::Auto,
                SpaceArg::Panel => HidSpace::Panel,
                SpaceArg::Ui => HidSpace::Ui,
            },
            flip_x: self.hid_flip_x,
            flip_y: self.hid_flip_y,
            swap_xy: self.hid_swap_xy,
        }
    }
}

impl StreamArgs {
    /// The probe window, if a probe was asked for.
    fn probe_window(&self) -> Option<Duration> {
        (self.probe.probe_secs > 0).then(|| Duration::from_secs(self.probe.probe_secs.min(MAX_PROBE_SECS)))
    }

    /// Whether touch and keys are connected.
    fn control(&self) -> bool {
        !self.no_control
    }

    /// The format of the output.
    fn format(&self) -> Format {
        self.format.unwrap_or(FormatArg::Raw).format()
    }

    /// Options that do not go together.
    fn check(&self) -> Result<(), String> {
        if self.probe.probe_raw.is_some() && self.probe.probe_secs == 0 {
            return Err("--probe-raw is for the probe: add --probe-secs N".into());
        }
        match (self.video_codec, self.probe.probe_secs > 0) {
            (CodecArg::H265, true) => Err("--probe-secs is for --video-codec h264".into()),
            _ => Ok(()),
        }
    }

    /// Where the stream goes, given whether stdout is a terminal.
    fn target(&self, stdout_is_terminal: bool) -> Result<Target, String> {
        match self.output.as_deref() {
            Some("-") => return Ok(Target::Stdout),
            Some(path) => return Ok(Target::File(PathBuf::from(path))),
            None => {}
        }
        // Only --record was asked for: the stream is not written anywhere else. A probe writes no video.
        let record_only = self.record.is_some() && self.format.is_none();
        let probe_without_events = self.probe_window().is_some() && self.format() == Format::Raw;
        if record_only || probe_without_events {
            return Ok(Target::Nothing);
        }
        if stdout_is_terminal {
            return Err("refusing to write the video to a terminal: pipe it (`scrios | ffplay -f h264 -`), or give \
                        --output FILE or --record FILE (--output - writes to the terminal anyway)"
                .into());
        }
        Ok(Target::Stdout)
    }

    /// What `--record` may be.
    fn check_record(&self) -> Result<(), String> {
        let Some(path) = &self.record else { return Ok(()) };
        if path == Path::new("-") {
            return Err("--record needs a file (the stream itself goes to stdout with --output -)".into());
        }
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        if CONTAINERS.contains(&ext.as_str()) {
            return Err(format!(
                "--record writes a raw Annex-B stream, not .{ext}: record to .h264 (or .h265) and wrap it, \
                 e.g. `ffmpeg -i {0}.h264 -c copy {0}.{ext}`",
                path.with_extension("").display()
            ));
        }
        Ok(())
    }

    /// The options of the library's stream.
    fn stream_options(&self) -> StreamOptions {
        let a = &self.advanced;
        if let Some(bps) = self.max_bitrate {
            warn!("--max-bitrate {bps} ignored: the display offer has no bitrate ceiling to set");
        }
        StreamOptions {
            udid: self.udid.clone(),
            usbmuxd: self.usbmuxd.clone(),
            display_id: self.display_id,
            codec: self.video_codec.kind(),
            allow_rtcp_fb: a.rtcp_fb,
            ltr: !a.no_ltr,
            log_answer: a.log_answer,
            control: self.control(),
            hid_mapping: a.hid_mapping(),
            visible_size: a.visible_size,
            rtcp_interval: interval("--rtcp-interval-ms", a.rtcp_interval_ms),
            keyframe_min_interval: interval("--keyframe-min-interval-ms", a.keyframe_min_interval_ms)
                .unwrap_or(Duration::ZERO),
            stats_interval: interval("--stats-interval-ms", a.stats_interval_ms),
            rtp_stall_keyframe: interval("--rtp-stall-keyframe-ms", a.rtp_stall_keyframe_ms),
            rtp_stall_timeout: interval("--rtp-stall-timeout-ms", a.rtp_stall_timeout_ms),
            device_hub_feedback: a.device_hub_feedback,
            display_poll: interval("--display-poll-ms", a.display_poll_ms),
            probe: self.probe_window().map(|window| Probe { window, raw_dump: self.probe.probe_raw.clone() }),
        }
    }
}

fn init_logging() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("scrios=info,idevice=warn,warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_target(false)
        .init();
}

/// Takes stdout for the stream: the descriptor is duplicated and fd 1 is pointed at stderr,
/// so a stray print anywhere in the process cannot corrupt the stream.
#[cfg(unix)]
fn take_stdout() -> std::io::Result<File> {
    use std::os::fd::FromRawFd;
    // SAFETY: dup and dup2 on this process's own standard descriptors; the new descriptor is
    // handed to exactly one File.
    unsafe {
        let fd = libc::dup(1);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::dup2(2, 1) < 0 {
            let e = std::io::Error::last_os_error();
            libc::close(fd);
            return Err(e);
        }
        Ok(File::from_raw_fd(fd))
    }
}

#[cfg(not(unix))]
fn take_stdout() -> std::io::Result<File> {
    Err(std::io::Error::other("only unix hosts are supported"))
}

/// One output of the stream: a name for the log and the writer.
struct Output {
    name: String,
    sink: Sink<Box<dyn Write + Send>>,
}

impl Output {
    fn new(name: String, format: Format, out: impl Write + Send + 'static) -> Output {
        Output { name, sink: Sink::new(format, Box::new(out)) }
    }
}

/// Writes what the stream yields to every output, until it ends or no output is left to write to (the reader of
/// stdout went away, the disk is full): then the stream is told that nobody is reading and stops.
fn consume(stream: &Stream, mut outputs: Vec<Output>) {
    let had_outputs = !outputs.is_empty();
    while let Some(item) = stream.recv() {
        outputs.retain_mut(|o| match o.sink.write_item(&item) {
            Ok(()) => true,
            Err(e) => {
                warn!("writing to {} failed: {e}", o.name);
                false
            }
        });
        if had_outputs && outputs.is_empty() {
            stream.hang_up();
            return;
        }
    }
    for o in &mut outputs {
        if let Err(e) = o.sink.flush() {
            warn!("flushing {} failed: {e}", o.name);
        }
    }
}

/// SIGTERM and SIGINT are a `stop`.
async fn stop_on_signals(tx: mpsc::Sender<Command>) {
    use tokio::signal::unix::{SignalKind, signal};
    let (mut term, mut int) = match (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) {
        (Ok(t), Ok(i)) => (t, i),
        _ => {
            warn!("cannot install the signal handlers");
            return;
        }
    };
    loop {
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
        info!("signal received: stopping");
        if tx.send(Command::Stop).await.is_err() {
            return;
        }
    }
}

/// Feeds the stream's input from stdin (if `read_stdin`) and from the signals, on a thread of its own.
fn spawn_input(input: Input, read_stdin: bool) {
    let spawned = std::thread::Builder::new().name("scrios-input".into()).spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                warn!("cannot read commands: {e}");
                return;
            }
        };
        rt.block_on(async move {
            let tx = input.sender();
            if read_stdin {
                tokio::spawn(supervised(read_commands(tokio::io::stdin(), tx.clone()), tx.clone()));
            }
            stop_on_signals(tx).await;
            // The thread (and its blocked read of stdin) ends with the process.
            std::future::pending::<()>().await;
        });
    });
    if let Err(e) = spawned {
        warn!("cannot read commands: {e}");
    }
}

/// Reports a failure to start: in the log and, if the primary output is a framed one, as its `error` event.
fn refuse(primary: Option<&mut Output>, message: String) -> i32 {
    error!("{message}");
    if let Some(o) = primary {
        let _ = o.sink.write_item(&Item::Event(Event::Error { message }));
    }
    EXIT_START_FAILED
}

fn run_stream(args: &StreamArgs) -> i32 {
    let format = args.format();
    let target = match args.target(std::io::stdout().is_terminal()) {
        Ok(t) => t,
        Err(why) => return refuse(None, why),
    };
    let primary = match &target {
        Target::Nothing => None,
        Target::Stdout => match take_stdout() {
            Ok(f) => Some(Output::new("stdout".into(), format, f)),
            Err(e) => return refuse(None, format!("cannot take over stdout: {e}")),
        },
        Target::File(path) => match File::create(path) {
            Ok(f) => Some(Output::new(path.display().to_string(), format, f)),
            Err(e) => return refuse(None, format!("cannot write --output {}: {e}", path.display())),
        },
    };
    let mut outputs: Vec<Output> = primary.into_iter().collect();

    if let Err(why) = args.check().and_then(|()| args.check_record()) {
        return refuse(outputs.first_mut(), why);
    }
    // The recording is opened before anything is started: a path that cannot be written should not cost a run.
    if let Some(path) = &args.record {
        match File::create(path) {
            Ok(f) => outputs.push(Output::new(path.display().to_string(), Format::Raw, f)),
            Err(e) => return refuse(outputs.first_mut(), format!("cannot write --record {}: {e}", path.display())),
        }
    }

    let stream = match Stream::start(args.stream_options()) {
        Ok(s) => s,
        Err(e) => return refuse(outputs.first_mut(), format!("cannot start the stream: {e}")),
    };
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let writer = std::thread::Builder::new().name("scrios-writer".into()).spawn({
        let stream = stream.clone();
        move || {
            consume(&stream, outputs);
            let _ = done_tx.send(());
        }
    });
    if let Err(e) = writer {
        stream.hang_up();
        error!("cannot start the writer thread: {e}");
        return EXIT_START_FAILED;
    }
    spawn_input(stream.input(), !args.advanced.no_stdin);

    let outcome = stream.wait();
    // Let the writer finish what is queued (the access units are stale now: only events are kept), but never wait
    // long for a reader that does not read.
    let _ = done_rx.recv_timeout(WRITER_GRACE);
    outcome.exit_code()
}

fn run_list(usbmuxd: Option<&str>) -> i32 {
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            error!("cannot build the async runtime: {e}");
            return EXIT_START_FAILED;
        }
    };
    match rt.block_on(list::list_devices(usbmuxd)) {
        Ok(devices) if devices.is_empty() => {
            eprintln!("no device is connected (usbmuxd lists none)");
            0
        }
        Ok(devices) => {
            // A closed pipe (`scrios list | head -1`) is not an error.
            let _ = std::io::stdout().lock().write_all(list::format_table(&devices).as_bytes());
            0
        }
        Err(why) => {
            error!("{why}");
            EXIT_START_FAILED
        }
    }
}

fn main() {
    let cli = Cli::parse();
    init_logging();
    let code = match cli.action() {
        Action::List { usbmuxd } => run_list(usbmuxd.as_deref()),
        Action::Stream(args) => run_stream(&args),
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;
    use scrios::offer::{Codec, OfferFlags};

    fn stream_args(args: &[&str]) -> StreamArgs {
        let argv = std::iter::once("scrios").chain(args.iter().copied());
        match Cli::parse_from(argv).action() {
            Action::Stream(a) => *a,
            other => panic!("not a stream: {other:?}"),
        }
    }

    fn options(args: &[&str]) -> StreamOptions {
        stream_args(args).stream_options()
    }

    fn offer(o: &StreamOptions) -> OfferFlags {
        OfferFlags {
            allow_rtcp_fb: o.allow_rtcp_fb,
            ltr: o.ltr,
            codec: match o.codec {
                CodecKind::H264 => Codec::H264,
                CodecKind::Hevc => Codec::Hevc,
            },
        }
    }

    #[test]
    fn the_command_line_is_well_formed() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn sizes_parse() {
        assert_eq!(parse_size("1206x2622"), Ok((1206, 2622)));
        assert_eq!(parse_size("640X480"), Ok((640, 480)));
        assert!(parse_size("1206").is_err());
        assert!(parse_size("axb").is_err());
        assert!(parse_size("0x10").is_err());
        assert!(parse_size("99999x10").is_err());
    }

    #[test]
    fn intervals_are_off_at_zero_and_clamped_to_a_day() {
        assert_eq!(interval("x", 0), None);
        assert_eq!(interval("x", 1500), Some(Duration::from_millis(1500)));
        let day = Duration::from_millis(MAX_INTERVAL_MS);
        assert_eq!(interval("x", u64::MAX), Some(day));
        assert_eq!(interval("x", MAX_INTERVAL_MS + 1), Some(day));
        // Adding it to a clock reading cannot overflow.
        let _ = std::time::Instant::now() + day;
    }

    #[test]
    fn no_subcommand_is_stream_and_the_subcommands_are_chosen() {
        assert!(matches!(Cli::parse_from(["scrios"]).action(), Action::Stream(_)));
        assert!(matches!(Cli::parse_from(["scrios", "stream"]).action(), Action::Stream(_)));
        match Cli::parse_from(["scrios", "stream", "--udid", "U", "--format", "framed"]).action() {
            Action::Stream(a) => {
                assert_eq!(a.udid.as_deref(), Some("U"));
                assert_eq!(a.format(), Format::Framed);
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(Cli::parse_from(["scrios", "list"]).action(), Action::List { usbmuxd: None }));
        match Cli::parse_from(["scrios", "list", "--usbmuxd", "127.0.0.1:27015"]).action() {
            Action::List { usbmuxd } => assert_eq!(usbmuxd.as_deref(), Some("127.0.0.1:27015")),
            other => panic!("{other:?}"),
        }
        // --list-devices is `list` (and takes --usbmuxd).
        assert!(matches!(Cli::parse_from(["scrios", "--list-devices"]).action(), Action::List { usbmuxd: None }));
        match Cli::parse_from(["scrios", "--list-devices", "--usbmuxd", "/tmp/mux"]).action() {
            Action::List { usbmuxd } => assert_eq!(usbmuxd.as_deref(), Some("/tmp/mux")),
            other => panic!("{other:?}"),
        }
        // The stream's options do not mix with a subcommand's position, and a stranger is refused.
        assert!(Cli::try_parse_from(["scrios", "--udid", "U", "list"]).is_err());
        assert!(Cli::try_parse_from(["scrios", "frobnicate"]).is_err());
    }

    #[test]
    fn the_cli_defaults_are_the_contracts() {
        let a = stream_args(&[]);
        assert_eq!(a.udid, None);
        assert_eq!(a.usbmuxd, None);
        assert_eq!(a.display_id, 1);
        assert_eq!(a.record, None);
        assert_eq!(a.output, None);
        assert_eq!(a.format(), Format::Raw, "raw is the default format");
        // Control is on, and its mapping is the automatic one.
        assert!(a.control());
        assert_eq!(a.advanced.hid_mapping(), HidMapping::default());
        assert_eq!(a.advanced.hid_mapping().space, HidSpace::Auto);
        // The codec is H.264 (an offer without the HEVC bank) and no probe is asked for.
        assert_eq!(a.video_codec, DEFAULT_CODEC);
        assert_eq!(DEFAULT_CODEC, CodecArg::H264, "the documented default");
        assert_eq!(a.probe_window(), None);
        assert_eq!(a.check(), Ok(()));
        assert_eq!(a.check_record(), Ok(()));

        let o = a.stream_options();
        assert_eq!(o.udid, None);
        assert_eq!(o.display_id, 1);
        assert_eq!(o.visible_size, None);
        assert_eq!(o.rtcp_interval, Some(Duration::from_secs(5)));
        assert_eq!(o.keyframe_min_interval, Duration::from_secs(2));
        assert_eq!(o.stats_interval, Some(Duration::from_secs(5)));
        assert_eq!(o.rtp_stall_keyframe, Some(Duration::from_secs(5)));
        assert_eq!(o.rtp_stall_timeout, Some(Duration::from_secs(15)));
        assert_eq!(o.display_poll, Some(Duration::from_secs(1)));
        assert!(!o.device_hub_feedback);
        assert!(o.control);
        assert!(o.probe.is_none());
        // The offer is the stock one, less the HEVC bank, unless asked otherwise.
        assert_eq!(offer(&o), OfferFlags { codec: Codec::H264, ..OfferFlags::default() });
        // The library's defaults are the command line's.
        let lib = StreamOptions::default();
        assert_eq!(
            (o.display_id, o.codec, o.control, o.rtcp_interval, o.keyframe_min_interval, o.stats_interval),
            (lib.display_id, lib.codec, lib.control, lib.rtcp_interval, lib.keyframe_min_interval, lib.stats_interval)
        );
        assert_eq!(
            (o.rtp_stall_keyframe, o.rtp_stall_timeout, o.display_poll, o.hid_mapping),
            (lib.rtp_stall_keyframe, lib.rtp_stall_timeout, lib.display_poll, lib.hid_mapping)
        );
    }

    #[test]
    fn the_video_codec_has_its_scrcpy_name_and_the_old_ones() {
        for (args, want) in [
            (vec!["--video-codec", "h264"], CodecKind::H264),
            (vec!["--video-codec", "h265"], CodecKind::Hevc),
            (vec!["--video-codec", "hevc"], CodecKind::Hevc),
            // The old flag still works.
            (vec!["--codec", "h264"], CodecKind::H264),
            (vec!["--codec", "hevc"], CodecKind::Hevc),
            (vec!["--codec", "h265"], CodecKind::Hevc),
        ] {
            assert_eq!(options(&args).codec, want, "{args:?}");
        }
        assert_eq!(offer(&options(&["--codec", "hevc"])), OfferFlags::default(), "HEVC is the stock offer itself");
        assert!(Cli::try_parse_from(["scrios", "--video-codec", "vp9"]).is_err());
        assert!(Cli::try_parse_from(["scrios", "--codec", "vp9"]).is_err());
    }

    #[test]
    fn the_codec_options_go_together_or_not() {
        // H.264 is the default, with or without the flag, and without a probe it is the stream.
        for args in [vec![], vec!["--video-codec", "h264"], vec!["--codec", "h264"]] {
            let a = stream_args(&args);
            assert_eq!(a.check(), Ok(()));
            assert_eq!(a.stream_options().codec, CodecKind::H264);
            assert_eq!(a.probe_window(), None);
        }
        // A probe needs no flag but the window: the default codec is H.264.
        let a = stream_args(&["--probe-secs", "5"]);
        assert_eq!(a.check(), Ok(()));
        assert_eq!(a.probe_window(), Some(Duration::from_secs(5)));
        let probe = a.stream_options().probe.unwrap();
        assert_eq!((probe.window, probe.raw_dump), (Duration::from_secs(5), None));
        assert_eq!(a.stream_options().check(), Ok(()));
        // The window is capped.
        let a = stream_args(&["--video-codec", "h264", "--probe-secs", "999999"]);
        assert_eq!(a.probe_window(), Some(Duration::from_secs(MAX_PROBE_SECS)));
        // A probe of HEVC is refused: the probe is for H.264.
        for flag in ["--video-codec", "--codec"] {
            let a = stream_args(&[flag, "hevc", "--probe-secs", "5"]);
            assert!(a.check().unwrap_err().contains("--video-codec h264"));
            assert!(a.stream_options().check().is_err(), "the library says so too");
        }
        // HEVC is the opt-in: the stock offer.
        let a = stream_args(&["--video-codec", "h265"]);
        assert_eq!(a.check(), Ok(()));
        assert_eq!(offer(&a.stream_options()).codec, Codec::Hevc);
        // The raw dump belongs to the probe.
        let a = stream_args(&["--probe-secs", "8", "--probe-raw", "/tmp/x.raw"]);
        assert_eq!(a.check(), Ok(()));
        assert_eq!(a.probe.probe_raw.as_deref(), Some(Path::new("/tmp/x.raw")));
        assert_eq!(a.stream_options().probe.unwrap().raw_dump.as_deref(), Some(Path::new("/tmp/x.raw")));
        let a = stream_args(&["--probe-raw", "/tmp/x.raw"]);
        assert!(a.check().unwrap_err().contains("--probe-raw is for the probe"));
    }

    #[test]
    fn the_old_options_still_parse() {
        let a = stream_args(&[
            "--udid",
            "U",
            "--usbmuxd",
            "127.0.0.1:27015",
            "--display-id",
            "2",
            "--max-bitrate",
            "4000000",
            "--visible-size",
            "1206x2622",
            "--rtcp-interval-ms",
            "0",
            "--keyframe-min-interval-ms",
            "3000",
            "--stats-interval-ms",
            "0",
            "--rtp-stall-keyframe-ms",
            "0",
            "--rtp-stall-timeout-ms",
            "9000",
            "--rtcp-fb",
            "--no-ltr",
            "--device-hub-feedback",
            "--log-answer",
            "--codec",
            "h264",
        ]);
        assert_eq!(a.udid.as_deref(), Some("U"));
        assert_eq!(a.max_bitrate, Some(4_000_000));
        let o = a.stream_options();
        assert_eq!(o.usbmuxd.as_deref(), Some("127.0.0.1:27015"));
        assert_eq!(o.display_id, 2);
        assert_eq!(o.visible_size, Some((1206, 2622)));
        assert_eq!(o.rtcp_interval, None);
        assert_eq!(o.keyframe_min_interval, Duration::from_secs(3));
        assert_eq!(o.stats_interval, None);
        assert_eq!(o.rtp_stall_keyframe, None);
        assert_eq!(o.rtp_stall_timeout, Some(Duration::from_secs(9)));
        assert_eq!(offer(&o), OfferFlags { allow_rtcp_fb: true, ltr: false, codec: Codec::H264 });
        assert!(o.device_hub_feedback);
        assert!(o.log_answer);
        let a = stream_args(&["--hid", "--hid-space", "ui", "--hid-flip-x", "--hid-swap-xy", "--display-poll-ms", "0"]);
        // --hid is the default now: accepted, and nothing changes.
        assert!(a.hid && a.control());
        assert_eq!(
            a.advanced.hid_mapping(),
            HidMapping { space: HidSpace::Ui, flip_x: true, flip_y: false, swap_xy: true }
        );
        assert_eq!(a.stream_options().display_poll, None);
        assert!(Cli::try_parse_from(["scrios", "--hid-space", "sideways"]).is_err());
        // Each offer flag on its own.
        assert_eq!(
            offer(&options(&["--rtcp-fb"])),
            OfferFlags { allow_rtcp_fb: true, codec: Codec::H264, ..OfferFlags::default() }
        );
        assert_eq!(
            offer(&options(&["--no-ltr"])),
            OfferFlags { ltr: false, codec: Codec::H264, ..OfferFlags::default() }
        );
    }

    #[test]
    fn control_is_on_unless_no_control() {
        assert!(options(&[]).control);
        assert!(options(&["--hid"]).control, "the old flag is a no-op");
        assert!(!options(&["--no-control"]).control);
        assert!(!options(&["-n"]).control);
        assert!(!options(&["--hid", "--no-control"]).control, "--no-control wins");
    }

    #[test]
    fn the_scrcpy_flags_parse() {
        let a = stream_args(&["-s", "U", "-r", "out.h264", "-o", "-", "-n"]);
        assert_eq!(a.udid.as_deref(), Some("U"));
        assert_eq!(a.record.as_deref(), Some(Path::new("out.h264")));
        assert_eq!(a.output.as_deref(), Some("-"));
        assert!(a.no_control);
        // Options scrcpy has and the display offer cannot honour are refused, not ignored.
        assert!(Cli::try_parse_from(["scrios", "--max-fps", "30"]).is_err());
        assert!(Cli::try_parse_from(["scrios", "--no-video"]).is_err());
    }

    #[test]
    fn the_output_goes_where_it_was_asked_and_never_to_a_terminal_by_default() {
        // No options: stdout, unless it is a terminal.
        assert_eq!(stream_args(&[]).target(false), Ok(Target::Stdout));
        assert!(stream_args(&[]).target(true).unwrap_err().contains("refusing to write the video to a terminal"));
        assert!(stream_args(&["--format", "framed"]).target(true).is_err());
        // --output is explicit: a terminal is allowed then.
        assert_eq!(stream_args(&["--output", "-"]).target(true), Ok(Target::Stdout));
        assert_eq!(stream_args(&["-o", "-"]).target(false), Ok(Target::Stdout));
        assert_eq!(stream_args(&["--output", "out.h264"]).target(true), Ok(Target::File("out.h264".into())));
        // --record alone records and writes the stream nowhere else; with --output or --format it is in addition.
        assert_eq!(stream_args(&["--record", "r.h264"]).target(true), Ok(Target::Nothing));
        assert_eq!(stream_args(&["--record", "r.h264"]).target(false), Ok(Target::Nothing));
        assert_eq!(stream_args(&["--record", "r.h264", "--output", "-"]).target(false), Ok(Target::Stdout));
        assert_eq!(stream_args(&["--record", "r.h264", "--format", "framed"]).target(false), Ok(Target::Stdout));
        assert!(stream_args(&["--record", "r.h264", "--format", "framed"]).target(true).is_err());
        // A probe writes no video: nothing to refuse in the raw format; its events are framed ones.
        assert_eq!(stream_args(&["--probe-secs", "3"]).target(true), Ok(Target::Nothing));
        assert_eq!(stream_args(&["--probe-secs", "3", "--format", "framed"]).target(false), Ok(Target::Stdout));
        assert!(stream_args(&["--probe-secs", "3", "--format", "framed"]).target(true).is_err());
    }

    #[test]
    fn the_formats_are_raw_by_default() {
        assert_eq!(stream_args(&[]).format(), Format::Raw);
        assert_eq!(stream_args(&["--format", "raw"]).format(), Format::Raw);
        assert_eq!(stream_args(&["--format", "framed"]).format(), Format::Framed);
        assert!(Cli::try_parse_from(["scrios", "--format", "mp4"]).is_err());
    }

    #[test]
    fn a_recording_is_a_raw_stream_not_a_container() {
        for ok in ["screen.h264", "screen.h265", "screen.264", "screen", "dir.mp4/screen.h264"] {
            assert_eq!(stream_args(&["--record", ok]).check_record(), Ok(()), "{ok}");
        }
        for bad in ["screen.mp4", "screen.MKV", "x.mov", "x.webm"] {
            let why = stream_args(&["--record", bad]).check_record().unwrap_err();
            assert!(why.contains("raw Annex-B") && why.contains("ffmpeg"), "{why}");
        }
        assert!(stream_args(&["--record", "-"]).check_record().unwrap_err().contains("needs a file"));
    }
}
