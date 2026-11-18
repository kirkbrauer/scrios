//! RTP depacketization into Annex-B access units, for HEVC (RFC 7798) and H.264 (RFC 6184).
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
//! The parameter sets seen in the stream (VPS, SPS and PPS for HEVC; SPS and PPS for H.264) are
//! cached. Every unit with a key picture is produced with the sets as its first NAL units, taken
//! from the cache if the device did not send them in that unit.
//!
//! The two codecs differ only in how an RTP payload is split into NAL units ([`NalFormat::parse`]),
//! which NAL types are parameter sets and key pictures, and a few header details. Everything else, the
//! sequence handling, the unit grouping, the loss rules and the parameter-set insertion, is one
//! implementation, [`UnitAssembler`], used with either format.

use std::marker::PhantomData;

pub const NAL_VPS: u8 = 32;
pub const NAL_SPS: u8 = 33;
pub const NAL_PPS: u8 = 34;
pub const NAL_AUD: u8 = 35;
const NAL_AP: u8 = 48;
const NAL_FU: u8 = 49;

const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// This many packets in a row that look far behind the cursor, one after the other, are not late
/// duplicates: the source jumped (it restarted with a new sequence base). The stream resyncs.
const RESYNC_AFTER: u8 = 3;
/// A packet this far or less behind the cursor is an ordinary late duplicate, never a jump.
const NEAR_LATE: u16 = 256;

/// Units larger than this are dropped (the frame protocol's payload limit, less room for the
/// parameter sets that may be prepended).
pub const MAX_UNIT_BYTES: usize = crate::frame::MAX_PAYLOAD - 4096;

/// The NAL unit type of an HEVC NAL unit header's first byte.
pub fn nal_type(byte0: u8) -> u8 {
    (byte0 >> 1) & 0x3f
}

/// HEVC IRAP pictures: BLA, IDR and CRA (types 16..=21).
pub fn is_irap(nal_type: u8) -> bool {
    (16..=21).contains(&nal_type)
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
    /// Holds a key picture (HEVC: an IRAP picture; H.264: an IDR slice).
    pub key: bool,
    /// The parameter sets are the first NAL units of `payload` (after an access unit delimiter).
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
    /// A key unit, and no parameter sets seen yet to put in front of it.
    MissingParams,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    Unit(AccessUnit),
    Dropped { timestamp: u32, reason: DropReason },
}

/// What one RTP payload turns into, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item<'a> {
    /// A payload that is not a fragment arrives: a fragmented NAL unit that was open never ended.
    EndsFragment,
    /// One whole NAL unit.
    Nal(&'a [u8]),
    /// A fragment of a NAL unit. `start` is set on the first one (it carries the rebuilt NAL header
    /// and the type); `end` on the last.
    Fragment { start: Option<FragmentStart>, fragment: &'a [u8], end: bool },
    /// Parameter sets from a configuration packet, as (index in the format's parameter-set list, NAL unit).
    Config(Vec<(usize, Vec<u8>)>),
    /// The payload cannot be parsed: the unit is dropped.
    Malformed,
    /// Nothing to do with the stream's pictures.
    Ignore,
}

/// The first fragment of a NAL unit: the NAL header to rebuild it with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FragmentStart {
    pub header: [u8; 2],
    pub header_len: usize,
    pub typ: u8,
}

/// What a NAL unit passed to the assembler is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NalCheck {
    Ok(u8),
    /// Not part of the base picture stream (an enhancement layer, a reserved type): left out.
    Skip,
    Malformed,
}

/// How a codec splits RTP payloads into NAL units, and which NAL units matter.
pub trait NalFormat {
    /// The parameter-set NAL types, in the order they go in front of a key unit.
    const PARAM_TYPES: &'static [u8];
    /// Which of them is the SPS.
    const SPS_INDEX: usize;
    /// The access unit delimiter's NAL type.
    const AUD: u8;
    /// A NAL type that makes the unit a key unit.
    fn is_key(nal_type: u8) -> bool;
    /// A whole NAL unit: its type, or why it is not wanted.
    fn check_nal(nal: &[u8]) -> NalCheck;
    /// Splits one RTP payload.
    fn parse<'a>(payload: &'a [u8], out: &mut Vec<Item<'a>>);
    /// A replacement for a whole NAL unit of type `typ` that goes into the stream instead of it
    /// (`None`: as it is). H.264 uses this to give its SPS a VUI bitstream restriction.
    fn transform_nal(_typ: u8, _nal: &[u8]) -> Option<Vec<u8>> {
        None
    }
    /// Whether a whole NAL unit is a B slice.
    fn is_b_slice(_typ: u8, _nal: &[u8]) -> bool {
        false
    }
}

/// HEVC, RFC 7798: single NAL unit, aggregation packets (48), fragmentation units (49).
pub struct Hevc;

impl NalFormat for Hevc {
    const PARAM_TYPES: &'static [u8] = &[NAL_VPS, NAL_SPS, NAL_PPS];
    const SPS_INDEX: usize = 1;
    const AUD: u8 = NAL_AUD;

    fn is_key(t: u8) -> bool {
        is_irap(t)
    }

    fn check_nal(nal: &[u8]) -> NalCheck {
        if nal.len() < 2 {
            return NalCheck::Malformed;
        }
        let layer_id = ((nal[0] & 1) << 5) | (nal[1] >> 3);
        let typ = nal_type(nal[0]);
        if layer_id != 0 || typ >= 41 {
            return NalCheck::Skip; // enhancement layers, reserved and unspecified types
        }
        if nal[1] & 7 == 0 {
            return NalCheck::Malformed; // nuh_temporal_id_plus1 is never 0
        }
        NalCheck::Ok(typ)
    }

    fn parse<'a>(p: &'a [u8], out: &mut Vec<Item<'a>>) {
        if p.len() < 2 || p[0] & 0x80 != 0 {
            out.push(Item::Malformed);
            return;
        }
        match nal_type(p[0]) {
            NAL_AP => {
                // [PayloadHdr][ u16 size, NAL unit ]+ (no DONL: not negotiated)
                out.push(Item::EndsFragment);
                let mut off = 2;
                while off < p.len() {
                    let Some(size) = p.get(off..off + 2) else {
                        out.push(Item::Malformed);
                        return;
                    };
                    let size = usize::from(u16::from_be_bytes([size[0], size[1]]));
                    off += 2;
                    let Some(nal) = p.get(off..off + size) else {
                        out.push(Item::Malformed);
                        return;
                    };
                    if size < 2 {
                        out.push(Item::Malformed);
                        return;
                    }
                    out.push(Item::Nal(nal));
                    off += size;
                }
            }
            NAL_FU => {
                // [PayloadHdr][FU header: S|E|type(6)][fragment]; the NAL header is rebuilt from
                // the PayloadHdr (layer id, TID) with the FU type.
                if p.len() < 3 {
                    out.push(Item::Malformed);
                    return;
                }
                let layer_id = ((p[0] & 1) << 5) | (p[1] >> 3);
                if layer_id != 0 {
                    out.push(Item::Ignore); // not the base layer
                    return;
                }
                let fu = p[2];
                let (start, end, fu_type) = (fu & 0x80 != 0, fu & 0x40 != 0, fu & 0x3f);
                if (start && end) || fu_type >= NAL_AP {
                    out.push(Item::Malformed);
                    return;
                }
                // The NAL header the unit is rebuilt with is judged as a whole NAL unit's is: reserved and
                // unspecified types are left out, a zero temporal id is malformed.
                let header = [(p[0] & 0x81) | (fu_type << 1), p[1]];
                match Self::check_nal(&header) {
                    NalCheck::Ok(_) => {}
                    NalCheck::Skip => {
                        out.push(Item::Ignore);
                        return;
                    }
                    NalCheck::Malformed => {
                        out.push(Item::Malformed);
                        return;
                    }
                }
                out.push(Item::Fragment {
                    start: start.then_some(FragmentStart { header, header_len: 2, typ: fu_type }),
                    fragment: &p[3..],
                    end,
                });
            }
            _ => {
                out.push(Item::EndsFragment);
                out.push(Item::Nal(p));
            }
        }
    }
}

