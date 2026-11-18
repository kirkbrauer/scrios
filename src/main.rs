//! `jumpstarter-ios-display`: streams an iPhone's screen to stdout as length-framed Annex-B HEVC
//! access units. See README.md for the protocol.

// stdout is the frame channel: nothing else may be printed there.
#![deny(clippy::print_stdout)]

mod commands;
mod depack;
mod device;
mod displayinfo;
mod engine;
mod frame;
mod hid;
mod keyframe;
mod offer;
mod outqueue;
mod rtpstats;
mod session;
mod sps;

use std::fs::File;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, ValueEnum};
use tokio::sync::{Notify, mpsc};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::commands::{Command, read_commands, supervised};
use crate::device::{DeviceOptions, Session};
use crate::engine::{ClockSource, Engine, EngineConfig};
use crate::frame::{Event, Frame};
use crate::frame::HidEvent;
use crate::hid::{Hid, HidBackend, HidMapping, HidSpace, NoHid, mapping_event};
use crate::offer::OfferFlags;
use crate::outqueue::{OutQueue, run_writer};
use crate::session::{Clock, DisplayLink, Exit, HidLink, Links, StopCause, TokioClock, run_session};

/// Exit status: the stream could not be started.
const EXIT_START_FAILED: i32 = 2;
/// Exit status: the stream failed while running.
const EXIT_FAILED: i32 = 1;
/// Exit status: an internal error (a panic).
const EXIT_INTERNAL: i32 = 101;
/// The longest interval accepted for the millisecond options (a day); longer ones are clamped.
const MAX_INTERVAL_MS: u64 = 24 * 60 * 60 * 1000;
/// How long the writer may take to finish at exit.
const WRITER_GRACE: Duration = Duration::from_secs(3);
/// How long the cleanup after a panic may take.
const PANIC_CLEANUP: Duration = Duration::from_secs(8);

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

#[derive(Parser, Debug)]
#[command(
    name = "jumpstarter-ios-display",
    version,
    about = "Streams an iPhone's screen (HEVC over RTP, CoreDevice displayservice) to stdout as length-framed Annex-B access units",
    long_about = "Connects to the phone through usbmuxd (using the pair record usbmuxd already has), opens the \
CoreDevice tunnel, starts the display stream and writes each picture to stdout as one frame: \
[u32 len][u64 pts_us][u8 flags][payload], big-endian. Control events are JSON frames (flag 0x80). \
Commands arrive on stdin, one per line: `keyframe`, `stop` (end of input also stops) and, with --hid, \
`touch down|move|up <x> <y> <id>` and `key <name> [short|long]`. Logs go to stderr. A clean stop can take up to about 8 seconds."
)]
struct Args {
    /// UDID of the phone (default: the only device usbmuxd lists)
    #[arg(long)]
    udid: Option<String>,

    /// usbmuxd address: a unix socket path or host:port (default: $USBMUXD_SOCKET_ADDRESS, else /var/run/usbmuxd)
    #[arg(long, value_name = "PATH|HOST:PORT")]
    usbmuxd: Option<String>,

    /// The display to stream (1 is the built-in display)
    #[arg(long, value_name = "N", default_value_t = 1)]
    display_id: i64,

    /// A ceiling for the bitrate (the display offer has no such field: logged and ignored)
    #[arg(long, value_name = "BPS")]
    max_bitrate: Option<u32>,

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

    /// Emit a stats event this often, in ms (0 = never)
    #[arg(long, value_name = "MS", default_value_t = 5000)]
    stats_interval_ms: u64,

    /// After this long without RTP (sender reports do not count), ask for a keyframe (ms, 0 = never)
    #[arg(long, value_name = "MS", default_value_t = 5000)]
    rtp_stall_keyframe_ms: u64,

    /// After this long without RTP, give up with an error (ms, 0 = never)
    #[arg(long, value_name = "MS", default_value_t = 15000)]
    rtp_stall_timeout_ms: u64,

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

    /// Ask the device for its display's size and orientation this often, in ms (0 = only at start
    /// and when the SPS changes). A change makes a new `config` event.
    #[arg(long, value_name = "MS", default_value_t = 1000)]
    display_poll_ms: u64,

    /// Accept `touch` and `key` commands: connect the device's HID services on the stream's tunnel
    #[arg(long)]
    hid: bool,

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

    /// Log the device's answer, server status and media support info to stderr (diagnostics)
    #[arg(long)]
    log_answer: bool,
}

