//! H.264 (RFC 6184) helpers for the codec probe: NAL unit types and the sequence parameter set.

use crate::sps::{Bits, SpsError, unescape_rbsp};

pub const NAL_IDR: u8 = 5;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;
pub const NAL_STAP_A: u8 = 24;
pub const NAL_FU_A: u8 = 28;

/// The type of an H.264 NAL unit (or of an RTP payload's first byte): the low 5 bits.
pub fn nal_type(byte0: u8) -> u8 {
    byte0 & 0x1f
}

/// What the NAL units in one RTP payload are, by the RFC 6184 packetization rules.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PayloadScan {
    /// The payload's own type (its first byte & 0x1f): 1..=23 a single NAL unit, 24 STAP-A, 28 FU-A, ...
    pub kind: Option<u8>,
    /// The types of the NAL units that start in this packet: the unit itself, each unit aggregated in a
    /// STAP-A, the inner type of an FU-A that starts here (its S bit).
    pub nal_types: Vec<u8>,
    /// The first SPS NAL unit found whole in the packet (a single NAL unit or inside a STAP-A).
    pub sps: Option<Vec<u8>>,
}

/// Reads one RTP payload as H.264. Never panics on short or odd input.
pub fn scan_payload(payload: &[u8]) -> PayloadScan {
    let mut scan = PayloadScan::default();
    let Some(&b0) = payload.first() else {
        return scan;
    };
    let kind = nal_type(b0);
    scan.kind = Some(kind);
    match kind {
        1..=23 => {
            scan.nal_types.push(kind);
            if kind == NAL_SPS {
                scan.sps = Some(payload.to_vec());
            }
        }
        NAL_STAP_A => {
            // [STAP-A header][u16 size, NAL unit]+
            let mut off = 1;
            while off + 2 <= payload.len() {
                let size = usize::from(u16::from_be_bytes([payload[off], payload[off + 1]]));
                off += 2;
                let Some(nal) = payload.get(off..off + size).filter(|n| !n.is_empty()) else {
                    break;
                };
                let t = nal_type(nal[0]);
                scan.nal_types.push(t);
                if t == NAL_SPS && scan.sps.is_none() {
                    scan.sps = Some(nal.to_vec());
                }
                off += size;
            }
        }
        NAL_FU_A => {
            // [FU indicator][FU header: S|E|R|type(5)][fragment]
            if let Some(&fu) = payload.get(1) {
                if fu & 0x80 != 0 {
                    scan.nal_types.push(fu & 0x1f);
                }
            }
        }
        _ => {}
    }
    scan
}

/// What the probe wants to know about an SPS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvcSps {
    pub profile_idc: u8,
    /// The byte holding `constraint_set0..5_flag` (bit 7 is set0) and two reserved bits.
    pub constraint_flags: u8,
    pub level_idc: u8,
    pub chroma_format_idc: u8,
    pub frame_mbs_only: bool,
    /// The picture size in pixels, after the cropping.
    pub width: u32,
    pub height: u32,
    /// The coded size: whole macroblocks, before the cropping.
    pub coded_width: u32,
    pub coded_height: u32,
}

impl AvcSps {
    /// The RFC 6381 codec string: `avc1.` and profile_idc, the constraint byte and level_idc in hex.
    pub fn codec_string(&self) -> String {
        format!(
            "avc1.{:02X}{:02X}{:02X}",
            self.profile_idc, self.constraint_flags, self.level_idc
        )
    }

    /// The constraint flags that are set, by name (`set0`..`set5`).
    pub fn constraint_names(&self) -> String {
        let names: Vec<String> = (0..6)
            .filter(|i| self.constraint_flags & (0x80 >> i) != 0)
            .map(|i| format!("set{i}"))
            .collect();
        if names.is_empty() {
            "none".to_string()
        } else {
            names.join(" ")
        }
    }
}

/// Profiles whose SPS carries the chroma format, bit depths and scaling matrices.
fn has_chroma_info(profile_idc: u8) -> bool {
    matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    )
}

fn skip_scaling_list(b: &mut Bits<'_>, size: usize) -> Result<(), SpsError> {
    let (mut last, mut next) = (8i64, 8i64);
    for _ in 0..size {
        if next != 0 {
            let delta = b.se()?;
            next = (last + delta + 256).rem_euclid(256);
        }
        if next != 0 {
            last = next;
        }
    }
    Ok(())
}