/// H.264, RFC 6184 (packetization mode 1): single NAL unit (1..=23), STAP-A (24), FU-A (28). The
/// interleaved types (STAP-B, MTAP, FU-B) are not supported. A payload of the proprietary kind 18 that
/// the iPad's display stream sends first is read as a configuration packet (see [`crate::avc::find_avc_config`]).
pub struct H264;

/// The NAL type the display stream's configuration packet carries (provisional: read off a capture).
pub const H264_CONFIG_KIND: u8 = 18;

/// Each of the two things worth saying about the configuration packet is said once per process.
static CONFIG_READ_LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static CONFIG_FAILED_LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static SPS_PATCH_FAILED_LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

impl NalFormat for H264 {
    const PARAM_TYPES: &'static [u8] = &[crate::avc::NAL_SPS, crate::avc::NAL_PPS];
    const SPS_INDEX: usize = 0;
    const AUD: u8 = 9;

    fn is_key(t: u8) -> bool {
        t == crate::avc::NAL_IDR
    }

    /// The SPS gets `bitstream_restriction` with no reordering (see `avc::add_bitstream_restriction`); one
    /// that cannot be read is passed on as it came, with one warning.
    fn transform_nal(typ: u8, nal: &[u8]) -> Option<Vec<u8>> {
        use crate::avc::{SpsRewrite, add_bitstream_restriction};
        use std::sync::atomic::Ordering::Relaxed;
        if typ != crate::avc::NAL_SPS {
            return None;
        }
        match add_bitstream_restriction(nal) {
            Ok(SpsRewrite::Patched(p)) => Some(p),
            Ok(SpsRewrite::AlreadyRestricted(_)) => None,
            Err(e) => {
                if !SPS_PATCH_FAILED_LOGGED.swap(true, Relaxed) {
                    tracing::warn!(
                        "an H.264 SPS could not be rewritten with a bitstream restriction ({e}): passing it on as it is, a browser's hardware decoder may buffer a full DPB before its first picture"
                    );
                }
                None
            }
        }
    }

    fn is_b_slice(typ: u8, nal: &[u8]) -> bool {
        typ == 1 && crate::avc::is_b_slice(nal)
    }

    fn check_nal(nal: &[u8]) -> NalCheck {
        let Some(&b0) = nal.first() else {
            return NalCheck::Malformed;
        };
        if b0 & 0x80 != 0 {
            return NalCheck::Malformed; // forbidden_zero_bit
        }
        match crate::avc::nal_type(b0) {
            t @ 1..=13 => NalCheck::Ok(t),
            _ => NalCheck::Skip, // reserved, extension and unspecified types
        }
    }

    fn parse<'a>(p: &'a [u8], out: &mut Vec<Item<'a>>) {
        let Some(&b0) = p.first() else {
            out.push(Item::Malformed);
            return;
        };
        // The display stream's configuration packet starts with 0x92: its forbidden bit is set, so it
        // has to be recognised before that bit is checked.
        if b0 & 0x80 != 0 && crate::avc::nal_type(b0) != H264_CONFIG_KIND {
            out.push(Item::Malformed);
            return;
        }
        match crate::avc::nal_type(b0) {
            H264_CONFIG_KIND => {
                out.push(Item::EndsFragment);
                match crate::avc::find_avc_config(p) {
                    Some(cfg) => {
                        use std::sync::atomic::Ordering::Relaxed;
                        if !CONFIG_READ_LOGGED.swap(true, Relaxed) {
                            tracing::info!(
                                "H.264 configuration packet read: {} SPS and {} PPS (profile {}, level {}), sets cached for the key units",
                                cfg.sps.len(),
                                cfg.pps.len(),
                                cfg.profile_indication,
                                cfg.level_indication
                            );
                        }
                        let mut sets = Vec::new();
                        if let Some(sps) = cfg.sps.first() {
                            sets.push((0, sps.clone()));
                        }
                        if let Some(pps) = cfg.pps.first() {
                            sets.push((1, pps.clone()));
                        }
                        out.push(if sets.is_empty() { Item::Ignore } else { Item::Config(sets) });
                    }
                    None => {
                        use std::sync::atomic::Ordering::Relaxed;
                        if !CONFIG_FAILED_LOGGED.swap(true, Relaxed) {
                            tracing::warn!(
                                "the H.264 configuration packet ({} bytes, NAL type {H264_CONFIG_KIND}) is not an avc1/avcC box, an avcC record or Annex-B sets: taking SPS and PPS from the stream's own NAL units (use --probe-raw to record the packet)",
                                p.len()
                            );
                        }
                        out.push(Item::Ignore)
                    }
                }
            }
            1..=23 => {
                out.push(Item::EndsFragment);
                out.push(Item::Nal(p));
            }
            crate::avc::NAL_STAP_A => {
                // [STAP-A header][ u16 size, NAL unit ]+
                out.push(Item::EndsFragment);
                let mut off = 1;
                while off < p.len() {
                    let Some(size) = p.get(off..off + 2) else {
                        out.push(Item::Malformed);
                        return;
                    };
                    let size = usize::from(u16::from_be_bytes([size[0], size[1]]));
                    off += 2;
                    let Some(nal) = p.get(off..off + size) else {
                        out.push(Item::Malformed);
                        return;
                    };
                    if size < 1 {
                        out.push(Item::Malformed);
                        return;
                    }
                    out.push(Item::Nal(nal));
                    off += size;
                }
            }
            crate::avc::NAL_FU_A => {
                // [FU indicator][FU header: S|E|R|type(5)][fragment]; the NAL header is rebuilt from the
                // indicator's F and NRI and the FU header's type.
                let Some(&fu) = p.get(1) else {
                    out.push(Item::Malformed);
                    return;
                };
                let (start, end, fu_type) = (fu & 0x80 != 0, fu & 0x40 != 0, fu & 0x1f);
                if (start && end) || fu_type == 0 || fu_type >= crate::avc::NAL_STAP_A {
                    out.push(Item::Malformed);
                    return;
                }
                // The NAL header the unit is rebuilt with is judged as a whole NAL unit's is: the types that
                // are not part of the base stream (prefix, extension and reserved ones) are left out.
                let header = [(b0 & 0xE0) | fu_type, 0];
                match Self::check_nal(&header[..1]) {
                    NalCheck::Ok(_) => {}
                    NalCheck::Skip => {
                        out.push(Item::Ignore);
                        return;
                    }
                    NalCheck::Malformed => {
                        out.push(Item::Malformed);
                        return;
                    }
                }
                out.push(Item::Fragment {
                    start: start.then_some(FragmentStart { header, header_len: 1, typ: fu_type }),
                    fragment: &p[2..],
                    end,
                });
            }
            25..=27 | 29 => out.push(Item::Malformed), // interleaved packetization: not supported
            _ => out.push(Item::Ignore),               // reserved
        }
    }
}

/// Says once that the stream has B slices although its SPS says it has no reordering.
fn note_b_slice(warned: &mut bool) {
    if !std::mem::replace(warned, true) {
        tracing::warn!("B slices with max_num_reorder_frames=0: pictures may be out of order");
    }
}

