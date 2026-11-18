//! The codec probe: start the stream with an offer that names only H.264, then for a few seconds
//! count what arrives (RTP packets per payload type, the first payload bytes, an NAL-type histogram by
//! H.264 rules, the first SPS) and report whether the device really sent H.264.
//!
//! Nothing here depacketizes or writes frames: it is a measurement, not a pipeline.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::time::Duration;

use idevice::core_device::{RtpPacket, is_rtcp};
use tokio::sync::{Notify, mpsc};

use crate::avc::{self, parse_avc_sps};
use crate::commands::Command;
use crate::session::DatagramSocket;

/// The payload type the stock offer gives H.264.
pub const H264_PAYLOAD_TYPE: u8 = 123;
/// The payload type the stock offer gives HEVC.
pub const HEVC_PAYLOAD_TYPE: u8 = 100;
/// How many of the first packets have their first payload bytes logged.
const FIRST_PACKETS: usize = 10;
/// How many payload bytes of each.
const FIRST_BYTES: usize = 12;
/// RTP packets whose payload starts with a byte that is not one of the usual H.264 kinds are logged in
/// full, up to this many payload bytes each and this many packets.
const ODD_BYTES: usize = 400;
const ODD_PACKETS: usize = 200;
/// The payload kinds (first byte & 0x1f) that are not odd: a non-IDR slice, an IDR slice, an FU-A.
const USUAL_KINDS: [u8; 3] = [1, 5, 28];

/// An RTP packet with an unusual payload kind (a config packet, SEI, an STAP-A, anything but a slice
/// or a fragment), kept whole enough to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OddPacket {
    pub payload_type: u8,
    pub seq: u16,
    pub timestamp: u32,
    pub marker: bool,
    pub ssrc: u32,
    /// The whole datagram's length.
    pub datagram_len: usize,
    /// The payload's length and its first `ODD_BYTES` (400) bytes.
    pub payload_len: usize,
    pub payload_head: Vec<u8>,
}

#[derive(Debug, Clone, Default)]
pub struct ProbeReport {
    pub window: Duration,
    /// RTP packets per payload type.
    pub rtp_by_pt: BTreeMap<u8, u64>,
    pub rtcp_packets: u64,
    /// Datagrams that were neither RTP nor RTCP.
    pub junk: u64,
    /// The first packets: (payload type, sequence number, the first payload bytes).
    pub first_payloads: Vec<(u8, u16, Vec<u8>)>,
    /// Payload kinds by H.264 rules (the first byte & 0x1f) of the packets that are not HEVC.
    pub packet_kinds: BTreeMap<u8, u64>,
    /// NAL unit types by H.264 rules of the packets that are not HEVC.
    pub nal_types: BTreeMap<u8, u64>,
    /// NAL unit types by HEVC rules of the packets of payload type 100 (to read an HEVC answer).
    pub hevc_nal_types: BTreeMap<u8, u64>,
    /// The first H.264 SPS NAL unit seen whole.
    pub sps: Option<Vec<u8>>,
    /// The packets whose payload kind is not 1, 5 or 28 (up to a cap), in arrival order.
    pub odd: Vec<OddPacket>,
    /// How many such packets there were in all.
    pub odd_total: u64,
    /// Datagrams and bytes written to the raw dump, if there was one.
    pub raw_datagrams: u64,
    pub raw_bytes: u64,
}

impl ProbeReport {
    pub fn new(window: Duration) -> Self {
        ProbeReport { window, ..Default::default() }
    }

