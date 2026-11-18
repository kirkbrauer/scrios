//! RTP/HEVC (RFC 7798) depacketization into Annex-B access units.
//!
//! Packets are taken in arrival order. The transport (a USB tunnel) preserves order, so there is
//! no reorder buffer: a gap in the sequence numbers is a loss, and a packet behind the cursor is a
//! late duplicate that is ignored.
//!
//! A unit is the run of packets with one RTP timestamp. The marker bit ends it; a packet with a
//! different timestamp ends it too (a device that does not set the marker still works, one packet
//! later). A unit that lost a packet, or whose NAL units cannot be parsed, is reported as dropped
//! and never produced.
//!
//! The VPS, SPS and PPS seen in the stream are cached. Every unit with an IRAP picture is
//! produced with VPS, SPS and PPS as its first three NAL units, taken from the cache if the device
//! did not send them in that unit.

pub const NAL_VPS: u8 = 32;
pub const NAL_SPS: u8 = 33;
pub const NAL_PPS: u8 = 34;
pub const NAL_AUD: u8 = 35;
const NAL_AP: u8 = 48;
const NAL_FU: u8 = 49;

/// This many packets in a row that look far behind the cursor, one after the other, are not late
/// duplicates: the source jumped (it restarted with a new sequence base). The stream resyncs.
const RESYNC_AFTER: u8 = 3;
/// A packet this far or less behind the cursor is an ordinary late duplicate, never a jump.
const NEAR_LATE: u16 = 256;

const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// Units larger than this are dropped (the frame protocol's payload limit, less room for the
/// parameter sets that may be prepended).
pub const MAX_UNIT_BYTES: usize = crate::frame::MAX_PAYLOAD - 4096;

/// The NAL unit type of a NAL unit header's first byte.
pub fn nal_type(byte0: u8) -> u8 {
    (byte0 >> 1) & 0x3f
}

/// IRAP pictures: BLA, IDR and CRA (types 16..=21).
pub fn is_irap(nal_type: u8) -> bool {
    (16..=21).contains(&nal_type)
}

fn is_param_set(nal_type: u8) -> bool {
    (NAL_VPS..=NAL_PPS).contains(&nal_type)
}

/// What the assembler needs from an RTP header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketMeta {
    pub seq: u16,
    pub timestamp: u32,
    pub marker: bool,
}

/// One picture as Annex-B.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessUnit {
    /// The RTP timestamp.
    pub timestamp: u32,
    /// Holds an IRAP picture.
    pub key: bool,
    /// VPS, SPS and PPS are the first three NAL units of `payload`.
    pub params: bool,
    /// Annex-B, 4-byte start codes.
    pub payload: Vec<u8>,
    /// The SPS NAL unit (no start code) when `params`.
    pub sps: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    /// A packet of the unit (or the ones around it) was lost.
    Loss,
    /// A NAL unit or fragment could not be parsed.
    Malformed,
    /// The unit would exceed [`MAX_UNIT_BYTES`].
    TooLarge,
    /// An IRAP unit, and no VPS/SPS/PPS seen yet to put in front of it.
    MissingParams,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    Unit(AccessUnit),
    Dropped { timestamp: u32, reason: DropReason },
}

#[derive(Debug)]
struct NalRef {
    /// Offset of the NAL unit's first byte (after its start code) in `Building::buf`.
    start: usize,
    /// Offset one past its last byte; 0 while a fragmented NAL unit is still open.
    end: usize,
    typ: u8,
}

#[derive(Debug)]
struct Building {
    ts: u32,
    buf: Vec<u8>,
    nals: Vec<NalRef>,
    broken: Option<DropReason>,
    fu_open: bool,
    /// VPS, SPS, PPS completed in this unit: cached only if the unit turns out whole.
    params: [Option<Vec<u8>>; 3],
}

impl Building {
    fn new(ts: u32) -> Self {
        Building {
            ts,
            buf: Vec::with_capacity(8 * 1024),
            nals: Vec::new(),
            broken: None,
            fu_open: false,
            params: [None, None, None],
        }
    }

    fn mark(&mut self, reason: DropReason) {
        self.broken.get_or_insert(reason);
    }
}

/// Turns RTP payloads into access units.
#[derive(Debug)]
pub struct Assembler {
    expected: Option<u16>,
    cur: Option<Building>,
    /// A gap was seen before the next unit starts: that unit may be missing its head.
    after_gap: bool,
    /// The last packet that looked far behind the cursor, and how many such in a row.
    late_run: Option<(u16, u8)>,
    vps: Option<Vec<u8>>,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    max_unit_bytes: usize,
    lost: u64,
    late: u64,
    units_without_marker: u64,
}

impl Default for Assembler {
    fn default() -> Self {
        Self::new()
    }
}

impl Assembler {
    pub fn new() -> Self {
        Self::with_max_unit_bytes(MAX_UNIT_BYTES)
    }

    pub fn with_max_unit_bytes(max_unit_bytes: usize) -> Self {
        Assembler {
            expected: None,
            cur: None,
            after_gap: false,
            late_run: None,
            vps: None,
            sps: None,
            pps: None,
            max_unit_bytes,
            lost: 0,
            late: 0,
            units_without_marker: 0,
        }
    }

