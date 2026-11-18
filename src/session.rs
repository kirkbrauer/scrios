//! The async loop that drives an [`Engine`] from a datagram socket, the command channel and the
//! clock. The socket and the clock are traits, so the loop runs against fakes in the tests.

use std::convert::Infallible;
use std::fmt;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use tokio::sync::{Notify, mpsc};
use tracing::{debug, warn};

use crate::commands::Command;
use crate::displayinfo::{DisplayInfoSource, DisplayQuery};
use crate::engine::Engine;
use crate::hid::{Action, ActionQueue, Hid, HidBackend};

/// One datagram from the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Datagram {
    pub src_port: u16,
    pub data: Vec<u8>,
}

/// The UDP socket the device streams to, on the tunnel.
pub trait DatagramSocket {
    /// The next datagram. Must be cancel-safe (the loop drops the future when another event wins).
    fn recv(&self) -> impl Future<Output = io::Result<Datagram>> + Send;
    /// Sends `data` to the device's `port`.
    fn send_to(&self, port: u16, data: Vec<u8>) -> impl Future<Output = io::Result<()>> + Send;
}

/// One RTCP datagram must go out within this, or it is given up on: a stalled tunnel must not
/// keep a `stop` waiting.
pub const RTCP_SEND_TIMEOUT: Duration = Duration::from_millis(500);

pub trait Clock {
    fn now(&self) -> Instant;
}

/// The runtime's clock (follows tokio's paused time in tests).
#[derive(Debug, Clone, Copy, Default)]
pub struct TokioClock;

impl Clock for TokioClock {
    fn now(&self) -> Instant {
        tokio::time::Instant::now().into_std()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopCause {
    /// A `stop` command, end of stdin, or a signal.
    Command,
    /// Nobody reads stdout any more.
    ConsumerGone,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    Stopped(StopCause),
    /// The stream is unusable; the message is for the `error` event.
    Failed(String),
}

/// How display info reaches the loop: queries are made one at a time, in the background of the
/// loop, whenever the engine wants one and every `poll_interval` (the orientation changes without
/// the stream saying so).
pub struct DisplayLink<'a> {
    pub source: &'a dyn DisplayInfoSource,
    pub poll_interval: Option<Duration>,
    /// A query already in flight, such as the one made at start that was slow to answer.
    pub pending: Option<DisplayQuery>,
}

// By hand: the source and the query in flight are `dyn` values, which have no `Debug`.
impl fmt::Debug for DisplayLink<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DisplayLink")
            .field("poll_interval", &self.poll_interval)
            .field("pending", &self.pending.as_ref().map(|_| "a query in flight"))
            .finish_non_exhaustive()
    }
}

/// Touch and keys: the controller and where its actions go.
pub struct HidLink<'a> {
    pub hid: &'a mut Hid,
    pub backend: &'a mut dyn HidBackend,
}

// By hand: the backend is a `dyn` value, which has no `Debug`.
impl fmt::Debug for HidLink<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HidLink").field("hid", &self.hid).finish_non_exhaustive()
    }
}

/// What the loop is connected to besides the socket.
#[derive(Debug, Default)]
pub struct Links<'a> {
    pub display: Option<DisplayLink<'a>>,
    pub hid: Option<HidLink<'a>>,
}

/// At the end, how long the actions still waiting for the HID service (the lift of a finger, the release of
/// keys, taking the keyboard down) are waited for in all. The service answers in a few milliseconds when it
/// is well; this only bounds the wait on one that is not (each action has its own, shorter, limit too).
pub const HID_SHUTDOWN_BUDGET: Duration = Duration::from_secs(5);

/// What the loop and the HID pump share. They run in one task (the backend is not `Send`), so this is
/// only ever touched between awaits; the lock is there for the type system.
#[derive(Default)]
struct HidShared {
    state: Mutex<HidState>,
    /// The pump has something to do.
    work: Notify,
    /// The pump finished an action: the loop may have a failure to read, or the end to see.
    done: Notify,
}

#[derive(Default)]
struct HidState {
    queue: ActionQueue,
    /// An action is being performed (it is not in the queue any more).
    busy: bool,
    /// What the backend refused or did not answer in time, to be told to the controller.
    failures: Vec<(Action, String)>,
}

impl HidShared {
    fn state(&self) -> MutexGuard<'_, HidState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues an action for the pump; false if it was refused (see [`ActionQueue::push`]).
    fn push(&self, action: Action) -> bool {
        let queued = self.state().queue.push(action);
        self.work.notify_one();
        queued
    }

    fn take_failures(&self) -> Vec<(Action, String)> {
        std::mem::take(&mut self.state().failures)
    }

    /// Nothing waits and nothing is being performed.
    fn idle(&self) -> bool {
        let state = self.state();
        state.queue.is_empty() && !state.busy
    }

    fn next_action(&self) -> Option<Action> {
        let mut state = self.state();
        let action = state.queue.pop();
        state.busy = action.is_some();
        action
    }

    fn finished(&self, action: Action, failure: Option<String>) {
        let mut state = self.state();
        state.busy = false;
        if let Some(failure) = failure {
            state.failures.push((action, failure));
        }
        drop(state);
        self.done.notify_one();
    }
}

/// Performs the queued actions one at a time, in order, each within its own time limit. It runs beside
/// the loop (see [`run_session`]), so a service that does not answer holds up only the actions behind it,
/// never the video, the commands or the clock. It does not end.
async fn hid_pump(backend: &mut dyn HidBackend, shared: &HidShared) -> Infallible {
    loop {
        let Some(action) = shared.next_action() else {
            shared.work.notified().await;
            continue;
        };
        let limit = action.timeout();
        let failure = match tokio::time::timeout(limit, backend.perform(action)).await {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(e),
            Err(_) => Some(format!("the HID service did not answer in {} ms", limit.as_millis())),
        };
        shared.finished(action, failure);
    }
}

/// The controller's side of the HID link, as the loop has it.
struct HidSide<'a> {
    hid: &'a mut Hid,
    shared: &'a HidShared,
}

impl HidSide<'_> {
    /// Tells the controller what failed and hands what it has queued to the pump. A failed keyboard action
    /// makes the controller queue a release, which goes on in the next round (a few rounds at most).
    fn sync<C: Clock>(&mut self, clock: &C) {
        for _ in 0..4 {
            let failures = self.shared.take_failures();
            for (action, failure) in &failures {
                self.hid.on_action_failed(clock.now(), failure, *action);
            }
            let actions = self.hid.take_actions();
            if failures.is_empty() && actions.is_empty() {
                return;
            }
            for action in actions {
                if !self.shared.push(action) {
                    let why = "the HID service is not keeping up: too many actions are waiting, one was dropped";
                    self.hid.on_action_failed(clock.now(), why, action);
                }
            }
        }
    }

    /// The end of the session: lets go of everything and waits (for [`HID_SHUTDOWN_BUDGET`] at most) until
    /// the actions that do so, and those still waiting before them, have been performed.
    async fn finish<C: Clock>(&mut self, clock: &C) {
        self.hid.shutdown();
        // The finger is lifted next: where it was on its way there is of no interest.
        self.shared.state().queue.drop_moves();
        let give_up = tokio::time::Instant::now() + HID_SHUTDOWN_BUDGET;
        loop {
            self.sync(clock);
            if self.shared.idle() {
                return;
            }
            tokio::select! {
                _ = self.shared.done.notified() => {}
                _ = tokio::time::sleep_until(give_up) => {
                    warn!("the HID service did not finish within {} s of the end: giving up", HID_SHUTDOWN_BUDGET.as_secs());
                    return;
                }
            }
        }
    }
}