    /// Takes one datagram from the device into the counts.
    pub fn on_datagram(&mut self, data: &[u8]) {
        if is_rtcp(data) {
            self.rtcp_packets += 1;
            return;
        }
        let Some(pkt) = RtpPacket::parse(data) else {
            self.junk += 1;
            return;
        };
        *self.rtp_by_pt.entry(pkt.payload_type).or_default() += 1;
        if self.first_payloads.len() < FIRST_PACKETS {
            let head = pkt.payload.iter().take(FIRST_BYTES).copied().collect();
            self.first_payloads.push((pkt.payload_type, pkt.sequence_number, head));
        }
        if pkt.payload_type == HEVC_PAYLOAD_TYPE {
            // HEVC: single NAL, aggregation (48) and fragmentation (49) by RFC 7798.
            if let Some(&b0) = pkt.payload.first() {
                *self.hevc_nal_types.entry((b0 >> 1) & 0x3f).or_default() += 1;
            }
            return;
        }
        let scan = avc::scan_payload(pkt.payload);
        if let Some(kind) = scan.kind {
            *self.packet_kinds.entry(kind).or_default() += 1;
        }
        if scan.kind.is_none_or(|k| !USUAL_KINDS.contains(&k)) {
            self.odd_total += 1;
            if self.odd.len() < ODD_PACKETS {
                self.odd.push(OddPacket {
                    payload_type: pkt.payload_type,
                    seq: pkt.sequence_number,
                    timestamp: pkt.timestamp,
                    marker: pkt.marker,
                    ssrc: pkt.ssrc,
                    datagram_len: data.len(),
                    payload_len: pkt.payload.len(),
                    payload_head: pkt.payload.iter().take(ODD_BYTES).copied().collect(),
                });
            }
        }
        for t in scan.nal_types {
            *self.nal_types.entry(t).or_default() += 1;
        }
        if self.sps.is_none() {
            self.sps = scan.sps;
        }
    }

    fn total_rtp(&self) -> u64 {
        self.rtp_by_pt.values().sum()
    }

    /// The report as log lines.
    pub fn lines(&self) -> Vec<String> {
        let mut out = vec![format!(
            "{} RTP packets in {} s ({} RTCP, {} neither); by payload type: {}",
            self.total_rtp(),
            self.window.as_secs(),
            self.rtcp_packets,
            self.junk,
            fmt_counts(&self.rtp_by_pt)
        )];
        for (pt, seq, head) in &self.first_payloads {
            out.push(format!("packet pt {pt} seq {seq}: {}", hex(head)));
        }
        if !self.packet_kinds.is_empty() {
            out.push(format!(
                "H.264 payload kinds (first byte & 0x1f; 1..23 single NAL unit, 24 STAP-A, 28 FU-A): {}",
                fmt_counts(&self.packet_kinds)
            ));
            out.push(format!(
                "H.264 NAL unit types (5 IDR, 7 SPS, 8 PPS, 1 other slice; FU-A counted at its start): {}",
                fmt_counts(&self.nal_types)
            ));
        }
        if !self.hevc_nal_types.is_empty() {
            out.push(format!(
                "HEVC NAL unit types of payload type 100 packets (by the first byte; 48 aggregation, 49 fragment): {}",
                fmt_counts(&self.hevc_nal_types)
            ));
        }
        if self.odd_total > 0 {
            out.push(format!(
                "{} packets have a payload kind other than 1, 5 and 28 (logged in full below, {} shown)",
                self.odd_total,
                self.odd.len()
            ));
        }
        for p in &self.odd {
            out.push(format!(
                "odd packet pt {} seq {} ts {} marker {} ssrc {:#x}, datagram {} bytes, payload {} bytes{}: {}",
                p.payload_type,
                p.seq,
                p.timestamp,
                u8::from(p.marker),
                p.ssrc,
                p.datagram_len,
                p.payload_len,
                if p.payload_len > p.payload_head.len() {
                    format!(" (first {} shown)", p.payload_head.len())
                } else {
                    String::new()
                },
                hex(&p.payload_head)
            ));
        }
        if self.raw_datagrams > 0 {
            out.push(format!("raw dump: {} datagrams, {} bytes of datagram data", self.raw_datagrams, self.raw_bytes));
        }
        match &self.sps {
            None => out.push("no H.264 SPS seen whole".to_string()),
            Some(nal) => match parse_avc_sps(nal) {
                Ok(s) => out.push(format!(
                    "H.264 SPS: profile_idc {}, constraint flags {:#04x} ({}), level_idc {} ({}), {}x{}, chroma_format_idc {}, {}, codec string {}",
                    s.profile_idc,
                    s.constraint_flags,
                    s.constraint_names(),
                    s.level_idc,
                    fmt_level(s.level_idc),
                    s.width,
                    s.height,
                    s.chroma_format_idc,
                    if s.frame_mbs_only { "progressive" } else { "may be interlaced" },
                    s.codec_string()
                )),
                Err(e) => out.push(format!("H.264 SPS found but not readable ({e}): {}", hex(nal))),
            },
        }
        out
    }

