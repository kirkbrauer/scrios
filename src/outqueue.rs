//! The one queue in front of the one writer.
//!
//! Everything that reaches stdout (access units and control frames) goes through this queue, in
//! order, and one thread writes it, so a control frame can never land in the middle of an access
//! unit. The queue is bounded: when stdout cannot keep up, the queued access units (all of them:
//! a delta unit is no use without its predecessors) are dropped and the caller is told, so it can
//! wait for the next keyframe and ask for one. Control frames are never dropped on overflow.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};

use crate::frame::{Frame, MAX_PAYLOAD};

/// Access units queued at most (about 0.7 s of a 45 fps stream).
pub const MAX_QUEUED_UNITS: usize = 32;
/// Payload bytes of queued access units, at most.
pub const MAX_QUEUED_BYTES: usize = 16 * 1024 * 1024;
/// Control frames queued at most before the oldest droppable one (stats) is discarded.
const MAX_QUEUED_CONTROLS: usize = 256;

#[derive(Debug)]
enum Item {
    Control { frame: Frame, droppable: bool },
    Unit(Frame),
}

#[derive(Debug, Default)]
struct Inner {
    items: VecDeque<Item>,
    units: usize,
    unit_bytes: usize,
    controls: usize,
    closed: bool,
}

impl Inner {
    fn drop_units(&mut self) -> usize {
        let n = self.units;
        self.items.retain(|i| matches!(i, Item::Control { .. }));
        self.units = 0;
        self.unit_bytes = 0;
        n
    }
}

/// What happened to an access unit offered to the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pushed {
    Queued,
    /// The queue was full (or the unit too large): this many units were dropped, the offered one
    /// included. Everything queued before it is gone, so the stream needs a keyframe.
    Overflow {
        dropped: usize,
    },
    /// The queue is closed.
    Closed,
}

/// What the writer has actually written (access units only).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Written {
    pub units: u64,
    pub bytes: u64,
    pub keyframes: u64,
}

#[derive(Debug)]
pub struct OutQueue {
    inner: Mutex<Inner>,
    ready: Condvar,
    max_units: usize,
    max_bytes: usize,
    written_units: AtomicU64,
    written_bytes: AtomicU64,
    written_keyframes: AtomicU64,
    /// Touch downs on the very edge of the digitizer (the HID side counts them here for the `stats` event).
    edge_downs: AtomicU64,
}

impl Default for OutQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl OutQueue {
    #[must_use]
    pub fn new() -> Self {
        Self::with_limits(MAX_QUEUED_UNITS, MAX_QUEUED_BYTES)
    }

    #[must_use]
    pub fn with_limits(max_units: usize, max_bytes: usize) -> Self {
        OutQueue {
            inner: Mutex::new(Inner::default()),
            ready: Condvar::new(),
            max_units,
            max_bytes,
            written_units: AtomicU64::new(0),
            written_bytes: AtomicU64::new(0),
            written_keyframes: AtomicU64::new(0),
            edge_downs: AtomicU64::new(0),
        }
    }