    /// RTP packets declared lost (gaps in the sequence numbers).
    pub fn lost(&self) -> u64 {
        self.lost
    }

    /// Packets ignored because they were behind the cursor (duplicates, reordered).
    pub fn late(&self) -> u64 {
        self.late
    }

    /// Units that ended by the next timestamp rather than by a marker bit.
    pub fn units_without_marker(&self) -> u64 {
        self.units_without_marker
    }

    pub fn has_parameter_sets(&self) -> bool {
        self.vps.is_some() && self.sps.is_some() && self.pps.is_some()
    }

    /// Forgets the sequence state, the unit in progress and the cached parameter sets (the stream
    /// restarted with a new source).
    pub fn reset(&mut self) {
        self.expected = None;
        self.cur = None;
        self.after_gap = false;
        self.late_run = None;
        self.vps = None;
        self.sps = None;
        self.pps = None;
    }

    /// Feeds one RTP packet; finished units and drops are appended to `out`.
    pub fn push(&mut self, meta: PacketMeta, payload: &[u8], out: &mut Vec<Output>) {
        let mut gap = false;
        if let Some(exp) = self.expected {
            let diff = meta.seq.wrapping_sub(exp);
            if diff >= 0x8000 {
                // Behind the cursor: a duplicate or a reordered packet whose place has passed ...
                self.late += 1;
                if diff > 0u16.wrapping_sub(NEAR_LATE) {
                    return; // ... and close behind it: nothing more to it
                }
                // ... or far behind. A few of those in a row, each following the last, are a
                // source that jumped (a restart with a new sequence base): start over from here,
                // as after a loss.
                let run = match self.late_run {
                    Some((last, n)) if meta.seq == last.wrapping_add(1) => n.saturating_add(1),
                    _ => 1,
                };
                self.late_run = Some((meta.seq, run));
                if run < RESYNC_AFTER {
                    return;
                }
                self.late_run = None;
                self.lost += 1;
                gap = true;
            } else {
                self.late_run = None;
                if diff != 0 {
                    self.lost += u64::from(diff);
                    gap = true;
                }
            }
        }
        self.expected = Some(meta.seq.wrapping_add(1));

        if gap {
            match self.cur.as_mut() {
                // The loss is inside the unit this packet belongs to.
                Some(c) if c.ts == meta.timestamp => {
                    c.mark(DropReason::Loss);
                    c.fu_open = false;
                }
                // The open unit never got its end; the new one may have lost its head.
                Some(c) => {
                    c.mark(DropReason::Loss);
                    c.fu_open = false;
                    self.after_gap = true;
                }
                None => self.after_gap = true,
            }
        }

        if self.cur.as_ref().is_some_and(|c| c.ts != meta.timestamp) {
            self.finish(out, false);
        }
        if self.cur.is_none() {
            let mut b = Building::new(meta.timestamp);
            if self.after_gap {
                b.mark(DropReason::Loss);
            }
            self.after_gap = false;
            self.cur = Some(b);
        }

        self.process_payload(payload);

        if meta.marker {
            self.finish(out, true);
        }
    }

    fn mark_broken(&mut self, reason: DropReason) {
        if let Some(c) = self.cur.as_mut() {
            c.mark(reason);
        }
    }

    fn process_payload(&mut self, p: &[u8]) {
        if p.len() < 2 || p[0] & 0x80 != 0 {
            self.mark_broken(DropReason::Malformed);
            return;
        }
        match nal_type(p[0]) {
            NAL_AP => self.aggregation(p),
            NAL_FU => self.fragmentation(p),
            _ => {
                self.close_dangling_fu();
                self.add_nal(p);
            }
        }
    }

    /// A NAL unit that is not a fragment arrived while a fragmented one was open: its end was lost.
    fn close_dangling_fu(&mut self) {
        if let Some(c) = self.cur.as_mut().filter(|c| c.fu_open) {
            c.mark(DropReason::Malformed);
            c.fu_open = false;
        }
    }

    /// AP (type 48): `[PayloadHdr][ u16 size, NAL unit ]+` (no DONL: not negotiated).
    fn aggregation(&mut self, p: &[u8]) {
        self.close_dangling_fu();
        let mut off = 2;
        while off < p.len() {
            let Some(size) = p.get(off..off + 2) else {
                self.mark_broken(DropReason::Malformed);
                return;
            };
            let size = usize::from(u16::from_be_bytes([size[0], size[1]]));
            off += 2;
            let Some(nal) = p.get(off..off + size) else {
                self.mark_broken(DropReason::Malformed);
                return;
            };
            if size < 2 {
                self.mark_broken(DropReason::Malformed);
                return;
            }
            self.add_nal(nal);
            off += size;
        }
    }