/// What a depacketizer offers the engine, whichever the codec.
pub trait Depacketizer {
    /// Feeds one RTP packet; finished units and drops are appended to `out`.
    fn push(&mut self, meta: PacketMeta, payload: &[u8], out: &mut Vec<Output>);
    /// RTP packets declared lost (gaps in the sequence numbers).
    fn lost(&self) -> u64;
    /// Packets ignored because they were behind the cursor (duplicates, reordered).
    fn late(&self) -> u64;
    /// Units that ended by the next timestamp rather than by a marker bit.
    fn units_without_marker(&self) -> u64;
    fn has_parameter_sets(&self) -> bool;
    /// B slices seen (H.264): the stream says it has no reordering, so these are a surprise.
    fn b_slices(&self) -> u64 {
        0
    }
    /// Forgets the sequence state, the unit in progress and the cached parameter sets (the stream
    /// restarted with a new source).
    fn reset(&mut self);
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
    /// Parameter sets completed in this unit (by position in the format's list): cached only if
    /// the unit turns out whole.
    params: Vec<Option<Vec<u8>>>,
}

impl Building {
    fn new(ts: u32, param_count: usize) -> Self {
        Building {
            ts,
            buf: Vec::with_capacity(8 * 1024),
            nals: Vec::new(),
            broken: None,
            fu_open: false,
            params: vec![None; param_count],
        }
    }

    fn mark(&mut self, reason: DropReason) {
        self.broken.get_or_insert(reason);
    }
}

/// Turns RTP payloads into access units, for the codec `F`.
#[derive(Debug)]
pub struct UnitAssembler<F: NalFormat> {
    expected: Option<u16>,
    cur: Option<Building>,
    /// A gap was seen before the next unit starts: that unit may be missing its head.
    after_gap: bool,
    /// The last packet that looked far behind the cursor, and how many such in a row.
    late_run: Option<(u16, u8)>,
    /// The cached parameter sets, by position in `F::PARAM_TYPES`.
    params: Vec<Option<Vec<u8>>>,
    max_unit_bytes: usize,
    lost: u64,
    late: u64,
    units_without_marker: u64,
    b_slices: u64,
    /// The warning about B slices has been given.
    b_warned: bool,
    _format: PhantomData<F>,
}

/// The HEVC assembler.
pub type Assembler = UnitAssembler<Hevc>;
/// The H.264 assembler.
pub type H264Assembler = UnitAssembler<H264>;

impl<F: NalFormat> Default for UnitAssembler<F> {
    fn default() -> Self {
        Self::new()
    }
}

impl<F: NalFormat> UnitAssembler<F> {
    pub fn new() -> Self {
        Self::with_max_unit_bytes(MAX_UNIT_BYTES)
    }

    pub fn with_max_unit_bytes(max_unit_bytes: usize) -> Self {
        UnitAssembler {
            expected: None,
            cur: None,
            after_gap: false,
            late_run: None,
            params: vec![None; F::PARAM_TYPES.len()],
            max_unit_bytes,
            lost: 0,
            late: 0,
            units_without_marker: 0,
            b_slices: 0,
            b_warned: false,
            _format: PhantomData,
        }
    }

    pub fn has_parameter_sets(&self) -> bool {
        self.params.iter().all(Option::is_some)
    }

    /// Forgets the sequence state, the unit in progress and the cached parameter sets (the stream
    /// restarted with a new source).
    pub fn reset(&mut self) {
        self.expected = None;
        self.cur = None;
        self.after_gap = false;
        self.late_run = None;
        self.params.iter_mut().for_each(|p| *p = None);
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
            let mut b = Building::new(meta.timestamp, F::PARAM_TYPES.len());
            if self.after_gap {
                b.mark(DropReason::Loss);
            }
            self.after_gap = false;
            self.cur = Some(b);
        }

        let mut items = Vec::new();
        F::parse(payload, &mut items);
        for item in items {
            self.apply(item);
        }