/// Runs until a stop, the consumer going away, or a failure. Before it returns, a finger still on
/// the screen is lifted and held keys are released.
///
/// What the controller asks of the HID service is performed beside the loop, by a pump in this same
/// task (see `hid_pump` and [`ActionQueue`]): the loop never awaits the service, so a service that
/// has stalled holds up neither the video nor the commands.
pub async fn run_session<S: DatagramSocket, C: Clock>(
    sock: &S,
    engine: &mut Engine,
    commands: &mut mpsc::Receiver<Command>,
    consumer_gone: &Notify,
    clock: &C,
    links: Links<'_>,
) -> Exit {
    let Links { display, hid } = links;
    let shared = HidShared::default();
    let (side, backend) = match hid {
        Some(HidLink { hid, backend }) => (Some(HidSide { hid, shared: &shared }), Some(backend)),
        None => (None, None),
    };
    let mut pump: Pin<Box<dyn Future<Output = Infallible> + '_>> = match backend {
        Some(backend) => Box::pin(hid_pump(backend, &shared)),
        None => Box::pin(std::future::pending()),
    };
    let session = session_loop(sock, engine, commands, consumer_gone, clock, display, side);
    tokio::select! {
        exit = session => exit,
        never = &mut pump => match never {},
    }
}

async fn session_loop<S: DatagramSocket, C: Clock>(
    sock: &S,
    engine: &mut Engine,
    commands: &mut mpsc::Receiver<Command>,
    consumer_gone: &Notify,
    clock: &C,
    display: Option<DisplayLink<'_>>,
    mut hid: Option<HidSide<'_>>,
) -> Exit {
    let (source, poll_every, mut query) = match display {
        Some(d) => (Some(d.source), d.poll_interval, d.pending),
        None => (None, None, None),
    };
    let mut query_wanted = false;
    let mut next_poll = poll_every.map(|d| clock.now() + d);
    let mut send_errors = 0u64;

    let exit = loop {
        query_wanted |= engine.take_display_query();
        if query_wanted
            && query.is_none()
            && let Some(source) = source
        {
            query = Some(source.query());
            query_wanted = false;
        }
        let now = clock.now();
        let deadline = [engine.next_deadline(), next_poll, hid.as_ref().and_then(|h| h.hid.next_deadline())]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(now + Duration::from_secs(3600));
        let sleep = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline));
        // The pump finished an action: a failure may be waiting for the controller.
        let pump_done = async {
            match hid.as_ref() {
                Some(h) => h.shared.done.notified().await,
                None => std::future::pending().await,
            }
        };

        tokio::select! {
            cmd = commands.recv() => match cmd {
                Some(Command::Keyframe) => engine.on_keyframe_command(clock.now()),
                Some(Command::Stop) | None => break Exit::Stopped(StopCause::Command),
                Some(Command::Touch(t)) => match hid.as_mut() {
                    Some(h) => h.hid.on_touch(clock.now(), t, engine.view()),
                    None => debug!("touch ignored: no HID link"),
                },
                Some(Command::Key(k, press)) => match hid.as_mut() {
                    Some(h) => h.hid.on_key(clock.now(), k, press),
                    None => debug!("key ignored: no HID link"),
                },
                Some(Command::Kb(kb)) => match hid.as_mut() {
                    Some(h) => h.hid.on_kb(clock.now(), kb),
                    None => debug!("kb ignored: no HID link"),
                },
                Some(Command::Invalid(why)) => match hid.as_mut() {
                    Some(h) => h.hid.on_bad_command(clock.now(), &why),
                    None => debug!("bad command ignored: {why}"),
                },
            },
            dg = sock.recv() => match dg {
                Ok(dg) => engine.on_datagram(clock.now(), dg.src_port, &dg.data),
                Err(e) => break Exit::Failed(format!("the display stream's socket failed: {e}")),
            },
            _ = sleep => {
                let now = clock.now();
                engine.on_tick(now);
                if next_poll.is_some_and(|t| now >= t) {
                    query_wanted = true;
                    next_poll = poll_every.map(|d| now + d);
                }
                if let Some(h) = hid.as_mut() {
                    h.hid.on_tick(now);
                }
            },
            reply = async { query.as_mut().expect("guarded").await }, if query.is_some() => {
                query = None;
                engine.on_display_info(reply);
            },
            _ = pump_done => {},
            _ = consumer_gone.notified() => break Exit::Stopped(StopCause::ConsumerGone),
        }

        if let Some(msg) = engine.take_fatal() {
            break Exit::Failed(msg);
        }
        for pkt in engine.take_rtcp() {
            let Some(port) = engine.rtcp_port() else { continue };
            let failure = match tokio::time::timeout(RTCP_SEND_TIMEOUT, sock.send_to(port, pkt)).await {
                Ok(Ok(())) => continue,
                Ok(Err(e)) => e.to_string(),
                Err(_) => format!("no progress in {} ms", RTCP_SEND_TIMEOUT.as_millis()),
            };
            send_errors += 1;
            if send_errors.is_power_of_two() {
                warn!("sending RTCP to the device failed ({send_errors} so far): {failure}");
            }
        }
        if let Some(h) = hid.as_mut() {
            h.sync(clock);
        }
    };

    if let Some(h) = hid.as_mut() {
        h.finish(clock).await;
    }
    exit
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        reason = "the tests number packets, bytes and times with small loop counters, so a cast does not truncate or lose precision"
    )]
    use std::sync::{Arc, Mutex};

    use tokio::sync::Mutex as AsyncMutex;

    use super::*;
    use crate::depack::testutil::*;
    use crate::engine::EngineConfig;
    use crate::frame::{FLAG_KEY, Frame};
    use crate::hid::NoHid;
    use crate::outqueue::OutQueue;
    use crate::sps::testutil::SpsSpec;

    const DEV: u32 = 0xD00D_0001;
    const OUR: u32 = 7;
    const PORT: u16 = 50001;

    #[derive(Clone)]
    #[allow(clippy::type_complexity)]
    struct FakeSocket {
        rx: Arc<AsyncMutex<mpsc::UnboundedReceiver<io::Result<Datagram>>>>,
        sent: Arc<Mutex<Vec<(u16, Vec<u8>)>>>,
        /// Sends never complete (a tunnel that has stalled).
        stall_send: Arc<std::sync::atomic::AtomicBool>,
    }

    fn fake() -> (FakeSocket, mpsc::UnboundedSender<io::Result<Datagram>>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (FakeSocket { rx: Arc::new(AsyncMutex::new(rx)), sent: Arc::default(), stall_send: Arc::default() }, tx)
    }

    impl DatagramSocket for FakeSocket {
        async fn recv(&self) -> io::Result<Datagram> {
            match self.rx.lock().await.recv().await {
                Some(r) => r,
                // Nothing more will come: park (the test ends the session by other means).
                None => std::future::pending().await,
            }
        }

        async fn send_to(&self, port: u16, data: Vec<u8>) -> io::Result<()> {
            if self.stall_send.load(std::sync::atomic::Ordering::Relaxed) {
                return std::future::pending().await;
            }
            self.sent.lock().unwrap().push((port, data));
            Ok(())
        }
    }

    fn dg(seq: u16, ts: u32, marker: bool, payload: &[u8]) -> io::Result<Datagram> {
        Ok(Datagram { src_port: PORT, data: rtp_bytes(seq, ts, marker, DEV, payload) })
    }

    /// The first picture: sets, then the IDR in three fragments (seq 1..=4).
    fn send_first_idr(tx: &mpsc::UnboundedSender<io::Result<Datagram>>) {
        let sps = SpsSpec::main_l150(1216, 2656).nal();
        tx.send(dg(1, 1000, false, &ap(&[&VPS, &sps, &PPS]))).unwrap();
        let frags = fu(&idr(&[5; 30]), 12);
        for (i, f) in frags.iter().enumerate() {
            tx.send(dg(2 + i as u16, 1000, i == frags.len() - 1, f)).unwrap();
        }
    }

    struct Rig {
        engine: Engine,
        queue: Arc<OutQueue>,
        commands: mpsc::Sender<Command>,
        rx: mpsc::Receiver<Command>,
        gone: Arc<Notify>,
    }

    fn rig(f: impl FnOnce(&mut EngineConfig)) -> Rig {
        let mut cfg = EngineConfig::new(OUR);
        cfg.rtcp_port = Some(PORT);
        f(&mut cfg);
        let queue = Arc::new(OutQueue::new());
        let (commands, rx) = mpsc::channel(16);
        Rig {
            engine: Engine::new(cfg, queue.clone(), TokioClock.now()),
            queue,
            commands,
            rx,
            gone: Arc::new(Notify::new()),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn datagrams_become_frames_and_stop_ends_the_session() {
        let (sock, tx) = fake();
        let mut r = rig(|_| {});
        send_first_idr(&tx);
        let cmds = r.commands.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            cmds.send(Command::Stop).await.unwrap();
        });
        let exit = run_session(&sock, &mut r.engine, &mut r.rx, &r.gone, &TokioClock, Links::default()).await;
        assert_eq!(exit, Exit::Stopped(StopCause::Command));
        let f: Vec<Frame> = r.queue.drain();
        assert_eq!(f.len(), 2);
        assert!(f[0].is_control());
        assert_eq!(f[1].flags & FLAG_KEY, FLAG_KEY);
    }

    #[tokio::test(start_paused = true)]
    async fn the_command_channel_closing_is_a_stop() {
        let (sock, _tx) = fake();
        let r = rig(|_| {});
        let Rig { mut engine, mut rx, gone, commands, .. } = r;
        drop(commands);
        let exit = run_session(&sock, &mut engine, &mut rx, &gone, &TokioClock, Links::default()).await;
        assert_eq!(exit, Exit::Stopped(StopCause::Command));
    }

    #[tokio::test(start_paused = true)]
    async fn receiver_reports_and_stats_follow_the_clock() {
        let (sock, tx) = fake();
        let mut r = rig(|c| {
            c.rtcp_interval = Some(Duration::from_secs(5));
            c.stats_interval = Some(Duration::from_secs(5));
            c.rtp_stall_keyframe = None;
            c.rtp_stall_timeout = None;
        });
        send_first_idr(&tx);
        let cmds = r.commands.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(12)).await;
            cmds.send(Command::Stop).await.unwrap();
        });
        let exit = run_session(&sock, &mut r.engine, &mut r.rx, &r.gone, &TokioClock, Links::default()).await;
        assert_eq!(exit, Exit::Stopped(StopCause::Command));

        // Two receiver reports (5 s and 10 s), to the device's RTP source port, from our SSRC.
        let sent = sock.sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 2);
        for (port, bytes) in &sent {
            assert_eq!(*port, PORT);
            assert_eq!(&bytes[..2], &[0x81, 201]);
            assert_eq!(&bytes[4..8], &OUR.to_be_bytes());
        }
        // Two stats events among the frames.
        let stats =
            r.queue.drain().iter().filter(|f| f.is_control() && f.payload.starts_with(br#"{"event":"stats""#)).count();
        assert_eq!(stats, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_keyframe_command_sends_the_request_and_a_second_one_waits_for_the_window() {
        let (sock, tx) = fake();
        let mut r = rig(|c| {
            c.rtcp_interval = None;
            c.stats_interval = None;
            c.keyframe_min_interval = Duration::from_secs(2);
            c.rtp_stall_keyframe = None;
            c.rtp_stall_timeout = None;
        });
        send_first_idr(&tx);
        let cmds = r.commands.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cmds.send(Command::Keyframe).await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            cmds.send(Command::Keyframe).await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            cmds.send(Command::Keyframe).await.unwrap();
            tokio::time::sleep(Duration::from_secs(1)).await;
            // At 1.3 s only the first went out.
            cmds.send(Command::Stop).await.unwrap();
        });
        run_session(&sock, &mut r.engine, &mut r.rx, &r.gone, &TokioClock, Links::default()).await;
        assert_eq!(sock.sent.lock().unwrap().len(), 1);
        assert_eq!(r.engine.stats().plis, 1);

        // Now let the window run out: the held request goes out by itself.
        let cmds = r.commands.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            cmds.send(Command::Stop).await.unwrap();
        });
        run_session(&sock, &mut r.engine, &mut r.rx, &r.gone, &TokioClock, Links::default()).await;
        assert_eq!(sock.sent.lock().unwrap().len(), 2);
        assert_eq!(r.engine.stats().plis, 2);
        // Both are the keyframe-request form (RR, SDES, PLI, FIR), to the device's port.
        for (port, bytes) in sock.sent.lock().unwrap().iter() {
            assert_eq!(*port, PORT);
            assert_eq!(bytes[1], 201);
            assert!(bytes.windows(2).any(|w| w == [0x81, 206]), "PLI present");
            assert!(bytes.windows(2).any(|w| w == [0x84, 206]), "FIR present");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_send_cannot_block_a_stop() {
        let (sock, tx) = fake();
        let mut r = rig(|c| {
            c.rtcp_interval = None;
            c.stats_interval = None;
            c.rtp_stall_keyframe = None;
            c.rtp_stall_timeout = None;
        });
        send_first_idr(&tx);
        sock.stall_send.store(true, std::sync::atomic::Ordering::Relaxed);
        let cmds = r.commands.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cmds.send(Command::Keyframe).await.unwrap(); // its PLI never leaves
            tokio::time::sleep(Duration::from_millis(100)).await;
            cmds.send(Command::Stop).await.unwrap();
        });
        let exit = tokio::time::timeout(
            Duration::from_secs(60),
            run_session(&sock, &mut r.engine, &mut r.rx, &r.gone, &TokioClock, Links::default()),
        )
        .await
        .expect("the session ended instead of waiting for the stalled send");
        assert_eq!(exit, Exit::Stopped(StopCause::Command));
        assert!(sock.sent.lock().unwrap().is_empty());
    }

    // ---- display info -----------------------------------------------------------------------

    use std::collections::VecDeque;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::commands::{Key, Press, Touch, TouchPhase};
    use crate::displayinfo::DisplayInfo;
    use crate::hid::{Action, HidMapping};

    fn info(w: u32, h: u32, current: &str) -> DisplayInfo {
        DisplayInfo {
            display_id: Some(1),
            name: "LCD".into(),
            primary: true,
            external: false,
            native_size: (w, h),
            point_scale: Some(2.0),
            current_orientation: Some(current.into()),
            native_orientation: Some("rot0".into()),
        }
    }

    /// Answers queries from a list (the last answer repeats), each after `delay`.
    #[allow(clippy::type_complexity)]
    struct FakeDisplay {
        replies: Arc<Mutex<VecDeque<Result<Vec<DisplayInfo>, String>>>>,
        calls: Arc<AtomicUsize>,
        delay: Duration,
    }

    impl FakeDisplay {
        fn new(replies: Vec<Result<Vec<DisplayInfo>, String>>, delay: Duration) -> Self {
            FakeDisplay { replies: Arc::new(Mutex::new(replies.into())), calls: Arc::default(), delay }
        }
    }

    impl DisplayInfoSource for FakeDisplay {
        fn query(&self) -> DisplayQuery {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let replies = self.replies.clone();
            let delay = self.delay;
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                let mut q = replies.lock().unwrap();
                if q.len() > 1 {
                    q.pop_front().unwrap()
                } else {
                    q.front().cloned().unwrap_or_else(|| Err("no reply".into()))
                }
            })
        }
    }

    /// The `config` events among the frames, as JSON.
    fn configs(frames: &[Frame]) -> Vec<serde_json::Value> {
        frames
            .iter()
            .filter(|f| f.is_control())
            .map(|f| serde_json::from_slice::<serde_json::Value>(&f.payload).unwrap())
            .filter(|v| v["event"] == "config")
            .collect()
    }

    fn sizes(configs: &[serde_json::Value]) -> Vec<(u64, u64)> {
        configs.iter().map(|c| (c["width"].as_u64().unwrap(), c["height"].as_u64().unwrap())).collect()
    }

    fn display_rig() -> Rig {
        rig(|c| {
            c.display_info = true;
            c.rtp_stall_keyframe = None;
            c.rtp_stall_timeout = None;
        })
    }

    async fn stop_after(r: &Rig, ms: u64) {
        let cmds = r.commands.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            cmds.send(Command::Stop).await.unwrap();
        });
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_first_display_answer_adds_a_second_config() {
        let (sock, tx) = fake();
        let mut r = display_rig();
        let src = FakeDisplay::new(vec![Ok(vec![info(1206, 2622, "rot0")])], Duration::from_secs(4));
        let pending = src.query(); // made at start, still waiting
        send_first_idr(&tx);
        stop_after(&r, 5000).await;
        let links = Links {
            display: Some(DisplayLink { source: &src, poll_interval: None, pending: Some(pending) }),
            hid: None,
        };
        run_session(&sock, &mut r.engine, &mut r.rx, &r.gone, &TokioClock, links).await;
        let c = configs(&r.queue.drain());
        // The stream did not wait: first the SPS size, then, when the answer came, the display's.
        assert_eq!(sizes(&c), vec![(1216, 2656), (1206, 2622)]);
        assert_eq!(c[1]["rotation"], 0);
        assert_eq!(c[1]["point_scale"], 2.0);
        assert_eq!(c[1]["current_orientation"], "rot0");
        assert_eq!(c[1]["coded_width"], 1216, "the coded size is the SPS's");
        assert_eq!(src.calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn the_orientation_is_polled_and_each_real_change_makes_one_config() {
        let (sock, tx) = fake();
        let mut r = display_rig();
        let portrait = Ok(vec![info(1206, 2622, "rot0")]);
        let landscape = Ok(vec![info(2622, 1206, "rot90")]);
        let src =
            FakeDisplay::new(vec![portrait.clone(), landscape.clone(), landscape, portrait], Duration::from_millis(10));
        send_first_idr(&tx);
        stop_after(&r, 5500).await;
        let links = Links {
            display: Some(DisplayLink { source: &src, poll_interval: Some(Duration::from_secs(1)), pending: None }),
            hid: None,
        };
        run_session(&sock, &mut r.engine, &mut r.rx, &r.gone, &TokioClock, links).await;
        let c = configs(&r.queue.drain());
        // The SPS size, then portrait, landscape, portrait: the repeated answers (the 3rd, 5th poll)
        // made no event.
        assert_eq!(sizes(&c), vec![(1216, 2656), (1206, 2622), (2622, 1206), (1206, 2622)]);
        assert_eq!(
            c.iter().map(|c| c["rotation"].as_u64()).collect::<Vec<_>>(),
            vec![None, Some(0), Some(90), Some(0)]
        );
        assert_eq!(src.calls.load(Ordering::Relaxed), 5, "a poll a second for 5.5 s");
    }

    #[tokio::test(start_paused = true)]
    async fn an_sps_change_asks_for_the_display_at_once_not_at_the_next_poll() {
        let (sock, tx) = fake();
        let mut r = display_rig();
        let src = FakeDisplay::new(vec![Ok(vec![info(2622, 1206, "rot90")])], Duration::from_millis(50));
        send_first_idr(&tx);
        let rotated = SpsSpec::main_l150(2656, 1216).nal();
        tx.send(dg(5, 2000, false, &ap(&[&VPS, &rotated, &PPS]))).unwrap();
        tx.send(dg(6, 2000, true, &idr(&[6; 8]))).unwrap();
        stop_after(&r, 500).await;
        let links =
            Links { display: Some(DisplayLink { source: &src, poll_interval: None, pending: None }), hid: None };
        run_session(&sock, &mut r.engine, &mut r.rx, &r.gone, &TokioClock, links).await;
        let c = configs(&r.queue.drain());
        assert_eq!(sizes(&c), vec![(1216, 2656), (2656, 1216), (2622, 1206)]);
        assert_eq!(src.calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failing_display_query_changes_nothing() {
        let (sock, tx) = fake();
        let mut r = display_rig();
        let src = FakeDisplay::new(vec![Err("the service is gone".into())], Duration::from_millis(10));
        send_first_idr(&tx);
        stop_after(&r, 3500).await;
        let links = Links {
            display: Some(DisplayLink { source: &src, poll_interval: Some(Duration::from_secs(1)), pending: None }),
            hid: None,
        };
        run_session(&sock, &mut r.engine, &mut r.rx, &r.gone, &TokioClock, links).await;
        assert_eq!(sizes(&configs(&r.queue.drain())), vec![(1216, 2656)]);
    }

    // ---- touch and keys ---------------------------------------------------------------------

    /// Records what it is asked to do.
    struct FakeHid {
        done: Arc<Mutex<Vec<Action>>>,
        stall: bool,
    }

    impl HidBackend for FakeHid {
        fn perform(&mut self, action: Action) -> Pin<Box<dyn Future<Output = Result<(), String>> + '_>> {
            let done = self.done.clone();
            let stall = self.stall;
            Box::pin(async move {
                if stall {
                    return std::future::pending().await;
                }
                done.lock().unwrap().push(action);
                Ok(())
            })
        }
    }

    fn touch(phase: TouchPhase, x: f64, y: f64) -> Command {
        Command::Touch(Touch { phase, x, y, id: 0 })
    }

    async fn send_later(r: &Rig, at_ms: u64, cmd: Command) {
        let cmds = r.commands.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(at_ms)).await;
            cmds.send(cmd).await.unwrap();
        });
    }

    fn hid_rig() -> Rig {
        rig(|c| {
            c.rtp_stall_keyframe = None;
            c.rtp_stall_timeout = None;
            c.no_data_timeout = Duration::from_secs(3600);
            c.rtcp_interval = None;
            c.stats_interval = None;
        })
    }

    #[tokio::test(start_paused = true)]
    async fn touches_and_keys_go_through_the_loop_and_a_finger_is_lifted_at_stop() {
        let (sock, tx) = fake();
        let mut r = hid_rig();
        send_first_idr(&tx); // config 1216x2656, no display info: rotation 0
        let done = Arc::new(Mutex::new(Vec::new()));
        let mut backend = FakeHid { done: done.clone(), stall: false };
        let mut hid = Hid::new(HidMapping::default(), r.queue.clone());
        send_later(&r, 100, touch(TouchPhase::Down, 607.5, 1327.5)).await;
        send_later(&r, 150, Command::Key(Key::Home, Press::Short)).await;
        send_later(&r, 250, Command::Key(Key::VolumeUp, Press::Long)).await;
        send_later(&r, 200, touch(TouchPhase::Move, 1216.0, 0.0)).await;
        send_later(&r, 300, Command::Stop).await; // the finger is still down
        let links = Links { display: None, hid: Some(HidLink { hid: &mut hid, backend: &mut backend }) };
        let exit = run_session(&sock, &mut r.engine, &mut r.rx, &r.gone, &TokioClock, links).await;
        assert_eq!(exit, Exit::Stopped(StopCause::Command));
        assert_eq!(
            *done.lock().unwrap(),
            vec![
                Action::Touch { down: true, x: 32768, y: 32768 },
                Action::Button { page: 0x0C, code: 0x40, down: true },
                Action::Touch { down: true, x: 65535, y: 0 },
                Action::Button { page: 0x0C, code: 0x40, down: false }, // 80 ms after the press
                Action::Button { page: 0x0C, code: 0xE9, down: true },  // the long volume press starts
                Action::Touch { down: false, x: 65535, y: 0 },          // the stop lifts the finger ...
                Action::Button { page: 0x0C, code: 0xE9, down: false }, // ... and lets go of the key
                Action::KeyboardRemove,                                 // and takes the keyboard down
            ]
        );
    }

    /// Records when each action was performed (on the paused clock).
    struct TimedHid {
        done: Arc<Mutex<Vec<(Action, tokio::time::Instant)>>>,
    }

    impl HidBackend for TimedHid {
        fn perform(&mut self, action: Action) -> Pin<Box<dyn Future<Output = Result<(), String>> + '_>> {
            let done = self.done.clone();
            Box::pin(async move {
                done.lock().unwrap().push((action, tokio::time::Instant::now()));
                Ok(())
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn short_and_long_presses_are_held_for_their_times_through_the_loop() {
        let (sock, tx) = fake();
        let mut r = hid_rig();
        send_first_idr(&tx);
        let done = Arc::new(Mutex::new(Vec::new()));
        let mut backend = TimedHid { done: done.clone() };
        let mut hid = Hid::new(HidMapping::default(), r.queue.clone());
        let t0 = tokio::time::Instant::now();
        send_later(&r, 100, Command::Key(Key::Home, Press::Long)).await; // 600 ms
        send_later(&r, 100, Command::Key(Key::Siri, Press::Short)).await; // 1200 ms
        send_later(&r, 100, Command::Key(Key::Power, Press::Long)).await; // 1200 ms
        send_later(&r, 100, Command::Key(Key::Mute, Press::Short)).await; // 80 ms
        send_later(&r, 4000, Command::Stop).await;
        let links = Links { display: None, hid: Some(HidLink { hid: &mut hid, backend: &mut backend }) };
        run_session(&sock, &mut r.engine, &mut r.rx, &r.gone, &TokioClock, links).await;
        let ups: Vec<(u64, u64)> = done
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(a, at)| match a {
                Action::Button { code, down: false, .. } => Some((*code, (*at - t0).as_millis() as u64)),
                _ => None,
            })
            .collect();
        // Each button is released at its press (100 ms) plus its hold time, in that order.
        assert_eq!(ups, vec![(0xE2, 180), (0x40, 700), (0xCF, 1300), (0x30, 1300)]);
    }

    #[tokio::test(start_paused = true)]
    async fn the_app_switcher_goes_through_the_loop_on_schedule_and_does_not_hold_it_up() {
        let (sock, tx) = fake();
        let mut r = hid_rig();
        send_first_idr(&tx);
        let done = Arc::new(Mutex::new(Vec::new()));
        let mut backend = TimedHid { done: done.clone() };
        let mut hid = Hid::new(HidMapping::default(), r.queue.clone());
        let t0 = tokio::time::Instant::now();
        send_later(&r, 100, Command::Key(Key::AppSwitcher, Press::Short)).await;
        // A touch in the gap (the first press is up at 180, the second goes down at 300) is performed when it comes.
        send_later(&r, 230, touch(TouchPhase::Down, 607.5, 1327.5)).await;
        send_later(&r, 1000, Command::Stop).await;
        let links = Links { display: None, hid: Some(HidLink { hid: &mut hid, backend: &mut backend }) };
        let exit = run_session(&sock, &mut r.engine, &mut r.rx, &r.gone, &TokioClock, links).await;
        assert_eq!(exit, Exit::Stopped(StopCause::Command));
        let done = done.lock().unwrap();
        let home = |down| Action::Button { page: 0x0C, code: 0x40, down };
        let timed: Vec<(Action, u64)> = done.iter().map(|(a, t)| (*a, (*t - t0).as_millis() as u64)).collect();
        assert_eq!(
            &timed[..5],
            &[
                (home(true), 100),
                (home(false), 180),
                (Action::Touch { down: true, x: 32768, y: 32768 }, 230),
                (home(true), 300),
                (home(false), 380),
            ]
        );
        // Then the stop, at 1000: the finger lifted and the keyboard taken down; no button after 380.
        assert!(timed[5..].iter().all(|(a, t)| *t == 1000 && !matches!(a, Action::Button { .. })), "{timed:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_in_the_middle_of_an_app_switcher_releases_the_button_first() {
        let (sock, tx) = fake();
        let mut r = hid_rig();
        send_first_idr(&tx);
        let done = Arc::new(Mutex::new(Vec::new()));
        let mut backend = TimedHid { done: done.clone() };
        let mut hid = Hid::new(HidMapping::default(), r.queue.clone());
        let t0 = tokio::time::Instant::now();
        send_later(&r, 100, Command::Key(Key::AppSwitcher, Press::Long)).await; // long is ignored
        send_later(&r, 130, Command::Stop).await; // the first press is still down (80 ms: until 180)
        let links = Links { display: None, hid: Some(HidLink { hid: &mut hid, backend: &mut backend }) };
        let exit = run_session(&sock, &mut r.engine, &mut r.rx, &r.gone, &TokioClock, links).await;
        assert_eq!(exit, Exit::Stopped(StopCause::Command));
        let timed: Vec<(Action, u64)> =
            done.lock().unwrap().iter().map(|(a, t)| (*a, (*t - t0).as_millis() as u64)).collect();
        let home = |down| Action::Button { page: 0x0C, code: 0x40, down };
        assert_eq!(timed, vec![(home(true), 100), (home(false), 130), (Action::KeyboardRemove, 130)]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_finger_down_when_stdin_ends_is_lifted() {
        // End of input is a Stop through the same channel.
        let (sock, tx) = fake();
        let r = hid_rig();
        send_first_idr(&tx);
        let done = Arc::new(Mutex::new(Vec::new()));
        let mut backend = FakeHid { done: done.clone(), stall: false };
        let mut hid = Hid::new(HidMapping::default(), r.queue.clone());
        send_later(&r, 100, touch(TouchPhase::Down, 0.0, 0.0)).await;
        let Rig { mut engine, mut rx, gone, commands, .. } = r;
        // Close the command channel instead of sending a stop (its sender is dropped: end of input).
        let closer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            drop(commands);
        });
        let links = Links { display: None, hid: Some(HidLink { hid: &mut hid, backend: &mut backend }) };
        let exit = run_session(&sock, &mut engine, &mut rx, &gone, &TokioClock, links).await;
        closer.await.unwrap();
        assert_eq!(exit, Exit::Stopped(StopCause::Command));
        let done = done.lock().unwrap();
        assert_eq!(done.len(), 3);
        assert_eq!(done[1], Action::Touch { down: false, x: 0, y: 0 });
        assert_eq!(done[2], Action::KeyboardRemove);
    }

    // ---- the keyboard through the loop ------------------------------------------------------

    use crate::commands::Kb;

    #[tokio::test(start_paused = true)]
    async fn a_long_type_goes_on_in_the_background_and_a_stop_aborts_it() {
        let (sock, tx) = fake();
        let mut r = hid_rig();
        send_first_idr(&tx);
        let done = Arc::new(Mutex::new(Vec::new()));
        let mut backend = TimedHid { done: done.clone() };
        let mut hid = Hid::new(HidMapping::default(), r.queue.clone());
        let t0 = tokio::time::Instant::now();
        send_later(&r, 100, Command::Kb(Kb::Type("a".repeat(200)))).await;
        // A touch in the middle of the typing is performed when it arrives, not after the string.
        send_later(&r, 130, touch(TouchPhase::Down, 607.5, 1327.5)).await;
        send_later(&r, 186, Command::Stop).await;
        let links = Links { display: None, hid: Some(HidLink { hid: &mut hid, backend: &mut backend }) };
        let exit = run_session(&sock, &mut r.engine, &mut r.rx, &r.gone, &TokioClock, links).await;
        assert_eq!(exit, Exit::Stopped(StopCause::Command));

        let done = done.lock().unwrap();
        let at = |a: &Action| done.iter().find(|(x, _)| x == a).map(|(_, t)| (*t - t0).as_millis() as u64);
        assert_eq!(at(&Action::Touch { down: true, x: 32768, y: 32768 }), Some(130), "not held up by the typing");
        // Characters typed every 12 ms from 100 ms, up to the stop at 186 ms: 100, 112, ... 184.
        let key_downs: Vec<u64> = done
            .iter()
            .filter(|(a, _)| *a == Action::Key { usage: 0x04, down: true })
            .map(|(_, t)| (*t - t0).as_millis() as u64)
            .collect();
        assert_eq!(key_downs, vec![100, 112, 124, 136, 148, 160, 172, 184]);
        // Then the stop: the key that was down (184), the finger and the keyboard all let go, and nothing is typed after.
        let tail: Vec<Action> = done.iter().rev().take(4).rev().map(|(a, _)| *a).collect();
        assert_eq!(
            tail,
            vec![
                Action::KeyboardRelease,
                Action::Touch { down: false, x: 32768, y: 32768 },
                Action::KeyboardRemove,
            ][..]
                .iter()
                .copied()
                .fold(vec![Action::Key { usage: 0x04, down: true }], |mut v, a| {
                    v.push(a);
                    v
                })
        );
        assert!(done.iter().all(|(a, t)| !matches!(a, Action::Key { .. }) || (*t - t0).as_millis() <= 186));
    }

    /// Fails every key press, records the rest.
    struct BrokenKeyboard {
        done: Arc<Mutex<Vec<Action>>>,
    }

    impl HidBackend for BrokenKeyboard {
        fn perform(&mut self, action: Action) -> Pin<Box<dyn Future<Output = Result<(), String>> + '_>> {
            let done = self.done.clone();
            Box::pin(async move {
                if matches!(action, Action::Key { down: true, .. }) {
                    return Err("keyboard: mainKeyboard transport is unavailable".to_string());
                }
                done.lock().unwrap().push(action);
                Ok(())
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_key_press_is_reported_and_everything_is_released() {
        let (sock, tx) = fake();
        let mut r = hid_rig();
        send_first_idr(&tx);
        let done = Arc::new(Mutex::new(Vec::new()));
        let mut backend = BrokenKeyboard { done: done.clone() };
        let mut hid = Hid::new(HidMapping::default(), r.queue.clone());
        send_later(&r, 100, Command::Kb(Kb::Down(0xE1))).await;
        send_later(&r, 200, Command::Stop).await;
        let links = Links { display: None, hid: Some(HidLink { hid: &mut hid, backend: &mut backend }) };
        run_session(&sock, &mut r.engine, &mut r.rx, &r.gone, &TokioClock, links).await;
        // The failure released the keyboard at once; at the stop there was nothing left to release.
        assert_eq!(*done.lock().unwrap(), vec![Action::KeyboardRelease, Action::KeyboardRemove]);
        let failure = r
            .queue
            .drain()
            .iter()
            .filter(|f| f.is_control())
            .map(|f| serde_json::from_slice::<serde_json::Value>(&f.payload).unwrap())
            .find(|v| v["event"] == "hid")
            .expect("a hid failure was reported");
        assert_eq!(failure["ok"], false);
        assert!(failure["message"].as_str().unwrap().contains("transport"));
    }

    #[tokio::test(start_paused = true)]
    async fn keys_held_when_the_viewer_goes_away_are_released_and_a_long_hold_is_cut_at_30_seconds() {
        let (sock, tx) = fake();
        let mut r = hid_rig();
        send_first_idr(&tx);
        let done = Arc::new(Mutex::new(Vec::new()));
        let mut backend = TimedHid { done: done.clone() };
        let mut hid = Hid::new(HidMapping::default(), r.queue.clone());
        let t0 = tokio::time::Instant::now();
        send_later(&r, 100, Command::Kb(Kb::Down(0xE1))).await;
        send_later(&r, 100, Command::Kb(Kb::Down(0x04))).await;
        send_later(&r, 45_000, Command::Stop).await;
        let links = Links { display: None, hid: Some(HidLink { hid: &mut hid, backend: &mut backend }) };
        run_session(&sock, &mut r.engine, &mut r.rx, &r.gone, &TokioClock, links).await;
        let done = done.lock().unwrap();
        let release =
            done.iter().find(|(a, _)| *a == Action::KeyboardRelease).map(|(_, t)| (*t - t0).as_millis() as u64);
        assert_eq!(release, Some(30_100), "30 s after the last keyboard command");
        // At the stop nothing was left down: only the keyboard is taken down.
        assert_eq!(done.last().map(|(a, _)| *a), Some(Action::KeyboardRemove));
        assert_eq!(done.iter().filter(|(a, _)| *a == Action::KeyboardRelease).count(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_hid_service_cannot_block_a_stop_and_is_reported() {
        let (sock, tx) = fake();
        let mut r = hid_rig();
        send_first_idr(&tx);
        let mut backend = FakeHid { done: Arc::default(), stall: true };
        let mut hid = Hid::new(HidMapping::default(), r.queue.clone());
        send_later(&r, 100, touch(TouchPhase::Down, 5.0, 5.0)).await;
        send_later(&r, 200, Command::Stop).await;
        let links = Links { display: None, hid: Some(HidLink { hid: &mut hid, backend: &mut backend }) };
        let exit = tokio::time::timeout(
            Duration::from_secs(60),
            run_session(&sock, &mut r.engine, &mut r.rx, &r.gone, &TokioClock, links),
        )
        .await
        .expect("the session ended instead of waiting for the stalled service");
        assert_eq!(exit, Exit::Stopped(StopCause::Command));
        let frames = r.queue.drain();
        let failure = frames
            .iter()
            .filter(|f| f.is_control())
            .map(|f| serde_json::from_slice::<serde_json::Value>(&f.payload).unwrap())
            .find(|v| v["event"] == "hid")
            .expect("a hid failure was reported");
        assert_eq!(failure["ok"], false);
        assert!(failure["message"].as_str().unwrap().contains("did not answer"));
    }

    // ---- a HID service that is stuck or slow does not hold up the loop --------------------------

    type Stamps = Arc<Mutex<Vec<tokio::time::Instant>>>;

    /// A [`FakeSocket`] that notes when the loop took each datagram and when it sent one.
    struct StampedSocket {
        inner: FakeSocket,
        received: Stamps,
        sent: Stamps,
    }

    impl DatagramSocket for StampedSocket {
        async fn recv(&self) -> io::Result<Datagram> {
            let dg = self.inner.recv().await;
            self.received.lock().unwrap().push(tokio::time::Instant::now());
            dg
        }

        async fn send_to(&self, port: u16, data: Vec<u8>) -> io::Result<()> {
            self.sent.lock().unwrap().push(tokio::time::Instant::now());
            self.inner.send_to(port, data).await
        }
    }

    fn stamped(sock: FakeSocket) -> (StampedSocket, Stamps, Stamps) {
        let (received, sent) = (Stamps::default(), Stamps::default());
        (StampedSocket { inner: sock, received: received.clone(), sent: sent.clone() }, received, sent)
    }

    fn ms_since(t0: tokio::time::Instant, stamps: &Stamps) -> Vec<u64> {
        stamps.lock().unwrap().iter().map(|t| (*t - t0).as_millis() as u64).collect()
    }

    /// A P frame (one NAL unit in one packet) at `at_ms`, with the sequence number and timestamp that follow
    /// the first picture's.
    fn send_p_frame_later(tx: &mpsc::UnboundedSender<io::Result<Datagram>>, at_ms: u64, seq: u16) {
        let tx = tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(at_ms)).await;
            tx.send(dg(seq, 1000 + 3000 * u32::from(seq), true, &trail(&[7; 20]))).unwrap();
        });
    }

    fn hid_failures(frames: &[Frame]) -> Vec<String> {
        frames
            .iter()
            .filter(|f| f.is_control())
            .map(|f| serde_json::from_slice::<serde_json::Value>(&f.payload).unwrap())
            .filter(|v| v["event"] == "hid" && v["ok"] == false)
            .map(|v| v["message"].as_str().unwrap().to_string())
            .collect()
    }

    #[tokio::test(start_paused = true)]
    async fn a_stuck_hid_service_does_not_hold_up_the_video_or_the_commands() {
        let (sock, tx) = fake();
        let (sock, received, sent) = stamped(sock);
        let mut r = hid_rig();
        send_first_idr(&tx);
        let mut backend = FakeHid { done: Arc::default(), stall: true };
        let mut hid = Hid::new(HidMapping::default(), r.queue.clone());
        let t0 = tokio::time::Instant::now();
        // At 100 ms a touch goes to the service, which never answers: it is given up on at 600 ms.
        send_later(&r, 100, touch(TouchPhase::Down, 5.0, 5.0)).await;
        // Meanwhile the picture goes on, and a keyframe is asked for.
        for (at, seq) in [(150, 5), (250, 6), (350, 7), (450, 8)] {
            send_p_frame_later(&tx, at, seq);
        }
        send_later(&r, 200, Command::Keyframe).await;
        send_later(&r, 500, Command::Stop).await;
        let links = Links { display: None, hid: Some(HidLink { hid: &mut hid, backend: &mut backend }) };
        let exit = run_session(&sock, &mut r.engine, &mut r.rx, &r.gone, &TokioClock, links).await;
        assert_eq!(exit, Exit::Stopped(StopCause::Command));

        // Every datagram was taken the moment it came (the first four are the first picture, at 0), and the
        // keyframe request went out at 200: the loop did not wait for the touch.
        assert_eq!(ms_since(t0, &received), vec![0, 0, 0, 0, 150, 250, 350, 450]);
        assert_eq!(ms_since(t0, &sent), vec![200]);
        let frames = r.queue.drain();
        assert_eq!(frames.iter().filter(|f| !f.is_control()).count(), 5, "the first picture and four more");
        assert!(hid_failures(&frames).iter().any(|m| m.contains("did not answer")), "{:?}", hid_failures(&frames));
    }

    /// Takes `delay` to perform each action, and notes when it did.
    struct SlowHid {
        delay: Duration,
        done: Arc<Mutex<Vec<(Action, tokio::time::Instant)>>>,
    }

    impl HidBackend for SlowHid {
        fn perform(&mut self, action: Action) -> Pin<Box<dyn Future<Output = Result<(), String>> + '_>> {
            let (delay, done) = (self.delay, self.done.clone());
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                done.lock().unwrap().push((action, tokio::time::Instant::now()));
                Ok(())
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_hid_service_gets_its_actions_in_order_with_the_waiting_moves_replaced() {
        let (sock, tx) = fake();
        let (sock, received, _sent) = stamped(sock);
        let mut r = hid_rig();
        send_first_idr(&tx);
        let done = Arc::new(Mutex::new(Vec::new()));
        let mut backend = SlowHid { delay: Duration::from_millis(30), done: done.clone() };
        let mut hid = Hid::new(HidMapping::default(), r.queue.clone());
        let t0 = tokio::time::Instant::now();
        // A drag: down at 100 ms, a move every 10 ms (the service takes 30 ms for each report), the lift at
        // 300 ms, then the Home button, then the stop.
        send_later(&r, 100, touch(TouchPhase::Down, 100.0, 1000.0)).await;
        for i in 1..=19u64 {
            send_later(&r, 100 + 10 * i, touch(TouchPhase::Move, 100.0 + 10.0 * i as f64, 1000.0)).await;
        }
        send_later(&r, 300, touch(TouchPhase::Up, 300.0, 1000.0)).await;
        send_later(&r, 310, Command::Key(Key::Home, Press::Short)).await;
        send_p_frame_later(&tx, 200, 5);
        send_later(&r, 700, Command::Stop).await;
        let links = Links { display: None, hid: Some(HidLink { hid: &mut hid, backend: &mut backend }) };
        let exit = run_session(&sock, &mut r.engine, &mut r.rx, &r.gone, &TokioClock, links).await;
        assert_eq!(exit, Exit::Stopped(StopCause::Command));
        // The picture was not held up by the service's 30 ms.
        assert_eq!(ms_since(t0, &received), vec![0, 0, 0, 0, 200]);

        let done: Vec<Action> = done.lock().unwrap().iter().map(|(a, _)| *a).collect();
        let home = |down| Action::Button { page: 0x0C, code: 0x40, down };
        // Order: the down first, then moves of a finger that only goes one way, the lift, the button, the end.
        let contacts: Vec<u16> = done
            .iter()
            .filter_map(|a| match a {
                Action::Touch { down: true, x, .. } => Some(*x),
                _ => None,
            })
            .collect();
        assert!(contacts.windows(2).all(|w| w[0] < w[1]), "the finger only goes right: {contacts:?}");
        assert!(contacts.len() >= 3 && contacts.len() < 15, "some moves were replaced by newer ones: {contacts:?}");
        // The picture is 1216x2656 and the mapping is the identity without display info.
        let at = |px: f64| crate::hid::map_touch(px, 1000.0, 1216, 2656, 0);
        assert_eq!(contacts[0], at(100.0).0, "the down is the first contact, where it was put");
        assert_eq!(*contacts.last().unwrap(), at(290.0).0, "the newest move is never lost");
        let pos = |a: Action| done.iter().position(|x| *x == a).unwrap_or_else(|| panic!("{a:?} missing in {done:?}"));
        let lift = Action::Touch { down: false, x: at(300.0).0, y: at(300.0).1 };
        assert!(done.iter().rposition(|a| matches!(a, Action::Touch { down: true, .. })).unwrap() < pos(lift));
        assert!(pos(lift) < pos(home(true)) && pos(home(true)) < pos(home(false)));
        assert_eq!(done.last(), Some(&Action::KeyboardRemove));
        assert_eq!(done.len(), contacts.len() + 4, "the lift, the two button reports and the keyboard: {done:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_backlog_for_a_stuck_service_is_reported_and_a_stop_waits_for_it_only_so_long() {
        let (sock, tx) = fake();
        let mut r = hid_rig();
        send_first_idr(&tx);
        let mut backend = FakeHid { done: Arc::default(), stall: true };
        let mut hid = Hid::new(HidMapping::default(), r.queue.clone());
        let t0 = tokio::time::Instant::now();
        // 90 distinct keys go down at 100 ms: the first is being performed (it never finishes), the queue
        // takes 64, and the others are turned away and said so.
        for usage in 4..94u8 {
            send_later(&r, 100, Command::Kb(Kb::Down(usage))).await;
        }
        send_later(&r, 1000, Command::Stop).await;
        let links = Links { display: None, hid: Some(HidLink { hid: &mut hid, backend: &mut backend }) };
        let exit = run_session(&sock, &mut r.engine, &mut r.rx, &r.gone, &TokioClock, links).await;
        assert_eq!(exit, Exit::Stopped(StopCause::Command));
        // The 65 that were taken would need half a minute to time out one by one: the end does not wait
        // for them for more than the budget.
        let took = t0.elapsed();
        assert!(took >= Duration::from_secs(1) + HID_SHUTDOWN_BUDGET, "{took:?}");
        assert!(took < Duration::from_secs(1) + HID_SHUTDOWN_BUDGET + Duration::from_millis(100), "{took:?}");
        let failures = hid_failures(&r.queue.drain());
        assert!(failures.iter().any(|m| m.contains("not keeping up")), "{failures:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn without_hid_a_touch_is_answered_with_why_not() {
        let (sock, tx) = fake();
        let mut r = hid_rig();
        send_first_idr(&tx);
        let mut backend = NoHid;
        let mut hid = Hid::new(HidMapping::default(), r.queue.clone());
        send_later(&r, 100, touch(TouchPhase::Down, 5.0, 5.0)).await;
        send_later(&r, 120, Command::Invalid("usage: key <name>".into())).await;
        send_later(&r, 200, Command::Stop).await;
        let links = Links { display: None, hid: Some(HidLink { hid: &mut hid, backend: &mut backend }) };
        run_session(&sock, &mut r.engine, &mut r.rx, &r.gone, &TokioClock, links).await;
        let messages: Vec<String> = r
            .queue
            .drain()
            .iter()
            .filter(|f| f.is_control())
            .map(|f| serde_json::from_slice::<serde_json::Value>(&f.payload).unwrap())
            .filter(|v| v["event"] == "hid")
            .map(|v| v["message"].as_str().unwrap().to_string())
            .collect();
        assert!(messages[0].contains("not enabled"), "{messages:?}");
        // The next failure inside the same second is held back (one reply a second).
        assert_eq!(messages.len(), 1, "{messages:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn silence_fails_the_session() {
        let (sock, _tx) = fake();
        let mut r = rig(|_| {});
        let exit = run_session(&sock, &mut r.engine, &mut r.rx, &r.gone, &TokioClock, Links::default()).await;
        match exit {
            Exit::Failed(m) => assert!(m.contains("no RTP media from the device for 15 s"), "{m}"),
            other => panic!("{other:?}"),
        }
        // With the RTP check off, the any-datagram check still ends it, later.
        let mut r = rig(|c| {
            c.rtp_stall_keyframe = None;
            c.rtp_stall_timeout = None;
            c.no_data_timeout = Duration::from_secs(30);
        });
        let exit = run_session(&sock, &mut r.engine, &mut r.rx, &r.gone, &TokioClock, Links::default()).await;
        match exit {
            Exit::Failed(m) => assert!(m.contains("no data from the device for 30 s"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_socket_error_fails_the_session() {
        let (sock, tx) = fake();
        let mut r = rig(|_| {});
        tx.send(Err(io::Error::new(io::ErrorKind::BrokenPipe, "adapter closed"))).unwrap();
        let exit = run_session(&sock, &mut r.engine, &mut r.rx, &r.gone, &TokioClock, Links::default()).await;
        match exit {
            Exit::Failed(m) => assert!(m.contains("adapter closed"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_gone_consumer_stops_the_session_even_if_the_notification_came_first() {
        let (sock, _tx) = fake();
        let mut r = rig(|_| {});
        r.gone.notify_one(); // the writer noticed before the loop started waiting
        let exit = run_session(&sock, &mut r.engine, &mut r.rx, &r.gone, &TokioClock, Links::default()).await;
        assert_eq!(exit, Exit::Stopped(StopCause::ConsumerGone));
    }
}