    /// FU (type 49): `[PayloadHdr][FU header: S|E|type(6)][fragment]`. The NAL unit header is rebuilt
    /// from the PayloadHdr (layer id, TID) with the FU type.
    fn fragmentation(&mut self, p: &[u8]) {
        if p.len() < 3 {
            self.mark_broken(DropReason::Malformed);
            return;
        }
        let layer_id = ((p[0] & 1) << 5) | (p[1] >> 3);
        if layer_id != 0 {
            return; // not the base layer
        }
        let fu = p[2];
        let (start, end, fu_type) = (fu & 0x80 != 0, fu & 0x40 != 0, fu & 0x3f);
        let fragment = &p[3..];
        if (start && end) || fu_type >= NAL_AP {
            self.mark_broken(DropReason::Malformed);
            return;
        }
        let max = self.max_unit_bytes;
        let Some(cur) = self.cur.as_mut() else { return };
        if cur.buf.len() + fragment.len() + 6 > max {
            cur.mark(DropReason::TooLarge);
            cur.fu_open = false;
            return;
        }

        if start {
            if cur.fu_open {
                cur.mark(DropReason::Malformed); // the previous one never ended
            }
            cur.buf.extend_from_slice(&START_CODE);
            let at = cur.buf.len();
            cur.buf.push((p[0] & 0x81) | (fu_type << 1));
            cur.buf.push(p[1]);
            cur.buf.extend_from_slice(fragment);
            cur.nals.push(NalRef {
                start: at,
                end: 0,
                typ: fu_type,
            });
            cur.fu_open = true;
        } else if cur.fu_open {
            cur.buf.extend_from_slice(fragment);
        } else {
            cur.mark(DropReason::Malformed); // a middle or end fragment without its start
            return;
        }

        if end && cur.fu_open {
            cur.fu_open = false;
            let n = cur.nals.last_mut().expect("an open fragment has an entry");
            n.end = cur.buf.len();
            let (s, e, t) = (n.start, n.end, n.typ);
            if is_param_set(t) {
                cur.params[usize::from(t - NAL_VPS)] = Some(cur.buf[s..e].to_vec());
            }
        }
    }

    /// Appends one complete NAL unit to the unit being built.
    fn add_nal(&mut self, nal: &[u8]) {
        if nal.len() < 2 {
            self.mark_broken(DropReason::Malformed);
            return;
        }
        let layer_id = ((nal[0] & 1) << 5) | (nal[1] >> 3);
        let typ = nal_type(nal[0]);
        if layer_id != 0 || typ >= 41 {
            return; // enhancement layers, reserved and unspecified types
        }
        if nal[1] & 7 == 0 {
            self.mark_broken(DropReason::Malformed); // nuh_temporal_id_plus1 is never 0
            return;
        }
        let max = self.max_unit_bytes;
        let Some(cur) = self.cur.as_mut() else { return };
        if cur.buf.len() + nal.len() + 4 > max {
            cur.mark(DropReason::TooLarge);
            return;
        }
        cur.buf.extend_from_slice(&START_CODE);
        let start = cur.buf.len();
        cur.buf.extend_from_slice(nal);
        cur.nals.push(NalRef {
            start,
            end: cur.buf.len(),
            typ,
        });
        if is_param_set(typ) {
            cur.params[usize::from(typ - NAL_VPS)] = Some(nal.to_vec());
        }
    }

    /// Ends the unit in progress.
    fn finish(&mut self, out: &mut Vec<Output>, by_marker: bool) {
        let Some(mut b) = self.cur.take() else { return };
        if b.fu_open {
            b.mark(DropReason::Malformed); // ended in the middle of a fragmented NAL unit
        }
        if let Some(reason) = b.broken {
            out.push(Output::Dropped {
                timestamp: b.ts,
                reason,
            });
            return;
        }
        // The unit is whole: what parameter sets it carried are good.
        for (slot, new) in [&mut self.vps, &mut self.sps, &mut self.pps]
            .into_iter()
            .zip(b.params.iter_mut())
        {
            if let Some(p) = new.take() {
                *slot = Some(p);
            }
        }
        if b.nals.is_empty() {
            return;
        }
        if !by_marker {
            self.units_without_marker += 1;
        }
        match self.build_unit(b) {
            Ok(unit) => out.push(Output::Unit(unit)),
            Err((timestamp, reason)) => out.push(Output::Dropped { timestamp, reason }),
        }
    }

