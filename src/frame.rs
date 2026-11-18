//! The stdout frame protocol: `[u32 len][u64 pts_us][u8 flags][payload]`, all integers big-endian.
//!
//! `len` is the payload length only (the 13 header bytes are not counted) and is at most
//! [`MAX_PAYLOAD`]. A frame is either one Annex-B access unit or (flag `CONTROL`) one JSON object.

use std::io::{self, Write};

use serde::{Deserialize, Serialize};

/// Bytes before the payload: `u32 len` + `u64 pts_us` + `u8 flags`.
pub const HEADER_LEN: usize = 13;
/// The largest payload a frame may carry.
pub const MAX_PAYLOAD: usize = 16 * 1024 * 1024;

/// The version of the framed protocol (see `docs/protocol.md`), carried by every `config` event as `"protocol"`.
/// It changes only when the framing, a flag or an existing event changes incompatibly; a new event or a new field
/// does not change it.
pub const PROTOCOL_VERSION: u32 = 1;

/// The access unit holds an IRAP picture (NAL types 16..=21).
pub const FLAG_KEY: u8 = 0x01;
/// VPS, SPS and PPS are the first three NAL units of the payload.
pub const FLAG_PARAMS: u8 = 0x02;
/// The payload is one UTF-8 JSON object, not an access unit.
pub const FLAG_CONTROL: u8 = 0x80;

/// One frame, ready to be written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub pts_us: u64,
    pub flags: u8,
    pub payload: Vec<u8>,
}

impl Frame {
    /// An access-unit frame.
    pub fn unit(pts_us: u64, key: bool, params: bool, payload: Vec<u8>) -> Frame {
        Frame { pts_us, flags: unit_flags(key, params), payload }
    }

    /// A control frame carrying `event` as JSON (pts is 0). A `config` event carries the protocol version
    /// ([`PROTOCOL_VERSION`]) as its last field.
    pub fn control(event: &Event) -> Frame {
        // Serializing these plain structs cannot fail; fall back to an error object anyway.
        let encoded = match event {
            Event::Config { .. } => serde_json::to_vec(&Versioned { event, protocol: PROTOCOL_VERSION }),
            _ => serde_json::to_vec(event),
        };
        let payload = encoded.unwrap_or_else(|_| br#"{"event":"error","message":"internal: event encoding"}"#.to_vec());
        Frame { pts_us: 0, flags: FLAG_CONTROL, payload }
    }

    pub fn is_control_frame(&self) -> bool {
        self.flags & FLAG_CONTROL != 0
    }

    #[cfg(test)]
    pub fn is_control(&self) -> bool {
        self.is_control_frame()
    }

    #[cfg(test)]
    pub fn is_key(&self) -> bool {
        self.flags & FLAG_KEY != 0
    }

    /// The 13 header bytes.
    pub fn header(&self) -> [u8; HEADER_LEN] {
        header(self.payload.len(), self.pts_us, self.flags)
    }

    /// Writes header and payload with one `write_all` each (the caller buffers and flushes).
    /// A payload over [`MAX_PAYLOAD`] is an error and nothing is written.
    pub fn write_to<W: Write>(&self, w: &mut W) -> io::Result<()> {
        write_frame(w, self.pts_us, self.flags, &self.payload)
    }

    /// The whole frame as one buffer.
    #[cfg(test)]
    pub fn encode(&self) -> io::Result<Vec<u8>> {
        let mut v = Vec::with_capacity(HEADER_LEN + self.payload.len());
        self.write_to(&mut v)?;
        Ok(v)
    }
}

/// The flags of an access-unit frame.
pub fn unit_flags(key: bool, params: bool) -> u8 {
    let mut flags = 0;
    if key {
        flags |= FLAG_KEY;
    }
    if params {
        flags |= FLAG_PARAMS;
    }
    flags
}

/// The 13 header bytes of a frame whose payload is `len` bytes.
fn header(len: usize, pts_us: u64, flags: u8) -> [u8; HEADER_LEN] {
    let mut h = [0u8; HEADER_LEN];
    // A payload is at most MAX_PAYLOAD (16 MiB): the callers refuse more.
    h[0..4].copy_from_slice(&u32::try_from(len).unwrap_or(u32::MAX).to_be_bytes());
    h[4..12].copy_from_slice(&pts_us.to_be_bytes());
    h[12] = flags;
    h
}

/// Writes one frame from its parts (what [`Frame::write_to`] does, for a payload that is not owned by a `Frame`):
/// the header and the payload with one `write_all` each. A payload over [`MAX_PAYLOAD`] is an error and nothing
/// is written.
pub fn write_frame<W: Write>(w: &mut W, pts_us: u64, flags: u8, payload: &[u8]) -> io::Result<()> {
    if payload.len() > MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("frame payload of {} bytes exceeds {MAX_PAYLOAD}", payload.len()),
        ));
    }
    w.write_all(&header(payload.len(), pts_us, flags))?;
    w.write_all(payload)
}

