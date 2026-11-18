//! The stream, as a library: start it, read what it yields, send it input.
//!
//! [`Stream::start`] begins a session in the background (usbmuxd, the CoreDevice tunnel, the display
//! stream and, unless [`StreamOptions::control`] is off, the touch and key services) and returns at
//! once. From then on:
//!
//! * [`Stream::recv`] (or [`Stream::items`]) yields what the session produces, in order: [`Item::Unit`] is one
//!   Annex-B access unit (one picture, with the parameter sets in front of every key unit), [`Item::Event`] is a
//!   control [`Event`] (the picture's size and orientation as a `config`, `stats`, `hid`, and last an `error` or
//!   `stopped`). `recv` blocks; it returns `None` once the session is over and everything it produced was read.
//! * [`Input`] (from [`Stream::input`]; it is `Clone`) sends touch, keys, text, a keyframe request or a stop.
//! * [`Stream::wait`] gives the [`Outcome`] (and the exit status the `scrios` binary uses for it).
//!
//! The queue between the session and the reader is bounded. A reader that cannot keep up does not slow the
//! device down: the access units that are waiting are dropped all together and the next picture is asked to be a
//! keyframe (`stats` counts them as `dropped_units`), so a reader that is slow sees a gap, never a stale or
//! half-decodable run of units. Events are not dropped (periodic `stats` may be, when the reader is far behind).
//! Dropping the last handle to the stream (the [`Stream`] and its clones) stops it, as [`Stream::hang_up`] does.
//!
//! ```no_run
//! use scrios::stream::{Item, Stream, StreamOptions};
//!
//! let stream = Stream::start(StreamOptions { udid: Some("00008110-001234567890401E".into()), ..Default::default() })?;
//! let input = stream.input();
//! std::thread::spawn({
//!     let input = input.clone();
//!     move || {
//!         std::thread::sleep(std::time::Duration::from_secs(10));
//!         input.stop();
//!     }
//! });
//! for item in stream.items() {
//!     match item {
//!         Item::Unit(au) => println!("{} bytes at {} us, key: {}", au.data.len(), au.pts_us, au.key),
//!         Item::Event(event) => println!("{event:?}"),
//!     }
//! }
//! println!("exit status {}", stream.wait().exit_code());
//! # Ok::<(), std::io::Error>(())
//! ```

use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use tokio::sync::{Notify, mpsc};
use tracing::{debug, error, info, warn};

use crate::codec::CodecKind;
use crate::commands::{Command, Kb, Key, Press, Touch, TouchPhase};
use crate::device::{DeviceOptions, Session};
use crate::engine::{self, ClockSource, Engine, EngineConfig};
use crate::frame::{Event, Frame, HidEvent};
use crate::hid::{Hid, HidBackend, HidMapping, NoHid, mapping_event};
use crate::offer::{Codec, OfferFlags};
use crate::outqueue::OutQueue;
use crate::session::{Clock, DisplayLink, Exit, HidLink, Links, StopCause, TokioClock, run_session};
use crate::{displayinfo, probe};

/// The exit status of the `scrios` binary after a stream that could not be started.
pub const EXIT_START_FAILED: i32 = 2;
/// The exit status of the `scrios` binary after a stream that failed while running.
pub const EXIT_FAILED: i32 = 1;
/// The exit status of the `scrios` binary after an internal error (a panic).
pub const EXIT_INTERNAL: i32 = 101;

/// Commands waiting for the session at most; a sender waits (or, from an async task, is refused) beyond that.
const COMMAND_QUEUE: usize = 64;
/// How long the cleanup after a panic may take.
const PANIC_CLEANUP: Duration = Duration::from_secs(8);