/// Parses an H.264 SPS NAL unit (with its one-byte header, without a start code): profile, level,
/// and the picture size with the cropping applied (ITU-T H.264 7.3.2.1.1).
pub fn parse_avc_sps(nal: &[u8]) -> Result<AvcSps, SpsError> {
    if nal.len() < 5 || nal[0] & 0x80 != 0 || nal_type(nal[0]) != NAL_SPS {
        return Err(SpsError::NotSps);
    }
    let (profile_idc, constraint_flags, level_idc) = (nal[1], nal[2], nal[3]);
    let rbsp = unescape_rbsp(&nal[4..]);
    let mut b = Bits::new(&rbsp);
    b.ue()?; // seq_parameter_set_id
    let mut chroma_format_idc = 1;
    if has_chroma_info(profile_idc) {
        chroma_format_idc = b.ue()?;
        if chroma_format_idc > 3 {
            return Err(SpsError::Invalid("chroma_format_idc"));
        }
        if chroma_format_idc == 3 {
            b.skip(1)?; // separate_colour_plane_flag
        }
        b.ue()?; // bit_depth_luma_minus8
        b.ue()?; // bit_depth_chroma_minus8
        b.skip(1)?; // qpprime_y_zero_transform_bypass_flag
        if b.bit()? == 1 {
            // seq_scaling_matrix_present_flag
            let lists = if chroma_format_idc == 3 { 12 } else { 8 };
            for i in 0..lists {
                if b.bit()? == 1 {
                    skip_scaling_list(&mut b, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    b.ue()?; // log2_max_frame_num_minus4
    match b.ue()? {
        0 => {
            b.ue()?; // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            b.skip(1)?; // delta_pic_order_always_zero_flag
            b.se()?; // offset_for_non_ref_pic
            b.se()?; // offset_for_top_to_bottom_field
            let n = b.ue()?;
            if n > 255 {
                return Err(SpsError::Invalid("num_ref_frames_in_pic_order_cnt_cycle"));
            }
            for _ in 0..n {
                b.se()?;
            }
        }
        _ => {}
    }
    b.ue()?; // max_num_ref_frames
    b.skip(1)?; // gaps_in_frame_num_value_allowed_flag
    let width_mbs = u64::from(b.ue()?) + 1;
    let height_map_units = u64::from(b.ue()?) + 1;
    let frame_mbs_only = b.bit()? == 1;
    if !frame_mbs_only {
        b.skip(1)?; // mb_adaptive_frame_field_flag
    }
    b.skip(1)?; // direct_8x8_inference_flag
    let mut crop = [0u64; 4]; // left, right, top, bottom
    if b.bit()? == 1 {
        for c in crop.iter_mut() {
            *c = u64::from(b.ue()?);
        }
    }
    let frame_height_mbs = (2 - u64::from(frame_mbs_only)) * height_map_units;
    // Cropping is in chroma sample units (ChromaArrayType 0 has none).
    let (unit_x, unit_y) = match chroma_format_idc {
        0 => (1, 2 - u64::from(frame_mbs_only)),
        1 => (2, 2 * (2 - u64::from(frame_mbs_only))),
        2 => (2, 2 - u64::from(frame_mbs_only)),
        _ => (1, 2 - u64::from(frame_mbs_only)),
    };
    let width = (width_mbs * 16).saturating_sub(unit_x * (crop[0] + crop[1]));
    let height = (frame_height_mbs * 16).saturating_sub(unit_y * (crop[2] + crop[3]));
    if width == 0 || height == 0 || width > 32768 || height > 32768 {
        return Err(SpsError::Invalid("picture size"));
    }
    Ok(AvcSps {
        profile_idc,
        constraint_flags,
        level_idc,
        chroma_format_idc: chroma_format_idc as u8,
        frame_mbs_only,
        width: width as u32,
        height: height as u32,
        coded_width: (width_mbs * 16) as u32,
        coded_height: (frame_height_mbs * 16) as u32,
    })
}

// ---- the display stream's configuration packet: an avc1 box with an avcC record ----------------

/// What an `avcC` record (ISO/IEC 14496-15 `AVCDecoderConfigurationRecord`) holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvcConfig {
    pub profile_indication: u8,
    pub profile_compatibility: u8,
    pub level_indication: u8,
    /// The size in bytes of the length prefix of the NAL units in samples (`lengthSizeMinusOne` + 1).
    pub length_size: u8,
    /// The SPS NAL units (with their NAL header, no length prefix).
    pub sps: Vec<Vec<u8>>,
    pub pps: Vec<Vec<u8>>,
}

fn be16(b: &[u8], at: usize) -> Option<usize> {
    Some(usize::from(u16::from_be_bytes([*b.get(at)?, *b.get(at + 1)?])))
}

/// Parses an `AVCDecoderConfigurationRecord`: version 1, profile, compatibility, level, the length
/// size, the SPS list and the PPS list (each entry a 16-bit length and the NAL unit). Anything after the
/// PPS list (the extension of the high profiles) is left alone.
pub fn parse_avcc(rec: &[u8]) -> Result<AvcConfig, String> {
    if rec.len() < 7 {
        return Err("avcC record too short".into());
    }
    if rec[0] != 1 {
        return Err(format!("avcC configurationVersion {} (expected 1)", rec[0]));
    }
    let mut off = 6;
    let mut sps = Vec::new();
    for _ in 0..(rec[5] & 0x1f) {
        let n = be16(rec, off).ok_or("avcC SPS list is cut short")?;
        let nal = rec.get(off + 2..off + 2 + n).ok_or("avcC SPS is cut short")?;
        if n == 0 || nal_type(nal[0]) != NAL_SPS {
            return Err("avcC SPS entry is not an SPS NAL unit".into());
        }
        sps.push(nal.to_vec());
        off += 2 + n;
    }
    let num_pps = *rec.get(off).ok_or("avcC has no PPS count")?;
    off += 1;
    let mut pps = Vec::new();
    for _ in 0..num_pps {
        let n = be16(rec, off).ok_or("avcC PPS list is cut short")?;
        let nal = rec.get(off + 2..off + 2 + n).ok_or("avcC PPS is cut short")?;
        if n == 0 || nal_type(nal[0]) != NAL_PPS {
            return Err("avcC PPS entry is not a PPS NAL unit".into());
        }
        pps.push(nal.to_vec());
        off += 2 + n;
    }
    Ok(AvcConfig {
        profile_indication: rec[1],
        profile_compatibility: rec[2],
        level_indication: rec[3],
        length_size: (rec[4] & 3) + 1,
        sps,
        pps,
    })
}

/// One box of an ISO base media file: its type and its content (what follows the header).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoxRef<'a> {
    pub kind: [u8; 4],
    pub content: &'a [u8],
}

/// The boxes of a sequence of boxes, up to the first one that is cut short or malformed. A size of 1
/// means a 64-bit size follows, a size of 0 means the box runs to the end.
pub fn boxes(mut buf: &[u8]) -> Vec<BoxRef<'_>> {
    let mut out = Vec::new();
    while buf.len() >= 8 {
        let size = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as u64;
        let kind = [buf[4], buf[5], buf[6], buf[7]];
        let (header, total) = match size {
            0 => (8, buf.len() as u64),
            1 => {
                let Some(big) = buf.get(8..16) else { break };
                (16, u64::from_be_bytes(big.try_into().expect("8 bytes")))
            }
            n => (8, n),
        };
        if total < header as u64 || total > buf.len() as u64 {
            break;
        }
        out.push(BoxRef {
            kind,
            content: &buf[header..total as usize],
        });
        buf = &buf[total as usize..];
    }
    out
}

/// The bytes of a `VisualSampleEntry` before its child boxes: 6 reserved, the data reference index, 16
/// pre-defined and reserved, width, height, the two resolutions, a reserved word, the frame count, the
/// 32-byte compressor name, the depth and a pre-defined word.
const VISUAL_SAMPLE_ENTRY_FIELDS: usize = 78;

/// Reads an `avc1` box (as the content of its header: the visual sample entry fields, then child boxes)
/// and returns the `avcC` record it holds.
pub fn parse_avc1_content(content: &[u8]) -> Result<AvcConfig, String> {
    let children = content
        .get(VISUAL_SAMPLE_ENTRY_FIELDS..)
        .ok_or("avc1 box is shorter than a visual sample entry")?;
    let avcc = boxes(children)
        .into_iter()
        .find(|b| &b.kind == b"avcC")
        .ok_or("avc1 box has no avcC child")?;
    parse_avcc(avcc.content)
}

/// SPS and PPS NAL units found as Annex-B (start codes `00 00 01` or `00 00 00 01`) anywhere in a payload.
fn annexb_config(payload: &[u8]) -> Option<AvcConfig> {
    let mut starts = Vec::new(); // (start of the NAL unit, start of its start code)
    let mut i = 0;
    while i + 3 <= payload.len() {
        if payload[i] == 0 && payload[i + 1] == 0 && payload[i + 2] == 1 {
            let code_at = if i > 0 && payload[i - 1] == 0 { i - 1 } else { i };
            starts.push((i + 3, code_at));
            i += 3;
        } else {
            i += 1;
        }
    }
    let (mut sps, mut pps) = (None, None);
    for (k, &(nal_at, _)) in starts.iter().enumerate() {
        let end = starts.get(k + 1).map_or(payload.len(), |&(_, code_at)| code_at);
        let nal = payload.get(nal_at..end)?;
        match nal.first().map(|b| nal_type(*b)) {
            Some(NAL_SPS) if sps.is_none() && nal.len() >= 4 => sps = Some(nal.to_vec()),
            Some(NAL_PPS) if pps.is_none() && nal.len() >= 2 => pps = Some(nal.to_vec()),
            _ => {}
        }
    }
    let sps = sps?;
    let pps = pps?;
    Some(AvcConfig {
        profile_indication: sps[1],
        profile_compatibility: sps[2],
        level_indication: sps[3],
        length_size: 4,
        sps: vec![sps],
        pps: vec![pps],
    })
}

/// Finds the codec configuration in a configuration packet of the display stream, reading it as
/// leniently as the evidence allows, in this order:
///
/// 1. an `avc1` box anywhere in the payload (the one seen on an iPad starts at offset 4, after a
///    4-byte header) with an `avcC` child;
/// 2. a bare `avcC` box;
/// 3. a bare `AVCDecoderConfigurationRecord` (version 1) at one of the first nine offsets;
/// 4. SPS and PPS NAL units in Annex-B form.
///
/// Returns `None` if none of them parses; the caller then takes the sets from the stream's own NAL units.
pub fn find_avc_config(payload: &[u8]) -> Option<AvcConfig> {
    find_avc_boxes(payload)
        .or_else(|| {
            (0..=8usize.min(payload.len().saturating_sub(1)))
                .find_map(|off| parse_avcc(&payload[off..]).ok())
        })
        .or_else(|| annexb_config(payload))
}

fn find_avc_boxes(payload: &[u8]) -> Option<AvcConfig> {
    let find = |kind: &[u8; 4]| {
        (4..payload.len().saturating_sub(3)).find_map(|i| (&payload[i..i + 4] == kind).then_some(i))
    };
    let box_at = |type_at: usize| -> Option<BoxRef<'_>> {
        boxes(payload.get(type_at - 4..)?).into_iter().next()
    };
    if let Some(i) = find(b"avc1") {
        if let Some(b) = box_at(i) {
            if let Ok(cfg) = parse_avc1_content(b.content) {
                return Some(cfg);
            }
        }
    }
    if let Some(i) = find(b"avcC") {
        if let Some(b) = box_at(i) {
            if let Ok(cfg) = parse_avcc(b.content) {
                return Some(cfg);
            }
        }
    }
    None
}

#[cfg(test)]
pub mod testutil {
    use super::*;
    use crate::sps::testutil::{BitWriter, escape_rbsp};

    pub const PPS: [u8; 4] = [0x68, 0xEE, 0x3C, 0x80];

    /// An IDR slice NAL unit (type 5).
    pub fn idr(body: &[u8]) -> Vec<u8> {
        let mut v = vec![0x65];
        v.extend_from_slice(body);
        v
    }

    /// A non-IDR slice NAL unit (type 1).
    pub fn slice(body: &[u8]) -> Vec<u8> {
        let mut v = vec![0x41];
        v.extend_from_slice(body);
        v
    }

    /// A STAP-A packet payload.
    pub fn stap_a(nals: &[&[u8]]) -> Vec<u8> {
        let mut v = vec![0x18];
        for n in nals {
            v.extend_from_slice(&(n.len() as u16).to_be_bytes());
            v.extend_from_slice(n);
        }
        v
    }

    /// FU-A payloads of at most `chunk` fragment bytes for `nal` (header byte included).
    pub fn fu_a(nal: &[u8], chunk: usize) -> Vec<Vec<u8>> {
        let (indicator, typ) = ((nal[0] & 0xE0) | 28, nal[0] & 0x1f);
        let parts: Vec<&[u8]> = nal[1..].chunks(chunk).collect();
        let n = parts.len();
        parts
            .into_iter()
            .enumerate()
            .map(|(i, part)| {
                let mut h = typ;
                if i == 0 {
                    h |= 0x80;
                }
                if i == n - 1 {
                    h |= 0x40;
                }
                let mut v = vec![indicator, h];
                v.extend_from_slice(part);
                v
            })
            .collect()
    }

    /// An `AVCDecoderConfigurationRecord`.
    pub fn avcc_record(profile: u8, compat: u8, level: u8, sps: &[&[u8]], pps: &[&[u8]]) -> Vec<u8> {
        let mut r = vec![1, profile, compat, level, 0xFF, 0xE0 | sps.len() as u8];
        for s in sps {
            r.extend_from_slice(&(s.len() as u16).to_be_bytes());
            r.extend_from_slice(s);
        }
        r.push(pps.len() as u8);
        for p in pps {
            r.extend_from_slice(&(p.len() as u16).to_be_bytes());
            r.extend_from_slice(p);
        }
        r
    }

    /// An ISO box: size, type, content.
    pub fn make_box(kind: &[u8; 4], content: &[u8]) -> Vec<u8> {
        let mut b = ((content.len() + 8) as u32).to_be_bytes().to_vec();
        b.extend_from_slice(kind);
        b.extend_from_slice(content);
        b
    }

    /// An `avc1` box: the visual sample entry fields for a `width` x `height` picture, then the child boxes.
    pub fn avc1_box(width: u16, height: u16, children: &[Vec<u8>]) -> Vec<u8> {
        let mut c = vec![0u8; 6]; // reserved
        c.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index
        c.extend_from_slice(&[0; 16]); // pre_defined, reserved
        c.extend_from_slice(&width.to_be_bytes());
        c.extend_from_slice(&height.to_be_bytes());
        c.extend_from_slice(&0x0048_0000u32.to_be_bytes()); // horizresolution
        c.extend_from_slice(&0x0048_0000u32.to_be_bytes()); // vertresolution
        c.extend_from_slice(&[0; 4]); // reserved
        c.extend_from_slice(&1u16.to_be_bytes()); // frame_count
        c.extend_from_slice(&[0; 32]); // compressorname
        c.extend_from_slice(&0x0018u16.to_be_bytes()); // depth
        c.extend_from_slice(&0xFFFFu16.to_be_bytes()); // pre_defined
        assert_eq!(c.len(), VISUAL_SAMPLE_ENTRY_FIELDS);
        for child in children {
            c.extend_from_slice(child);
        }
        make_box(b"avc1", &c)
    }

    pub struct AvcSpsSpec {
        pub profile_idc: u8,
        pub constraint_flags: u8,
        pub level_idc: u8,
        pub chroma_format_idc: u32,
        /// For the high profiles: the scaling lists present as (index, deltas to write).
        pub scaling_lists: Option<Vec<(usize, Vec<i64>)>>,
        pub poc_type: u32,
        /// `log2_max_pic_order_cnt_lsb_minus4` for poc type 0 (its value is never used).
        pub poc_lsb: u32,
        pub width_mbs: u32,
        pub height_map_units: u32,
        pub frame_mbs_only: bool,
        pub crop: Option<[u32; 4]>,
    }

    impl AvcSpsSpec {
        /// Baseline, level 3.0, 640x480.
        pub fn baseline_vga() -> Self {
            AvcSpsSpec {
                profile_idc: 66,
                constraint_flags: 0xC0,
                level_idc: 30,
                chroma_format_idc: 1,
                scaling_lists: None,
                poc_type: 2,
                poc_lsb: 2,
                width_mbs: 40,
                height_map_units: 30,
                frame_mbs_only: true,
                crop: None,
            }
        }

        /// High, level 4.0, 1920x1088 coded and cropped to 1080.
        pub fn high_1080p() -> Self {
            AvcSpsSpec {
                profile_idc: 100,
                constraint_flags: 0,
                level_idc: 40,
                chroma_format_idc: 1,
                scaling_lists: None,
                poc_type: 0,
                poc_lsb: 2,
                width_mbs: 120,
                height_map_units: 68,
                frame_mbs_only: true,
                crop: Some([0, 0, 0, 4]),
            }
        }

        pub fn nal(&self) -> Vec<u8> {
            let mut w = BitWriter::default();
            w.ue(0); // seq_parameter_set_id
            if has_chroma_info(self.profile_idc) {
                w.ue(self.chroma_format_idc);
                if self.chroma_format_idc == 3 {
                    w.bit(0);
                }
                w.ue(0); // bit_depth_luma_minus8
                w.ue(0); // bit_depth_chroma_minus8
                w.bit(0); // qpprime_y_zero_transform_bypass_flag
                match &self.scaling_lists {
                    None => w.bit(0),
                    Some(lists) => {
                        w.bit(1);
                        let n = if self.chroma_format_idc == 3 { 12 } else { 8 };
                        for i in 0..n {
                            match lists.iter().find(|(idx, _)| *idx == i) {
                                None => w.bit(0),
                                Some((_, deltas)) => {
                                    w.bit(1);
                                    for d in deltas {
                                        w.se(*d);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            w.ue(4); // log2_max_frame_num_minus4
            w.ue(self.poc_type);
            match self.poc_type {
                0 => w.ue(self.poc_lsb),
                1 => {
                    w.bit(0);
                    w.se(-3);
                    w.se(2);
                    w.ue(2);
                    w.se(5);
                    w.se(-7);
                }
                _ => {}
            }
            w.ue(4); // max_num_ref_frames
            w.bit(0); // gaps_in_frame_num_value_allowed_flag
            w.ue(self.width_mbs - 1);
            w.ue(self.height_map_units - 1);
            w.bit(u32::from(self.frame_mbs_only));
            if !self.frame_mbs_only {
                w.bit(0); // mb_adaptive_frame_field_flag
            }
            w.bit(1); // direct_8x8_inference_flag
            match self.crop {
                Some(c) => {
                    w.bit(1);
                    for v in c {
                        w.ue(v);
                    }
                }
                None => w.bit(0),
            }
            w.bit(0); // vui_parameters_present_flag
            let rbsp = w.finish();
            let mut nal = vec![0x67, self.profile_idc, self.constraint_flags, self.level_idc];
            nal.extend_from_slice(&escape_rbsp(&rbsp));
            nal
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;

    #[test]
    fn a_baseline_sps_gives_profile_level_and_size() {
        let sps = parse_avc_sps(&AvcSpsSpec::baseline_vga().nal()).unwrap();
        assert_eq!((sps.profile_idc, sps.constraint_flags, sps.level_idc), (66, 0xC0, 30));
        assert_eq!((sps.width, sps.height), (640, 480));
        assert!(sps.frame_mbs_only);
        assert_eq!(sps.codec_string(), "avc1.42C01E");
        assert_eq!(sps.constraint_names(), "set0 set1");
    }

    #[test]
    fn a_high_profile_sps_is_cropped_to_the_picture() {
        let sps = parse_avc_sps(&AvcSpsSpec::high_1080p().nal()).unwrap();
        assert_eq!((sps.profile_idc, sps.level_idc), (100, 40));
        // 1920x1088 coded, the bottom cropped by 4 chroma units of 2 rows each.
        assert_eq!((sps.width, sps.height), (1920, 1080));
        assert_eq!(sps.codec_string(), "avc1.640028");
        assert_eq!(sps.constraint_names(), "none");
        assert_eq!(sps.chroma_format_idc, 1);
    }

    #[test]
    fn the_size_of_a_phones_screen_stream() {
        // 1216x2656 as macroblocks: 76 x 166.
        let mut spec = AvcSpsSpec::high_1080p();
        spec.width_mbs = 76;
        spec.height_map_units = 166;
        spec.crop = Some([0, 5, 0, 17]);
        let sps = parse_avc_sps(&spec.nal()).unwrap();
        assert_eq!((sps.width, sps.height), (1216 - 10, 2656 - 34));
    }

    #[test]
    fn scaling_matrices_and_poc_type_1_are_skipped_over() {
        let mut spec = AvcSpsSpec::high_1080p();
        spec.profile_idc = 110;
        // A list that ends early (a delta that makes nextScale 0), a full one, and the last (64-entry) one.
        let ends_early = vec![-8]; // lastScale 8 + -8 = 0: the rest of the list repeats
        let full: Vec<i64> = (0..16).map(|i| if i % 2 == 0 { 3 } else { -2 }).collect();
        let long: Vec<i64> = (0..64).map(|i| (i % 5) - 2).collect();
        spec.scaling_lists = Some(vec![(0, ends_early), (3, full), (7, long)]);
        spec.poc_type = 1;
        let sps = parse_avc_sps(&spec.nal()).unwrap();
        assert_eq!((sps.width, sps.height), (1920, 1080));
        assert_eq!(sps.codec_string(), "avc1.6E0028");
    }

    #[test]
    fn interlaced_streams_double_the_height_and_the_crop_unit() {
        let mut spec = AvcSpsSpec::baseline_vga();
        spec.frame_mbs_only = false;
        spec.height_map_units = 15; // 15 map units of 2 macroblock rows each
        let sps = parse_avc_sps(&spec.nal()).unwrap();
        assert_eq!((sps.width, sps.height), (640, 480));
        assert!(!sps.frame_mbs_only);
        spec.crop = Some([0, 0, 0, 2]); // 2 * 2 * 2 = 8 rows
        assert_eq!(parse_avc_sps(&spec.nal()).unwrap().height, 472);
    }

    #[test]
    fn emulation_prevention_bytes_do_not_confuse_the_parser() {
        // A field with a long run of zero bits makes `00 00` in the bytes, and the encoder then writes
        // `00 00 03`. Find a value that does, and check that the size still reads right.
        let mut found = 0;
        // ue(2^n - 1) is n zero bits, a one, and n more zero bits: a long run of zeros.
        for n in 18..=30u32 {
            let mut spec = AvcSpsSpec::high_1080p();
            spec.crop = None;
            spec.poc_lsb = (1u32 << n) - 1;
            let nal = spec.nal();
            let escaped = nal.windows(3).any(|w| w == [0, 0, 3]);
            found += usize::from(escaped);
            let sps = parse_avc_sps(&nal).unwrap();
            assert_eq!((sps.width, sps.height), (1920, 1088), "poc_lsb {}", spec.poc_lsb);
        }
        assert!(found >= 3, "only {found} variants needed an emulation prevention byte");
    }

    #[test]
    fn bad_sps_input_is_an_error_not_a_panic() {
        assert_eq!(parse_avc_sps(&[]), Err(SpsError::NotSps));
        assert_eq!(parse_avc_sps(&[0x68, 1, 2, 3, 4]), Err(SpsError::NotSps)); // a PPS
        assert_eq!(parse_avc_sps(&[0xE7, 66, 0, 30, 0xFF]), Err(SpsError::NotSps)); // forbidden bit
        let nal = AvcSpsSpec::baseline_vga().nal();
        for n in 0..nal.len() - 1 {
            // Every shorter prefix: an error or (rarely) a smaller valid reading, never a panic.
            let _ = parse_avc_sps(&nal[..n]);
        }
        assert!(parse_avc_sps(&nal[..6]).is_err());
        for fill in 0..=255u8 {
            let mut v = vec![0x67, 100, 0, 40];
            v.extend(std::iter::repeat(fill).take(24));
            let _ = parse_avc_sps(&v);
        }
    }

    #[test]
    fn single_nal_units_are_counted_by_their_own_type() {
        let sps = AvcSpsSpec::baseline_vga().nal();
        let s = scan_payload(&sps);
        assert_eq!((s.kind, s.nal_types.clone()), (Some(7), vec![7]));
        assert_eq!(s.sps.as_deref(), Some(&sps[..]));
        let idr = [0x65, 0x88, 0x84];
        let s = scan_payload(&idr);
        assert_eq!((s.kind, s.nal_types, s.sps), (Some(5), vec![5], None));
        let slice = [0x41, 0x9A];
        assert_eq!(scan_payload(&slice).nal_types, vec![1]);
    }

    #[test]
    fn stap_a_lists_the_aggregated_units_and_finds_the_sps() {
        let sps = AvcSpsSpec::high_1080p().nal();
        let pps = [0x68, 0xEE, 0x3C, 0x80];
        let idr = [0x65, 1, 2, 3];
        let payload = stap_a(&[&sps, &pps, &idr]);
        let s = scan_payload(&payload);
        assert_eq!(s.kind, Some(24));
        assert_eq!(s.nal_types, vec![7, 8, 5]);
        assert_eq!(s.sps.as_deref(), Some(&sps[..]));
        // A truncated STAP-A: what is whole is counted, the rest ignored.
        let cut = &payload[..payload.len() - 2];
        assert_eq!(scan_payload(cut).nal_types, vec![7, 8]);
    }

    #[test]
    fn fu_a_counts_the_inner_type_at_its_start_fragment_only() {
        // FU indicator 0x7C (type 28, NRI 3), FU header S=1, type 5.
        let start = [0x7C, 0x85, 1, 2, 3];
        let middle = [0x7C, 0x05, 4, 5];
        let end = [0x7C, 0x45, 6];
        assert_eq!(scan_payload(&start).nal_types, vec![5]);
        assert_eq!(scan_payload(&start).kind, Some(28));
        assert!(scan_payload(&middle).nal_types.is_empty());
        assert!(scan_payload(&end).nal_types.is_empty());
        assert_eq!(scan_payload(&middle).kind, Some(28));
    }

    // ---- the configuration packet ----------------------------------------------------------

    fn sps_nal() -> Vec<u8> {
        AvcSpsSpec::baseline_vga().nal()
    }

    #[test]
    fn an_avcc_record_gives_its_parameter_sets() {
        let sps = sps_nal();
        let rec = avcc_record(66, 0xC0, 30, &[&sps], &[&PPS]);
        let cfg = parse_avcc(&rec).unwrap();
        assert_eq!((cfg.profile_indication, cfg.profile_compatibility, cfg.level_indication), (66, 0xC0, 30));
        assert_eq!(cfg.length_size, 4);
        assert_eq!(cfg.sps, vec![sps.clone()]);
        assert_eq!(cfg.pps, vec![PPS.to_vec()]);
        // Several of each, and a 2-byte length size.
        let sps2 = AvcSpsSpec::high_1080p().nal();
        let mut rec = avcc_record(100, 0, 40, &[&sps, &sps2], &[&PPS, &PPS]);
        rec[4] = 0xFD;
        let cfg = parse_avcc(&rec).unwrap();
        assert_eq!((cfg.sps.len(), cfg.pps.len(), cfg.length_size), (2, 2, 2));
        // What the high profiles add after the PPS list is ignored.
        rec.extend_from_slice(&[0xFD, 0xF8, 0xF8, 0x00]);
        assert!(parse_avcc(&rec).is_ok());
    }

    #[test]
    fn a_bad_avcc_record_is_an_error() {
        let sps = sps_nal();
        let good = avcc_record(66, 0xC0, 30, &[&sps], &[&PPS]);
        assert!(parse_avcc(&[]).unwrap_err().contains("too short"));
        let mut v = good.clone();
        v[0] = 2;
        assert!(parse_avcc(&v).unwrap_err().contains("configurationVersion"));
        // Cut short anywhere inside the lists.
        for n in 6..good.len() {
            assert!(parse_avcc(&good[..n]).is_err(), "cut at {n}");
        }
        // An SPS entry that is a PPS, and a PPS entry that is an SPS.
        let wrong = avcc_record(66, 0xC0, 30, &[&PPS], &[&PPS]);
        assert!(parse_avcc(&wrong).unwrap_err().contains("SPS"));
        let wrong = avcc_record(66, 0xC0, 30, &[&sps], &[&sps]);
        assert!(parse_avcc(&wrong).unwrap_err().contains("PPS"));
        // An empty entry.
        let mut empty = vec![1, 66, 0, 30, 0xFF, 0xE1, 0, 0, 1, 0, 0];
        assert!(parse_avcc(&empty).is_err());
        empty[7] = 4; // a length that runs past the end
        assert!(parse_avcc(&empty).is_err());
    }

    #[test]
    fn boxes_are_walked_with_their_sizes() {
        let a = make_box(b"abcd", &[1, 2, 3]);
        let b = make_box(b"efgh", &[]);
        let mut buf = a.clone();
        buf.extend_from_slice(&b);
        let got = boxes(&buf);
        assert_eq!(got.len(), 2);
        assert_eq!((&got[0].kind, got[0].content), (b"abcd", &[1u8, 2, 3][..]));
        assert_eq!((&got[1].kind, got[1].content), (b"efgh", &[][..]));
        // A size of 0 runs to the end; a size of 1 is followed by 64 bits.
        let mut zero = vec![0, 0, 0, 0];
        zero.extend_from_slice(b"last");
        zero.extend_from_slice(&[9, 9]);
        assert_eq!(boxes(&zero)[0].content, &[9, 9]);
        let mut big = vec![0, 0, 0, 1];
        big.extend_from_slice(b"big!");
        big.extend_from_slice(&19u64.to_be_bytes());
        big.extend_from_slice(&[7, 7, 7]);
        let got = boxes(&big);
        assert_eq!((&got[0].kind, got[0].content), (b"big!", &[7u8, 7, 7][..]));
        // A box that claims more than there is, one smaller than its header, and a short tail.
        assert!(boxes(&[0, 0, 1, 0, b'x', b'x', b'x', b'x']).is_empty());
        assert!(boxes(&[0, 0, 0, 4, b'x', b'x', b'x', b'x']).is_empty());
        assert!(boxes(&[0, 0, 0]).is_empty());
        assert_eq!(boxes(&[a.as_slice(), &[0, 0, 0]].concat()).len(), 1);
    }

    #[test]
    fn the_config_packet_of_the_display_stream_is_an_avc1_box_with_an_avcc() {
        let sps = sps_nal();
        let avcc = make_box(b"avcC", &avcc_record(66, 0xC0, 30, &[&sps], &[&PPS]));
        // The packet seen on an iPad: 92 e6 c0 a3, then an `avc1` box.
        let mut packet = vec![0x92, 0xE6, 0xC0, 0xA3];
        packet.extend_from_slice(&avc1_box(640, 480, &[avcc.clone()]));
        let cfg = find_avc_config(&packet).unwrap();
        assert_eq!(cfg.sps, vec![sps.clone()]);
        assert_eq!(cfg.pps, vec![PPS.to_vec()]);
        // Other child boxes before the avcC (a bitrate box, a pixel aspect ratio box).
        let btrt = make_box(b"btrt", &[0; 12]);
        let pasp = make_box(b"pasp", &[0, 0, 0, 1, 0, 0, 0, 1]);
        let mut packet = vec![0x92, 0xE6, 0xC0, 0xA3];
        packet.extend_from_slice(&avc1_box(640, 480, &[btrt, pasp, avcc.clone()]));
        assert_eq!(find_avc_config(&packet).unwrap().sps, vec![sps.clone()]);
        // A bare avcC box after a header, with no avc1 around it.
        let mut packet = vec![0x92, 0xE6, 0xC0, 0xA3];
        packet.extend_from_slice(&avcc);
        assert_eq!(find_avc_config(&packet).unwrap().pps, vec![PPS.to_vec()]);
        // The box at some other offset.
        let mut packet = vec![0x92; 11];
        packet.extend_from_slice(&avc1_box(640, 480, &[make_box(b"avcC", &avcc_record(66, 0, 30, &[&sps], &[&PPS]))]));
        assert!(find_avc_config(&packet).is_some());
    }

    /// The configuration packet of a real iPad (iPad14,3, iOS 27.0.1): `92 e6 c0 a3` and a 134-byte `avc1`
    /// box whose `avcC` child is 44 bytes (High profile, level 5.1, one SPS of 17 bytes and one PPS of 4).
    pub fn ipad_config_packet() -> Vec<u8> {
        let hex = |s: &str| -> Vec<u8> {
            (0..s.len() / 2).map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap()).collect()
        };
        let mut avcc_content = hex("016400 33ffe1 0011".replace(' ', "").as_str());
        avcc_content.extend(hex("27640033ac131450096034e9b810101010"));
        avcc_content.extend(hex("01 0004".replace(' ', "").as_str()));
        avcc_content.extend(hex("28ee3cb0"));
        avcc_content.extend(hex("02000000"));
        let avcc = make_box(b"avcC", &avcc_content);
        assert_eq!(avcc.len(), 44);
        // The avc1 box: 8 + 78 + 44 = 130, and 4 more bytes make the 134 of the capture.
        let mut avc1 = avc1_box(2400, 1680, &[avcc, vec![0, 0, 0, 0]]);
        assert_eq!(avc1.len(), 134);
        assert_eq!(&avc1[..8], &[0, 0, 0, 0x86, b'a', b'v', b'c', b'1']);
        let mut packet = vec![0x92, 0xE6, 0xC0, 0xA3];
        packet.append(&mut avc1);
        packet
    }

    #[test]
    fn the_real_ipad_config_packet_is_read() {
        let packet = ipad_config_packet();
        assert_eq!(&packet[..12], &[0x92, 0xE6, 0xC0, 0xA3, 0, 0, 0, 0x86, b'a', b'v', b'c', b'1']);
        let cfg = find_avc_config(&packet).unwrap();
        assert_eq!((cfg.profile_indication, cfg.profile_compatibility, cfg.level_indication), (100, 0, 51));
        assert_eq!(cfg.length_size, 4);
        assert_eq!(cfg.sps.len(), 1);
        assert_eq!(cfg.sps[0].len(), 17);
        assert_eq!(cfg.pps, vec![vec![0x28, 0xEE, 0x3C, 0xB0]]);
        // The SPS: High profile, level 5.1, the codec string the browser was given.
        let sps = parse_avc_sps(&cfg.sps[0]).unwrap();
        assert_eq!(sps.codec_string(), "avc1.640033");
        assert_eq!((sps.profile_idc, sps.level_idc), (100, 51));
        assert!(sps.frame_mbs_only);
        assert_eq!(sps.chroma_format_idc, 1);
        // The stream is the 2400x1680 "wireless" display of the measurements.
        assert_eq!((sps.width, sps.height), (2400, 1680));
    }

    #[test]
    fn a_config_that_is_not_an_avc1_box_is_still_read_when_it_is_a_record_or_annexb() {
        let sps = sps_nal();
        // A bare avcC record, with and without a header in front of it.
        let rec = avcc_record(66, 0xC0, 30, &[&sps], &[&PPS]);
        assert_eq!(find_avc_config(&rec).unwrap().pps, vec![PPS.to_vec()]);
        let mut with_header = vec![0x92, 0xE6, 0xC0, 0xA3];
        with_header.extend_from_slice(&rec);
        assert_eq!(find_avc_config(&with_header).unwrap().sps, vec![sps.clone()]);
        // SPS and PPS as Annex-B, with 3- and 4-byte start codes, other NAL units around them.
        let mut annexb = vec![0x92, 0xE6, 0xC0, 0xA3, 0, 0, 0, 1];
        annexb.extend_from_slice(&sps);
        annexb.extend_from_slice(&[0, 0, 1]);
        annexb.extend_from_slice(&PPS);
        annexb.extend_from_slice(&[0, 0, 0, 1, 0x65, 1, 2, 3]);
        let cfg = find_avc_config(&annexb).unwrap();
        assert_eq!(cfg.sps, vec![sps.clone()]);
        assert_eq!(cfg.pps, vec![PPS.to_vec()]);
        assert_eq!((cfg.profile_indication, cfg.level_indication), (sps[1], sps[3]));
        // Only an SPS: no usable configuration.
        let mut only_sps = vec![0, 0, 0, 1];
        only_sps.extend_from_slice(&sps);
        assert_eq!(find_avc_config(&only_sps), None);
        // The boxes still come first when both could be read.
        let avcc = make_box(b"avcC", &avcc_record(66, 0xC0, 30, &[&sps], &[&PPS]));
        let mut packet = vec![0x92, 0xE6, 0xC0, 0xA3];
        packet.extend_from_slice(&avc1_box(640, 480, &[avcc]));
        assert!(find_avc_config(&packet).is_some());
    }

    #[test]
    fn a_packet_without_a_usable_config_gives_none() {
        assert_eq!(find_avc_config(&[]), None);
        assert_eq!(find_avc_config(&[0x92, 0xE6, 0xC0, 0xA3]), None);
        // `avc1` without an avcC, and one whose avcC is broken.
        let mut p = vec![0x92, 0xE6, 0xC0, 0xA3];
        p.extend_from_slice(&avc1_box(640, 480, &[make_box(b"btrt", &[0; 12])]));
        assert_eq!(find_avc_config(&p), None);
        let mut p = vec![0x92, 0xE6, 0xC0, 0xA3];
        p.extend_from_slice(&avc1_box(640, 480, &[make_box(b"avcC", &[2, 1, 2, 3])]));
        assert_eq!(find_avc_config(&p), None);
        // A box whose size field lies.
        let mut p = vec![0x92, 0xE6, 0xC0, 0xA3];
        let mut b = avc1_box(640, 480, &[]);
        b[3] = 0xFF;
        p.extend_from_slice(&b);
        assert_eq!(find_avc_config(&p), None);
        // Random bytes with the word `avc1` in them never panic.
        for len in 4..120usize {
            let mut v: Vec<u8> = (0..len).map(|i| (i * 31 % 251) as u8).collect();
            v[len / 2..].iter_mut().zip(b"avc1avcC").for_each(|(d, s)| *d = *s);
            let _ = find_avc_config(&v);
        }
    }

    #[test]
    fn odd_payloads_are_harmless() {
        assert_eq!(scan_payload(&[]), PayloadScan::default());
        assert_eq!(scan_payload(&[0x7C]).nal_types, Vec::<u8>::new()); // an FU-A with no header
        assert_eq!(scan_payload(&[0x18, 0x00]).nal_types, Vec::<u8>::new()); // a STAP-A cut in a size
        assert_eq!(scan_payload(&[0x18, 0, 0]).nal_types, Vec::<u8>::new()); // a zero-sized unit
        assert_eq!(scan_payload(&[0x19, 1, 2]).kind, Some(25)); // STAP-B: its own kind, not unpacked
        assert_eq!(scan_payload(&[0x00, 1]).kind, Some(0));
    }
}