    fn build_unit(&self, b: Building) -> Result<AccessUnit, (u32, DropReason)> {
        let key = b.nals.iter().any(|n| is_irap(n.typ));
        let has = |t: u8| b.nals.iter().any(|n| n.typ == t);
        let carries_all = has(NAL_VPS) && has(NAL_SPS) && has(NAL_PPS);

        if !key && !carries_all {
            return Ok(AccessUnit {
                timestamp: b.ts,
                key: false,
                params: false,
                payload: b.buf,
                sps: None,
            });
        }

        // The parameter sets that go in front: the cache already holds the unit's own copies.
        let (Some(vps), Some(sps), Some(pps)) = (&self.vps, &self.sps, &self.pps) else {
            if key {
                return Err((b.ts, DropReason::MissingParams));
            }
            return Ok(AccessUnit {
                timestamp: b.ts,
                key: false,
                params: false,
                payload: b.buf,
                sps: None,
            });
        };

        // An access unit delimiter, if the unit starts with one, stays the first NAL unit.
        let aud = usize::from(b.nals.first().is_some_and(|n| n.typ == NAL_AUD));
        let already_first = b.nals.len() >= aud + 3
            && b.nals[aud].typ == NAL_VPS
            && b.nals[aud + 1].typ == NAL_SPS
            && b.nals[aud + 2].typ == NAL_PPS
            && !b.nals[aud + 3..].iter().any(|n| is_param_set(n.typ));
        let payload = if already_first {
            b.buf
        } else {
            let rest = |i: &usize, n: &&NalRef| !(*i < aud) && !is_param_set(n.typ);
            let rest_len: usize = b
                .nals
                .iter()
                .enumerate()
                .filter(|(i, n)| rest(i, n))
                .map(|(_, n)| 4 + n.end - n.start)
                .sum();
            let aud_len = if aud == 1 { 4 + b.nals[0].end - b.nals[0].start } else { 0 };
            let mut p = Vec::with_capacity(12 + vps.len() + sps.len() + pps.len() + rest_len + aud_len);
            if aud == 1 {
                p.extend_from_slice(&START_CODE);
                p.extend_from_slice(&b.buf[b.nals[0].start..b.nals[0].end]);
            }
            for set in [vps, sps, pps] {
                p.extend_from_slice(&START_CODE);
                p.extend_from_slice(set);
            }
            for (_, n) in b.nals.iter().enumerate().filter(|(i, n)| rest(i, n)) {
                p.extend_from_slice(&START_CODE);
                p.extend_from_slice(&b.buf[n.start..n.end]);
            }
            p
        };
        if payload.len() > crate::frame::MAX_PAYLOAD {
            return Err((b.ts, DropReason::TooLarge));
        }
        Ok(AccessUnit {
            timestamp: b.ts,
            key,
            params: true,
            payload,
            sps: Some(sps.clone()),
        })
    }
}

/// RTP payload builders shared by the tests of this crate.
#[cfg(test)]
pub mod testutil {
    use super::*;

    pub const VPS: [u8; 6] = [0x40, 0x01, 0x0C, 0x01, 0xFF, 0xFF];
    pub const PPS: [u8; 5] = [0x44, 0x01, 0xC1, 0x72, 0xB4];
    /// An IDR_W_RADL slice (type 19), TID 0.
    pub fn idr(body: &[u8]) -> Vec<u8> {
        let mut v = vec![19 << 1, 0x01];
        v.extend_from_slice(body);
        v
    }
    /// A TRAIL_R slice (type 1).
    pub fn trail(body: &[u8]) -> Vec<u8> {
        let mut v = vec![1 << 1, 0x01];
        v.extend_from_slice(body);
        v
    }
    /// An SPS-typed NAL unit with an opaque body (use `sps::testutil` for a parseable one).
    pub fn fake_sps() -> Vec<u8> {
        vec![0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90]
    }

    pub fn ap(nals: &[&[u8]]) -> Vec<u8> {
        let mut v = vec![NAL_AP << 1, 0x01];
        for n in nals {
            v.extend_from_slice(&(n.len() as u16).to_be_bytes());
            v.extend_from_slice(n);
        }
        v
    }

    /// Splits `nal` (with its 2-byte header) into FU packets of at most `chunk` fragment bytes.
    pub fn fu(nal: &[u8], chunk: usize) -> Vec<Vec<u8>> {
        let t = nal_type(nal[0]);
        let body = &nal[2..];
        let parts: Vec<&[u8]> = body.chunks(chunk).collect();
        let n = parts.len();
        parts
            .into_iter()
            .enumerate()
            .map(|(i, part)| {
                let mut v = vec![(nal[0] & 0x81) | (NAL_FU << 1), nal[1]];
                let mut h = t;
                if i == 0 {
                    h |= 0x80;
                }
                if i == n - 1 {
                    h |= 0x40;
                }
                v.push(h);
                v.extend_from_slice(part);
                v
            })
            .collect()
    }

    pub fn annexb(nals: &[&[u8]]) -> Vec<u8> {
        let mut v = Vec::new();
        for n in nals {
            v.extend_from_slice(&START_CODE);
            v.extend_from_slice(n);
        }
        v
    }