/// What a stream is started with. [`Default`] is what the `scrios` binary does without options: the only device
/// usbmuxd lists, its built-in display, H.264, touch and keys on, the intervals of the protocol.
#[derive(Debug, Clone)]
pub struct StreamOptions {
    /// UDID of the device (default: the only device usbmuxd lists).
    pub udid: Option<String>,
    /// The usbmuxd address, a unix socket path or `host:port` (default: `$USBMUXD_SOCKET_ADDRESS`, else the
    /// system's socket).
    pub usbmuxd: Option<String>,
    /// The display to stream (1 is the built-in display).
    pub display_id: i64,
    /// The codec to ask for: H.264 (an offer that leaves the HEVC bank out; the default) or HEVC (the stock offer).
    pub codec: CodecKind,
    /// Offer `allowRTCPFB` 1 (the stock offer has 0).
    pub allow_rtcp_fb: bool,
    /// Offer long-term reference pictures (the stock offer does).
    pub ltr: bool,
    /// Log the device's answer, server status and media support info (diagnostics).
    pub log_answer: bool,
    /// Connect the device's touch and key services, so that [`Input`] can act on the device. If they cannot be
    /// connected the stream runs without and a `hid` event says why.
    pub control: bool,
    /// Where the digitizer's axes lie and how they are exchanged or mirrored.
    pub hid_mapping: HidMapping,
    /// Override of the picture's visible size (default: the display's size from the device, else the SPS).
    pub visible_size: Option<(u32, u32)>,
    /// Send an RTCP receiver report this often (`None`: never; the device stops a stream without one for 20 s).
    pub rtcp_interval: Option<Duration>,
    /// Never send two keyframe requests closer than this.
    pub keyframe_min_interval: Duration,
    /// Emit a `stats` event this often (`None`: never).
    pub stats_interval: Option<Duration>,
    /// After this long without RTP, ask for a keyframe (`None`: never).
    pub rtp_stall_keyframe: Option<Duration>,
    /// After this long without RTP, give up with an error (`None`: never).
    pub rtp_stall_timeout: Option<Duration>,
    /// Also send Device Hub's per-frame acknowledgements and RCTL reports (experimental).
    pub device_hub_feedback: bool,
    /// Ask the device for its display's size and orientation this often (`None`: only at start and when the SPS
    /// changes). A change makes a new `config` event.
    pub display_poll: Option<Duration>,
    /// Run a probe instead of a stream (a diagnostic, see [`Probe`]).
    pub probe: Option<Probe>,
}

impl Default for StreamOptions {
    fn default() -> Self {
        StreamOptions {
            udid: None,
            usbmuxd: None,
            display_id: 1,
            codec: CodecKind::H264,
            allow_rtcp_fb: false,
            ltr: true,
            log_answer: false,
            control: true,
            hid_mapping: HidMapping::default(),
            visible_size: None,
            rtcp_interval: Some(Duration::from_secs(5)),
            keyframe_min_interval: Duration::from_secs(2),
            stats_interval: Some(Duration::from_secs(5)),
            rtp_stall_keyframe: Some(Duration::from_secs(5)),
            rtp_stall_timeout: Some(Duration::from_secs(15)),
            device_hub_feedback: false,
            display_poll: Some(Duration::from_secs(1)),
            probe: None,
        }
    }
}

/// A probe: start the stream, count for `window` what arrives (RTP packets per payload type, the first payload
/// bytes, H.264 NAL types, the first SPS), log it, stop, and say by the [`Outcome`] whether it was H.264. No access
/// units are yielded; the events are `stopped` or `error`. Needs [`StreamOptions::codec`] to be H.264.
#[derive(Debug, Clone)]
pub struct Probe {
    pub window: Duration,
    /// Write every datagram received during the window, RTP and RTCP, to this file, each as a 4-byte big-endian
    /// length followed by its bytes.
    pub raw_dump: Option<PathBuf>,
}

impl StreamOptions {
    /// Options that do not go together.
    pub fn check(&self) -> Result<(), String> {
        match (&self.probe, self.codec) {
            (Some(_), CodecKind::Hevc) => Err("the probe is for the H.264 offer: ask for H.264, not HEVC".into()),
            _ => Ok(()),
        }
    }

    fn offer_flags(&self) -> OfferFlags {
        OfferFlags {
            allow_rtcp_fb: self.allow_rtcp_fb,
            ltr: self.ltr,
            codec: match self.codec {
                CodecKind::Hevc => Codec::Hevc,
                CodecKind::H264 => Codec::H264,
            },
        }
    }
}

/// One access unit: one picture, as Annex-B with 4-byte start codes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessUnit {
    /// The codec of the stream (the one in [`StreamOptions::codec`]).
    pub codec: CodecKind,
    /// Microseconds since the first access unit, from the RTP timestamp at the stream's clock rate.
    pub pts_us: u64,
    /// The unit holds a key picture (HEVC: an IRAP picture; H.264: an IDR slice): a decoder can start here.
    pub key: bool,
    /// The parameter sets (HEVC: VPS, SPS and PPS; H.264: SPS and PPS) are at the start of the unit. Every key unit
    /// has them.
    pub params: bool,
    pub data: Vec<u8>,
}

/// What a stream yields.
#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    Unit(AccessUnit),
    Event(Event),
}

/// How a stream ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// A clean stop: a stop was asked for (a command, end of input, a signal), the probe finished, or nobody was
    /// reading any more.
    Stopped,
    /// The stream could not be started (no device, no tunnel, options that do not go together, ...). The message
    /// was also yielded as an `error` event.
    StartFailed(String),
    /// The stream failed while running. The message was also yielded as an `error` event.
    Failed(String),
    /// An internal error (a panic).
    Panicked,
}

