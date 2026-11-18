//! The threads behind the window.
//!
//! ```text
//!  stream --recv--> decode thread --FrameSlot (the newest picture only)--> window thread --> screen
//!  window thread --Command--> input thread --Feed::send--> stream
//! ```
//!
//! * The **decode thread** takes each access unit the stream yields, writes it to the recording if there is one, decodes
//!   it at once and puts the picture in the [`FrameSlot`], which holds one picture: a picture the window has not drawn
//!   when the next one comes is dropped (it is stale), so nothing queues between the decoder and the screen. Events go to
//!   the window as they come.
//! * The **input thread** carries the window's commands to the stream, so that a stream that is slow to take them never
//!   stalls the window; runs of mouse moves are merged into the last of them.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tracing::{debug, warn};

use crate::commands::{Command, TouchPhase};
use crate::frame::Event;
use crate::sink::{Format, Sink};
use crate::stream::{Item, Stream};
use crate::viewer::decode::{DecodeError, Decoder, Picture};

/// What the viewer needs of a stream: [`Stream`] is one; a test's list of items is another.
pub trait Feed: Send + Sync + 'static {
    /// The next item; blocks; `None` when the stream is over.
    fn recv(&self) -> Option<Item>;
    /// Sends a command; `false` when the stream is over.
    fn send(&self, command: Command) -> bool;
    /// Tells the stream nobody is reading any more.
    fn hang_up(&self);
    /// Waits for the stream to end and returns the exit status for how it did (0 after a clean stop).
    fn wait(&self) -> i32;
}

impl Feed for Stream {
    fn recv(&self) -> Option<Item> {
        Stream::recv(self)
    }

    fn send(&self, command: Command) -> bool {
        self.input().send(command)
    }

    fn hang_up(&self) {
        Stream::hang_up(self);
    }

    fn wait(&self) -> i32 {
        Stream::wait(self).exit_code()
    }
}

/// What the other threads tell the window thread.
#[derive(Debug, Clone, PartialEq)]
pub enum Wake {
    /// A new picture is in the slot.
    Frame,
    /// An event of the stream.
    Event(Event),
    /// The stream is over.
    Ended,
    /// The viewer cannot go on (no decoder): the message says why.
    Failed(String),
}

/// A decoded picture and when it came.
#[derive(Debug)]
pub struct Frame {
    pub picture: Picture,
    /// When the stream handed the access unit over.
    pub arrived: Instant,
    /// How long decoding took.
    pub decode: Duration,
}

/// The newest picture the window has not drawn yet.
#[derive(Debug, Default)]
pub struct FrameSlot {
    frame: Mutex<Option<Frame>>,
    /// The window has been woken for what is in the slot and has not looked yet.
    pending: AtomicBool,
    /// Pictures replaced before they were drawn.
    stale: Mutex<u64>,
}

impl FrameSlot {
    /// Puts a picture in the slot, in place of the one waiting. Returns whether the window needs waking: it does not
    /// if it was woken already and has not looked.
    pub fn publish(&self, frame: Frame) -> bool {
        let replaced = self.frame.lock().unwrap_or_else(PoisonError::into_inner).replace(frame).is_some();
        if replaced {
            *self.stale.lock().unwrap_or_else(PoisonError::into_inner) += 1;
        }
        !self.pending.swap(true, Ordering::AcqRel)
    }

    /// Takes the picture, if any: the window looks. The next publish wakes it again.
    pub fn take(&self) -> Option<Frame> {
        self.pending.store(false, Ordering::Release);
        self.frame.lock().unwrap_or_else(PoisonError::into_inner).take()
    }