fn parse_size(s: &str) -> Result<(u32, u32), String> {
    let (w, h) = s
        .split_once(['x', 'X'])
        .ok_or_else(|| format!("expected WxH, got {s:?}"))?;
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

impl Args {
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

    fn offer_flags(&self) -> OfferFlags {
        OfferFlags {
            allow_rtcp_fb: self.rtcp_fb,
            ltr: !self.no_ltr,
        }
    }
}

fn init_logging() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("jumpstarter_ios_display=info,idevice=warn,warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_target(false)
        .init();
}

/// Takes stdout for the frame channel: the descriptor is duplicated and fd 1 is pointed at stderr,
/// so a stray print anywhere in the process cannot corrupt a frame.
#[cfg(unix)]
fn take_frame_channel() -> std::io::Result<File> {
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
fn take_frame_channel() -> std::io::Result<File> {
    Err(std::io::Error::other("only unix hosts are supported"))
}

fn main() {
    let args = Args::parse();
    init_logging();

    let out = match take_frame_channel() {
        Ok(f) => f,
        Err(e) => {
            error!("cannot take over stdout: {e}");
            std::process::exit(EXIT_START_FAILED);
        }
    };

    let queue = Arc::new(OutQueue::new());
    let gone = Arc::new(Notify::new());
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    {
        let queue = queue.clone();
        let gone = gone.clone();
        std::thread::Builder::new()
            .name("frame-writer".into())
            .spawn(move || {
                if let Err(e) = run_writer(&queue, out) {
                    // Nobody reads the frames any more.
                    warn!("writing to stdout failed: {e}");
                    queue.close();
                    gone.notify_one();
                }
                let _ = done_tx.send(());
            })
            .expect("spawn the writer thread");
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build the tokio runtime");

    // The session lives out here, so that a panic inside `run` cannot take with it the means to
    // stop the stream on the device.
    let session = Arc::new(tokio::sync::Mutex::new(Session::default()));
    let result = catch_unwind(AssertUnwindSafe(|| {
        rt.block_on(run(args, queue.clone(), gone.clone(), session.clone()))
    }));
    let code = match result {
        Ok(code) => code,
        Err(_) => {
            error!("internal error (a panic); stopping the stream");
            queue.push_control(Frame::control(&Event::Error {
                message: "internal error".into(),
            }));
            let cleanup = catch_unwind(AssertUnwindSafe(|| {
                rt.block_on(async {
                    let _ = tokio::time::timeout(PANIC_CLEANUP, async {
                        session.lock().await.shutdown().await;
                    })
                    .await;
                })
            }));
            if cleanup.is_err() {
                error!("the cleanup panicked too");
            }
            EXIT_INTERNAL
        }
    };

    // Let the writer finish what is queued (the access units are stale now: only controls are
    // kept), but never wait long for a reader that does not read.
    queue.close_discarding_units();
    let _ = done_rx.recv_timeout(WRITER_GRACE);
    // The stdin reader may be blocked in a read that cannot be cancelled.
    rt.shutdown_background();
    std::process::exit(code);
}

/// Reports a failure on the frame channel and returns the exit status.
fn fail(queue: &OutQueue, code: i32, message: String) -> i32 {
    error!("{message}");
    queue.push_control(Frame::control(&Event::Error { message }));
    code
}

/// Waits for a `stop` (or for the command channel to close), ignoring other commands.
async fn wait_for_stop(commands: &mut mpsc::Receiver<Command>) {
    loop {
        match commands.recv().await {
            Some(Command::Stop) | None => return,
            // Anything else before the stream is up has nothing to act on.
            Some(_) => {}
        }
    }
}

async fn run(
    args: Args,
    queue: Arc<OutQueue>,
    gone: Arc<Notify>,
    session: Arc<tokio::sync::Mutex<Session>>,
) -> i32 {
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<Command>(64);
    tokio::spawn(supervised(
        read_commands(tokio::io::stdin(), cmd_tx.clone()),
        cmd_tx.clone(),
    ));
    tokio::spawn(stop_on_signals(cmd_tx));

    if let Some(bps) = args.max_bitrate {
        warn!("--max-bitrate {bps} ignored: the display offer has no bitrate ceiling to set");
    }
    let rtcp_interval = interval("--rtcp-interval-ms", args.rtcp_interval_ms);
    match rtcp_interval {
        Some(d) if d > Duration::from_secs(15) => {
            warn!("--rtcp-interval-ms is above 15 s: the device stops a stream without receiver reports after 20 s");
        }
        None => warn!("receiver reports are off: the device stops a stream without them after 20 s"),
        _ => {}
    }

    let opts = DeviceOptions {
        udid: args.udid.clone(),
        usbmuxd: args.usbmuxd.clone(),
        display_id: args.display_id,
        log_answer: args.log_answer,
        offer: args.offer_flags(),
    };

    let mut sess = session.lock().await;
    // Start, but a stop (a command, the end of stdin, a signal) ends it at once.
    let started = tokio::select! {
        r = sess.start(&opts) => Some(r),
        _ = wait_for_stop(&mut cmd_rx) => None,
    };
    match started {
        None => {
            info!("stop requested while starting");
            sess.shutdown().await;
            queue.push_control(Frame::control(&Event::Stopped));
            return 0;
        }
        Some(Err(e)) => {
            // Whatever was started on the device is stopped before the failure is reported.
            sess.shutdown().await;
            return fail(&queue, EXIT_START_FAILED, e);
        }
        Some(Ok(())) => {}
    }

    // Touch and keys, if asked for: the HID services on the same tunnel. They never stop the stream.
    let mut hid = Hid::new(args.hid_mapping(), queue.clone());
    let mut backend: Box<dyn HidBackend> = Box::new(NoHid);
    // What the device said about its display at start: the touch mapping needs the native orientation.
    let first_display = sess.take_first_display();
    if args.hid {
        let connected = tokio::select! {
            h = sess.connect_hid() => Some(h),
            _ = wait_for_stop(&mut cmd_rx) => None,
        };
        let Some(device_hid) = connected else {
            info!("stop requested while connecting HID");
            sess.shutdown().await;
            queue.push_control(Frame::control(&Event::Stopped));
            return 0;
        };
        let message = [device_hid.touch_error.clone(), device_hid.buttons_error.clone()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("; ");
        let ready = device_hid.ready();
        if ready {
            info!("HID ready{}", if message.is_empty() { String::new() } else { format!(" ({message})") });
        } else {
            warn!("HID not ready: {message}");
        }
        // The mapping in effect, from the display info that is in by now (without it, the identity).
        let native = first_display
            .as_ref()
            .and_then(|r| r.as_ref().ok())
            .and_then(|displays| displayinfo::primary(displays))
            .and_then(|d| d.native_orientation.as_deref())
            .and_then(displayinfo::orientation_degrees);
        let effective = hid.effective(native);
        info!(
            "HID mapping: {} (swap {}, flip x {}, flip y {})",
            effective.label, effective.axes.swap_xy, effective.axes.flip_x, effective.axes.flip_y
        );
        queue.push_control(Frame::control(&mapping_event(
            Some(ready),
            (!message.is_empty()).then_some(message),
            &effective,
        )));
        hid.announce(effective);
        // What the device's own HID listing says that does not fit: a warning, not an error.
        for warning in &device_hid.warnings {
            warn!("HID: {warning}");
            queue.push_control(Frame::control(&Event::Hid(HidEvent {
                warning: Some(warning.clone()),
                ..HidEvent::default()
            })));
        }
        backend = Box::new(device_hid);
    }

    // The engine's clocks start only now: connecting HID above can take a few seconds, during which
    // the stream's packets wait in the socket.
    let mut cfg = EngineConfig::new(sess.our_ssrc);
    cfg.rtcp_interval = rtcp_interval;
    cfg.keyframe_min_interval = interval("--keyframe-min-interval-ms", args.keyframe_min_interval_ms)
        .unwrap_or(Duration::ZERO);
    cfg.stats_interval = interval("--stats-interval-ms", args.stats_interval_ms);
    cfg.rtp_stall_keyframe = interval("--rtp-stall-keyframe-ms", args.rtp_stall_keyframe_ms);
    cfg.rtp_stall_timeout = interval("--rtp-stall-timeout-ms", args.rtp_stall_timeout_ms);
    cfg.visible_size = args.visible_size;
    cfg.device_hub_feedback = args.device_hub_feedback;
    cfg.device_ssrc = sess.info.device_ssrc;
    cfg.rtcp_port = Some(sess.info.source_port.unwrap_or(Session::FALLBACK_RTCP_PORT));
    cfg.fps = sess.info.fps.unwrap_or(engine::DEFAULT_FPS);
    if let Some(hz) = sess.info.clock_hz {
        cfg.clock_hz = hz;
        cfg.clock_source = ClockSource::Negotiated;
    }
    cfg.display_info = sess.display_source().is_some();

    let mut engine = Engine::new(cfg, queue.clone(), TokioClock.now());
    if let Some(first) = first_display {
        engine.on_display_info(first);
    }
    let pending = sess.take_pending_display();

    let exit = {
        let Some(video) = sess.video() else {
            return fail(&queue, EXIT_FAILED, "the video stream is gone".into());
        };
        let display = sess.display_source().map(|source| DisplayLink {
            source,
            poll_interval: interval("--display-poll-ms", args.display_poll_ms),
            pending,
        });
        run_session(
            video,
            &mut engine,
            &mut cmd_rx,
            &gone,
            &TokioClock,
            Links {
                display,
                hid: Some(HidLink {
                    hid: &mut hid,
                    backend: &mut *backend,
                }),
            },
        )
        .await
    };
    info!("done: {}", engine.summary());

    sess.shutdown().await;
    match exit {
        Exit::Stopped(StopCause::Command) => {
            queue.push_control(Frame::control(&Event::Stopped));
            0
        }
        // Nobody is reading: there is nobody to tell.
        Exit::Stopped(StopCause::ConsumerGone) => 0,
        Exit::Failed(message) => fail(&queue, EXIT_FAILED, message),
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn the_cli_defaults_are_the_contracts() {
        let a = Args::parse_from(["jumpstarter-ios-display"]);
        assert_eq!(a.udid, None);
        assert_eq!(a.usbmuxd, None);
        assert_eq!(a.display_id, 1);
        assert_eq!(a.rtcp_interval_ms, 5000);
        assert_eq!(a.keyframe_min_interval_ms, 2000);
        assert_eq!(a.stats_interval_ms, 5000);
        assert_eq!(a.rtp_stall_keyframe_ms, 5000);
        assert_eq!(a.rtp_stall_timeout_ms, 15000);
        assert_eq!(a.visible_size, None);
        assert_eq!(a.max_bitrate, None);
        // The offer is the stock one unless asked.
        assert_eq!(a.offer_flags(), OfferFlags::default());
        assert!(!a.device_hub_feedback);
        // HID is off, and its mapping is the automatic one.
        assert!(!a.hid);
        assert_eq!(a.hid_mapping(), HidMapping::default());
        assert_eq!(a.hid_mapping().space, HidSpace::Auto);
        assert_eq!(a.display_poll_ms, 1000);
    }

    #[test]
    fn the_cli_options_parse() {
        let a = Args::parse_from([
            "jumpstarter-ios-display",
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
        ]);
        assert_eq!(a.udid.as_deref(), Some("U"));
        assert_eq!(a.display_id, 2);
        assert_eq!(a.visible_size, Some((1206, 2622)));
        assert_eq!(interval("x", a.rtcp_interval_ms), None);
        assert_eq!(interval("x", a.rtp_stall_keyframe_ms), None);
        assert_eq!(interval("x", a.rtp_stall_timeout_ms), Some(Duration::from_secs(9)));
        assert_eq!(
            a.offer_flags(),
            OfferFlags {
                allow_rtcp_fb: true,
                ltr: false
            }
        );
        assert!(a.device_hub_feedback);
        let a = Args::parse_from([
            "jumpstarter-ios-display",
            "--hid",
            "--hid-space",
            "ui",
            "--hid-flip-x",
            "--hid-swap-xy",
            "--display-poll-ms",
            "0",
        ]);
        assert!(a.hid);
        assert_eq!(
            a.hid_mapping(),
            HidMapping { space: HidSpace::Ui, flip_x: true, flip_y: false, swap_xy: true }
        );
        assert_eq!(interval("x", a.display_poll_ms), None);
        assert!(Args::try_parse_from(["jumpstarter-ios-display", "--hid-space", "sideways"]).is_err());
        // Each offer flag on its own.
        let a = Args::parse_from(["jumpstarter-ios-display", "--rtcp-fb"]);
        assert_eq!(a.offer_flags(), OfferFlags { allow_rtcp_fb: true, ltr: true });
        let a = Args::parse_from(["jumpstarter-ios-display", "--no-ltr"]);
        assert_eq!(a.offer_flags(), OfferFlags { allow_rtcp_fb: false, ltr: false });
    }
}