impl Outcome {
    /// The exit status of the `scrios` binary: 0, [`EXIT_START_FAILED`], [`EXIT_FAILED`] or [`EXIT_INTERNAL`].
    pub fn exit_code(&self) -> i32 {
        match self {
            Outcome::Stopped => 0,
            Outcome::StartFailed(_) => EXIT_START_FAILED,
            Outcome::Failed(_) => EXIT_FAILED,
            Outcome::Panicked => EXIT_INTERNAL,
        }
    }
}

/// A running stream. Cheap to clone: the clones are the same stream, so that one thread can read it while another
/// waits for its end. Dropping the last one stops the stream.
#[derive(Debug, Clone)]
pub struct Stream {
    shared: Arc<Shared>,
}

#[derive(Debug)]
struct Shared {
    queue: Arc<OutQueue>,
    gone: Arc<Notify>,
    input: mpsc::Sender<Command>,
    codec: CodecKind,
    runner: Mutex<Option<JoinHandle<Outcome>>>,
    outcome: OnceLock<Outcome>,
}

impl Shared {
    fn hang_up(&self) {
        self.queue.close();
        self.gone.notify_one();
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        // Nobody can read or wait any more: the session ends cleanly instead of running for nobody.
        self.hang_up();
    }
}

impl Stream {
    /// Starts the stream in the background and returns at once; connecting, starting the display stream and
    /// connecting touch and keys happen behind it (a start that fails is an `error` event and an
    /// [`Outcome::StartFailed`]). Fails only if the thread cannot be spawned.
    pub fn start(opts: StreamOptions) -> io::Result<Stream> {
        let queue = Arc::new(OutQueue::new());
        let gone = Arc::new(Notify::new());
        let (input, commands) = mpsc::channel::<Command>(COMMAND_QUEUE);
        let codec = opts.codec;
        let runner = {
            let (queue, gone) = (queue.clone(), gone.clone());
            std::thread::Builder::new()
                .name("scrios-stream".into())
                .spawn(move || run_thread(opts, queue, gone, commands))?
        };
        Ok(Stream {
            shared: Arc::new(Shared {
                queue,
                gone,
                input,
                codec,
                runner: Mutex::new(Some(runner)),
                outcome: OnceLock::new(),
            }),
        })
    }

    /// The next thing the stream produced; blocks until there is one. `None` once the stream is over (or hung up
    /// on) and everything it produced was read.
    pub fn recv(&self) -> Option<Item> {
        loop {
            let frame = self.shared.queue.pop()?;
            // What the reader is handed counts as written (the `stats` event's `access_units` and `bytes`).
            self.shared.queue.note_written(&frame);
            if let Some(item) = item_of(frame, self.shared.codec) {
                return Some(item);
            }
        }
    }

    /// What [`recv`](Self::recv) yields, as an iterator that ends when the stream does.
    pub fn items(&self) -> impl Iterator<Item = Item> + '_ {
        std::iter::from_fn(|| self.recv())
    }

    /// A handle for touch, keys, text, keyframe requests and stop.
    pub fn input(&self) -> Input {
        Input { tx: self.shared.input.clone() }
    }

    /// Tells the stream that nobody is reading any more: it stops cleanly (it ends with [`Outcome::Stopped`]; the
    /// events still queued are not lost, but no `stopped` event is added). What a failed write to the reader's
    /// output means.
    pub fn hang_up(&self) {
        self.shared.hang_up();
    }

    /// Waits for the stream to end and says how it did. Can be called again, from any clone, and gives the same
    /// answer. The end of the stream is also the end of [`recv`](Self::recv), once its last events were read.
    pub fn wait(&self) -> Outcome {
        let mut runner = self.shared.runner.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(handle) = runner.take() {
            let outcome = handle.join().unwrap_or(Outcome::Panicked);
            let _ = self.shared.outcome.set(outcome);
        }
        self.shared.outcome.get().cloned().unwrap_or(Outcome::Panicked)
    }
}

/// The way in: sends commands to the stream. Cloning is cheap.
///
/// Touch and keys act on the device only if [`StreamOptions::control`] was on and the device's services could be
/// connected; otherwise a `hid` event with `ok: false` says so for each. Every method returns whether the command
/// was queued: `false` means the stream is over (or, from inside an async task, that the command queue is full).
/// From a thread that is not an async task, sending waits while the queue (64 commands) is full.
#[derive(Debug, Clone)]
pub struct Input {
    tx: mpsc::Sender<Command>,
}