    /// Records that `frame` reached the output (the writer calls this after the flush).
    pub fn note_written(&self, frame: &Frame) {
        if frame.is_control_frame() {
            return;
        }
        self.written_units.fetch_add(1, Ordering::Relaxed);
        self.written_bytes.fetch_add(frame.payload.len() as u64, Ordering::Relaxed);
        if frame.flags & crate::frame::FLAG_KEY != 0 {
            self.written_keyframes.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Counts a touch down that landed on the very edge of the digitizer.
    pub fn count_edge_down(&self) {
        self.edge_downs.fetch_add(1, Ordering::Relaxed);
    }

    /// Touch downs on the very edge of the digitizer so far.
    pub fn edge_downs(&self) -> u64 {
        self.edge_downs.load(Ordering::Relaxed)
    }

    /// Access units, payload bytes and keyframes written so far.
    pub fn written(&self) -> Written {
        Written {
            units: self.written_units.load(Ordering::Relaxed),
            bytes: self.written_bytes.load(Ordering::Relaxed),
            keyframes: self.written_keyframes.load(Ordering::Relaxed),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues a control frame; false if the queue is closed.
    pub fn push_control(&self, frame: Frame) -> bool {
        self.push_control_inner(frame, false)
    }

    /// Like [`push_control`](Self::push_control), but the frame may be discarded if the writer is
    /// far behind (periodic stats).
    pub fn push_droppable_control(&self, frame: Frame) -> bool {
        self.push_control_inner(frame, true)
    }

    fn push_control_inner(&self, frame: Frame, droppable: bool) -> bool {
        let mut q = self.lock();
        if q.closed {
            return false;
        }
        if q.controls >= MAX_QUEUED_CONTROLS
            && let Some(i) = q.items.iter().position(|i| matches!(i, Item::Control { droppable: true, .. }))
        {
            q.items.remove(i);
            q.controls -= 1;
        }
        q.items.push_back(Item::Control { frame, droppable });
        q.controls += 1;
        drop(q);
        self.ready.notify_one();
        true
    }

    /// Queues an access unit, or drops the whole backlog if it does not fit.
    pub fn push_unit(&self, frame: Frame) -> Pushed {
        let mut q = self.lock();
        if q.closed {
            return Pushed::Closed;
        }
        let len = frame.payload.len();
        if len > MAX_PAYLOAD {
            let dropped = q.drop_units();
            return Pushed::Overflow { dropped: dropped + 1 };
        }
        if q.units >= self.max_units || q.unit_bytes + len > self.max_bytes {
            let dropped = q.drop_units();
            return Pushed::Overflow { dropped: dropped + 1 };
        }
        q.units += 1;
        q.unit_bytes += len;
        q.items.push_back(Item::Unit(frame));
        drop(q);
        self.ready.notify_one();
        Pushed::Queued
    }

    /// Blocks until a frame is available; `None` once the queue is closed and empty.
    pub fn pop(&self) -> Option<Frame> {
        let mut q = self.lock();
        loop {
            if let Some(item) = q.items.pop_front() {
                return Some(match item {
                    Item::Control { frame, .. } => {
                        q.controls -= 1;
                        frame
                    }
                    Item::Unit(frame) => {
                        q.units -= 1;
                        q.unit_bytes -= frame.payload.len();
                        frame
                    }
                });
            }
            if q.closed {
                return None;
            }
            q = self.ready.wait(q).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Ends the queue: what is queued is still written, then the writer stops.
    pub fn close(&self) {
        self.lock().closed = true;
        self.ready.notify_all();
    }

    /// Like [`close`](Self::close), but the queued access units are discarded (they are stale at
    /// shutdown); queued control frames are still written.
    pub fn close_discarding_units(&self) {
        let mut q = self.lock();
        q.drop_units();
        q.closed = true;
        drop(q);
        self.ready.notify_all();
    }

    /// (access units, payload bytes) waiting.
    #[cfg(test)]
    pub fn queued_units(&self) -> (usize, usize) {
        let q = self.lock();
        (q.units, q.unit_bytes)
    }

    /// Takes everything queued without blocking (tests).
    #[cfg(test)]
    pub fn drain(&self) -> Vec<Frame> {
        let mut q = self.lock();
        let items: Vec<Item> = q.items.drain(..).collect();
        q.units = 0;
        q.unit_bytes = 0;
        q.controls = 0;
        drop(q);
        // Taking a frame out here stands for the writer writing it.
        items
            .into_iter()
            .map(|i| match i {
                Item::Control { frame, .. } | Item::Unit(frame) => {
                    self.note_written(&frame);
                    frame
                }
            })
            .collect()
    }
}

/// The writer: pops frames and writes each with one flush, until the queue is closed and empty.
/// Returns the first write error (the reader of stdout went away).
pub fn run_writer<W: Write>(queue: &OutQueue, out: W) -> io::Result<()> {
    let mut out = io::BufWriter::with_capacity(256 * 1024, out);
    while let Some(frame) = queue.pop() {
        frame.write_to(&mut out)?;
        out.flush()?;
        queue.note_written(&frame);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    use super::*;
    use crate::frame::{Event, decode_all};

    fn unit(n: u8, len: usize) -> Frame {
        Frame::unit(u64::from(n), false, false, vec![n; len])
    }

    #[test]
    fn frames_come_out_in_the_order_they_went_in() {
        let q = OutQueue::new();
        assert!(q.push_control(Frame::control(&Event::Stopped)));
        assert_eq!(q.push_unit(unit(1, 10)), Pushed::Queued);
        assert!(q.push_control(Frame::control(&Event::Stopped)));
        assert_eq!(q.push_unit(unit(2, 10)), Pushed::Queued);
        let f = q.drain();
        let kinds: Vec<(bool, u64)> = f.iter().map(|f| (f.is_control(), f.pts_us)).collect();
        assert_eq!(kinds, vec![(true, 0), (false, 1), (true, 0), (false, 2)]);
    }

    #[test]
    fn overflow_by_count_drops_every_queued_unit_and_keeps_the_controls() {
        let q = OutQueue::with_limits(3, 1 << 20);
        q.push_control(Frame::control(&Event::Stopped));
        for i in 0..3 {
            assert_eq!(q.push_unit(unit(i, 10)), Pushed::Queued);
        }
        // The fourth does not fit: the three queued and this one are dropped.
        assert_eq!(q.push_unit(unit(9, 10)), Pushed::Overflow { dropped: 4 });
        assert_eq!(q.queued_units(), (0, 0));
        let f = q.drain();
        assert_eq!(f.len(), 1);
        assert!(f[0].is_control());
        // The queue works again afterwards.
        assert_eq!(q.push_unit(unit(5, 10)), Pushed::Queued);
    }

    #[test]
    fn overflow_by_bytes() {
        let q = OutQueue::with_limits(100, 1000);
        assert_eq!(q.push_unit(unit(1, 600)), Pushed::Queued);
        assert_eq!(q.push_unit(unit(2, 300)), Pushed::Queued);
        assert_eq!(q.push_unit(unit(3, 200)), Pushed::Overflow { dropped: 3 });
        assert_eq!(q.queued_units(), (0, 0));
        // A single unit over the byte limit is dropped on its own.
        assert_eq!(q.push_unit(unit(4, 2000)), Pushed::Overflow { dropped: 1 });
    }

    #[test]
    fn a_unit_over_the_frame_limit_is_never_queued() {
        let q = OutQueue::new();
        let big = Frame::unit(0, true, true, vec![0; MAX_PAYLOAD + 1]);
        assert_eq!(q.push_unit(big), Pushed::Overflow { dropped: 1 });
        assert_eq!(q.queued_units(), (0, 0));
    }

    #[test]
    fn popping_frees_room() {
        let q = OutQueue::with_limits(2, 1 << 20);
        q.push_unit(unit(1, 5));
        q.push_unit(unit(2, 5));
        assert_eq!(q.pop().unwrap().pts_us, 1);
        assert_eq!(q.queued_units().0, 1);
        assert_eq!(q.push_unit(unit(3, 5)), Pushed::Queued);
    }

    #[test]
    fn droppable_controls_are_shed_when_the_writer_is_far_behind() {
        let q = OutQueue::new();
        q.push_control(Frame::control(&Event::Stopped)); // the important one, first
        for _ in 0..(MAX_QUEUED_CONTROLS + 50) {
            q.push_droppable_control(Frame::control(&Event::Stopped));
        }
        let f = q.drain();
        assert_eq!(f.len(), MAX_QUEUED_CONTROLS);
    }

    #[test]
    fn close_lets_the_writer_drain_then_stop() {
        let q = Arc::new(OutQueue::new());
        q.push_unit(unit(1, 3));
        q.push_control(Frame::control(&Event::Stopped));
        q.close();
        assert!(!q.push_control(Frame::control(&Event::Stopped)));
        assert_eq!(q.push_unit(unit(2, 3)), Pushed::Closed);
        assert!(q.pop().is_some());
        assert!(q.pop().is_some());
        assert!(q.pop().is_none());
    }

    #[test]
    fn close_discarding_units_keeps_the_controls() {
        let q = OutQueue::new();
        q.push_unit(unit(1, 3));
        q.push_control(Frame::control(&Event::Stopped));
        q.close_discarding_units();
        assert!(q.pop().unwrap().is_control());
        assert!(q.pop().is_none());
    }

    #[test]
    fn pop_blocks_until_a_frame_or_close() {
        let q = Arc::new(OutQueue::new());
        let q2 = q.clone();
        let t = thread::spawn(move || q2.pop());
        thread::sleep(Duration::from_millis(50));
        q.push_unit(unit(7, 1));
        assert_eq!(t.join().unwrap().unwrap().pts_us, 7);

        let q2 = q.clone();
        let t = thread::spawn(move || q2.pop());
        thread::sleep(Duration::from_millis(50));
        q.close();
        assert!(t.join().unwrap().is_none());
    }

    #[test]
    fn the_writer_writes_whole_frames_in_order_from_many_producers() {
        let q = Arc::new(OutQueue::with_limits(10_000, 1 << 28));
        let sink = Arc::new(Mutex::new(Vec::<u8>::new()));

        struct Shared(Arc<Mutex<Vec<u8>>>);
        impl Write for Shared {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let writer = {
            let q = q.clone();
            let sink = sink.clone();
            thread::spawn(move || run_writer(&q, Shared(sink)))
        };
        let producers: Vec<_> = (0..4u8)
            .map(|p| {
                let q = q.clone();
                thread::spawn(move || {
                    for i in 0..200u32 {
                        if i % 7 == 0 {
                            q.push_control(Frame::control(&Event::Error { message: format!("p{p} #{i}") }));
                        } else {
                            q.push_unit(Frame::unit(u64::from(i), false, false, vec![p; 1 + (i as usize % 90) * 100]));
                        }
                    }
                })
            })
            .collect();
        for p in producers {
            p.join().unwrap();
        }
        q.close();
        writer.join().unwrap().unwrap();

        // Every frame decodes cleanly: no control frame landed inside an access unit.
        let bytes = sink.lock().unwrap().clone();
        let frames = decode_all(&bytes).expect("a clean frame stream");
        assert_eq!(frames.len(), 4 * 200);
        for f in &frames {
            if f.is_control() {
                assert!(f.payload.starts_with(br#"{"event":"error""#));
            } else {
                let first = f.payload[0];
                assert!(f.payload.iter().all(|b| *b == first), "payload bytes of two units mixed");
            }
        }
        // Within one producer, frames keep their order.
        for p in 0..4u8 {
            let pts: Vec<u64> =
                frames.iter().filter(|f| !f.is_control() && f.payload[0] == p).map(|f| f.pts_us).collect();
            assert!(pts.windows(2).all(|w| w[0] < w[1]));
        }
    }

    #[test]
    fn the_writer_reports_a_closed_pipe() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::from(io::ErrorKind::BrokenPipe))
            }
            fn flush(&mut self) -> io::Result<()> {
                Err(io::Error::from(io::ErrorKind::BrokenPipe))
            }
        }
        let q = OutQueue::new();
        q.push_unit(unit(1, 10));
        q.close();
        let err = run_writer(&q, Broken).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    }
}