/// A `config` event as it is written: the event, then the protocol version.
#[derive(Serialize)]
struct Versioned<'a> {
    #[serde(flatten)]
    event: &'a Event,
    protocol: u32,
}

/// The control events of the protocol (`{"event": ...}`).
///
/// They are read back as well as written (a program that embeds the library gets them as values);
/// the `protocol` field of a `config` event is not part of the value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// Sent once before the first access unit, and again whenever the SPS changes.
    Config {
        /// RFC 6381 / WebCodecs codec string built from the SPS, e.g. `hev1.1.6.L150.B0`.
        codec: String,
        /// Visible size of the screen.
        width: u32,
        height: u32,
        /// The SPS picture size (the encoder pads to it).
        coded_width: u32,
        coded_height: u32,
        fps: u32,
        clock_hz: u32,
        /// Where `clock_hz` comes from: `negotiated` (the device's answer), `measured` (from the
        /// device's sender reports) or `default` (24 kHz assumed).
        clock_source: String,
        /// Our SSRC (the one the offer declares and every RTCP packet carries).
        ssrc: u32,
        /// The device's SSRC.
        device_ssrc: u32,
        /// From the device's display info (omitted when it could not be read): how far the
        /// framebuffer must be turned clockwise to be upright, the degrees of `current_orientation`
        /// (`rot90` is 90). The width and height above are the framebuffer's; the upright picture
        /// has them swapped for 90 and 270.
        #[serde(skip_serializing_if = "Option::is_none")]
        rotation: Option<u16>,
        /// Points to pixels of the primary display.
        #[serde(skip_serializing_if = "Option::is_none")]
        point_scale: Option<f64>,
        /// `rot0`, `rot90`, `rot180` or `rot270`, as the device reports them.
        #[serde(skip_serializing_if = "Option::is_none")]
        current_orientation: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        native_orientation: Option<String>,
    },
    /// Counters since start.
    Stats(Stats),
    /// The touch and key service: `ready` once after it was asked for (with the mapping in effect),
    /// `ok: false` when a command failed (nothing for a command that worked), `warning` for a
    /// surprise that is not an error, and the mapping again when it changes.
    Hid(HidEvent),
    /// Once, just before a non-zero exit.
    Error { message: String },
    /// Once, last, on a clean stop.
    Stopped,
}

/// The fields of a `hid` event; only those that are set are written.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HidEvent {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ready: Option<bool>,
    /// With `ready`: the keyboard (`kb` commands) can be used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keyboard: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ok: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// How touches are mapped to the digitizer: `auto`, `identity (no display info)`, `panel` or `ui`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mapping: Option<String>,
    /// With `mapping`: the net exchange and mirroring of the axes of the framebuffer-normalized
    /// point (the swap first, then x, then y), the automatic mapping and the overrides together.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub swap_xy: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flip_x: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flip_y: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// The counters of a `stats` event (all since start).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stats {
    /// RTP packets accepted.
    pub packets: u64,
    /// Access units written.
    pub access_units: u64,
    /// Payload bytes of the access units written.
    pub bytes: u64,
    /// RTP packets declared lost (gaps in the sequence numbers).
    pub lost: u64,
    /// Access units that were not written: incomplete, withheld until a keyframe, or dropped
    /// because stdout could not keep up.
    pub dropped_units: u64,
    /// KEY access units written.
    pub keyframes: u64,
    /// Keyframe requests (PLI + FIR) sent to the device.
    pub plis: u64,
    /// RTCP datagrams sent to the device (receiver reports and keyframe requests).
    pub rtcp: u64,
    /// B slices seen in an H.264 stream. The SPS is rewritten to say there is no reordering, so a B slice is
    /// a surprise (pictures may then come out of order); nothing is done about it but to count it.
    pub b_slices: u64,
    /// Touch downs whose position on the digitizer was at its limit on an axis (0 or 65535): the
    /// swipes that start at the very edge of the screen.
    pub edge_downs: u64,
}

/// Decodes one frame from the front of `buf` (tests and tools): the frame and the bytes used.
#[cfg(test)]
pub fn decode(buf: &[u8]) -> Result<(Frame, usize), String> {
    if buf.len() < HEADER_LEN {
        return Err("short header".into());
    }
    let len = u32::from_be_bytes(buf[0..4].try_into().unwrap()) as usize;
    if len > MAX_PAYLOAD {
        return Err("too long".into());
    }
    let pts_us = u64::from_be_bytes(buf[4..12].try_into().unwrap());
    let flags = buf[12];
    let payload = buf.get(HEADER_LEN..HEADER_LEN + len).ok_or("short payload")?.to_vec();
    Ok((Frame { pts_us, flags, payload }, HEADER_LEN + len))
}