    /// Did the device send H.264? `Ok` carries a one-line summary, `Err` the reason it did not.
    pub fn verdict(&self) -> Result<String, String> {
        let total = self.total_rtp();
        if total == 0 {
            return Err(format!(
                "no RTP packets arrived in {} s ({} RTCP, {} other datagrams)",
                self.window.as_secs(),
                self.rtcp_packets,
                self.junk
            ));
        }
        let (&pt, &n) = self.rtp_by_pt.iter().max_by_key(|(_, n)| **n).expect("there are packets");
        let looks_h264 = self.nal_types.contains_key(&avc::NAL_SPS) || self.nal_types.contains_key(&avc::NAL_IDR);
        match pt {
            H264_PAYLOAD_TYPE => {
                Ok(format!("the device sent H.264: {n} of {total} RTP packets have payload type {H264_PAYLOAD_TYPE}"))
            }
            HEVC_PAYLOAD_TYPE => Err(format!(
                "the device answered with HEVC (payload type {HEVC_PAYLOAD_TYPE}: {n} of {total} RTP packets) although the offer named only H.264"
            )),
            other if looks_h264 => Ok(format!(
                "the packets have payload type {other} (not the offered {H264_PAYLOAD_TYPE}) but carry H.264 NAL units (SPS or IDR seen)"
            )),
            other => Err(format!(
                "the packets have payload type {other}, which is neither H.264 ({H264_PAYLOAD_TYPE}) nor HEVC ({HEVC_PAYLOAD_TYPE}), and no H.264 SPS or IDR was seen"
            )),
        }
    }
}

fn fmt_counts(m: &BTreeMap<u8, u64>) -> String {
    if m.is_empty() {
        return "none".to_string();
    }
    m.iter().map(|(k, v)| format!("{k}: {v}")).collect::<Vec<_>>().join(", ")
}

fn fmt_level(level_idc: u8) -> String {
    format!("level {}.{}", level_idc / 10, level_idc % 10)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(" ")
}

/// How the probe window ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The whole window ran.
    Finished,
    /// A `stop` (or end of input, or a signal) cut it short.
    Stopped,
    /// Nobody reads stdout any more.
    ConsumerGone,
    /// The socket failed.
    Failed(String),
}

/// Writes one datagram to the raw dump: its length as 4 bytes big-endian, then the bytes.
pub fn write_raw(out: &mut dyn Write, data: &[u8]) -> io::Result<()> {
    let len =
        u32::try_from(data.len()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "datagram too long"))?;
    out.write_all(&len.to_be_bytes())?;
    out.write_all(data)
}

