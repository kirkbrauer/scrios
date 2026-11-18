//! Writing what a [`Stream`](crate::stream::Stream) yields to a file or a pipe, in one of two formats.
//!
//! * [`Format::Raw`]: the access units' payloads, one after the other: a plain Annex-B elementary stream (4-byte
//!   start codes, the parameter sets in front of every key unit), which `ffplay -f h264 -`, `mpv` and `ffmpeg`
//!   read as it is. Events are not written. A recording starts at a key unit, because the stream does.
//! * [`Format::Framed`]: the machine protocol of `docs/protocol.md`: every access unit and every event as
//!   `[u32 len][u64 pts_us][u8 flags][payload]`, big-endian.
//!
//! Each item is written and flushed on its own, so a reader sees a picture as soon as it exists.

use std::io::{self, BufWriter, Write};

use crate::frame::{Frame, unit_flags, write_frame};
use crate::stream::Item;

/// The buffer in front of the output (a large access unit is written in one go).
const BUFFER: usize = 256 * 1024;

/// What is written (see the module).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Raw,
    Framed,
}

/// One output: a format and a writer.
#[derive(Debug)]
pub struct Sink<W: Write> {
    format: Format,
    out: BufWriter<W>,
}

impl<W: Write> Sink<W> {
    pub fn new(format: Format, out: W) -> Self {
        Sink { format, out: BufWriter::with_capacity(BUFFER, out) }
    }

    pub fn format(&self) -> Format {
        self.format
    }

    /// Writes `item` and flushes. Nothing is written for an event in the raw format.
    pub fn write_item(&mut self, item: &Item) -> io::Result<()> {
        match (self.format, item) {
            (Format::Raw, Item::Unit(au)) => self.out.write_all(&au.data)?,
            (Format::Raw, Item::Event(_)) => return Ok(()),
            (Format::Framed, Item::Unit(au)) => {
                write_frame(&mut self.out, au.pts_us, unit_flags(au.key, au.params), &au.data)?;
            }
            (Format::Framed, Item::Event(event)) => Frame::control(event).write_to(&mut self.out)?,
        }
        self.out.flush()
    }

    /// Flushes what is buffered (each item is flushed as it is written; this is for the end).
    pub fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::CodecKind;
    use crate::frame::{Event, decode_all};
    use crate::stream::AccessUnit;

    fn unit(pts_us: u64, key: bool, data: &[u8]) -> Item {
        Item::Unit(AccessUnit { codec: CodecKind::H264, pts_us, key, params: key, data: data.to_vec() })
    }

    fn items() -> Vec<Item> {
        vec![
            Item::Event(Event::Stats(Default::default())),
            unit(0, true, &[0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2, 0, 0, 0, 1, 0x65, 3]),
            unit(16_666, false, &[0, 0, 0, 1, 0x41, 4]),
            Item::Event(Event::Stopped),
        ]
    }

    fn written(format: Format, items: &[Item]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut sink = Sink::new(format, &mut out);
        for item in items {
            sink.write_item(item).unwrap();
        }
        sink.flush().unwrap();
        drop(sink);
        out
    }

    #[test]
    fn raw_is_the_access_units_back_to_back_and_no_events() {
        let out = written(Format::Raw, &items());
        assert_eq!(
            out,
            [&[0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2, 0, 0, 0, 1, 0x65, 3][..], &[0, 0, 0, 1, 0x41, 4]].concat()
        );
    }

    #[test]
    fn framed_is_the_documented_protocol_byte_for_byte() {
        let out = written(Format::Framed, &items());
        // The same bytes as the frames the session queues (what the protocol was before the library existed).
        let expected = [
            Frame::control(&Event::Stats(Default::default())),
            Frame::unit(0, true, true, vec![0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2, 0, 0, 0, 1, 0x65, 3]),
            Frame::unit(16_666, false, false, vec![0, 0, 0, 1, 0x41, 4]),
            Frame::control(&Event::Stopped),
        ];
        let mut want = Vec::new();
        for f in &expected {
            f.write_to(&mut want).unwrap();
        }
        assert_eq!(out, want);
        assert_eq!(decode_all(&out).unwrap(), expected);
        // The header of the first unit: len 18, pts 0, flags KEY|PARAMS.
        let at = Frame::control(&Event::Stats(Default::default())).encode().unwrap().len();
        assert_eq!(&out[at..at + 13], &[0, 0, 0, 18, 0, 0, 0, 0, 0, 0, 0, 0, 0x03]);
    }

    #[test]
    fn a_failing_writer_is_reported() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "gone"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "gone"))
            }
        }
        for format in [Format::Raw, Format::Framed] {
            let mut sink = Sink::new(format, Broken);
            assert_eq!(sink.write_item(&unit(0, true, &[1])).unwrap_err().kind(), io::ErrorKind::BrokenPipe);
        }
    }
}