    /// How many pictures were replaced unseen since the last call.
    pub fn take_stale(&self) -> u64 {
        std::mem::take(&mut *self.stale.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// The recording: every access unit as it came, raw Annex-B.
pub struct Recorder {
    name: String,
    sink: Sink<Box<dyn Write + Send>>,
}

impl std::fmt::Debug for Recorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Recorder").field("name", &self.name).finish_non_exhaustive()
    }
}

impl Recorder {
    #[must_use]
    pub fn new(name: String, out: impl Write + Send + 'static) -> Recorder {
        Recorder { name, sink: Sink::new(Format::Raw, Box::new(out)) }
    }
}

/// Runs the decode thread's loop until the stream ends: every item of `feed` is recorded, decoded and published, and
/// `wake` is told what the window needs to know. A recording that fails is given up on (the view goes on).
pub fn decode_loop(
    feed: &dyn Feed,
    decoder: &mut dyn Decoder,
    slot: &FrameSlot,
    mut recorder: Option<Recorder>,
    wake: &dyn Fn(Wake),
) {
    let mut failures = 0u64;
    while let Some(item) = feed.recv() {
        let arrived = Instant::now();
        if let Some(r) = recorder.as_mut()
            && let Err(e) = r.sink.write_item(&item)
        {
            warn!("recording to {} stopped: {e}", r.name);
            recorder = None;
        }
        let unit = match item {
            Item::Event(event) => {
                wake(Wake::Event(event));
                continue;
            }
            Item::Unit(unit) => unit,
        };
        let started = Instant::now();
        match decoder.decode(&unit) {
            Ok(Some(picture)) => {
                let frame = Frame { picture, arrived, decode: started.elapsed() };
                if slot.publish(frame) {
                    wake(Wake::Frame);
                }
            }
            Ok(None) => {}
            Err(DecodeError::Unit(why)) => {
                failures += 1;
                // The first, then each doubling: a stream of bad units is one line in the log, not thousands.
                if failures.is_power_of_two() {
                    warn!("{} could not decode a unit ({failures} so far): {why}", decoder.name());
                }
                // The device is asked for a key unit to start again from (rate-limited by the stream).
                let _ = feed.send(Command::Keyframe);
            }
            Err(DecodeError::Fatal(why)) => {
                wake(Wake::Failed(format!("{} cannot decode the stream: {why}", decoder.name())));
                feed.hang_up();
                break;
            }
        }
    }
    debug!("the stream is over");
    wake(Wake::Ended);
}

/// Merges runs of touch moves into the last of each run: while a command waits for the stream, the finger's older
/// positions are of no use to it.
#[must_use]
pub fn coalesce(batch: Vec<Command>) -> Vec<Command> {
    let is_move = |c: &Command| matches!(c, Command::Touch(t) if t.phase == TouchPhase::Move);
    let mut out = Vec::with_capacity(batch.len());
    let mut iter = batch.into_iter().peekable();
    while let Some(command) = iter.next() {
        if is_move(&command) && iter.peek().is_some_and(is_move) {
            continue;
        }
        out.push(command);
    }
    out
}

/// The window's way to send commands: to the input thread, which does not block the window.
#[derive(Debug, Clone)]
pub struct Control {
    tx: Option<Sender<Command>>,
}

impl Control {
    /// A control that sends nothing (`--no-control`).
    #[must_use]
    pub fn off() -> Control {
        Control { tx: None }
    }

    /// Starts the input thread, which sends what this control is given to `feed` until the stream is over.
    ///
    /// # Errors
    ///
    /// The thread could not be started.
    pub fn start(feed: Arc<dyn Feed>) -> std::io::Result<Control> {
        let (tx, rx) = channel();
        std::thread::Builder::new().name("scrios-viewer-input".into()).spawn(move || pump(&rx, feed.as_ref()))?;
        Ok(Control { tx: Some(tx) })
    }

    /// Whether commands are sent at all.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.tx.is_some()
    }

    /// Queues a command for the stream. Never blocks.
    pub fn send(&self, command: Command) {
        if let Some(tx) = &self.tx {
            // The thread is gone when the stream is: there is nobody to tell.
            let _ = tx.send(command);
        }
    }
}