/// Counts what the device sends for `window` (or until a stop). Every datagram, RTP and RTCP, is also
/// written to `raw` if there is one (a write error is logged once and ends the dump).
pub async fn collect<S: DatagramSocket>(
    sock: &S,
    window: Duration,
    commands: &mut mpsc::Receiver<Command>,
    consumer_gone: &Notify,
    mut raw: Option<&mut dyn Write>,
) -> (ProbeReport, Outcome) {
    let mut report = ProbeReport::new(window);
    let end = tokio::time::sleep(window);
    tokio::pin!(end);
    let outcome = loop {
        tokio::select! {
            _ = &mut end => break Outcome::Finished,
            dg = sock.recv() => match dg {
                Ok(dg) => {
                    if let Some(out) = raw.as_deref_mut() {
                        match write_raw(out, &dg.data) {
                            Ok(()) => {
                                report.raw_datagrams += 1;
                                report.raw_bytes += dg.data.len() as u64;
                            }
                            Err(e) => {
                                tracing::warn!("the raw dump stopped: {e}");
                                raw = None;
                            }
                        }
                    }
                    report.on_datagram(&dg.data);
                }
                Err(e) => break Outcome::Failed(format!("the display stream's socket failed: {e}")),
            },
            cmd = commands.recv() => match cmd {
                Some(Command::Stop) | None => break Outcome::Stopped,
                Some(_) => {} // nothing else means anything in a probe
            },
            _ = consumer_gone.notified() => break Outcome::ConsumerGone,
        }
    };
    (report, outcome)
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::cast_possible_truncation,
        reason = "the tests number packets, bytes and times with small loop counters, so a cast does not truncate or lose precision"
    )]
    use std::io;
    use std::sync::Arc;

    use tokio::sync::Mutex as AsyncMutex;

    use super::*;
    use crate::avc::testutil::AvcSpsSpec;
    use crate::depack::testutil::rtp_bytes;
    use crate::session::Datagram;

    fn rtp(pt: u8, seq: u16, payload: &[u8]) -> Vec<u8> {
        let mut p = rtp_bytes(seq, 1000, false, 0xD00D, payload);
        p[1] = pt; // the marker bit stays clear
        p
    }

    fn stap_a(nals: &[&[u8]]) -> Vec<u8> {
        let mut v = vec![0x18];
        for n in nals {
            v.extend_from_slice(&(n.len() as u16).to_be_bytes());
            v.extend_from_slice(n);
        }
        v
    }

    /// What an H.264 stream starts with: sets and an IDR in a STAP-A, an IDR in fragments, delta slices.
    fn h264_stream(pt: u8) -> Vec<Vec<u8>> {
        let sps = AvcSpsSpec::high_1080p().nal();
        let pps = [0x68, 0xEE, 0x3C, 0x80];
        let idr = [0x65, 1, 2, 3, 4];
        vec![
            rtp(pt, 1, &stap_a(&[&sps, &pps])),
            rtp(pt, 2, &[0x7C, 0x85, 1, 2, 3]),
            rtp(pt, 3, &[0x7C, 0x05, 4, 5]),
            rtp(pt, 4, &[0x7C, 0x45, 6]),
            rtp(pt, 5, &[0x41, 0x9A, 1]),
            rtp(pt, 6, &[0x41, 0x9A, 2]),
            rtp(pt, 7, &idr),
        ]
    }

    #[test]
    fn an_h264_stream_is_counted_logged_and_accepted() {
        let mut r = ProbeReport::new(Duration::from_secs(5));
        for d in h264_stream(123) {
            r.on_datagram(&d);
        }
        // RTCP and junk are counted apart.
        r.on_datagram(&[0x80, 200, 0, 6, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        r.on_datagram(&[1, 2, 3]);
        assert_eq!(r.rtp_by_pt, BTreeMap::from([(123, 7)]));
        assert_eq!((r.rtcp_packets, r.junk), (1, 1));
        // Packet kinds: STAP-A once, FU-A three times, single slices (1) twice and an IDR (5) once.
        assert_eq!(r.packet_kinds, BTreeMap::from([(1, 2), (5, 1), (24, 1), (28, 3)]));
        // NAL unit types: SPS and PPS from the STAP-A, the FU-A IDR at its start, the single IDR, two slices.
        assert_eq!(r.nal_types, BTreeMap::from([(1, 2), (5, 2), (7, 1), (8, 1)]));
        assert!(r.hevc_nal_types.is_empty());
        assert!(r.verdict().unwrap().contains("the device sent H.264"));
        assert_eq!(r.first_payloads.len(), 7);
        assert_eq!(r.first_payloads[1], (123, 2, vec![0x7C, 0x85, 1, 2, 3]));

        let text = r.lines().join("\n");
        assert!(text.contains("by payload type: 123: 7"), "{text}");
        assert!(text.contains("packet pt 123 seq 2: 7c 85 01 02 03"), "{text}");
        assert!(text.contains("profile_idc 100"), "{text}");
        assert!(text.contains("level_idc 40 (level 4.0)"), "{text}");
        assert!(text.contains("1920x1080"), "{text}");
        assert!(text.contains("avc1.640028"), "{text}");
    }

    #[test]
    fn only_the_first_twelve_bytes_of_the_first_ten_packets_are_kept() {
        let mut r = ProbeReport::new(Duration::from_secs(1));
        for seq in 0..30u16 {
            r.on_datagram(&rtp(123, seq, &[0x41; 100]));
        }
        assert_eq!(r.first_payloads.len(), 10);
        assert!(r.first_payloads.iter().all(|(_, _, h)| h.len() == 12));
        assert_eq!(r.rtp_by_pt[&123], 30);
    }

    #[test]
    fn an_hevc_answer_is_an_error_that_says_so() {
        let mut r = ProbeReport::new(Duration::from_secs(5));
        // HEVC: an aggregation packet (type 48) and an IDR fragment (type 49).
        for seq in 0..20u16 {
            r.on_datagram(&rtp(100, seq, &[0x60, 0x01, 0, 4, 0x40, 1]));
        }
        r.on_datagram(&rtp(100, 20, &[0x62, 0x01, 0x93, 1, 2]));
        r.on_datagram(&rtp(123, 21, &[0x41, 1]));
        let err = r.verdict().unwrap_err();
        assert!(err.contains("answered with HEVC"), "{err}");
        assert!(err.contains("21 of 22"), "{err}");
        assert_eq!(r.hevc_nal_types, BTreeMap::from([(48, 20), (49, 1)]));
        // The H.264 histogram only counted the one packet that was not payload type 100.
        assert_eq!(r.nal_types, BTreeMap::from([(1, 1)]));
        assert!(r.lines().join("\n").contains("HEVC NAL unit types"));
    }

    #[test]
    fn no_packets_and_odd_payload_types_are_errors() {
        let r = ProbeReport::new(Duration::from_secs(3));
        assert!(r.verdict().unwrap_err().contains("no RTP packets arrived in 3 s"));
        // Another payload type with nothing H.264-like in it.
        let mut r = ProbeReport::new(Duration::from_secs(3));
        r.on_datagram(&rtp(96, 1, &[0x41, 1, 2]));
        assert!(r.verdict().unwrap_err().contains("payload type 96"));
        // Another payload type, but with an SPS: H.264 under a different number is worth saying so.
        let sps = AvcSpsSpec::baseline_vga().nal();
        let mut r = ProbeReport::new(Duration::from_secs(3));
        r.on_datagram(&rtp(96, 1, &sps));
        let ok = r.verdict().unwrap();
        assert!(ok.contains("payload type 96") && ok.contains("H.264 NAL units"), "{ok}");
        // An SPS that cannot be read is logged, not fatal.
        let mut r = ProbeReport::new(Duration::from_secs(3));
        r.on_datagram(&rtp(123, 1, &[0x67, 66, 0, 30, 0xFF]));
        assert!(r.lines().join("\n").contains("not readable"));
        assert!(r.verdict().is_ok());
    }

    #[test]
    fn garbage_datagrams_never_panic() {
        let mut r = ProbeReport::new(Duration::from_secs(1));
        let mut x: u64 = 0x1234_5678_9ABC_DEF0;
        for _ in 0..5000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let len = (x % 60) as usize;
            let d: Vec<u8> = (0..len).map(|i| (x >> (i % 8 * 8)) as u8 ^ i as u8).collect();
            r.on_datagram(&d);
            let mut v = vec![0x80, 123];
            v.extend_from_slice(&d);
            r.on_datagram(&v);
        }
        let _ = r.lines();
        let _ = r.verdict();
    }

    /// The shape of the iPad's first packet: a proprietary payload kind 18 that starts with a box size and `avc1`.
    fn config_packet() -> Vec<u8> {
        let mut p = vec![0x92, 0xE6, 0xC0, 0xA3, 0x00, 0x00, 0x00, 0x86, b'a', b'v', b'c', b'1'];
        p.extend((0..600u32).map(|i| (i % 251) as u8));
        p
    }

    #[test]
    fn packets_with_an_unusual_payload_kind_are_logged_in_full() {
        let mut r = ProbeReport::new(Duration::from_secs(8));
        let config = config_packet();
        r.on_datagram(&rtp(123, 7, &config));
        // The usual kinds (non-IDR slice 1, IDR slice 5, FU-A 28) are not odd; a STAP-A (24) and an SEI (6) are.
        r.on_datagram(&rtp(123, 8, &[0x41, 1, 2]));
        r.on_datagram(&rtp(123, 9, &[0x65, 1, 2]));
        r.on_datagram(&rtp(123, 10, &[0x7C, 0x85, 1]));
        r.on_datagram(&rtp(123, 11, &stap_a(&[&[0x68, 1, 2]])));
        r.on_datagram(&rtp(123, 12, &[0x06, 5, 5]));
        assert_eq!(r.odd_total, 3);
        assert_eq!(r.odd.iter().map(|p| p.seq).collect::<Vec<_>>(), vec![7, 11, 12]);
        let first = &r.odd[0];
        assert_eq!((first.payload_type, first.ssrc, first.timestamp), (123, 0xD00D, 1000));
        assert_eq!(first.payload_len, config.len());
        assert_eq!(first.payload_head.len(), 400);
        assert_eq!(first.payload_head[..], config[..400]);
        let text = r.lines().join("\n");
        assert!(text.contains("3 packets have a payload kind other than 1, 5 and 28"), "{text}");
        assert!(text.contains("odd packet pt 123 seq 7 ts 1000 marker 0 ssrc 0xd00d"), "{text}");
        assert!(text.contains("payload 612 bytes (first 400 shown): 92 e6 c0 a3 00 00 00 86 61 76 63 31"), "{text}");
        // A short odd packet is shown whole, without a note.
        assert!(text.contains("payload 3 bytes: 06 05 05"), "{text}");
    }

    #[test]
    fn the_odd_packet_log_is_capped() {
        let mut r = ProbeReport::new(Duration::from_secs(1));
        for seq in 0..1000u16 {
            r.on_datagram(&rtp(123, seq, &[0x06, 1]));
        }
        assert_eq!(r.odd_total, 1000);
        assert_eq!(r.odd.len(), 200);
        assert!(
            r.lines()
                .join("\n")
                .contains("1000 packets have a payload kind other than 1, 5 and 28 (logged in full below, 200 shown)")
        );
    }

    /// Reads a raw dump back: 4-byte big-endian length, then the datagram.
    fn read_raw(mut b: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        while !b.is_empty() {
            let n = u32::from_be_bytes(b[..4].try_into().unwrap()) as usize;
            out.push(b[4..4 + n].to_vec());
            b = &b[4 + n..];
        }
        out
    }

    #[test]
    fn raw_dump_frames_are_length_prefixed_big_endian() {
        let mut sink = Vec::new();
        write_raw(&mut sink, &[1, 2, 3]).unwrap();
        write_raw(&mut sink, &[]).unwrap();
        write_raw(&mut sink, &[9; 300]).unwrap();
        assert_eq!(&sink[..7], &[0, 0, 0, 3, 1, 2, 3]);
        assert_eq!(&sink[7..11], &[0, 0, 0, 0]);
        assert_eq!(&sink[11..15], &[0, 0, 1, 44]);
        assert_eq!(read_raw(&sink), vec![vec![1, 2, 3], vec![], vec![9; 300]]);
    }

    // ---- collect, on a fake socket and a paused clock ---------------------------------------

    #[tokio::test(start_paused = true)]
    async fn every_datagram_rtp_and_rtcp_goes_to_the_raw_dump_in_order() {
        let (sock, tx) = fake();
        let sr = vec![0x80, 200, 0, 6, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let mut sent = vec![rtp(123, 1, &config_packet()), sr.clone(), vec![1, 2, 3]];
        sent.extend(h264_stream(123));
        feed(&tx, sent.clone());
        let (_ctx, mut crx) = mpsc::channel(4);
        let gone = Notify::new();
        let mut dump: Vec<u8> = Vec::new();
        let (report, outcome) = collect(&sock, Duration::from_secs(2), &mut crx, &gone, Some(&mut dump)).await;
        assert_eq!(outcome, Outcome::Finished);
        assert_eq!(read_raw(&dump), sent, "all of them, junk and RTCP too, byte for byte");
        assert_eq!(report.raw_datagrams, sent.len() as u64);
        assert_eq!(report.raw_bytes, sent.iter().map(|d| d.len() as u64).sum::<u64>());
        assert_eq!(report.rtcp_packets, 1);
        assert!(report.lines().join("\n").contains(&format!("raw dump: {} datagrams", sent.len())));
        // The counts are the same as without a dump.
        assert_eq!(report.rtp_by_pt[&123], 1 + 7);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failing_dump_ends_the_dump_not_the_probe() {
        struct FailsAfter(usize);
        impl Write for FailsAfter {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                if self.0 < b.len() {
                    return Err(io::Error::new(io::ErrorKind::StorageFull, "disk full"));
                }
                self.0 -= b.len();
                Ok(b.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let (sock, tx) = fake();
        feed(&tx, h264_stream(123));
        let (_ctx, mut crx) = mpsc::channel(4);
        let gone = Notify::new();
        let mut dump = FailsAfter(100);
        let (report, outcome) = collect(&sock, Duration::from_secs(2), &mut crx, &gone, Some(&mut dump)).await;
        assert_eq!(outcome, Outcome::Finished);
        assert!(report.raw_datagrams < 7, "the dump stopped early: {}", report.raw_datagrams);
        assert_eq!(report.rtp_by_pt[&123], 7, "every packet was still counted");
    }

    struct FakeSocket {
        rx: AsyncMutex<mpsc::UnboundedReceiver<io::Result<Datagram>>>,
    }

    impl DatagramSocket for FakeSocket {
        async fn recv(&self) -> io::Result<Datagram> {
            match self.rx.lock().await.recv().await {
                Some(r) => r,
                None => std::future::pending().await,
            }
        }

        async fn send_to(&self, _: u16, _: Vec<u8>) -> io::Result<()> {
            Ok(())
        }
    }

    fn fake() -> (FakeSocket, mpsc::UnboundedSender<io::Result<Datagram>>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (FakeSocket { rx: AsyncMutex::new(rx) }, tx)
    }

    fn feed(tx: &mpsc::UnboundedSender<io::Result<Datagram>>, datagrams: Vec<Vec<u8>>) {
        for data in datagrams {
            tx.send(Ok(Datagram { src_port: 50001, data })).unwrap();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_window_runs_its_length_then_the_verdict_is_read() {
        let (sock, tx) = fake();
        feed(&tx, h264_stream(123));
        let (_ctx, mut crx) = mpsc::channel(4);
        let gone = Arc::new(Notify::new());
        let t0 = tokio::time::Instant::now();
        let (report, outcome) = collect(&sock, Duration::from_secs(5), &mut crx, &gone, None).await;
        assert_eq!(outcome, Outcome::Finished);
        assert_eq!(tokio::time::Instant::now() - t0, Duration::from_secs(5));
        assert_eq!(report.rtp_by_pt[&123], 7);
        assert!(report.verdict().is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn a_stop_cuts_the_window_short() {
        let (sock, tx) = fake();
        feed(&tx, h264_stream(123));
        let (ctx, mut crx) = mpsc::channel(4);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            ctx.send(Command::Stop).await.unwrap();
        });
        let gone = Notify::new();
        let t0 = tokio::time::Instant::now();
        let (report, outcome) = collect(&sock, Duration::from_secs(60), &mut crx, &gone, None).await;
        assert_eq!(outcome, Outcome::Stopped);
        assert_eq!(tokio::time::Instant::now() - t0, Duration::from_secs(1));
        assert_eq!(report.rtp_by_pt[&123], 7);
    }

    #[tokio::test(start_paused = true)]
    async fn a_socket_error_ends_the_probe_and_other_commands_are_ignored() {
        let (sock, tx) = fake();
        let (ctx, mut crx) = mpsc::channel(4);
        ctx.send(Command::Keyframe).await.unwrap();
        tx.send(Err(io::Error::new(io::ErrorKind::BrokenPipe, "adapter closed"))).unwrap();
        let gone = Notify::new();
        let (_, outcome) = collect(&sock, Duration::from_secs(5), &mut crx, &gone, None).await;
        match outcome {
            Outcome::Failed(m) => assert!(m.contains("adapter closed"), "{m}"),
            other => panic!("{other:?}"),
        }
        // A gone consumer ends it too.
        let (sock, _tx) = fake();
        let (_ctx, mut crx) = mpsc::channel(4);
        let gone = Notify::new();
        gone.notify_one();
        let (_, outcome) = collect(&sock, Duration::from_secs(5), &mut crx, &gone, None).await;
        assert_eq!(outcome, Outcome::ConsumerGone);
    }
}