/// Splits a byte stream into frames (tests).
#[cfg(test)]
pub fn decode_all(mut buf: &[u8]) -> Result<Vec<Frame>, String> {
    let mut out = Vec::new();
    while !buf.is_empty() {
        let (f, used) = decode(buf)?;
        out.push(f);
        buf = &buf[used..];
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_header_is_big_endian_with_the_documented_layout() {
        let f = Frame::unit(0x0102_0304_0506_0708, true, true, vec![0xAA; 5]);
        let bytes = f.encode().unwrap();
        assert_eq!(bytes.len(), HEADER_LEN + 5);
        // len (u32, payload only)
        assert_eq!(&bytes[0..4], &[0, 0, 0, 5]);
        // pts_us (u64)
        assert_eq!(&bytes[4..12], &[1, 2, 3, 4, 5, 6, 7, 8]);
        // flags: KEY | PARAMS
        assert_eq!(bytes[12], 0x03);
        assert_eq!(&bytes[13..], &[0xAA; 5]);
    }

    #[test]
    fn flag_values_are_the_contracts() {
        assert_eq!(FLAG_KEY, 0x01);
        assert_eq!(FLAG_PARAMS, 0x02);
        assert_eq!(FLAG_CONTROL, 0x80);
        assert_eq!(Frame::unit(0, false, false, vec![1]).flags, 0);
        assert_eq!(Frame::unit(0, true, false, vec![1]).flags, 0x01);
        assert_eq!(Frame::unit(0, false, true, vec![1]).flags, 0x02);
    }

    #[test]
    fn length_is_a_four_byte_big_endian_field() {
        let f = Frame::unit(0, false, false, vec![0; 0x0001_0203]);
        assert_eq!(&f.header()[0..4], &[0x00, 0x01, 0x02, 0x03]);
    }

    #[test]
    fn control_frames_have_pts_zero_and_the_control_flag() {
        let f = Frame::control(&Event::Stopped);
        assert!(f.is_control());
        assert_eq!(f.flags, 0x80);
        assert_eq!(f.pts_us, 0);
        assert_eq!(f.payload, br#"{"event":"stopped"}"#);
        // The header of a control frame: len, zero pts, 0x80.
        let h = f.header();
        assert_eq!(&h[4..12], &[0; 8]);
        assert_eq!(h[12], 0x80);
        assert_eq!(u32::from_be_bytes(h[0..4].try_into().unwrap()) as usize, f.payload.len());
    }

    #[test]
    fn config_event_json_has_the_contracts_fields_in_order() {
        let e = Event::Config {
            codec: "hev1.1.6.L150.B0".into(),
            width: 1206,
            height: 2622,
            coded_width: 1216,
            coded_height: 2656,
            fps: 60,
            clock_hz: 24000,
            clock_source: "default".into(),
            ssrc: 123,
            device_ssrc: 456,
            rotation: None,
            point_scale: None,
            current_orientation: None,
            native_orientation: None,
        };
        let json = String::from_utf8(Frame::control(&e).payload).unwrap();
        assert_eq!(
            json,
            r#"{"event":"config","codec":"hev1.1.6.L150.B0","width":1206,"height":2622,"coded_width":1216,"coded_height":2656,"fps":60,"clock_hz":24000,"clock_source":"default","ssrc":123,"device_ssrc":456,"protocol":1}"#
        );
    }

    #[test]
    fn config_event_carries_the_display_facts_when_known() {
        let e = Event::Config {
            codec: "hev1.1.6.L150.B0".into(),
            width: 2388,
            height: 1668,
            coded_width: 2400,
            coded_height: 1680,
            fps: 60,
            clock_hz: 24000,
            clock_source: "negotiated".into(),
            ssrc: 1,
            device_ssrc: 2,
            rotation: Some(180),
            point_scale: Some(2.0),
            current_orientation: Some("rot90".into()),
            native_orientation: Some("rot270".into()),
        };
        let json = String::from_utf8(Frame::control(&e).payload).unwrap();
        assert!(
            json.ends_with(r#""device_ssrc":2,"rotation":180,"point_scale":2.0,"current_orientation":"rot90","native_orientation":"rot270","protocol":1}"#),
            "{json}"
        );
    }

    #[test]
    fn stats_error_and_stopped_json() {
        let s = Event::Stats(Stats {
            packets: 1,
            access_units: 2,
            bytes: 3,
            lost: 4,
            dropped_units: 5,
            keyframes: 6,
            plis: 7,
            rtcp: 8,
            b_slices: 9,
            edge_downs: 10,
        });
        assert_eq!(
            String::from_utf8(Frame::control(&s).payload).unwrap(),
            r#"{"event":"stats","packets":1,"access_units":2,"bytes":3,"lost":4,"dropped_units":5,"keyframes":6,"plis":7,"rtcp":8,"b_slices":9,"edge_downs":10}"#
        );
        let e = Event::Error { message: "boom \"x\"".into() };
        assert_eq!(
            String::from_utf8(Frame::control(&e).payload).unwrap(),
            r#"{"event":"error","message":"boom \"x\""}"#
        );
    }

    fn sample_config() -> Event {
        Event::Config {
            codec: "avc1.640033".into(),
            width: 1206,
            height: 2622,
            coded_width: 1216,
            coded_height: 2656,
            fps: 60,
            clock_hz: 24000,
            clock_source: "negotiated".into(),
            ssrc: 1,
            device_ssrc: 2,
            rotation: Some(90),
            point_scale: Some(3.0),
            current_orientation: Some("rot90".into()),
            native_orientation: Some("rot0".into()),
        }
    }

    #[test]
    fn only_the_config_event_carries_the_protocol_version() {
        assert_eq!(PROTOCOL_VERSION, 1, "the documented version");
        let config = String::from_utf8(Frame::control(&sample_config()).payload).unwrap();
        assert!(config.starts_with(r#"{"event":"config","codec":"avc1.640033""#), "{config}");
        assert!(config.ends_with(r#","protocol":1}"#), "{config}");
        let value: serde_json::Value = serde_json::from_str(&config).unwrap();
        assert_eq!(value["protocol"], 1);
        for e in [Event::Stopped, Event::Error { message: "x".into() }, Event::Hid(HidEvent::default())] {
            let json = String::from_utf8(Frame::control(&e).payload).unwrap();
            assert!(!json.contains("protocol"), "{json}");
        }
    }

    #[test]
    fn events_read_back_as_the_values_that_were_written() {
        let events = vec![
            sample_config(),
            Event::Stats(Stats { packets: 1, access_units: 2, bytes: 3, edge_downs: 10, ..Stats::default() }),
            Event::Hid(HidEvent {
                ready: Some(true),
                keyboard: Some(false),
                message: Some("the keyboard did not open".into()),
                mapping: Some("auto".into()),
                swap_xy: Some(true),
                flip_x: Some(true),
                flip_y: Some(false),
                ..HidEvent::default()
            }),
            Event::Hid(HidEvent { ok: Some(false), message: Some("bad".into()), ..HidEvent::default() }),
            Event::Error { message: "boom".into() },
            Event::Stopped,
        ];
        for e in events {
            let frame = Frame::control(&e);
            let back: Event = serde_json::from_slice(&frame.payload).unwrap();
            assert_eq!(back, e);
            // Written again, a read-back event is the same bytes: a program may pass events through.
            assert_eq!(Frame::control(&back), frame);
        }
    }

    #[test]
    fn a_frame_written_from_its_parts_is_the_frame_written_whole() {
        for f in [
            Frame::unit(40_000, true, true, vec![0, 0, 0, 1, 0x65, 1, 2, 3]),
            Frame::unit(80_000, false, false, vec![9; 100]),
            Frame::control(&Event::Stopped),
        ] {
            let mut parts = Vec::new();
            write_frame(&mut parts, f.pts_us, f.flags, &f.payload).unwrap();
            assert_eq!(parts, f.encode().unwrap());
        }
        assert_eq!(unit_flags(true, true), FLAG_KEY | FLAG_PARAMS);
        assert_eq!(unit_flags(false, false), 0);
        let mut sink = Vec::new();
        assert!(write_frame(&mut sink, 0, 0, &vec![0; MAX_PAYLOAD + 1]).is_err());
        assert!(sink.is_empty());
    }

    #[test]
    fn oversized_payload_is_refused_and_nothing_is_written() {
        let f = Frame::unit(0, false, false, vec![0; MAX_PAYLOAD + 1]);
        let mut sink = Vec::new();
        let err = f.write_to(&mut sink).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(sink.is_empty());
        // Exactly the limit is fine.
        let ok = Frame::unit(0, false, false, vec![0; MAX_PAYLOAD]);
        assert_eq!(ok.encode().unwrap().len(), HEADER_LEN + MAX_PAYLOAD);
    }

    #[test]
    fn decode_round_trips_a_stream_of_frames() {
        let frames = vec![
            Frame::control(&Event::Stopped),
            Frame::unit(40_000, true, true, vec![0, 0, 0, 1, 0x40, 0x01]),
            Frame::unit(80_000, false, false, vec![9; 100]),
        ];
        let mut bytes = Vec::new();
        for f in &frames {
            f.write_to(&mut bytes).unwrap();
        }
        assert_eq!(decode_all(&bytes).unwrap(), frames);
        assert!(decode(&bytes[..5]).is_err());
    }
}