fn pump(rx: &Receiver<Command>, feed: &dyn Feed) {
    while let Ok(first) = rx.recv() {
        let mut batch = vec![first];
        while let Ok(more) = rx.try_recv() {
            batch.push(more);
        }
        for command in coalesce(batch) {
            if !feed.send(command) {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::CodecKind;
    use crate::commands::{Kb, Touch};
    use crate::frame::Stats;
    use crate::stream::AccessUnit;
    use crate::viewer::decode::software::Software;
    use crate::viewer::decode::testing::*;
    use std::collections::VecDeque;

    /// A feed of items made up beforehand, that records what is sent to it.
    struct ListFeed {
        items: Mutex<VecDeque<Item>>,
        sent: Mutex<Vec<Command>>,
        hung_up: AtomicBool,
    }

    impl ListFeed {
        fn new(items: Vec<Item>) -> ListFeed {
            ListFeed { items: Mutex::new(items.into()), sent: Mutex::new(Vec::new()), hung_up: AtomicBool::new(false) }
        }
    }

    impl Feed for ListFeed {
        fn recv(&self) -> Option<Item> {
            self.items.lock().unwrap().pop_front()
        }
        fn send(&self, command: Command) -> bool {
            self.sent.lock().unwrap().push(command);
            true
        }
        fn hang_up(&self) {
            self.hung_up.store(true, Ordering::SeqCst);
        }
        fn wait(&self) -> i32 {
            0
        }
    }

    /// A decoder whose first unit fails and that decodes the rest.
    struct Flaky(Software, bool);

    impl Decoder for Flaky {
        fn name(&self) -> &'static str {
            "flaky"
        }
        fn decode(&mut self, unit: &AccessUnit) -> Result<Option<Picture>, DecodeError> {
            if !std::mem::replace(&mut self.1, true) {
                return Err(DecodeError::Unit("the first unit is bad".into()));
            }
            self.0.decode(unit)
        }
    }

    /// A decoder that cannot decode at all.
    struct Broken;

    impl Decoder for Broken {
        fn name(&self) -> &'static str {
            "broken"
        }
        fn decode(&mut self, _: &AccessUnit) -> Result<Option<Picture>, DecodeError> {
            Err(DecodeError::Fatal("no hardware".into()))
        }
    }

    /// A writer whose bytes the test can read.
    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A disk that is full.
    struct Full;

    impl Write for Full {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("disk full"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("disk full"))
        }
    }

    fn touch(phase: TouchPhase, x: f64) -> Command {
        Command::Touch(Touch { phase, x, y: 0.0, id: 0 })
    }

    fn frame(n: u8) -> Frame {
        let mut picture = Picture::blank(2, 2);
        picture.y.data[0] = n;
        Frame { picture, arrived: Instant::now(), decode: Duration::ZERO }
    }

    #[test]
    fn the_slot_keeps_only_the_newest_picture_and_wakes_the_window_once() {
        let slot = FrameSlot::default();
        assert!(slot.take().is_none());
        assert!(slot.publish(frame(1)), "the first picture wakes the window");
        assert!(!slot.publish(frame(2)), "it is awake already");
        assert!(!slot.publish(frame(3)));
        assert_eq!(slot.take_stale(), 2, "two pictures were replaced unseen");
        assert_eq!(slot.take_stale(), 0);
        let f = slot.take().expect("a picture");
        assert_eq!(f.picture.y.data[0], 3, "the newest");
        assert!(slot.take().is_none());
        // After the window looked, the next picture wakes it again.
        assert!(slot.publish(frame(4)));
    }

    #[test]
    fn a_run_of_moves_is_the_last_move_and_the_rest_keeps_its_order() {
        use TouchPhase::{Down, Move, Up};
        let batch = vec![
            touch(Down, 1.0),
            touch(Move, 2.0),
            touch(Move, 3.0),
            touch(Move, 4.0),
            touch(Up, 5.0),
            Command::Kb(Kb::Down(4)),
            touch(Move, 6.0),
            Command::Keyframe,
            touch(Move, 7.0),
            touch(Move, 8.0),
        ];
        assert_eq!(
            coalesce(batch),
            vec![
                touch(Down, 1.0),
                touch(Move, 4.0),
                touch(Up, 5.0),
                Command::Kb(Kb::Down(4)),
                touch(Move, 6.0),
                Command::Keyframe,
                touch(Move, 8.0),
            ]
        );
        assert!(coalesce(Vec::new()).is_empty());
    }

    #[test]
    fn the_input_thread_sends_what_it_is_given_in_order_and_stops_with_the_stream() {
        let feed = Arc::new(ListFeed::new(Vec::new()));
        let control = Control::start(feed.clone()).expect("thread");
        assert!(control.enabled());
        control.send(touch(TouchPhase::Down, 1.0));
        control.send(Command::Kb(Kb::Down(4)));
        control.send(Command::Stop);
        let deadline = Instant::now() + Duration::from_secs(5);
        while feed.sent.lock().unwrap().len() < 3 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            *feed.sent.lock().unwrap(),
            vec![touch(TouchPhase::Down, 1.0), Command::Kb(Kb::Down(4)), Command::Stop]
        );
        // A control that is off sends nothing and does not mind being used.
        let off = Control::off();
        assert!(!off.enabled());
        off.send(Command::Stop);
    }

    fn items_of_the_fixture() -> Vec<Item> {
        let mut items = vec![Item::Event(Event::Stats(Stats::default()))];
        items.extend(fixture_units().into_iter().map(Item::Unit));
        items.push(Item::Event(Event::Stopped));
        items
    }

    #[test]
    fn the_decode_loop_publishes_the_newest_picture_forwards_events_and_ends() {
        let feed = ListFeed::new(items_of_the_fixture());
        let mut decoder = Software::new().expect("decoder");
        let slot = FrameSlot::default();
        let woken = Mutex::new(Vec::new());
        decode_loop(&feed, &mut decoder, &slot, None, &|w| woken.lock().unwrap().push(w));
        let woken = woken.into_inner().unwrap();
        // The events come in order, the window is woken for the pictures (once, as it never looked), and the end is last.
        assert_eq!(woken.first(), Some(&Wake::Event(Event::Stats(Stats::default()))));
        assert_eq!(woken.iter().filter(|w| **w == Wake::Frame).count(), 1);
        assert!(woken.contains(&Wake::Event(Event::Stopped)));
        assert_eq!(woken.last(), Some(&Wake::Ended));
        // Four units decoded, one slot: three were replaced unseen and what is left is the last picture.
        assert_eq!(slot.take_stale(), 3);
        let f = slot.take().expect("a picture");
        assert!(near(sample(&f.picture, true, true), RED, 4));
        assert!(near(sample(&f.picture, false, false), GRAY, 4));
        assert!(f.decode > Duration::ZERO || f.arrived <= Instant::now());
        assert!(feed.sent.lock().unwrap().is_empty());
    }

    #[test]
    fn a_window_that_keeps_up_gets_every_picture() {
        let feed = ListFeed::new(items_of_the_fixture());
        let mut decoder = Software::new().expect("decoder");
        let slot = Arc::new(FrameSlot::default());
        let seen = Mutex::new(Vec::new());
        // The "window" takes the picture the moment it is woken.
        decode_loop(&feed, &mut decoder, &slot, None, &|w| {
            if w == Wake::Frame {
                seen.lock().unwrap().push(slot.take().expect("a picture"));
            }
        });
        let seen = seen.into_inner().unwrap();
        assert_eq!(seen.len(), 4);
        assert_eq!(slot.take_stale(), 0);
        assert!(near(sample(&seen[2].picture, false, false), BLACK, 4));
    }

    #[test]
    fn a_unit_that_cannot_be_decoded_asks_for_a_keyframe_and_the_loop_goes_on() {
        let mut items = vec![Item::Unit(AccessUnit {
            codec: CodecKind::H264,
            pts_us: 0,
            key: true,
            params: false,
            data: vec![0, 0, 0, 1, 0x65, 0, 0],
        })];
        items.extend(fixture_units().into_iter().map(Item::Unit));
        let feed = ListFeed::new(items);
        let mut decoder = Flaky(Software::new().expect("decoder"), false);
        let slot = FrameSlot::default();
        decode_loop(&feed, &mut decoder, &slot, None, &|_| {});
        assert_eq!(*feed.sent.lock().unwrap(), vec![Command::Keyframe]);
        assert!(slot.take().is_some(), "the units after it were decoded");
        assert!(!feed.hung_up.load(Ordering::SeqCst));
    }

    #[test]
    fn a_decoder_that_breaks_ends_the_viewer_with_a_reason() {
        let feed = ListFeed::new(items_of_the_fixture());
        let woken = Mutex::new(Vec::new());
        decode_loop(&feed, &mut Broken, &FrameSlot::default(), None, &|w| woken.lock().unwrap().push(w));
        let woken = woken.into_inner().unwrap();
        assert!(
            woken.iter().any(|w| matches!(w, Wake::Failed(m) if m.contains("broken") && m.contains("no hardware")))
        );
        assert_eq!(woken.last(), Some(&Wake::Ended));
        assert!(feed.hung_up.load(Ordering::SeqCst), "the stream is told nobody is reading");
    }

    #[test]
    fn the_recording_is_the_raw_stream_of_every_unit_and_a_broken_one_is_given_up() {
        let out = Shared::default();
        let feed = ListFeed::new(items_of_the_fixture());
        let mut decoder = Software::new().expect("decoder");
        let recorder = Recorder::new("test".into(), out.clone());
        decode_loop(&feed, &mut decoder, &FrameSlot::default(), Some(recorder), &|_| {});
        let expected: Vec<u8> = fixture_units().iter().flat_map(|u| u.data.clone()).collect();
        assert_eq!(*out.0.lock().unwrap(), expected, "the units back to back, no events");

        // A disk that fills up ends the recording, not the view.
        let feed = ListFeed::new(items_of_the_fixture());
        let mut decoder = Software::new().expect("decoder");
        let slot = FrameSlot::default();
        decode_loop(&feed, &mut decoder, &slot, Some(Recorder::new("full".into(), Full)), &|_| {});
        assert!(slot.take().is_some(), "still decoded and shown");
    }
}