        if meta.marker {
            self.finish(out, true);
        }
    }

    fn mark_broken(&mut self, reason: DropReason) {
        if let Some(c) = self.cur.as_mut() {
            c.mark(reason);
        }
    }

    fn apply(&mut self, item: Item<'_>) {
        match item {
            Item::EndsFragment => self.close_dangling_fu(),
            Item::Nal(nal) => self.add_nal(nal),
            Item::Fragment { start, fragment, end } => self.fragment(start, fragment, end),
            Item::Config(sets) => {
                if let Some(cur) = self.cur.as_mut() {
                    for (i, nal) in sets {
                        let nal = F::PARAM_TYPES.get(i).and_then(|t| F::transform_nal(*t, &nal)).unwrap_or(nal);
                        if let Some(slot) = cur.params.get_mut(i) {
                            *slot = Some(nal);
                        }
                    }
                }
            }
            Item::Malformed => self.mark_broken(DropReason::Malformed),
            Item::Ignore => {}
        }
    }

    /// A NAL unit that is not a fragment arrived while a fragmented one was open: its end was lost.
    fn close_dangling_fu(&mut self) {
        if let Some(c) = self.cur.as_mut().filter(|c| c.fu_open) {
            c.mark(DropReason::Malformed);
            c.fu_open = false;
        }
    }

    /// A fragment of a NAL unit: the first one starts it, the last one ends it.
    fn fragment(&mut self, start: Option<FragmentStart>, fragment: &[u8], end: bool) {
        let max = self.max_unit_bytes;
        let Some(cur) = self.cur.as_mut() else { return };
        if cur.buf.len() + fragment.len() + 6 > max {
            cur.mark(DropReason::TooLarge);
            cur.fu_open = false;
            return;
        }

        if let Some(s) = start {
            if cur.fu_open {
                cur.mark(DropReason::Malformed); // the previous one never ended
            }
            cur.buf.extend_from_slice(&START_CODE);
            let at = cur.buf.len();
            cur.buf.extend_from_slice(&s.header[..s.header_len]);
            cur.buf.extend_from_slice(fragment);
            cur.nals.push(NalRef { start: at, end: 0, typ: s.typ });
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
            let (s, t) = (n.start, n.typ);
            // The finished NAL unit may be replaced (an SPS gets its restriction): it is the tail of the buffer.
            if let Some(new) = F::transform_nal(t, &cur.buf[s..]) {
                cur.buf.truncate(s);
                cur.buf.extend_from_slice(&new);
                n.end = cur.buf.len();
            }
            let e = n.end;
            if F::is_b_slice(t, &cur.buf[s..e]) {
                self.b_slices += 1;
                note_b_slice(&mut self.b_warned);
            }
            if let Some(i) = F::PARAM_TYPES.iter().position(|p| *p == t) {
                cur.params[i] = Some(cur.buf[s..e].to_vec());
            }
        }
    }

    /// Appends one complete NAL unit to the unit being built.
    fn add_nal(&mut self, nal: &[u8]) {
        let typ = match F::check_nal(nal) {
            NalCheck::Ok(t) => t,
            NalCheck::Skip => return,
            NalCheck::Malformed => {
                self.mark_broken(DropReason::Malformed);
                return;
            }
        };
        if F::is_b_slice(typ, nal) {
            self.b_slices += 1;
            note_b_slice(&mut self.b_warned);
        }
        // An SPS may be rewritten on its way in; the unit and the cache get the same bytes.
        let replaced = F::transform_nal(typ, nal);
        let nal = replaced.as_deref().unwrap_or(nal);
        let max = self.max_unit_bytes;
        let Some(cur) = self.cur.as_mut() else { return };
        if cur.buf.len() + nal.len() + 4 > max {
            cur.mark(DropReason::TooLarge);
            return;
        }
        cur.buf.extend_from_slice(&START_CODE);
        let start = cur.buf.len();
        cur.buf.extend_from_slice(nal);
        cur.nals.push(NalRef { start, end: cur.buf.len(), typ });
        if let Some(i) = F::PARAM_TYPES.iter().position(|p| *p == typ) {
            cur.params[i] = Some(nal.to_vec());
        }
    }

    /// Ends the unit in progress.
    fn finish(&mut self, out: &mut Vec<Output>, by_marker: bool) {
        let Some(mut b) = self.cur.take() else { return };
        if b.fu_open {
            b.mark(DropReason::Malformed); // ended in the middle of a fragmented NAL unit
        }
        if let Some(reason) = b.broken {
            out.push(Output::Dropped { timestamp: b.ts, reason });
            return;
        }
        // The unit is whole: what parameter sets it carried are good.
        for (slot, new) in self.params.iter_mut().zip(b.params.iter_mut()) {
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
        let is_param = |t: u8| F::PARAM_TYPES.contains(&t);
        let key = b.nals.iter().any(|n| F::is_key(n.typ));
        let carries_all = F::PARAM_TYPES.iter().all(|t| b.nals.iter().any(|n| n.typ == *t));

        let plain = |b: Building| AccessUnit { timestamp: b.ts, key: false, params: false, payload: b.buf, sps: None };
        if !key && !carries_all {
            return Ok(plain(b));
        }

        // The parameter sets that go in front: the cache already holds the unit's own copies.
        let Some(sets) = self.params.iter().cloned().collect::<Option<Vec<Vec<u8>>>>() else {
            if key {
                return Err((b.ts, DropReason::MissingParams));
            }
            return Ok(plain(b));
        };

        // An access unit delimiter, if the unit starts with one, stays the first NAL unit.
        let aud = usize::from(b.nals.first().is_some_and(|n| n.typ == F::AUD));
        let n_params = F::PARAM_TYPES.len();
        let already_first = b.nals.len() >= aud + n_params
            && F::PARAM_TYPES.iter().enumerate().all(|(i, t)| b.nals[aud + i].typ == *t)
            && !b.nals[aud + n_params..].iter().any(|n| is_param(n.typ));
        let payload = if already_first {
            b.buf
        } else {
            let rest = |i: &usize, n: &&NalRef| *i >= aud && !is_param(n.typ);
            let rest_len: usize =
                b.nals.iter().enumerate().filter(|(i, n)| rest(i, n)).map(|(_, n)| 4 + n.end - n.start).sum();
            let aud_len = if aud == 1 { 4 + b.nals[0].end - b.nals[0].start } else { 0 };
            let sets_len: usize = sets.iter().map(|s| 4 + s.len()).sum();
            let mut p = Vec::with_capacity(sets_len + rest_len + aud_len);
            if aud == 1 {
                p.extend_from_slice(&START_CODE);
                p.extend_from_slice(&b.buf[b.nals[0].start..b.nals[0].end]);
            }
            for set in &sets {
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
        Ok(AccessUnit { timestamp: b.ts, key, params: true, payload, sps: Some(sets[F::SPS_INDEX].clone()) })
    }
}

impl<F: NalFormat> Depacketizer for UnitAssembler<F> {
    fn push(&mut self, meta: PacketMeta, payload: &[u8], out: &mut Vec<Output>) {
        UnitAssembler::push(self, meta, payload, out)
    }
    fn lost(&self) -> u64 {
        self.lost
    }
    fn late(&self) -> u64 {
        self.late
    }
    fn units_without_marker(&self) -> u64 {
        self.units_without_marker
    }
    fn has_parameter_sets(&self) -> bool {
        UnitAssembler::has_parameter_sets(self)
    }
    fn b_slices(&self) -> u64 {
        self.b_slices
    }
    fn reset(&mut self) {
        UnitAssembler::reset(self)
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
        PacketMeta { seq, timestamp: ts, marker }
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
        assert_eq!(out, vec![Output::Dropped { timestamp: 50, reason: DropReason::MissingParams }]);
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
        assert_eq!(out, vec![Output::Dropped { timestamp: 2000, reason: DropReason::Loss }]);
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
                Output::Dropped { timestamp: 2000, reason: DropReason::Loss },
                Output::Dropped { timestamp: 3000, reason: DropReason::Loss }
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
        assert_eq!(out, vec![Output::Dropped { timestamp: 4000, reason: DropReason::Loss }]);
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
            assert_eq!(out, vec![Output::Dropped { timestamp: 100, reason: DropReason::Malformed }], "payload {bad:?}");
            // And the stream goes on.
            assert_eq!(feed(&mut a, 2, 200, true, &n).len(), 1);
        }
        // A truncated aggregation packet: the size says 9, three bytes follow.
        let mut a = Assembler::new();
        let ap = vec![NAL_AP << 1, 0x01, 0x00, 0x09, 0x02, 0x01, 0x00];
        assert!(matches!(feed(&mut a, 1, 100, true, &ap)[0], Output::Dropped { reason: DropReason::Malformed, .. }));
        // A middle fragment with no start.
        let mut a = Assembler::new();
        let frags = fu(&trail(&[4; 30]), 10);
        assert!(matches!(
            feed(&mut a, 1, 100, true, &frags[1])[0],
            Output::Dropped { reason: DropReason::Malformed, .. }
        ));
        // A unit that ends in the middle of a fragmented NAL unit.
        let mut a = Assembler::new();
        let frags = fu(&trail(&[4; 30]), 10);
        let out = feed(&mut a, 1, 100, true, &frags[0]);
        assert!(matches!(out[0], Output::Dropped { reason: DropReason::Malformed, .. }));
        // A single NAL unit while a fragment is open: the fragment's end was lost.
        let mut a = Assembler::new();
        feed(&mut a, 1, 100, false, &frags[0]);
        let out = feed(&mut a, 2, 100, true, &trail(&[1; 2]));
        assert!(matches!(out[0], Output::Dropped { reason: DropReason::Malformed, .. }));
        // temporal id plus 1 of zero.
        let mut a = Assembler::new();
        let out = feed(&mut a, 1, 100, true, &[0x02, 0x00, 0xAA]);
        assert!(matches!(out[0], Output::Dropped { .. }));
    }

    #[test]
    fn enhancement_layers_and_reserved_types_are_skipped() {
        let mut a = Assembler::new();
        let base = trail(&[1; 4]);
        let enh = vec![(1 << 1), (1 << 3) | 1, 0xEE]; // layer id 1
        let reserved = vec![45 << 1, 0x01, 0xEE];
        let mut out = feed(&mut a, 1, 100, false, &enh);
        out.extend(feed(&mut a, 2, 100, false, &reserved));
        out.extend(feed(&mut a, 3, 100, true, &base));
        assert_eq!(unit(&out[0]).payload, annexb(&[&base]));
    }

    #[test]
    fn a_fragmented_nal_unit_is_judged_by_its_rebuilt_header_like_a_whole_one() {
        let mut a = Assembler::new();
        let base = trail(&[1; 4]);
        // A reserved type (45), fragmented: every fragment is left out, as the whole unit would be.
        let reserved = vec![45 << 1, 0x01, 0xEE, 0xEE, 0xEE, 0xEE, 0xEE];
        let frags = fu(&reserved, 3);
        assert!(frags.len() > 1);
        let mut out = Vec::new();
        for (i, f) in frags.iter().enumerate() {
            out.extend(feed(&mut a, 1 + i as u16, 100, false, f));
        }
        let mut seq = 1 + frags.len() as u16;
        out.extend(feed(&mut a, seq, 100, true, &base));
        assert_eq!(out.len(), 1);
        assert_eq!(unit(&out[0]).payload, annexb(&[&base]));
        seq += 1;

        // A temporal id plus 1 of 0 is never valid: the unit is dropped, as with a whole NAL unit of it.
        let bad = vec![1 << 1, 0x00, 1, 2, 3, 4, 5, 6];
        let frags = fu(&bad, 3);
        let mut out = Vec::new();
        for (i, f) in frags.iter().enumerate() {
            out.extend(feed(&mut a, seq + i as u16, 200, i == frags.len() - 1, f));
        }
        assert_eq!(out, vec![Output::Dropped { timestamp: 200, reason: DropReason::Malformed }]);
        let whole = feed(&mut a, seq + frags.len() as u16, 300, true, &bad);
        assert!(matches!(whole[0], Output::Dropped { reason: DropReason::Malformed, .. }));
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
        assert_eq!(out, vec![Output::Dropped { timestamp: 7, reason: DropReason::TooLarge }]);
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
        assert_eq!(unit(&out[0]).payload, annexb(&[&aud, &VPS, &fake_sps(), &PPS, &slice]));
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
        assert_eq!(out, vec![Output::Dropped { timestamp: 4000, reason: DropReason::Loss }]);
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
        assert_eq!(unit(&out[0]).payload, annexb(&[&VPS, &fake_sps(), &PPS, &idr(&[2; 4])]));
    }

    #[test]
    fn a_gap_inside_a_fragmented_nal_unit_drops_the_unit_once_with_the_loss_reason() {
        let mut a = Assembler::new();
        let frags = fu(&trail(&[7; 60]), 20);
        feed(&mut a, 1, 100, false, &frags[0]);
        // The middle fragment is lost; the end arrives.
        let out = feed(&mut a, 3, 100, true, &frags[2]);
        assert_eq!(out, vec![Output::Dropped { timestamp: 100, reason: DropReason::Loss }]);
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

#[cfg(test)]
mod h264_tests {
    use super::*;
    use crate::avc::NAL_FU_A;
    use crate::avc::testutil::*;

    fn meta(seq: u16, ts: u32, marker: bool) -> PacketMeta {
        PacketMeta { seq, timestamp: ts, marker }
    }

    fn feed(a: &mut H264Assembler, seq: u16, ts: u32, marker: bool, p: &[u8]) -> Vec<Output> {
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

    fn annexb(nals: &[&[u8]]) -> Vec<u8> {
        let mut v = Vec::new();
        for n in nals {
            v.extend_from_slice(&START_CODE);
            v.extend_from_slice(n);
        }
        v
    }

    /// The SPS as the device sends it (no VUI).
    fn raw_sps() -> Vec<u8> {
        AvcSpsSpec::baseline_vga().nal()
    }

    /// The SPS as the assembler passes it on: with a VUI bitstream restriction.
    fn sps() -> Vec<u8> {
        match crate::avc::add_bitstream_restriction(&raw_sps()).unwrap() {
            crate::avc::SpsRewrite::Patched(p) => p,
            other => panic!("{other:?}"),
        }
    }

    /// The first picture: SPS and PPS in a STAP-A, the IDR slice in three FU-A fragments (seq, seq+1..=seq+3).
    fn first_idr(a: &mut H264Assembler, seq: u16, ts: u32) -> Vec<Output> {
        let mut out = feed(a, seq, ts, false, &stap_a(&[&raw_sps(), &PPS]));
        let frags = fu_a(&idr(&[7; 30]), 12);
        assert_eq!(frags.len(), 3);
        for (i, f) in frags.iter().enumerate() {
            out.extend(feed(a, seq + 1 + i as u16, ts, i == 2, f));
        }
        out
    }

    #[test]
    fn a_single_nal_packet_with_the_marker_is_one_plain_delta_unit() {
        let mut a = H264Assembler::new();
        let nal = slice(&[1, 2, 3, 4]);
        let out = feed(&mut a, 10, 1000, true, &nal);
        assert_eq!(out.len(), 1);
        let u = unit(&out[0]);
        assert_eq!((u.timestamp, u.key, u.params), (1000, false, false));
        assert_eq!(u.sps, None);
        assert_eq!(u.payload, annexb(&[&nal]));
    }

    #[test]
    fn stap_a_and_fu_a_make_a_key_unit_with_sps_and_pps_in_front() {
        let mut a = H264Assembler::new();
        let out = first_idr(&mut a, 100, 5000);
        assert_eq!(out.len(), 1);
        let u = unit(&out[0]);
        assert!(u.key && u.params);
        // The FU-A's NAL header is rebuilt from the indicator's F and NRI and the FU header's type.
        assert_eq!(u.payload, annexb(&[&sps(), &PPS, &idr(&[7; 30])]));
        assert_eq!(u.sps.as_deref(), Some(&sps()[..]));
        assert_eq!(a.lost(), 0);
        assert!(a.has_parameter_sets());
    }

    #[test]
    fn the_nal_header_of_a_fragmented_unit_keeps_its_nri() {
        let mut a = H264Assembler::new();
        feed(&mut a, 1, 1, false, &stap_a(&[&raw_sps(), &PPS]));
        // NRI 1 (0x20) instead of 3: the rebuilt header is 0x25, not 0x65.
        let nal = [0x25u8, 9, 9, 9, 9, 9];
        let frags = fu_a(&nal, 3);
        assert_eq!(frags[0][0], 0x3C); // indicator: NRI 1, type 28
        let mut out = Vec::new();
        for (i, f) in frags.iter().enumerate() {
            out.extend(feed(&mut a, 2 + i as u16, 1, i == frags.len() - 1, f));
        }
        assert_eq!(unit(&out[0]).payload, annexb(&[&sps(), &PPS, &nal]));
        assert!(unit(&out[0]).key);
    }

    #[test]
    fn multi_slice_pictures_keep_every_slice_in_one_unit() {
        let mut a = H264Assembler::new();
        let (s1, s2) = (idr(&[1; 8]), idr(&[2; 8]));
        let mut out = feed(&mut a, 1, 9, false, &stap_a(&[&raw_sps(), &PPS]));
        out.extend(feed(&mut a, 2, 9, false, &s1));
        out.extend(feed(&mut a, 3, 9, true, &s2));
        assert_eq!(out.len(), 1);
        assert_eq!(unit(&out[0]).payload, annexb(&[&sps(), &PPS, &s1, &s2]));
    }

    #[test]
    fn a_key_unit_without_parameter_sets_gets_the_cached_ones_in_front() {
        let mut a = H264Assembler::new();
        first_idr(&mut a, 100, 5000);
        let t = slice(&[9; 6]);
        let out = feed(&mut a, 104, 5533, true, &t);
        assert_eq!(unit(&out[0]).payload, annexb(&[&t]));
        assert!(!unit(&out[0]).params);
        // The next IDR comes with no sets of its own (a keyframe request's answer).
        let idr2 = idr(&[8; 20]);
        let out = feed(&mut a, 105, 6000, true, &idr2);
        let u = unit(&out[0]);
        assert!(u.key && u.params);
        assert_eq!(u.payload, annexb(&[&sps(), &PPS, &idr2]));
        assert_eq!(u.sps.as_deref(), Some(&sps()[..]));
    }

    #[test]
    fn partial_or_misordered_sets_are_normalised() {
        let mut a = H264Assembler::new();
        first_idr(&mut a, 1, 100);
        // PPS, an SEI (type 6), the IDR, then the SPS after the slice.
        let sei = [0x06u8, 0xAA, 0xBB];
        let slice_nal = idr(&[3; 5]);
        let mut out = feed(&mut a, 5, 200, false, &stap_a(&[&PPS, &sei]));
        out.extend(feed(&mut a, 6, 200, false, &slice_nal));
        out.extend(feed(&mut a, 7, 200, true, &raw_sps()));
        assert_eq!(unit(&out[0]).payload, annexb(&[&sps(), &PPS, &sei, &slice_nal]));
    }

    #[test]
    fn a_delta_unit_with_both_sets_gets_the_params_flag_and_one_with_only_a_pps_does_not() {
        let mut a = H264Assembler::new();
        let t = slice(&[1; 3]);
        let out = feed(&mut a, 1, 50, true, &stap_a(&[&t, &raw_sps(), &PPS]));
        let u = unit(&out[0]);
        assert!(!u.key && u.params);
        assert_eq!(u.payload, annexb(&[&sps(), &PPS, &t]));
        let out = feed(&mut a, 2, 60, true, &stap_a(&[&PPS, &t]));
        assert!(!unit(&out[0]).params);
    }

    #[test]
    fn a_key_unit_before_any_parameter_set_is_dropped() {
        let mut a = H264Assembler::new();
        let out = feed(&mut a, 1, 50, true, &idr(&[1; 4]));
        assert_eq!(out, vec![Output::Dropped { timestamp: 50, reason: DropReason::MissingParams }]);
    }

    #[test]
    fn an_access_unit_delimiter_stays_first() {
        let mut a = H264Assembler::new();
        first_idr(&mut a, 1, 100);
        let aud = [0x09u8, 0xF0];
        let sl = idr(&[9; 6]);
        let mut out = feed(&mut a, 5, 200, false, &aud);
        out.extend(feed(&mut a, 6, 200, true, &sl));
        assert_eq!(unit(&out[0]).payload, annexb(&[&aud, &sps(), &PPS, &sl]));
    }

    #[test]
    fn loss_inside_a_unit_drops_it_and_the_next_one_is_fine() {
        let mut a = H264Assembler::new();
        first_idr(&mut a, 100, 1000); // seq 100..=103
        let frags = fu_a(&slice(&[0; 40]), 14);
        assert_eq!(frags.len(), 3);
        let mut out = feed(&mut a, 104, 2000, false, &frags[0]);
        out.extend(feed(&mut a, 106, 2000, true, &frags[2])); // 105 lost
        assert_eq!(out, vec![Output::Dropped { timestamp: 2000, reason: DropReason::Loss }]);
        assert_eq!(a.lost(), 1);
        let c = slice(&[5; 4]);
        assert_eq!(unit(&feed(&mut a, 107, 3000, true, &c)[0]).payload, annexb(&[&c]));
    }

    #[test]
    fn a_lost_marker_packet_drops_the_open_unit_and_the_one_that_may_have_lost_its_head() {
        let mut a = H264Assembler::new();
        first_idr(&mut a, 100, 1000);
        assert!(feed(&mut a, 104, 2000, false, &slice(&[1; 4])).is_empty());
        let out = feed(&mut a, 106, 3000, true, &slice(&[2; 4]));
        assert_eq!(
            out,
            vec![
                Output::Dropped { timestamp: 2000, reason: DropReason::Loss },
                Output::Dropped { timestamp: 3000, reason: DropReason::Loss },
            ]
        );
        let d = slice(&[3; 4]);
        assert_eq!(unit(&feed(&mut a, 107, 4000, true, &d)[0]).payload, annexb(&[&d]));
    }

    #[test]
    fn units_end_at_the_next_timestamp_without_a_marker_and_sequence_numbers_wrap() {
        let mut a = H264Assembler::new();
        let (n1, n2) = (slice(&[1; 4]), slice(&[2; 4]));
        assert!(feed(&mut a, 65534, 100, false, &n1).is_empty());
        let out = feed(&mut a, 65535, 200, false, &n2);
        assert_eq!(unit(&out[0]).payload, annexb(&[&n1]));
        assert_eq!(a.units_without_marker(), 1);
        let out = feed(&mut a, 0, 300, true, &n1);
        assert_eq!(unit(&out[0]).payload, annexb(&[&n2]));
        assert_eq!(a.lost(), 0);
    }

    #[test]
    fn duplicates_are_ignored_and_a_far_jump_resyncs() {
        let mut a = H264Assembler::new();
        let n = slice(&[1; 4]);
        assert_eq!(feed(&mut a, 10, 100, true, &n).len(), 1);
        assert_eq!(feed(&mut a, 11, 200, true, &n).len(), 1);
        assert!(feed(&mut a, 11, 200, true, &n).is_empty());
        assert_eq!(a.late(), 1);
        // A source that restarted with another sequence base.
        assert!(feed(&mut a, 40000, 1, true, &n).is_empty());
        assert!(feed(&mut a, 40001, 2, true, &n).is_empty());
        let out = feed(&mut a, 40002, 3, true, &n);
        assert_eq!(out, vec![Output::Dropped { timestamp: 3, reason: DropReason::Loss }]);
        assert_eq!(feed(&mut a, 40003, 4, true, &n).len(), 1);
    }

    #[test]
    fn malformed_packets_drop_the_unit_without_panicking() {
        let n = slice(&[1; 4]);
        let malformed = |p: &[u8]| {
            let mut a = H264Assembler::new();
            let out = feed(&mut a, 1, 100, true, p);
            assert_eq!(out, vec![Output::Dropped { timestamp: 100, reason: DropReason::Malformed }], "{p:02x?}");
            // The stream goes on.
            assert_eq!(feed(&mut a, 2, 200, true, &n).len(), 1);
        };
        malformed(&[]); // empty
        malformed(&[0x81, 1]); // forbidden bit
        malformed(&[0x18, 0x00, 0x09, 0x41, 0x00]); // a STAP-A cut in a unit
        malformed(&[0x18, 0x00, 0x00]); // a STAP-A with an empty unit
        malformed(&[0x18, 0x00]); // a STAP-A cut in a size
        malformed(&[0x7C]); // an FU-A without its header
        malformed(&[0x7C, 0x05, 1, 2]); // a middle fragment with no start
        malformed(&[0x7C, 0xC5, 1, 2]); // start and end both set
        malformed(&[0x7C, 0x80, 1, 2]); // a fragment of NAL type 0
        malformed(&[0x7C, 0x98, 1, 2]); // a fragment of NAL type 24
        malformed(&[0x19, 0, 0, 1, 2]); // STAP-B: interleaving is not supported
        malformed(&[0x1A, 1, 2]); // MTAP16
        malformed(&[0x1D, 1, 2]); // FU-B
        // A unit that ends in the middle of a fragmented NAL unit.
        let mut a = H264Assembler::new();
        let frags = fu_a(&slice(&[4; 30]), 10);
        let out = feed(&mut a, 1, 100, true, &frags[0]);
        assert!(matches!(out[0], Output::Dropped { reason: DropReason::Malformed, .. }));
        // A single NAL unit while a fragment is open: the fragment's end was lost.
        let mut a = H264Assembler::new();
        feed(&mut a, 1, 100, false, &frags[0]);
        let out = feed(&mut a, 2, 100, true, &slice(&[1; 2]));
        assert!(matches!(out[0], Output::Dropped { reason: DropReason::Malformed, .. }));
    }

    #[test]
    fn reserved_and_extension_nal_types_and_unspecified_payloads_are_left_out() {
        let mut a = H264Assembler::new();
        let base = slice(&[1; 4]);
        let mut out = feed(&mut a, 1, 100, false, &[0x0E, 1, 2]); // type 14 (prefix NAL unit)
        out.extend(feed(&mut a, 2, 100, false, &[0x1E, 1, 2])); // payload type 30: reserved
        out.extend(feed(&mut a, 3, 100, true, &base));
        assert_eq!(unit(&out[0]).payload, annexb(&[&base]));
    }

    #[test]
    fn fragmented_extension_and_prefix_nal_units_are_left_out_like_whole_ones() {
        let mut a = H264Assembler::new();
        let base = slice(&[1; 4]);
        let mut out = Vec::new();
        let mut seq = 1;
        // A prefix NAL unit (14) and a slice extension (20), each in three FU-A fragments.
        for typ in [14u8, 20] {
            let frags = fu_a(&[0x60 | typ, 1, 2, 3, 4, 5, 6, 7], 3);
            assert!(frags.len() > 1);
            for f in &frags {
                out.extend(feed(&mut a, seq, 100, false, f));
                seq += 1;
            }
        }
        out.extend(feed(&mut a, seq, 100, true, &base));
        assert_eq!(out.len(), 1);
        assert_eq!(unit(&out[0]).payload, annexb(&[&base]));
    }

    #[test]
    fn an_oversized_unit_is_dropped_and_memory_stays_bounded() {
        let mut a = H264Assembler::with_max_unit_bytes(1000);
        let frags = fu_a(&slice(&[1; 1500]), 100);
        let mut out = Vec::new();
        for (i, f) in frags.iter().enumerate() {
            a.push(meta(i as u16, 7, i == frags.len() - 1), f, &mut out);
        }
        assert_eq!(out, vec![Output::Dropped { timestamp: 7, reason: DropReason::TooLarge }]);
        let ok = slice(&[1; 200]);
        assert_eq!(unit(&feed(&mut a, frags.len() as u16, 8, true, &ok)[0]).payload, annexb(&[&ok]));
    }

    #[test]
    fn parameter_sets_of_a_broken_unit_are_not_cached() {
        let mut a = H264Assembler::new();
        first_idr(&mut a, 1, 100);
        let other_sps = [0x67u8, 0xEE, 0xEE, 0xEE, 0xEE];
        let mut out = feed(&mut a, 5, 200, false, &stap_a(&[&other_sps, &PPS]));
        out.extend(feed(&mut a, 7, 200, true, &idr(&[1; 4]))); // 6 lost
        assert!(matches!(out[0], Output::Dropped { reason: DropReason::Loss, .. }));
        let out = feed(&mut a, 8, 300, true, &idr(&[2; 4]));
        assert_eq!(unit(&out[0]).payload, annexb(&[&sps(), &PPS, &idr(&[2; 4])]));
    }

    #[test]
    fn reset_forgets_everything() {
        let mut a = H264Assembler::new();
        first_idr(&mut a, 100, 1000);
        assert!(a.has_parameter_sets());
        a.reset();
        assert!(!a.has_parameter_sets());
        assert_eq!(feed(&mut a, 5000, 9, true, &slice(&[1; 4])).len(), 1);
        assert_eq!(a.lost(), 0);
    }

    // ---- the display stream's configuration packet -------------------------------------------

    fn config_packet() -> Vec<u8> {
        let avcc = make_box(b"avcC", &avcc_record(66, 0xC0, 30, &[&raw_sps()], &[&PPS]));
        let mut p = vec![0x92, 0xE6, 0xC0, 0xA3];
        p.extend_from_slice(&avc1_box(640, 480, &[avcc]));
        p
    }

    #[test]
    fn a_configuration_packet_gives_the_sets_that_a_later_idr_is_given() {
        let mut a = H264Assembler::new();
        // The config packet has a timestamp of its own and ends its unit; it makes no picture.
        assert!(feed(&mut a, 1, 100, true, &config_packet()).is_empty());
        assert!(a.has_parameter_sets());
        // An IDR with no in-band sets: the config's go in front.
        let sl = idr(&[7; 9]);
        let out = feed(&mut a, 2, 200, true, &sl);
        let u = unit(&out[0]);
        assert!(u.key && u.params);
        assert_eq!(u.payload, annexb(&[&sps(), &PPS, &sl]));
        assert_eq!(u.sps.as_deref(), Some(&sps()[..]));
    }

    #[test]
    fn a_configuration_packet_in_the_same_unit_as_the_idr_works_too() {
        let mut a = H264Assembler::new();
        assert!(feed(&mut a, 1, 100, false, &config_packet()).is_empty());
        let sl = idr(&[7; 9]);
        let out = feed(&mut a, 2, 100, true, &sl);
        assert_eq!(unit(&out[0]).payload, annexb(&[&sps(), &PPS, &sl]));
    }

    #[test]
    fn a_configuration_packet_that_does_not_parse_is_ignored_and_does_not_break_the_unit() {
        let mut a = H264Assembler::new();
        assert!(feed(&mut a, 1, 100, true, &[0x92, 0xE6, 0xC0, 0xA3, 1, 2, 3]).is_empty());
        assert!(!a.has_parameter_sets());
        // And one with an unusable record: nothing cached.
        let mut p = vec![0x92, 0xE6, 0xC0, 0xA3];
        p.extend_from_slice(&avc1_box(640, 480, &[make_box(b"avcC", &[9, 1, 2, 3, 4, 5, 6])]));
        assert!(feed(&mut a, 2, 200, true, &p).is_empty());
        assert!(!a.has_parameter_sets());
    }

    #[test]
    fn without_a_readable_config_packet_the_sets_come_from_the_streams_own_nal_units() {
        let mut a = H264Assembler::new();
        // The config packet is not an avc1 box, a record or Annex-B: it is ignored (and said once in the log).
        assert!(feed(&mut a, 1, 100, true, &[0x92, 0xE6, 0xC0, 0xA3, 9, 9, 9, 9, 9]).is_empty());
        assert!(!a.has_parameter_sets());
        // A key unit without sets cannot be sent yet ...
        let out = feed(&mut a, 2, 200, true, &idr(&[1; 4]));
        assert_eq!(out, vec![Output::Dropped { timestamp: 200, reason: DropReason::MissingParams }]);
        // ... but the in-band sets of the next one are used, and cached for the ones after it.
        let mut out = feed(&mut a, 3, 300, false, &stap_a(&[&raw_sps(), &PPS]));
        out.extend(feed(&mut a, 4, 300, true, &idr(&[2; 4])));
        assert_eq!(unit(&out[0]).payload, annexb(&[&sps(), &PPS, &idr(&[2; 4])]));
        let out = feed(&mut a, 5, 400, true, &idr(&[3; 4]));
        assert_eq!(unit(&out[0]).payload, annexb(&[&sps(), &PPS, &idr(&[3; 4])]));
    }

    #[test]
    fn a_configuration_packet_that_is_a_bare_record_or_annexb_works_too() {
        let record = avcc_record(66, 0xC0, 30, &[&raw_sps()], &[&PPS]);
        let mut annexb_config = vec![0x92, 0xE6, 0xC0, 0xA3, 0, 0, 0, 1];
        annexb_config.extend_from_slice(&raw_sps());
        annexb_config.extend_from_slice(&[0, 0, 0, 1]);
        annexb_config.extend_from_slice(&PPS);
        for (name, packet) in
            [("a bare record", [vec![0x92, 0xE6, 0xC0, 0xA3], record].concat()), ("Annex-B", annexb_config)]
        {
            let mut a = H264Assembler::new();
            assert!(feed(&mut a, 1, 100, true, &packet).is_empty(), "{name}");
            assert!(a.has_parameter_sets(), "{name}");
            let sl = idr(&[7; 9]);
            let out = feed(&mut a, 2, 200, true, &sl);
            assert_eq!(unit(&out[0]).payload, annexb(&[&sps(), &PPS, &sl]), "{name}");
        }
    }

    #[test]
    fn sei_and_access_unit_delimiters_are_kept_in_the_unit() {
        let mut a = H264Assembler::new();
        first_idr(&mut a, 1, 100);
        let aud = [0x09u8, 0xF0];
        let sei = [0x06u8, 0x05, 0x01, 0xAA, 0x80];
        let sl = slice(&[5; 5]);
        let mut out = feed(&mut a, 5, 200, false, &aud);
        out.extend(feed(&mut a, 6, 200, false, &sei));
        out.extend(feed(&mut a, 7, 200, true, &sl));
        // A delta unit is plain: every NAL unit as it came, in order.
        assert_eq!(unit(&out[0]).payload, annexb(&[&aud, &sei, &sl]));
        // In a key unit the sets go after the delimiter and before the SEI.
        let mut out = feed(&mut a, 8, 300, false, &aud);
        out.extend(feed(&mut a, 9, 300, false, &sei));
        out.extend(feed(&mut a, 10, 300, true, &idr(&[6; 5])));
        assert_eq!(unit(&out[0]).payload, annexb(&[&aud, &sps(), &PPS, &sei, &idr(&[6; 5])]));
    }

    #[test]
    fn the_config_of_a_broken_unit_is_not_cached() {
        let mut a = H264Assembler::new();
        feed(&mut a, 1, 100, false, &config_packet());
        // 2 is lost: the unit is broken, and so are the sets it carried.
        let out = feed(&mut a, 3, 100, true, &slice(&[1; 3]));
        assert!(matches!(out[0], Output::Dropped { reason: DropReason::Loss, .. }));
        assert!(!a.has_parameter_sets());
    }

    // ---- the SPS rewrite and the B-slice guard, end to end -------------------------------------

    #[test]
    fn the_sps_goes_on_with_its_restriction_whichever_way_it_arrives() {
        let want = sps();
        assert_ne!(want, raw_sps());
        // In a STAP-A (first_idr): the payload and the unit's SPS are the rewritten one.
        let mut a = H264Assembler::new();
        let out = first_idr(&mut a, 1, 100);
        assert_eq!(unit(&out[0]).sps.as_deref(), Some(&want[..]));
        assert!(unit(&out[0]).payload.windows(want.len()).any(|w| w == &want[..]));
        assert!(!unit(&out[0]).payload.windows(raw_sps().len()).any(|w| w == &raw_sps()[..]));
        // As a single NAL unit.
        let mut a = H264Assembler::new();
        let mut out = feed(&mut a, 1, 100, false, &raw_sps());
        out.extend(feed(&mut a, 2, 100, false, &PPS));
        out.extend(feed(&mut a, 3, 100, true, &idr(&[1; 4])));
        assert_eq!(unit(&out[0]).payload, annexb(&[&want, &PPS, &idr(&[1; 4])]));
        // In FU-A fragments.
        let mut a = H264Assembler::new();
        let mut out = Vec::new();
        let frags = fu_a(&raw_sps(), 3);
        assert!(frags.len() > 2);
        for (i, f) in frags.iter().enumerate() {
            out.extend(feed(&mut a, 1 + i as u16, 100, false, f));
        }
        let n = frags.len() as u16;
        out.extend(feed(&mut a, 1 + n, 100, false, &PPS));
        out.extend(feed(&mut a, 2 + n, 100, true, &idr(&[2; 4])));
        assert_eq!(unit(&out[0]).payload, annexb(&[&want, &PPS, &idr(&[2; 4])]));
        // Through the configuration packet: the cached set is the rewritten one.
        let mut a = H264Assembler::new();
        assert!(feed(&mut a, 1, 100, true, &config_packet()).is_empty());
        let out = feed(&mut a, 2, 200, true, &idr(&[3; 4]));
        assert_eq!(unit(&out[0]).payload, annexb(&[&want, &PPS, &idr(&[3; 4])]));
        assert_eq!(unit(&out[0]).sps.as_deref(), Some(&want[..]));
    }

    #[test]
    fn the_rewritten_sps_describes_the_same_stream() {
        let described = |nal: &[u8]| crate::codec::CodecKind::H264.describe_sps(nal).unwrap();
        assert_eq!(described(&sps()), described(&raw_sps()));
    }

    #[test]
    fn an_sps_that_cannot_be_read_goes_on_as_it_was_and_one_that_is_restricted_already_is_not_touched() {
        // Garbage after a valid SPS header: not parseable to its end.
        let junk_sps = vec![0x67u8, 100, 0, 51, 0x00, 0x00, 0x03, 0xFF, 0xFF];
        let mut a = H264Assembler::new();
        let mut out = feed(&mut a, 1, 100, false, &junk_sps);
        out.extend(feed(&mut a, 2, 100, false, &PPS));
        out.extend(feed(&mut a, 3, 100, true, &idr(&[1; 4])));
        assert_eq!(unit(&out[0]).payload, annexb(&[&junk_sps, &PPS, &idr(&[1; 4])]));
        // A restriction of its own stays.
        let own = crate::avc::Restriction {
            motion_vectors_over_pic_boundaries: true,
            max_bytes_per_pic_denom: 0,
            max_bits_per_mb_denom: 0,
            log2_max_mv_length_horizontal: 13,
            log2_max_mv_length_vertical: 13,
            max_num_reorder_frames: 1,
            max_dec_frame_buffering: 2,
        };
        let mut spec = AvcSpsSpec::baseline_vga();
        spec.vui = Some(crate::avc::testutil::VuiSpec { restriction: Some(own), ..Default::default() });
        let restricted = spec.nal();
        let mut a = H264Assembler::new();
        let mut out = feed(&mut a, 1, 100, false, &restricted);
        out.extend(feed(&mut a, 2, 100, false, &PPS));
        out.extend(feed(&mut a, 3, 100, true, &idr(&[1; 4])));
        assert_eq!(unit(&out[0]).payload, annexb(&[&restricted, &PPS, &idr(&[1; 4])]));
    }

    /// A slice NAL unit of the given NAL header byte and slice_type.
    fn typed_slice(header: u8, slice_type: u32) -> Vec<u8> {
        let mut w = crate::sps::testutil::BitWriter::default();
        w.ue(0);
        w.ue(slice_type);
        w.ue(0);
        for _ in 0..20 {
            w.u(8, 0xA5);
        }
        let mut n = vec![header];
        n.extend_from_slice(&crate::sps::testutil::escape_rbsp(&w.finish()));
        n
    }

    #[test]
    fn b_slices_are_counted_and_change_nothing() {
        let mut a = H264Assembler::new();
        first_idr(&mut a, 1, 100);
        assert_eq!(a.b_slices(), 0);
        // P and I slices: not counted.
        let (p, i) = (typed_slice(0x41, 0), typed_slice(0x41, 7));
        assert_eq!(unit(&feed(&mut a, 5, 200, true, &p)[0]).payload, annexb(&[&p]));
        assert_eq!(unit(&feed(&mut a, 6, 300, true, &i)[0]).payload, annexb(&[&i]));
        assert_eq!(a.b_slices(), 0);
        // slice_type 1 and 6 are B slices: the unit is produced as ever, and counted.
        let (b1, b6) = (typed_slice(0x41, 1), typed_slice(0x41, 6));
        assert_eq!(unit(&feed(&mut a, 7, 400, true, &b1)[0]).payload, annexb(&[&b1]));
        assert_eq!(a.b_slices(), 1);
        assert_eq!(unit(&feed(&mut a, 8, 500, true, &b6)[0]).payload, annexb(&[&b6]));
        assert_eq!(a.b_slices(), 2);
        // Inside a STAP-A and in an FU-A, too.
        feed(&mut a, 9, 600, true, &stap_a(&[&typed_slice(0x41, 1), &typed_slice(0x41, 6)]));
        assert_eq!(a.b_slices(), 4);
        let frags = fu_a(&typed_slice(0x41, 6), 9);
        assert!(frags.len() > 2);
        let n = frags.len();
        for (k, f) in frags.iter().enumerate() {
            feed(&mut a, 10 + k as u16, 700, k == n - 1, f);
        }
        assert_eq!(a.b_slices(), 5);
        // An IDR picture has only I slices: never counted, whatever the header says.
        feed(&mut a, 10 + n as u16, 800, true, &typed_slice(0x65, 1));
        assert_eq!(a.b_slices(), 5);
        // HEVC has no such count.
        assert_eq!(Assembler::new().b_slices(), 0);
    }

    #[test]
    fn random_bytes_never_panic() {
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let mut a = H264Assembler::with_max_unit_bytes(4096);
        let mut out = Vec::new();
        for i in 0..20_000u32 {
            let len = (next() % 80) as usize;
            let mut p: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            if let Some(b) = p.first_mut() {
                match next() % 5 {
                    0 => *b = (*b & 0xE0) | 24,
                    1 => *b = (*b & 0xE0) | NAL_FU_A,
                    2 => *b = (*b & 0xE0) | 18,
                    _ => {}
                }
            }
            let seq = (i as u16).wrapping_add(if next() % 17 == 0 { 3 } else { 0 });
            a.push(meta(seq, (i / 3) * 100, next() % 3 == 0), &p, &mut out);
            out.clear();
        }
    }
}