    /// A tiny RTP packet writer for the tests that go through the byte parser.
    pub fn rtp_bytes(seq: u16, ts: u32, marker: bool, ssrc: u32, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![0x80, 100 | if marker { 0x80 } else { 0 }];
        v.extend_from_slice(&seq.to_be_bytes());
        v.extend_from_slice(&ts.to_be_bytes());
        v.extend_from_slice(&ssrc.to_be_bytes());
        v.extend_from_slice(payload);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;

    fn meta(seq: u16, ts: u32, marker: bool) -> PacketMeta {
        PacketMeta {
            seq,
            timestamp: ts,
            marker,
        }
    }

    fn feed(a: &mut Assembler, seq: u16, ts: u32, marker: bool, p: &[u8]) -> Vec<Output> {
        let mut out = Vec::new();
        a.push(meta(seq, ts, marker), p, &mut out);
        out
    }

    fn unit(o: &Output) -> &AccessUnit {
        match o {
            Output::Unit(u) => u,
            other => panic!("expected a unit, got {other:?}"),
        }
    }

    /// The first access unit as the phone sends it: an aggregation packet with VPS, SPS, PPS, then
    /// the IDR slice fragmented over three packets.
    fn first_idr(a: &mut Assembler, seq: u16, ts: u32) -> Vec<Output> {
        let sps = fake_sps();
        let mut out = feed(a, seq, ts, false, &ap(&[&VPS, &sps, &PPS]));
        let frags = fu(&idr(&[7; 30]), 12);
        assert_eq!(frags.len(), 3);
        for (i, f) in frags.iter().enumerate() {
            out.extend(feed(a, seq + 1 + i as u16, ts, i == 2, f));
        }
        out
    }

    #[test]
    fn a_single_nal_packet_with_the_marker_is_one_plain_delta_unit() {
        let mut a = Assembler::new();
        let nal = trail(&[1, 2, 3, 4]);
        let out = feed(&mut a, 10, 1000, true, &nal);
        assert_eq!(out.len(), 1);
        let u = unit(&out[0]);
        assert_eq!(u.timestamp, 1000);
        assert!(!u.key);
        assert!(!u.params);
        assert_eq!(u.sps, None);
        assert_eq!(u.payload, annexb(&[&nal]));
    }

    #[test]
    fn aggregation_and_fragmentation_make_a_key_unit_with_parameter_sets_in_front() {
        let mut a = Assembler::new();
        let out = first_idr(&mut a, 100, 5000);
        assert_eq!(out.len(), 1);
        let u = unit(&out[0]);
        assert!(u.key);
        assert!(u.params);
        let want = annexb(&[&VPS, &fake_sps(), &PPS, &idr(&[7; 30])]);
        assert_eq!(u.payload, want);
        assert_eq!(u.sps.as_deref(), Some(&fake_sps()[..]));
        assert_eq!(a.lost(), 0);
        assert!(a.has_parameter_sets());
    }

    #[test]
    fn multi_slice_pictures_keep_every_slice_in_one_unit() {
        let mut a = Assembler::new();
        let sps = fake_sps();
        let s1 = idr(&[1; 8]);
        let s2 = idr(&[2; 8]);
        let mut out = feed(&mut a, 1, 9, false, &ap(&[&VPS, &sps, &PPS]));
        out.extend(feed(&mut a, 2, 9, false, &s1));
        out.extend(feed(&mut a, 3, 9, true, &s2));
        assert_eq!(out.len(), 1);
        assert_eq!(unit(&out[0]).payload, annexb(&[&VPS, &sps, &PPS, &s1, &s2]));
    }

    #[test]
    fn a_key_unit_without_parameter_sets_gets_the_cached_ones_in_front() {
        let mut a = Assembler::new();
        first_idr(&mut a, 100, 5000);
        // A delta unit in between stays plain.
        let t = trail(&[9; 6]);
        let out = feed(&mut a, 104, 5533, true, &t);
        assert_eq!(unit(&out[0]).payload, annexb(&[&t]));
        assert!(!unit(&out[0]).params);
        // The next IDR comes with no VPS/SPS/PPS of its own (a PLI answer from a device that does
        // not repeat them): the cache is put in front.
        let idr2 = idr(&[8; 20]);
        let out = feed(&mut a, 105, 6000, true, &idr2);
        let u = unit(&out[0]);
        assert!(u.key && u.params);
        assert_eq!(u.payload, annexb(&[&VPS, &fake_sps(), &PPS, &idr2]));
        assert_eq!(u.sps.as_deref(), Some(&fake_sps()[..]));
    }

    #[test]
    fn a_key_unit_with_misordered_or_partial_sets_is_normalised() {
        let mut a = Assembler::new();
        let sps = fake_sps();
        first_idr(&mut a, 1, 100);
        // PPS, SEI-ish NAL (type 39), IDR, then the SPS and VPS after the slice.
        let sei = vec![39 << 1, 0x01, 0xAA];
        let slice = idr(&[3; 5]);
        let mut out = feed(&mut a, 5, 200, false, &ap(&[&PPS, &sei]));
        out.extend(feed(&mut a, 6, 200, false, &slice));
        out.extend(feed(&mut a, 7, 200, true, &ap(&[&sps, &VPS])));
        let u = unit(&out[0]);
        assert_eq!(u.payload, annexb(&[&VPS, &sps, &PPS, &sei, &slice]));
        assert!(u.key && u.params);
    }

    #[test]
    fn a_delta_unit_that_carries_all_three_sets_gets_them_first_and_the_params_flag() {
        let mut a = Assembler::new();
        let sps = fake_sps();
        let t = trail(&[1; 3]);
        let out = feed(&mut a, 1, 50, true, &ap(&[&t, &VPS, &sps, &PPS]));
        let u = unit(&out[0]);
        assert!(!u.key);
        assert!(u.params);
        assert_eq!(u.payload, annexb(&[&VPS, &sps, &PPS, &t]));
        // Only a PPS: left alone, no flag.
        let out = feed(&mut a, 2, 60, true, &ap(&[&PPS, &t]));
        let u = unit(&out[0]);
        assert!(!u.params);
        assert_eq!(u.payload, annexb(&[&PPS, &t]));
    }

    #[test]
    fn a_key_unit_before_any_parameter_set_is_dropped() {
        let mut a = Assembler::new();
        let out = feed(&mut a, 1, 50, true, &idr(&[1; 4]));
        assert_eq!(
            out,
            vec![Output::Dropped {
                timestamp: 50,
                reason: DropReason::MissingParams
            }]
        );
    }

    #[test]
    fn a_lost_packet_drops_its_unit_and_the_next_one_is_fine() {
        let mut a = Assembler::new();
        first_idr(&mut a, 100, 1000); // seq 100..=103
        // Unit B: 104, (105 lost), 106 with the marker.
        let b = trail(&[0; 40]);
        let frags = fu(&b, 14);
        assert_eq!(frags.len(), 3);
        let mut out = feed(&mut a, 104, 2000, false, &frags[0]);
        out.extend(feed(&mut a, 106, 2000, true, &frags[2]));
        assert_eq!(
            out,
            vec![Output::Dropped {
                timestamp: 2000,
                reason: DropReason::Loss
            }]
        );
        assert_eq!(a.lost(), 1);
        // Unit C is complete and arrives without a gap: produced.
        let c = trail(&[5; 4]);
        let out = feed(&mut a, 107, 3000, true, &c);
        assert_eq!(unit(&out[0]).payload, annexb(&[&c]));
        assert_eq!(a.lost(), 1);
    }

    #[test]
    fn a_lost_marker_packet_drops_the_open_unit_and_the_one_that_may_have_lost_its_head() {
        let mut a = Assembler::new();
        first_idr(&mut a, 100, 1000);
        // Unit B: 104 (no marker), the marker packet 105 is lost.
        let b1 = trail(&[1; 4]);
        assert!(feed(&mut a, 104, 2000, false, &b1).is_empty());
        // Unit C starts at 106: B is dropped when C's first packet shows the gap, and C itself is
        // dropped because the lost packet may have been its first one.
        let c = trail(&[2; 4]);
        let out = feed(&mut a, 106, 3000, true, &c);
        assert_eq!(
            out,
            vec![
                Output::Dropped {
                    timestamp: 2000,
                    reason: DropReason::Loss
                },
                Output::Dropped {
                    timestamp: 3000,
                    reason: DropReason::Loss
                }
            ]
        );
        // From here on, units flow again.
        let d = trail(&[3; 4]);
        let out = feed(&mut a, 107, 4000, true, &d);
        assert_eq!(unit(&out[0]).payload, annexb(&[&d]));
    }

    #[test]
    fn a_whole_unit_lost_between_two_complete_ones_costs_the_next_unit_only_when_it_may_be_headless() {
        let mut a = Assembler::new();
        first_idr(&mut a, 100, 1000);
        // Packets 104..=106 (a whole unit) are lost; 107 starts a unit.
        let c = trail(&[2; 4]);
        let out = feed(&mut a, 107, 4000, true, &c);
        assert_eq!(
            out,
            vec![Output::Dropped {
                timestamp: 4000,
                reason: DropReason::Loss
            }]
        );
        assert_eq!(a.lost(), 3);
    }

    #[test]
    fn a_unit_ends_at_the_next_timestamp_when_the_device_sets_no_marker() {
        let mut a = Assembler::new();
        let n1 = trail(&[1; 4]);
        let n2 = trail(&[2; 4]);
        assert!(feed(&mut a, 1, 100, false, &n1).is_empty());
        let out = feed(&mut a, 2, 200, false, &n2);
        assert_eq!(out.len(), 1);
        assert_eq!(unit(&out[0]).payload, annexb(&[&n1]));
        assert_eq!(a.units_without_marker(), 1);
    }

    #[test]
    fn sequence_numbers_wrap() {
        let mut a = Assembler::new();
        let n = trail(&[1; 4]);
        assert_eq!(feed(&mut a, 65534, 100, true, &n).len(), 1);
        assert_eq!(feed(&mut a, 65535, 200, true, &n).len(), 1);
        assert_eq!(feed(&mut a, 0, 300, true, &n).len(), 1);
        assert_eq!(feed(&mut a, 1, 400, true, &n).len(), 1);
        assert_eq!(a.lost(), 0);
    }

    #[test]
    fn duplicates_and_late_packets_are_ignored() {
        let mut a = Assembler::new();
        let n = trail(&[1; 4]);
        assert_eq!(feed(&mut a, 10, 100, true, &n).len(), 1);
        assert_eq!(feed(&mut a, 11, 200, true, &n).len(), 1);
        // The same packet again, and one from before.
        assert!(feed(&mut a, 11, 200, true, &n).is_empty());
        assert!(feed(&mut a, 9, 50, true, &n).is_empty());
        assert_eq!(a.late(), 2);
        assert_eq!(a.lost(), 0);
        assert_eq!(feed(&mut a, 12, 300, true, &n).len(), 1);
    }

    #[test]
    fn malformed_packets_drop_the_unit_without_panicking() {
        let n = trail(&[1; 4]);
        // Too short, forbidden bit set.
        for bad in [vec![], vec![0x02], vec![0x82, 0x01, 0x00]] {
            let mut a = Assembler::new();
            let out = feed(&mut a, 1, 100, true, &bad);
            assert_eq!(
                out,
                vec![Output::Dropped {
                    timestamp: 100,
                    reason: DropReason::Malformed
                }],
                "payload {bad:?}"
            );
            // And the stream goes on.
            assert_eq!(feed(&mut a, 2, 200, true, &n).len(), 1);
        }
        // A truncated aggregation packet: the size says 9, three bytes follow.
        let mut a = Assembler::new();
        let ap = vec![NAL_AP << 1, 0x01, 0x00, 0x09, 0x02, 0x01, 0x00];
        assert!(matches!(
            feed(&mut a, 1, 100, true, &ap)[0],
            Output::Dropped {
                reason: DropReason::Malformed,
                ..
            }
        ));
        // A middle fragment with no start.
        let mut a = Assembler::new();
        let frags = fu(&trail(&[4; 30]), 10);
        assert!(matches!(
            feed(&mut a, 1, 100, true, &frags[1])[0],
            Output::Dropped {
                reason: DropReason::Malformed,
                ..
            }
        ));
        // A unit that ends in the middle of a fragmented NAL unit.
        let mut a = Assembler::new();
        let frags = fu(&trail(&[4; 30]), 10);
        let out = feed(&mut a, 1, 100, true, &frags[0]);
        assert!(matches!(
            out[0],
            Output::Dropped {
                reason: DropReason::Malformed,
                ..
            }
        ));
        // A single NAL unit while a fragment is open: the fragment's end was lost.
        let mut a = Assembler::new();
        feed(&mut a, 1, 100, false, &frags[0]);
        let out = feed(&mut a, 2, 100, true, &trail(&[1; 2]));
        assert!(matches!(
            out[0],
            Output::Dropped {
                reason: DropReason::Malformed,
                ..
            }
        ));
        // temporal id plus 1 of zero.
        let mut a = Assembler::new();
        let out = feed(&mut a, 1, 100, true, &[0x02, 0x00, 0xAA]);
        assert!(matches!(out[0], Output::Dropped { .. }));
    }

    #[test]
    fn enhancement_layers_and_reserved_types_are_skipped() {
        let mut a = Assembler::new();
        let base = trail(&[1; 4]);
        let enh = vec![(1 << 1) | 0, (1 << 3) | 1, 0xEE]; // layer id 1
        let reserved = vec![45 << 1, 0x01, 0xEE];
        let mut out = feed(&mut a, 1, 100, false, &enh);
        out.extend(feed(&mut a, 2, 100, false, &reserved));
        out.extend(feed(&mut a, 3, 100, true, &base));
        assert_eq!(unit(&out[0]).payload, annexb(&[&base]));
    }

    #[test]
    fn an_oversized_unit_is_dropped_and_memory_stays_bounded() {
        let mut a = Assembler::with_max_unit_bytes(1000);
        let big = trail(&[1; 1500]);
        let frags = fu(&big, 100);
        let mut out = Vec::new();
        for (i, f) in frags.iter().enumerate() {
            a.push(meta(i as u16, 7, i == frags.len() - 1), f, &mut out);
        }
        assert_eq!(
            out,
            vec![Output::Dropped {
                timestamp: 7,
                reason: DropReason::TooLarge
            }]
        );
        // And one that fits goes through.
        let ok = trail(&[1; 200]);
        let out = feed(&mut a, frags.len() as u16, 8, true, &ok);
        assert_eq!(unit(&out[0]).payload, annexb(&[&ok]));
    }

    #[test]
    fn reset_forgets_everything() {
        let mut a = Assembler::new();
        first_idr(&mut a, 100, 1000);
        assert!(a.has_parameter_sets());
        a.reset();
        assert!(!a.has_parameter_sets());
        // A sequence number far from the old one is no gap after a reset.
        let n = trail(&[1; 4]);
        assert_eq!(feed(&mut a, 5000, 9, true, &n).len(), 1);
        assert_eq!(a.lost(), 0);
    }

    #[test]
    fn an_access_unit_delimiter_stays_first_when_the_sets_are_put_in_front() {
        let mut a = Assembler::new();
        first_idr(&mut a, 1, 100); // seq 1..=4
        let aud = vec![NAL_AUD << 1, 0x01, 0x50];
        let slice = idr(&[9; 6]);
        let mut out = feed(&mut a, 5, 200, false, &aud);
        out.extend(feed(&mut a, 6, 200, true, &slice));
        assert_eq!(
            unit(&out[0]).payload,
            annexb(&[&aud, &VPS, &fake_sps(), &PPS, &slice])
        );
        // With the sets already behind it, nothing moves.
        let sps = fake_sps();
        let mut out = feed(&mut a, 7, 300, false, &ap(&[&aud, &VPS, &sps, &PPS]));
        out.extend(feed(&mut a, 8, 300, true, &slice));
        assert_eq!(unit(&out[0]).payload, annexb(&[&aud, &VPS, &sps, &PPS, &slice]));
        assert!(unit(&out[0]).params);
    }

    #[test]
    fn a_source_that_jumps_far_is_followed_after_three_packets_as_a_loss() {
        let mut a = Assembler::new();
        first_idr(&mut a, 100, 1000); // the next expected is 104
        let n = trail(&[1; 4]);
        // 40000 reads as far behind the cursor; two such packets in a row are ignored ...
        assert!(feed(&mut a, 40000, 2000, true, &n).is_empty());
        assert!(feed(&mut a, 40001, 3000, true, &n).is_empty());
        // ... the third resyncs. Its unit may be missing its head: dropped, a keyframe is wanted.
        let out = feed(&mut a, 40002, 4000, true, &n);
        assert_eq!(
            out,
            vec![Output::Dropped {
                timestamp: 4000,
                reason: DropReason::Loss
            }]
        );
        assert_eq!(a.lost(), 1);
        // The stream goes on from the new position.
        assert_eq!(feed(&mut a, 40003, 5000, true, &n).len(), 1);
        assert_eq!(a.lost(), 1);
    }

    #[test]
    fn far_late_packets_that_do_not_follow_each_other_never_resync() {
        let mut a = Assembler::new();
        first_idr(&mut a, 100, 1000);
        let n = trail(&[1; 4]);
        for seq in [40000u16, 41000, 42000, 43000, 44000] {
            assert!(feed(&mut a, seq, 2000, true, &n).is_empty());
        }
        assert_eq!(feed(&mut a, 104, 3000, true, &n).len(), 1);
        assert_eq!(a.lost(), 0);
    }

    #[test]
    fn late_duplicates_close_behind_never_resync() {
        let mut a = Assembler::new();
        let n = trail(&[1; 4]);
        for seq in 10..=20u16 {
            assert_eq!(feed(&mut a, seq, u32::from(seq) * 100, true, &n).len(), 1);
        }
        // 16..=19 again, each following the last: close behind, so just late copies.
        for seq in 16..=19u16 {
            assert!(feed(&mut a, seq, u32::from(seq) * 100, true, &n).is_empty());
        }
        assert_eq!(feed(&mut a, 21, 2100, true, &n).len(), 1);
        assert_eq!(a.lost(), 0);
        assert_eq!(a.late(), 4);
    }

    #[test]
    fn parameter_sets_of_a_broken_unit_are_not_cached() {
        let mut a = Assembler::new();
        first_idr(&mut a, 1, 100); // seq 1..=4 caches the first SPS
        // A unit that carries another SPS but loses a packet (seq 6).
        let other_sps = vec![0x42, 0x01, 0xEE, 0xEE];
        let mut out = feed(&mut a, 5, 200, false, &ap(&[&VPS, &other_sps, &PPS]));
        out.extend(feed(&mut a, 7, 200, true, &idr(&[1; 4])));
        assert!(matches!(out[0], Output::Dropped { reason: DropReason::Loss, .. }));
        // The next keyframe, with no sets of its own, gets the old, good ones.
        let out = feed(&mut a, 8, 300, true, &idr(&[2; 4]));
        assert_eq!(
            unit(&out[0]).payload,
            annexb(&[&VPS, &fake_sps(), &PPS, &idr(&[2; 4])])
        );
    }

    #[test]
    fn a_gap_inside_a_fragmented_nal_unit_drops_the_unit_once_with_the_loss_reason() {
        let mut a = Assembler::new();
        let frags = fu(&trail(&[7; 60]), 20);
        feed(&mut a, 1, 100, false, &frags[0]);
        // The middle fragment is lost; the end arrives.
        let out = feed(&mut a, 3, 100, true, &frags[2]);
        assert_eq!(
            out,
            vec![Output::Dropped {
                timestamp: 100,
                reason: DropReason::Loss
            }]
        );
    }

    #[test]
    fn random_bytes_never_panic() {
        // A fixed xorshift stream of packets of random content and length.
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let mut a = Assembler::with_max_unit_bytes(4096);
        let mut out = Vec::new();
        for i in 0..20_000u32 {
            let len = (next() % 80) as usize;
            let mut p: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            // Steer a share of them into the aggregation/fragmentation paths.
            if let Some(b) = p.first_mut() {
                match next() % 4 {
                    0 => *b = (NAL_AP << 1) | (*b & 0x81),
                    1 => *b = (NAL_FU << 1) | (*b & 0x81),
                    _ => {}
                }
            }
            let seq = (i as u16).wrapping_add(if next() % 17 == 0 { 3 } else { 0 });
            a.push(meta(seq, (i / 3) * 100, next() % 3 == 0), &p, &mut out);
            out.clear();
        }
    }
}