impl Input {
    /// Sends any command.
    pub fn send(&self, command: Command) -> bool {
        if tokio::runtime::Handle::try_current().is_ok() {
            // Blocking inside an async task is not allowed.
            return self.tx.try_send(command).is_ok();
        }
        self.tx.blocking_send(command).is_ok()
    }

    /// The command channel itself, for async code that wants to `send(..).await` (the stdin reader of the binary
    /// does).
    pub fn sender(&self) -> mpsc::Sender<Command> {
        self.tx.clone()
    }

    /// A finger: `x` and `y` are pixels of the upright picture (the `config` event's `width` and `height`, swapped
    /// for a rotation of 90 or 270). Only finger 0 is handled.
    pub fn touch(&self, phase: TouchPhase, x: f64, y: f64, id: u32) -> bool {
        self.send(Command::Touch(Touch { phase, x, y, id }))
    }

    /// A hardware button, pressed briefly or held.
    pub fn key(&self, key: Key, press: Press) -> bool {
        self.send(Command::Key(key, press))
    }

    /// A key of the HID keyboard page goes down (by its usage: `0x04` is A) and stays down until
    /// [`key_up`](Self::key_up).
    pub fn key_down(&self, usage: u8) -> bool {
        self.send(Command::Kb(Kb::Down(usage)))
    }

    pub fn key_up(&self, usage: u8) -> bool {
        self.send(Command::Kb(Kb::Up(usage)))
    }

    /// Types a string through the US layout (at most [`MAX_TYPE_CHARS`](crate::commands::MAX_TYPE_CHARS)
    /// characters waiting at a time; characters that are not in the layout are skipped).
    pub fn type_text(&self, text: impl Into<String>) -> bool {
        self.send(Command::Kb(Kb::Type(text.into())))
    }

    /// Asks the device for a fresh keyframe (rate-limited by [`StreamOptions::keyframe_min_interval`]).
    pub fn keyframe(&self) -> bool {
        self.send(Command::Keyframe)
    }

    /// Stops the stream cleanly: a finger still down is lifted, the device's stream is stopped, a `stopped` event
    /// is yielded. It can take a few seconds.
    pub fn stop(&self) -> bool {
        self.send(Command::Stop)
    }
}

/// A frame the session queued, as what the stream yields. `None` for a control frame that cannot be read back.
fn item_of(frame: Frame, codec: CodecKind) -> Option<Item> {
    if !frame.is_control_frame() {
        return Some(Item::Unit(AccessUnit {
            codec,
            pts_us: frame.pts_us,
            key: frame.flags & crate::frame::FLAG_KEY != 0,
            params: frame.flags & crate::frame::FLAG_PARAMS != 0,
            data: frame.payload,
        }));
    }
    match serde_json::from_slice::<Event>(&frame.payload) {
        Ok(event) => Some(Item::Event(event)),
        Err(e) => {
            warn!("a control event could not be read back, left out: {e}");
            None
        }
    }
}

// ---- the session thread -----------------------------------------------------------------------

/// Runs the session on its own runtime and returns how it ended. Whatever happens, the queue is closed at the end,
/// so a reader of the stream sees it end.
fn run_thread(
    opts: StreamOptions,
    queue: Arc<OutQueue>,
    gone: Arc<Notify>,
    commands: mpsc::Receiver<Command>,
) -> Outcome {
    let rt = match tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            let outcome = start_failed(&queue, format!("cannot build the async runtime: {e}"));
            queue.close_discarding_units();
            return outcome;
        }
    };

    // The session lives out here, so that a panic inside `run` cannot take with it the means to
    // stop the stream on the device.
    // Shared only between this thread and the runtime's tasks it blocks on, never sent to another thread.
    #[allow(clippy::arc_with_non_send_sync)]
    let session = Arc::new(tokio::sync::Mutex::new(Session::default()));
    let result = catch_unwind(AssertUnwindSafe(|| {
        rt.block_on(run(opts, queue.clone(), gone.clone(), commands, session.clone()))
    }));
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(_) => {
            error!("internal error (a panic); stopping the stream");
            queue.push_control(Frame::control(&Event::Error { message: "internal error".into() }));
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
            Outcome::Panicked
        }
    };

    // What is queued is still read (the access units are stale now: only events are kept).
    queue.close_discarding_units();
    // A blocked read of the command source cannot be cancelled.
    rt.shutdown_background();
    outcome
}

/// Reports a failure to start, on the queue and in the log.
fn start_failed(queue: &OutQueue, message: String) -> Outcome {
    report(queue, &message);
    Outcome::StartFailed(message)
}

/// Reports a failure while running, on the queue and in the log.
fn failed(queue: &OutQueue, message: String) -> Outcome {
    report(queue, &message);
    Outcome::Failed(message)
}

fn report(queue: &OutQueue, message: &str) {
    error!("{message}");
    queue.push_control(Frame::control(&Event::Error { message: message.to_string() }));
}

/// The codec probe: count what the device sends for `window`, log it, stop the stream, and say by the outcome
/// whether it was H.264. No access units are queued, only `stopped` or `error`.
async fn run_probe(
    sess: &mut Session,
    window: Duration,
    cmd_rx: &mut mpsc::Receiver<Command>,
    gone: &Notify,
    queue: &OutQueue,
    raw: Option<&mut dyn std::io::Write>,
) -> Outcome {
    info!(
        "probing for {} s: payload type 123 (H.264) is the answer wanted, 100 (HEVC) is what the stock offer gets",
        window.as_secs()
    );
    info!("stream parameters: our SSRC {:#x}, {:?}", sess.our_ssrc, sess.info);
    let collected = match sess.video() {
        Some(video) => Ok(probe::collect(video, window, cmd_rx, gone, raw).await),
        None => Err("the video stream is gone".to_string()),
    };
    sess.shutdown().await;
    let (report, outcome) = match collected {
        Ok(c) => c,
        Err(why) => return failed(queue, why),
    };
    for line in report.lines() {
        info!("probe: {line}");
    }
    match outcome {
        probe::Outcome::Failed(why) => failed(queue, why),
        probe::Outcome::ConsumerGone => Outcome::Stopped,
        probe::Outcome::Stopped => {
            info!("probe: stopped before the window ended");
            queue.push_control(Frame::control(&Event::Stopped));
            Outcome::Stopped
        }
        probe::Outcome::Finished => match report.verdict() {
            Ok(summary) => {
                info!("probe: {summary}");
                queue.push_control(Frame::control(&Event::Stopped));
                Outcome::Stopped
            }
            Err(why) => failed(queue, why),
        },
    }
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
    opts: StreamOptions,
    queue: Arc<OutQueue>,
    gone: Arc<Notify>,
    mut cmd_rx: mpsc::Receiver<Command>,
    session: Arc<tokio::sync::Mutex<Session>>,
) -> Outcome {
    match opts.rtcp_interval {
        Some(d) if d > Duration::from_secs(15) => {
            warn!("the receiver report interval is above 15 s: the device stops a stream without reports after 20 s");
        }
        None => warn!("receiver reports are off: the device stops a stream without them after 20 s"),
        _ => {}
    }

    if let Err(why) = opts.check() {
        return start_failed(&queue, why);
    }
    // The raw dump file is opened first: a path that cannot be written should not cost a run on the device.
    let mut raw_dump = match opts.probe.as_ref().and_then(|p| p.raw_dump.as_ref()) {
        Some(path) => match std::fs::File::create(path) {
            Ok(f) => Some(std::io::BufWriter::new(f)),
            Err(e) => {
                return start_failed(
                    &queue,
                    format!("cannot write the probe's raw datagram file {}: {e}", path.display()),
                );
            }
        },
        None => None,
    };

    let dev = DeviceOptions {
        udid: opts.udid.clone(),
        usbmuxd: opts.usbmuxd.clone(),
        display_id: opts.display_id,
        // A probe is about reading the answer.
        log_answer: opts.log_answer || opts.probe.is_some(),
        offer: opts.offer_flags(),
    };

    let mut sess = session.lock().await;
    // Start, but a stop (a command, the end of the input, a signal) ends it at once.
    let started = tokio::select! {
        r = sess.start(&dev) => Some(r),
        _ = wait_for_stop(&mut cmd_rx) => None,
    };
    match started {
        None => {
            info!("stop requested while starting");
            sess.shutdown().await;
            queue.push_control(Frame::control(&Event::Stopped));
            return Outcome::Stopped;
        }
        Some(Err(e)) => {
            // Whatever was started on the device is stopped before the failure is reported.
            sess.shutdown().await;
            return start_failed(&queue, e);
        }
        Some(Ok(())) => {}
    }

    if let Some(probe) = &opts.probe {
        let outcome = run_probe(
            &mut sess,
            probe.window,
            &mut cmd_rx,
            &gone,
            &queue,
            raw_dump.as_mut().map(|w| w as &mut dyn std::io::Write),
        )
        .await;
        if let (Some(w), Some(path)) = (raw_dump.as_mut(), probe.raw_dump.as_ref()) {
            use std::io::Write as _;
            match w.flush() {
                Ok(()) => info!("probe: the raw datagrams are in {}", path.display()),
                Err(e) => warn!("probe: could not finish {}: {e}", path.display()),
            }
        }
        return outcome;
    }

    // Touch and keys, if asked for: the HID services on the same tunnel. They never stop the stream.
    let mut hid = Hid::new(opts.hid_mapping, queue.clone());
    let mut backend: Box<dyn HidBackend> = Box::new(NoHid);
    // What the device said about its display at start: the touch mapping needs the native orientation.
    let first_display = sess.take_first_display();
    if opts.control {
        let connected = tokio::select! {
            h = sess.connect_hid() => Some(h),
            _ = wait_for_stop(&mut cmd_rx) => None,
        };
        let Some(device_hid) = connected else {
            info!("stop requested while connecting HID");
            sess.shutdown().await;
            queue.push_control(Frame::control(&Event::Stopped));
            return Outcome::Stopped;
        };
        let message =
            [device_hid.touch_error.clone(), device_hid.buttons_error.clone(), device_hid.keyboard_error.clone()]
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
        let mut event = mapping_event(Some(ready), (!message.is_empty()).then_some(message), &effective);
        if let Event::Hid(h) = &mut event {
            h.keyboard = Some(device_hid.keyboard_ready());
        }
        queue.push_control(Frame::control(&event));
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
    } else {
        debug!("touch and keys are off");
    }

    // The engine's clocks start only now: connecting HID above can take a few seconds, during which
    // the stream's packets wait in the socket.
    let mut cfg = EngineConfig::new(sess.our_ssrc).for_codec(opts.codec);
    cfg.rtcp_interval = opts.rtcp_interval;
    cfg.keyframe_min_interval = opts.keyframe_min_interval;
    cfg.stats_interval = opts.stats_interval;
    cfg.rtp_stall_keyframe = opts.rtp_stall_keyframe;
    cfg.rtp_stall_timeout = opts.rtp_stall_timeout;
    cfg.visible_size = opts.visible_size;
    cfg.device_hub_feedback = opts.device_hub_feedback;
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
            return failed(&queue, "the video stream is gone".into());
        };
        let display =
            sess.display_source().map(|source| DisplayLink { source, poll_interval: opts.display_poll, pending });
        run_session(
            video,
            &mut engine,
            &mut cmd_rx,
            &gone,
            &TokioClock,
            Links { display, hid: Some(HidLink { hid: &mut hid, backend: &mut *backend }) },
        )
        .await
    };
    info!("done: {}", engine.summary());

    sess.shutdown().await;
    match exit {
        Exit::Stopped(StopCause::Command) => {
            queue.push_control(Frame::control(&Event::Stopped));
            Outcome::Stopped
        }
        // Nobody is reading: there is nobody to tell.
        Exit::Stopped(StopCause::ConsumerGone) => Outcome::Stopped,
        Exit::Failed(message) => failed(&queue, message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{FLAG_KEY, FLAG_PARAMS, Stats};

    #[test]
    fn the_defaults_are_what_the_binary_does_without_options() {
        let o = StreamOptions::default();
        assert_eq!(o.display_id, 1);
        assert_eq!(o.codec, CodecKind::H264);
        assert!(o.control);
        assert!(o.probe.is_none());
        assert_eq!(o.rtcp_interval, Some(Duration::from_secs(5)));
        assert_eq!(o.keyframe_min_interval, Duration::from_secs(2));
        assert_eq!(o.stats_interval, Some(Duration::from_secs(5)));
        assert_eq!(o.rtp_stall_keyframe, Some(Duration::from_secs(5)));
        assert_eq!(o.rtp_stall_timeout, Some(Duration::from_secs(15)));
        assert_eq!(o.display_poll, Some(Duration::from_secs(1)));
        assert_eq!(o.hid_mapping, HidMapping::default());
        // The offer: the stock one, less the HEVC bank.
        assert_eq!(o.offer_flags(), OfferFlags { codec: Codec::H264, ..OfferFlags::default() });
        assert_eq!(o.check(), Ok(()));
    }

    #[test]
    fn the_offer_follows_the_options() {
        let o = StreamOptions { codec: CodecKind::Hevc, ..StreamOptions::default() };
        assert_eq!(o.offer_flags(), OfferFlags::default(), "HEVC is the stock offer itself");
        let o = StreamOptions { allow_rtcp_fb: true, ltr: false, ..StreamOptions::default() };
        assert_eq!(o.offer_flags(), OfferFlags { allow_rtcp_fb: true, ltr: false, codec: Codec::H264 });
    }

    #[test]
    fn a_probe_needs_the_h264_offer() {
        let probe = Some(Probe { window: Duration::from_secs(5), raw_dump: None });
        let o = StreamOptions { probe: probe.clone(), ..StreamOptions::default() };
        assert_eq!(o.check(), Ok(()));
        let o = StreamOptions { probe, codec: CodecKind::Hevc, ..StreamOptions::default() };
        assert!(o.check().unwrap_err().contains("H.264"));
    }

    #[test]
    fn the_outcomes_are_the_documented_exit_statuses() {
        assert_eq!(Outcome::Stopped.exit_code(), 0);
        assert_eq!(Outcome::Failed("x".into()).exit_code(), 1);
        assert_eq!(Outcome::StartFailed("x".into()).exit_code(), 2);
        assert_eq!(Outcome::Panicked.exit_code(), 101);
    }

    #[test]
    fn frames_become_items_with_their_flags() {
        let unit = Frame::unit(1500, true, true, vec![0, 0, 0, 1, 0x67, 1]);
        assert_eq!(unit.flags, FLAG_KEY | FLAG_PARAMS);
        assert_eq!(
            item_of(unit, CodecKind::H264),
            Some(Item::Unit(AccessUnit {
                codec: CodecKind::H264,
                pts_us: 1500,
                key: true,
                params: true,
                data: vec![0, 0, 0, 1, 0x67, 1],
            }))
        );
        let delta = item_of(Frame::unit(3000, false, false, vec![7]), CodecKind::Hevc);
        assert_eq!(
            delta,
            Some(Item::Unit(AccessUnit {
                codec: CodecKind::Hevc,
                pts_us: 3000,
                key: false,
                params: false,
                data: vec![7]
            }))
        );
    }

    #[test]
    fn control_frames_become_events() {
        let stats = Event::Stats(Stats { packets: 3, ..Stats::default() });
        assert_eq!(item_of(Frame::control(&stats), CodecKind::H264), Some(Item::Event(stats)));
        assert_eq!(item_of(Frame::control(&Event::Stopped), CodecKind::H264), Some(Item::Event(Event::Stopped)));
        // A control frame that is not an event of the protocol is left out.
        let bad = Frame { pts_us: 0, flags: crate::frame::FLAG_CONTROL, payload: br#"{"event":"nope"}"#.to_vec() };
        assert_eq!(item_of(bad, CodecKind::H264), None);
    }

    /// A stream whose session is not run: the queue and the channels are the real ones.
    fn rig() -> (Stream, Arc<OutQueue>, mpsc::Receiver<Command>) {
        let queue = Arc::new(OutQueue::new());
        let (input, commands) = mpsc::channel(COMMAND_QUEUE);
        let runner = std::thread::spawn(|| Outcome::Stopped);
        let shared = Shared {
            queue: queue.clone(),
            gone: Arc::new(Notify::new()),
            input,
            codec: CodecKind::H264,
            runner: Mutex::new(Some(runner)),
            outcome: OnceLock::new(),
        };
        (Stream { shared: Arc::new(shared) }, queue, commands)
    }

    #[test]
    fn recv_yields_in_order_counts_what_it_hands_over_and_ends_with_the_queue() {
        let (stream, queue, _commands) = rig();
        queue.push_control(Frame::control(&Event::Stopped));
        assert_eq!(queue.push_unit(Frame::unit(10, true, true, vec![1, 2])), crate::outqueue::Pushed::Queued);
        queue.push_control(Frame::control(&Event::Error { message: "x".into() }));
        queue.close();
        let items: Vec<Item> = stream.items().collect();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0], Item::Event(Event::Stopped));
        assert!(matches!(&items[1], Item::Unit(au) if au.pts_us == 10 && au.key && au.data == [1, 2]));
        assert_eq!(items[2], Item::Event(Event::Error { message: "x".into() }));
        assert_eq!(queue.written().units, 1);
        assert_eq!(queue.written().bytes, 2);
        assert_eq!(queue.written().keyframes, 1);
        assert_eq!(stream.recv(), None);
    }

    #[test]
    fn wait_gives_the_same_outcome_every_time() {
        let (stream, _queue, _commands) = rig();
        let other = stream.clone();
        assert_eq!(stream.wait(), Outcome::Stopped);
        assert_eq!(other.wait(), Outcome::Stopped);
        assert_eq!(stream.wait(), Outcome::Stopped);
    }

    #[test]
    fn input_methods_send_the_commands_of_the_protocol() {
        let (stream, _queue, mut commands) = rig();
        let input = stream.input();
        assert!(input.touch(TouchPhase::Down, 10.0, 20.5, 0));
        assert!(input.key(Key::Home, Press::Long));
        assert!(input.key_down(0x04));
        assert!(input.key_up(0x04));
        assert!(input.type_text("hi"));
        assert!(input.keyframe());
        assert!(input.stop());
        let got: Vec<Command> = std::iter::from_fn(|| commands.try_recv().ok()).collect();
        assert_eq!(
            got,
            vec![
                Command::Touch(Touch { phase: TouchPhase::Down, x: 10.0, y: 20.5, id: 0 }),
                Command::Key(Key::Home, Press::Long),
                Command::Kb(Kb::Down(4)),
                Command::Kb(Kb::Up(4)),
                Command::Kb(Kb::Type("hi".into())),
                Command::Keyframe,
                Command::Stop,
            ]
        );
        // Once the stream's side is gone, nothing is queued.
        drop(commands);
        assert!(!input.stop());
    }

    #[tokio::test]
    async fn input_does_not_block_inside_an_async_task() {
        let (stream, _queue, commands) = rig();
        let input = stream.input();
        // The queue holds 64 commands; the 65th is refused here instead of waiting.
        let queued = (0..COMMAND_QUEUE + 1).filter(|_| input.keyframe()).count();
        assert_eq!(queued, COMMAND_QUEUE);
        drop(commands);
    }

    #[test]
    fn dropping_the_last_handle_hangs_up() {
        let (stream, queue, _commands) = rig();
        let other = stream.clone();
        drop(stream);
        assert_eq!(queue.push_unit(Frame::unit(1, false, false, vec![1])), crate::outqueue::Pushed::Queued);
        drop(other);
        assert_eq!(queue.push_unit(Frame::unit(2, false, false, vec![1])), crate::outqueue::Pushed::Closed);
    }

    /// A path for a socket that does not exist yet.
    fn socket_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("scrios-{name}-{}.sock", std::process::id()))
    }

    #[test]
    fn a_start_that_fails_is_an_error_event_then_the_end_and_an_outcome() {
        let path = socket_path("missing");
        let stream =
            Stream::start(StreamOptions { usbmuxd: Some(path.display().to_string()), ..StreamOptions::default() })
                .unwrap();
        let items: Vec<Item> = stream.items().collect();
        match items.as_slice() {
            [Item::Event(Event::Error { message })] => {
                assert!(message.contains("cannot connect to usbmuxd"), "{message}")
            }
            other => panic!("{other:?}"),
        }
        let outcome = stream.wait();
        assert!(matches!(&outcome, Outcome::StartFailed(m) if m.contains("usbmuxd")), "{outcome:?}");
        assert_eq!(outcome.exit_code(), EXIT_START_FAILED);
        // Once over, the input has nobody to talk to.
        assert!(!stream.input().stop());
    }

    #[test]
    fn options_that_do_not_go_together_fail_the_start_before_any_device_is_touched() {
        let stream = Stream::start(StreamOptions {
            codec: CodecKind::Hevc,
            probe: Some(Probe { window: Duration::from_secs(1), raw_dump: None }),
            usbmuxd: Some(socket_path("never-used").display().to_string()),
            ..StreamOptions::default()
        })
        .unwrap();
        let items: Vec<Item> = stream.items().collect();
        assert!(matches!(items.as_slice(), [Item::Event(Event::Error { message })] if message.contains("H.264")));
        assert!(matches!(stream.wait(), Outcome::StartFailed(m) if m.contains("H.264")));
    }

    #[test]
    fn a_probe_that_cannot_write_its_dump_fails_the_start() {
        let stream = Stream::start(StreamOptions {
            probe: Some(Probe { window: Duration::from_secs(1), raw_dump: Some("/nonexistent-dir/x.raw".into()) }),
            usbmuxd: Some(socket_path("never-used").display().to_string()),
            ..StreamOptions::default()
        })
        .unwrap();
        let items: Vec<Item> = stream.items().collect();
        assert!(
            matches!(items.as_slice(), [Item::Event(Event::Error { message })] if message.contains("cannot write"))
        );
        assert_eq!(stream.wait().exit_code(), EXIT_START_FAILED);
    }

    #[test]
    fn a_stop_while_connecting_ends_the_start_cleanly() {
        // A usbmuxd that accepts and never answers: the start waits for it.
        let path = socket_path("silent");
        let _ = std::fs::remove_file(&path);
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let stream =
            Stream::start(StreamOptions { usbmuxd: Some(path.display().to_string()), ..StreamOptions::default() })
                .unwrap();
        let (_held, _) = listener.accept().unwrap();
        assert!(stream.input().stop());
        let items: Vec<Item> = stream.items().collect();
        assert_eq!(items, vec![Item::Event(Event::Stopped)]);
        assert_eq!(stream.wait(), Outcome::Stopped);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn hang_up_closes_the_queue_but_keeps_what_is_queued() {
        let (stream, queue, _commands) = rig();
        queue.push_control(Frame::control(&Event::Stopped));
        stream.hang_up();
        assert_eq!(stream.recv(), Some(Item::Event(Event::Stopped)));
        assert_eq!(stream.recv(), None);
    }
}
