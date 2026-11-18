//! H.264 (RFC 6184) helpers for the codec probe: NAL unit types and the sequence parameter set.

// The parser modules read untrusted bytes: a malformed input is an error, never a panic, so the lints that find the
// panics are denied here (the tests below may use them).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use crate::sps::{BitWriter, Bits, SpsError, escape_rbsp, unescape_rbsp};

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
            let mut rest = payload.get(1..).unwrap_or_default();
            while let Some((size, tail)) = rest.split_first_chunk::<2>() {
                let Some((nal, tail)) = tail.split_at_checked(usize::from(u16::from_be_bytes(*size))) else {
                    break;
                };
                let Some(&first) = nal.first() else {
                    break;
                };
                let t = nal_type(first);
                scan.nal_types.push(t);
                if t == NAL_SPS && scan.sps.is_none() {
                    scan.sps = Some(nal.to_vec());
                }
                rest = tail;
            }
        }
        NAL_FU_A => {
            // [FU indicator][FU header: S|E|R|type(5)][fragment]
            if let Some(&fu) = payload.get(1)
                && fu & 0x80 != 0
            {
                scan.nal_types.push(fu & 0x1f);
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
        format!("avc1.{:02X}{:02X}{:02X}", self.profile_idc, self.constraint_flags, self.level_idc)
    }

    /// The constraint flags that are set, by name (`set0`..`set5`).
    pub fn constraint_names(&self) -> String {
        // Flag `i` is bit `7 - i`: rotated left by `i` it is the top bit.
        let names: Vec<String> = (0..6u32)
            .filter(|i| self.constraint_flags.rotate_left(*i) & 0x80 != 0)
            .map(|i| format!("set{i}"))
            .collect();
        if names.is_empty() { "none".to_string() } else { names.join(" ") }
    }
}

/// Profiles whose SPS carries the chroma format, bit depths and scaling matrices.
fn has_chroma_info(profile_idc: u8) -> bool {
    matches!(profile_idc, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135)
}

fn skip_scaling_list(b: &mut Bits<'_>, size: usize) -> Result<(), SpsError> {
    let (mut last, mut next) = (8i64, 8i64);
    for _ in 0..size {
        if next != 0 {
            let delta = b.se()?;
            next = last.saturating_add(delta).rem_euclid(256);
        }
        if next != 0 {
            last = next;
        }
    }
    Ok(())
}

/// The longest side of a picture accepted (the same limit as for HEVC), coded or cropped.
const MAX_PICTURE_SIDE: u64 = 32768;

/// Parses an H.264 SPS NAL unit (with its one-byte header, without a start code): profile, level,
/// and the picture size with the cropping applied (ITU-T H.264 7.3.2.1.1).
pub fn parse_avc_sps(nal: &[u8]) -> Result<AvcSps, SpsError> {
    let (_, payload) = split_sps_nal(nal)?;
    // Emulation prevention covers everything after the NAL header byte, the profile, constraint flags and level
    // too (a `00 00` there is followed by `03` if the next byte is 0..=3): unescape it all, as the rewrite does.
    let rbsp = unescape_rbsp(payload);
    let mut b = Bits::new(&rbsp);
    let (profile_idc, constraint_flags, level_idc) = (b.byte(8)?, b.byte(8)?, b.byte(8)?);
    b.ue()?; // seq_parameter_set_id
    let mut chroma_format_idc = 1u8;
    if has_chroma_info(profile_idc) {
        chroma_format_idc = chroma_format_idc_of(b.ue()?)?;
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
    let width_mbs = u64::from(b.ue()?).saturating_add(1);
    let height_map_units = u64::from(b.ue()?).saturating_add(1);
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
    let [crop_left, crop_right, crop_top, crop_bottom] = crop;
    // 2 for a picture of two fields per frame, 1 for a frame only (a macroblock is 16 luma rows of a frame).
    let field_factor: u64 = if frame_mbs_only { 1 } else { 2 };
    // Every product below is of at most 2^33 and a small factor, so none saturates; a saturated value would only
    // be refused as too big, as it should.
    let frame_height_mbs = field_factor.saturating_mul(height_map_units);
    let coded_width = width_mbs.saturating_mul(16);
    let coded_height = frame_height_mbs.saturating_mul(16);
    // The coded size is checked as well as the cropped one: it is what `coded_width`/`coded_height` hold, and
    // macroblock counts read off the wire are up to 2^32, which would wrap there.
    if coded_width > MAX_PICTURE_SIDE || coded_height > MAX_PICTURE_SIDE {
        return Err(SpsError::Invalid("picture size"));
    }
    // Cropping is in chroma sample units (ChromaArrayType 0 has none).
    let (unit_x, unit_y): (u64, u64) = match chroma_format_idc {
        0 => (1, field_factor),
        1 => (2, field_factor.saturating_mul(2)),
        2 => (2, field_factor),
        _ => (1, field_factor),
    };
    let width = coded_width.saturating_sub(unit_x.saturating_mul(crop_left.saturating_add(crop_right)));
    let height = coded_height.saturating_sub(unit_y.saturating_mul(crop_top.saturating_add(crop_bottom)));
    let size = |side: u64| u32::try_from(side).ok().filter(|_| (1..=MAX_PICTURE_SIDE).contains(&side));
    let sizes = (size(width), size(height), u32::try_from(coded_width), u32::try_from(coded_height));
    let (Some(width), Some(height), Ok(coded_width), Ok(coded_height)) = sizes else {
        return Err(SpsError::Invalid("picture size"));
    };
    Ok(AvcSps {
        profile_idc,
        constraint_flags,
        level_idc,
        chroma_format_idc,
        frame_mbs_only,
        width,
        height,
        coded_width,
        coded_height,
    })
}

/// The `chroma_format_idc` of an SPS: 0 to 3.
fn chroma_format_idc_of(v: u32) -> Result<u8, SpsError> {
    u8::try_from(v).ok().filter(|c| *c <= 3).ok_or(SpsError::Invalid("chroma_format_idc"))
}

/// An H.264 SPS NAL unit (of at least 5 bytes: its header byte and four more) as its header byte and the rest;
/// `NotSps` for anything else.
fn split_sps_nal(nal: &[u8]) -> Result<(u8, &[u8]), SpsError> {
    match nal.split_first() {
        Some((&header, rest)) if nal.len() >= 5 && header & 0x80 == 0 && nal_type(header) == NAL_SPS => {
            Ok((header, rest))
        }
        _ => Err(SpsError::NotSps),
    }
}

// ---- the SPS rewrite: a VUI bitstream restriction ------------------------------------------------
//
// A browser's hardware H.264 decoder that finds no `bitstream_restriction` in the SPS assumes the level's
// whole decoded picture buffer may be needed to reorder pictures and holds that many frames (11 at
// 2400x1680, level 5.1) before it outputs the first: a latency of several hundred milliseconds. The
// display stream has no B-frames, so saying so costs nothing: the SPS is rewritten to carry
// `max_num_reorder_frames = 0` and `max_dec_frame_buffering = max_num_ref_frames`.

/// The fields of the VUI's `bitstream_restriction`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Restriction {
    pub motion_vectors_over_pic_boundaries: bool,
    pub max_bytes_per_pic_denom: u32,
    pub max_bits_per_mb_denom: u32,
    pub log2_max_mv_length_horizontal: u32,
    pub log2_max_mv_length_vertical: u32,
    pub max_num_reorder_frames: u32,
    pub max_dec_frame_buffering: u32,
}

/// What came of the rewrite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpsRewrite {
    /// The SPS with the restriction added (the NAL unit, emulation prevention included).
    Patched(Vec<u8>),
    /// The SPS has a restriction already: it is left as it is.
    AlreadyRestricted(Restriction),
}

/// Reads bits and, if asked to, writes the same bits.
struct Copier<'a> {
    r: Bits<'a>,
    w: Option<BitWriter>,
}

impl Copier<'_> {
    fn u(&mut self, n: u32) -> Result<u32, SpsError> {
        let v = self.r.u(n)?;
        if let Some(w) = self.w.as_mut() {
            w.u(n, u64::from(v));
        }
        Ok(v)
    }

    fn ue(&mut self) -> Result<u32, SpsError> {
        let v = self.r.ue()?;
        if let Some(w) = self.w.as_mut() {
            w.ue(v);
        }
        Ok(v)
    }

    fn se(&mut self) -> Result<i64, SpsError> {
        let v = self.r.se()?;
        if let Some(w) = self.w.as_mut() {
            w.se(v);
        }
        Ok(v)
    }

    fn byte(&mut self, n: u32) -> Result<u8, SpsError> {
        u8::try_from(self.u(n)?).map_err(|_| SpsError::Invalid("a field wider than a byte"))
    }

    fn scaling_list(&mut self, size: usize) -> Result<(), SpsError> {
        let (mut last, mut next) = (8i64, 8i64);
        for _ in 0..size {
            if next != 0 {
                next = last.saturating_add(self.se()?).rem_euclid(256);
            }
            if next != 0 {
                last = next;
            }
        }
        Ok(())
    }

    /// `hrd_parameters()` (Annex E).
    fn hrd(&mut self) -> Result<(), SpsError> {
        let cpb_cnt = self.ue()?;
        if cpb_cnt > 31 {
            return Err(SpsError::Invalid("cpb_cnt_minus1"));
        }
        self.u(4)?; // bit_rate_scale
        self.u(4)?; // cpb_size_scale
        for _ in 0..=cpb_cnt {
            self.ue()?; // bit_rate_value_minus1
            self.ue()?; // cpb_size_value_minus1
            self.u(1)?; // cbr_flag
        }
        self.u(5)?; // initial_cpb_removal_delay_length_minus1
        self.u(5)?; // cpb_removal_delay_length_minus1
        self.u(5)?; // dpb_output_delay_length_minus1
        self.u(5)?; // time_offset_length
        Ok(())
    }
}

/// Walks the SPS up to its `bitstream_restriction_flag`. With `write`, the bits are copied to a writer, and an
/// SPS with no VUI gets one that holds nothing but flags that are off. Returns the number of reference
/// frames, the restriction if the SPS has one, and the writer (positioned at the restriction flag).
fn walk_sps(nal: &[u8], write: bool) -> Result<(u32, Option<Restriction>, Option<BitWriter>), SpsError> {
    let (_, payload) = split_sps_nal(nal)?;
    let rbsp = unescape_rbsp(payload);
    let mut c = Copier { r: Bits::new(&rbsp), w: write.then(BitWriter::default) };
    let profile_idc = c.byte(8)?;
    c.u(8)?; // constraint flags
    c.u(8)?; // level_idc
    c.ue()?; // seq_parameter_set_id
    if has_chroma_info(profile_idc) {
        let chroma_format_idc = chroma_format_idc_of(c.ue()?)?;
        if chroma_format_idc == 3 {
            c.u(1)?; // separate_colour_plane_flag
        }
        c.ue()?; // bit_depth_luma_minus8
        c.ue()?; // bit_depth_chroma_minus8
        c.u(1)?; // qpprime_y_zero_transform_bypass_flag
        if c.u(1)? == 1 {
            // seq_scaling_matrix_present_flag
            for i in 0..(if chroma_format_idc == 3 { 12 } else { 8 }) {
                if c.u(1)? == 1 {
                    c.scaling_list(if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    c.ue()?; // log2_max_frame_num_minus4
    match c.ue()? {
        0 => {
            c.ue()?; // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            c.u(1)?; // delta_pic_order_always_zero_flag
            c.se()?; // offset_for_non_ref_pic
            c.se()?; // offset_for_top_to_bottom_field
            let n = c.ue()?;
            if n > 255 {
                return Err(SpsError::Invalid("num_ref_frames_in_pic_order_cnt_cycle"));
            }
            for _ in 0..n {
                c.se()?;
            }
        }
        _ => {}
    }
    let max_num_ref_frames = c.ue()?;
    c.u(1)?; // gaps_in_frame_num_value_allowed_flag
    c.ue()?; // pic_width_in_mbs_minus1
    c.ue()?; // pic_height_in_map_units_minus1
    if c.u(1)? == 0 {
        c.u(1)?; // mb_adaptive_frame_field_flag
    }
    c.u(1)?; // direct_8x8_inference_flag
    if c.u(1)? == 1 {
        // frame_cropping_flag
        for _ in 0..4 {
            c.ue()?;
        }
    }

    // vui_parameters_present_flag, and the VUI up to bitstream_restriction_flag.
    if c.r.bit()? == 0 {
        if let Some(w) = c.w.as_mut() {
            // A VUI of its own: present, with every flag before the restriction off (aspect ratio,
            // overscan, video signal type, chroma location, timing, NAL HRD, VCL HRD, pic_struct).
            w.u(1, 1);
            for _ in 0..8 {
                w.u(1, 0);
            }
        }
        return Ok((max_num_ref_frames, None, c.w));
    }
    if let Some(w) = c.w.as_mut() {
        w.u(1, 1);
    }
    if c.u(1)? == 1 {
        // aspect_ratio_info_present_flag
        if c.u(8)? == 255 {
            c.u(16)?; // sar_width
            c.u(16)?; // sar_height
        }
    }
    if c.u(1)? == 1 {
        c.u(1)?; // overscan_appropriate_flag
    }
    if c.u(1)? == 1 {
        // video_signal_type_present_flag
        c.u(3)?; // video_format
        c.u(1)?; // video_full_range_flag
        if c.u(1)? == 1 {
            c.u(8)?; // colour_primaries
            c.u(8)?; // transfer_characteristics
            c.u(8)?; // matrix_coefficients
        }
    }
    if c.u(1)? == 1 {
        c.ue()?; // chroma_sample_loc_type_top_field
        c.ue()?; // chroma_sample_loc_type_bottom_field
    }
    if c.u(1)? == 1 {
        // timing_info_present_flag
        c.u(32)?; // num_units_in_tick
        c.u(32)?; // time_scale
        c.u(1)?; // fixed_frame_rate_flag
    }
    let nal_hrd = c.u(1)? == 1;
    if nal_hrd {
        c.hrd()?;
    }
    let vcl_hrd = c.u(1)? == 1;
    if vcl_hrd {
        c.hrd()?;
    }
    if nal_hrd || vcl_hrd {
        c.u(1)?; // low_delay_hrd_flag
    }
    c.u(1)?; // pic_struct_present_flag
    if c.r.bit()? == 0 {
        // bitstream_restriction_flag: not written, the caller writes it with its fields
        return Ok((max_num_ref_frames, None, c.w));
    }
    let restriction = Restriction {
        motion_vectors_over_pic_boundaries: c.r.bit()? == 1,
        max_bytes_per_pic_denom: c.r.ue()?,
        max_bits_per_mb_denom: c.r.ue()?,
        log2_max_mv_length_horizontal: c.r.ue()?,
        log2_max_mv_length_vertical: c.r.ue()?,
        max_num_reorder_frames: c.r.ue()?,
        max_dec_frame_buffering: c.r.ue()?,
    };
    Ok((max_num_ref_frames, Some(restriction), c.w))
}

/// The VUI `bitstream_restriction` of an SPS NAL unit, if it has one.
#[cfg(test)]
pub fn sps_restriction(nal: &[u8]) -> Result<Option<Restriction>, SpsError> {
    Ok(walk_sps(nal, false)?.1)
}

/// Adds `bitstream_restriction` (no reordering; a decoded picture buffer of `max_num_ref_frames`, at
/// least 1) to an H.264 SPS NAL unit, copying every bit before it as it was. An SPS with no VUI gets one
/// that holds only the restriction. An SPS that has a restriction already is not touched. The NAL header
/// byte is kept, and the result has its emulation prevention bytes.
pub fn add_bitstream_restriction(nal: &[u8]) -> Result<SpsRewrite, SpsError> {
    let (max_num_ref_frames, existing, writer) = walk_sps(nal, true)?;
    if let Some(r) = existing {
        return Ok(SpsRewrite::AlreadyRestricted(r));
    }
    let (Some(mut w), Some(&header)) = (writer, nal.first()) else {
        return Err(SpsError::NotSps); // not reached: `walk_sps` wrote, and read a header byte
    };
    w.u(1, 1); // bitstream_restriction_flag
    w.u(1, 1); // motion_vectors_over_pic_boundaries_flag
    w.ue(0); // max_bytes_per_pic_denom
    w.ue(0); // max_bits_per_mb_denom
    w.ue(15); // log2_max_mv_length_horizontal
    w.ue(15); // log2_max_mv_length_vertical
    w.ue(0); // max_num_reorder_frames
    w.ue(max_num_ref_frames.max(1)); // max_dec_frame_buffering
    let mut out = vec![header];
    out.extend_from_slice(&escape_rbsp(&w.finish()));
    Ok(SpsRewrite::Patched(out))
}

/// The slice type of a slice NAL unit (types 1 and 5): the second `ue(v)` of its header, 0 to 9.
pub fn slice_type(nal: &[u8]) -> Option<u32> {
    if !matches!(nal_type(*nal.first()?), 1 | 5) {
        return None;
    }
    let head = unescape_rbsp(nal.get(1..nal.len().min(17))?);
    let mut b = Bits::new(&head);
    b.ue().ok()?; // first_mb_in_slice
    let t = b.ue().ok()?;
    (t <= 9).then_some(t)
}

/// A B slice (slice_type 1 or 6) of a non-IDR picture: an IDR picture has only I slices (H.264 7.4.3),
/// so a slice of NAL type 5 is never counted, whatever its header says.
pub fn is_b_slice(nal: &[u8]) -> bool {
    nal.first().is_some_and(|&b| nal_type(b) == 1) && matches!(slice_type(nal), Some(1 | 6))
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

/// Parses an `AVCDecoderConfigurationRecord`: version 1, profile, compatibility, level, the length
/// size, the SPS list and the PPS list (each entry a 16-bit length and the NAL unit). Anything after the
/// PPS list (the extension of the high profiles) is left alone.
pub fn parse_avcc(rec: &[u8]) -> Result<AvcConfig, String> {
    let [version, profile_indication, profile_compatibility, level_indication, length, num_sps, _, ..] = rec else {
        return Err("avcC record too short".into());
    };
    if *version != 1 {
        return Err(format!("avcC configurationVersion {version} (expected 1)"));
    }
    let lists = rec.get(6..).unwrap_or_default();
    let (sps, lists) = parameter_set_list(lists, num_sps & 0x1f, "SPS", NAL_SPS)?;
    let (num_pps, lists) = lists.split_first().ok_or("avcC has no PPS count")?;
    let (pps, _) = parameter_set_list(lists, *num_pps, "PPS", NAL_PPS)?;
    Ok(AvcConfig {
        profile_indication: *profile_indication,
        profile_compatibility: *profile_compatibility,
        level_indication: *level_indication,
        length_size: (length & 3).saturating_add(1),
        sps,
        pps,
    })
}

/// `count` entries of an avcC parameter-set list (each a 16-bit length and a NAL unit of type `nal_kind`, `what`
/// is its name for the errors): the NAL units, and what follows the list.
fn parameter_set_list<'a>(
    mut data: &'a [u8],
    count: u8,
    what: &str,
    nal_kind: u8,
) -> Result<(Vec<Vec<u8>>, &'a [u8]), String> {
    let mut sets = Vec::new();
    for _ in 0..count {
        let (len, tail) = data.split_first_chunk::<2>().ok_or_else(|| format!("avcC {what} list is cut short"))?;
        let (nal, tail) = tail
            .split_at_checked(usize::from(u16::from_be_bytes(*len)))
            .ok_or_else(|| format!("avcC {what} is cut short"))?;
        if nal.first().is_none_or(|b| nal_type(*b) != nal_kind) {
            let article = if nal_kind == NAL_SPS { "an" } else { "a" };
            return Err(format!("avcC {what} entry is not {article} {what} NAL unit"));
        }
        sets.push(nal.to_vec());
        data = tail;
    }
    Ok((sets, data))
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
    while let Some((&[s0, s1, s2, s3, k0, k1, k2, k3], after_header)) = buf.split_first_chunk::<8>() {
        let kind = [k0, k1, k2, k3];
        let (header, total) = match u32::from_be_bytes([s0, s1, s2, s3]) {
            0 => (8, Some(buf.len())),
            1 => {
                let Some(big) = after_header.first_chunk::<8>() else { break };
                (16, usize::try_from(u64::from_be_bytes(*big)).ok())
            }
            n => (8, usize::try_from(n).ok()),
        };
        // A size that is below the header's or above what is left ends the boxes (so does one that is no usize).
        let Some((whole, tail)) = total.and_then(|total| buf.split_at_checked(total)) else { break };
        let Some(content) = whole.get(header..) else { break };
        out.push(BoxRef { kind, content });
        buf = tail;
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
    let children = content.get(VISUAL_SAMPLE_ENTRY_FIELDS..).ok_or("avc1 box is shorter than a visual sample entry")?;
    let avcc = boxes(children).into_iter().find(|b| &b.kind == b"avcC").ok_or("avc1 box has no avcC child")?;
    parse_avcc(avcc.content)
}

/// SPS and PPS NAL units found as Annex-B (start codes `00 00 01` or `00 00 00 01`) anywhere in a payload.
fn annexb_config(payload: &[u8]) -> Option<AvcConfig> {
    let mut starts = Vec::new(); // (start of the NAL unit, start of its start code)
    let mut i = 0usize;
    while let Some(&[a, b, c]) = payload.get(i..).and_then(<[u8]>::first_chunk::<3>) {
        if (a, b, c) == (0, 0, 1) {
            // A zero byte before the start code makes it a 4-byte one.
            let code_at = i.checked_sub(1).filter(|before| payload.get(*before) == Some(&0)).unwrap_or(i);
            starts.push((i.saturating_add(3), code_at));
            i = i.saturating_add(3);
        } else {
            i = i.saturating_add(1);
        }
    }
    let (mut sps, mut pps) = (None, None);
    for (k, &(nal_at, _)) in starts.iter().enumerate() {
        let end = starts.get(k.saturating_add(1)).map_or(payload.len(), |&(_, code_at)| code_at);
        let nal = payload.get(nal_at..end)?;
        match nal.first().map(|b| nal_type(*b)) {
            Some(NAL_SPS) if sps.is_none() && nal.len() >= 4 => sps = Some(nal.to_vec()),
            Some(NAL_PPS) if pps.is_none() && nal.len() >= 2 => pps = Some(nal.to_vec()),
            _ => {}
        }
    }
    let sps: Vec<u8> = sps?;
    let pps = pps?;
    let &[_, profile_indication, profile_compatibility, level_indication, ..] = sps.as_slice() else {
        return None; // not reached: an SPS is kept only with 4 bytes or more
    };
    Some(AvcConfig {
        profile_indication,
        profile_compatibility,
        level_indication,
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
            (0..=8usize.min(payload.len().saturating_sub(1))).find_map(|off| parse_avcc(payload.get(off..)?).ok())
        })
        .or_else(|| annexb_config(payload))
}

fn find_avc_boxes(payload: &[u8]) -> Option<AvcConfig> {
    // The position of a box type at 4 or later (after the 4-byte size of its box).
    let find = |kind: &[u8; 4]| payload.windows(4).enumerate().skip(4).find(|(_, w)| w == kind).map(|(i, _)| i);
    let box_at =
        |type_at: usize| -> Option<BoxRef<'_>> { boxes(payload.get(type_at.checked_sub(4)?..)?).into_iter().next() };
    if let Some(i) = find(b"avc1")
        && let Some(b) = box_at(i)
        && let Ok(cfg) = parse_avc1_content(b.content)
    {
        return Some(cfg);
    }
    if let Some(i) = find(b"avcC")
        && let Some(b) = box_at(i)
        && let Ok(cfg) = parse_avcc(b.content)
    {
        return Some(cfg);
    }
    None
}

#[cfg(test)]
pub mod testutil {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        reason = "test code: a panic is a failed test, and the inputs are small, fixed values"
    )]
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
            v.extend_from_slice(&u16::try_from(n.len()).unwrap().to_be_bytes());
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
        let mut r = vec![1, profile, compat, level, 0xFF, 0xE0 | u8::try_from(sps.len()).unwrap()];
        for s in sps {
            r.extend_from_slice(&u16::try_from(s.len()).unwrap().to_be_bytes());
            r.extend_from_slice(s);
        }
        r.push(u8::try_from(pps.len()).unwrap());
        for p in pps {
            r.extend_from_slice(&u16::try_from(p.len()).unwrap().to_be_bytes());
            r.extend_from_slice(p);
        }
        r
    }

    /// An ISO box: size, type, content.
    pub fn make_box(kind: &[u8; 4], content: &[u8]) -> Vec<u8> {
        let mut b = u32::try_from(content.len() + 8).unwrap().to_be_bytes().to_vec();
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

    /// The HRD parameters of a VUI.
    #[derive(Debug, Clone, Default)]
    pub struct HrdSpec {
        pub cpb_cnt_minus1: u32,
        pub bit_rate_scale: u32,
        pub cpb_size_scale: u32,
        pub entries: Vec<(u32, u32, bool)>,
        /// initial_cpb_removal_delay_length_minus1, cpb_removal_delay_length_minus1,
        /// dpb_output_delay_length_minus1, time_offset_length
        pub lengths: [u32; 4],
    }

    /// The VUI of an SPS, field by field.
    #[allow(clippy::type_complexity)]
    #[derive(Debug, Clone, Default)]
    pub struct VuiSpec {
        /// aspect_ratio_idc, and the SAR for 255.
        pub aspect: Option<(u8, Option<(u16, u16)>)>,
        pub overscan: Option<bool>,
        /// video_format, full range, and the three colour description bytes.
        pub video_signal: Option<(u32, bool, Option<(u8, u8, u8)>)>,
        pub chroma_loc: Option<(u32, u32)>,
        /// num_units_in_tick, time_scale, fixed_frame_rate_flag
        pub timing: Option<(u32, u32, bool)>,
        pub nal_hrd: Option<HrdSpec>,
        pub vcl_hrd: Option<HrdSpec>,
        pub low_delay_hrd: bool,
        pub pic_struct: bool,
        pub restriction: Option<Restriction>,
    }

    impl VuiSpec {
        /// What the iPad's SPS has: the video signal type with full range and no colour description.
        pub fn ipad() -> Self {
            VuiSpec { video_signal: Some((5, true, None)), ..VuiSpec::default() }
        }

        /// Everything there is, with an HRD of two CPBs for both kinds.
        pub fn everything() -> Self {
            let hrd = HrdSpec {
                cpb_cnt_minus1: 1,
                bit_rate_scale: 4,
                cpb_size_scale: 6,
                entries: vec![(9_999, 19_999, false), (49_999, 99_999, true)],
                lengths: [23, 23, 23, 24],
            };
            VuiSpec {
                aspect: Some((255, Some((4, 3)))),
                overscan: Some(true),
                video_signal: Some((5, true, Some((1, 1, 1)))),
                chroma_loc: Some((0, 1)),
                timing: Some((1001, 60_000, true)),
                nal_hrd: Some(hrd.clone()),
                vcl_hrd: Some(hrd),
                low_delay_hrd: true,
                pic_struct: true,
                restriction: None,
            }
        }

        fn write(&self, w: &mut BitWriter) {
            w.bit(1); // vui_parameters_present_flag
            match self.aspect {
                None => w.bit(0),
                Some((idc, sar)) => {
                    w.bit(1);
                    w.u(8, u64::from(idc));
                    if idc == 255 {
                        let (x, y) = sar.expect("the extended SAR needs its size");
                        w.u(16, u64::from(x));
                        w.u(16, u64::from(y));
                    }
                }
            }
            match self.overscan {
                None => w.bit(0),
                Some(v) => {
                    w.bit(1);
                    w.bit(u32::from(v));
                }
            }
            match self.video_signal {
                None => w.bit(0),
                Some((format, full_range, colour)) => {
                    w.bit(1);
                    w.u(3, u64::from(format));
                    w.bit(u32::from(full_range));
                    match colour {
                        None => w.bit(0),
                        Some((p, t, m)) => {
                            w.bit(1);
                            w.u(8, u64::from(p));
                            w.u(8, u64::from(t));
                            w.u(8, u64::from(m));
                        }
                    }
                }
            }
            match self.chroma_loc {
                None => w.bit(0),
                Some((top, bottom)) => {
                    w.bit(1);
                    w.ue(top);
                    w.ue(bottom);
                }
            }
            match self.timing {
                None => w.bit(0),
                Some((ticks, scale, fixed)) => {
                    w.bit(1);
                    w.u(32, u64::from(ticks));
                    w.u(32, u64::from(scale));
                    w.bit(u32::from(fixed));
                }
            }
            for hrd in [&self.nal_hrd, &self.vcl_hrd] {
                match hrd {
                    None => w.bit(0),
                    Some(h) => {
                        w.bit(1);
                        w.ue(h.cpb_cnt_minus1);
                        w.u(4, u64::from(h.bit_rate_scale));
                        w.u(4, u64::from(h.cpb_size_scale));
                        for (rate, size, cbr) in &h.entries {
                            w.ue(*rate);
                            w.ue(*size);
                            w.bit(u32::from(*cbr));
                        }
                        for l in h.lengths {
                            w.u(5, u64::from(l));
                        }
                    }
                }
            }
            if self.nal_hrd.is_some() || self.vcl_hrd.is_some() {
                w.bit(u32::from(self.low_delay_hrd));
            }
            w.bit(u32::from(self.pic_struct));
            match self.restriction {
                None => w.bit(0),
                Some(r) => {
                    w.bit(1);
                    w.bit(u32::from(r.motion_vectors_over_pic_boundaries));
                    w.ue(r.max_bytes_per_pic_denom);
                    w.ue(r.max_bits_per_mb_denom);
                    w.ue(r.log2_max_mv_length_horizontal);
                    w.ue(r.log2_max_mv_length_vertical);
                    w.ue(r.max_num_reorder_frames);
                    w.ue(r.max_dec_frame_buffering);
                }
            }
        }
    }

    /// The restriction the rewrite adds, for a stream with `refs` reference frames.
    pub fn added_restriction(refs: u32) -> Restriction {
        Restriction {
            motion_vectors_over_pic_boundaries: true,
            max_bytes_per_pic_denom: 0,
            max_bits_per_mb_denom: 0,
            log2_max_mv_length_horizontal: 15,
            log2_max_mv_length_vertical: 15,
            max_num_reorder_frames: 0,
            max_dec_frame_buffering: refs.max(1),
        }
    }

    #[derive(Debug, Clone)]
    pub struct AvcSpsSpec {
        pub sps_id: u32,
        pub profile_idc: u8,
        pub constraint_flags: u8,
        pub level_idc: u8,
        pub chroma_format_idc: u32,
        /// For the high profiles: the scaling lists present as (index, deltas to write).
        pub scaling_lists: Option<Vec<(usize, Vec<i64>)>>,
        pub poc_type: u32,
        /// `log2_max_pic_order_cnt_lsb_minus4` for poc type 0 (its value is never used).
        pub poc_lsb: u32,
        pub num_ref_frames: u32,
        pub vui: Option<VuiSpec>,
        pub width_mbs: u32,
        pub height_map_units: u32,
        pub frame_mbs_only: bool,
        pub crop: Option<[u32; 4]>,
    }

    impl AvcSpsSpec {
        /// Baseline, level 3.0, 640x480.
        pub fn baseline_vga() -> Self {
            AvcSpsSpec {
                sps_id: 0,
                profile_idc: 66,
                constraint_flags: 0xC0,
                level_idc: 30,
                chroma_format_idc: 1,
                scaling_lists: None,
                poc_type: 2,
                poc_lsb: 2,
                num_ref_frames: 4,
                vui: None,
                width_mbs: 40,
                height_map_units: 30,
                frame_mbs_only: true,
                crop: None,
            }
        }

        /// High, level 4.0, 1920x1088 coded and cropped to 1080.
        pub fn high_1080p() -> Self {
            AvcSpsSpec {
                sps_id: 0,
                profile_idc: 100,
                constraint_flags: 0,
                level_idc: 40,
                chroma_format_idc: 1,
                scaling_lists: None,
                poc_type: 0,
                poc_lsb: 2,
                num_ref_frames: 4,
                vui: None,
                width_mbs: 120,
                height_map_units: 68,
                frame_mbs_only: true,
                crop: Some([0, 0, 0, 4]),
            }
        }

        pub fn nal(&self) -> Vec<u8> {
            let mut w = BitWriter::default();
            w.ue(self.sps_id); // seq_parameter_set_id
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
            w.ue(self.num_ref_frames); // max_num_ref_frames
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
            match &self.vui {
                None => w.bit(0), // vui_parameters_present_flag
                Some(v) => v.write(&mut w),
            }
            let rbsp = w.finish();
            let mut nal = vec![0x67, self.profile_idc, self.constraint_flags, self.level_idc];
            nal.extend_from_slice(&escape_rbsp(&rbsp));
            nal
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        reason = "test code: a panic is a failed test, and the inputs are small, fixed values"
    )]
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
    fn a_coded_size_over_32768_is_refused_even_when_the_cropped_one_is_not() {
        // 2048 macroblocks across is 32768: the limit itself is fine.
        let mut spec = AvcSpsSpec::baseline_vga();
        spec.width_mbs = 2048;
        let sps = parse_avc_sps(&spec.nal()).unwrap();
        assert_eq!((sps.width, sps.coded_width), (32768, 32768));
        spec.height_map_units = 2048;
        assert_eq!(parse_avc_sps(&spec.nal()).unwrap().coded_height, 32768);
        // One macroblock more is 32784 coded, and cropping 8 chroma units off it (16 columns) would leave
        // 32768: the picture is still too big for the coded size.
        spec.width_mbs = 2049;
        spec.crop = Some([0, 8, 0, 0]);
        assert_eq!(parse_avc_sps(&spec.nal()), Err(SpsError::Invalid("picture size")));
        let mut spec = AvcSpsSpec::baseline_vga();
        spec.height_map_units = 2049;
        spec.crop = Some([0, 0, 0, 8]);
        assert_eq!(parse_avc_sps(&spec.nal()), Err(SpsError::Invalid("picture size")));
        // Interlaced: the frame is two map units tall for each.
        let mut spec = AvcSpsSpec::baseline_vga();
        spec.frame_mbs_only = false;
        spec.height_map_units = 1025; // 2050 macroblocks of frame height
        assert_eq!(parse_avc_sps(&spec.nal()), Err(SpsError::Invalid("picture size")));
        // Macroblock counts that do not fit 32 bits once multiplied by 16 are an error, not a wrapped size.
        let mut spec = AvcSpsSpec::baseline_vga();
        spec.width_mbs = 1 << 28; // x 16 = 2^32: wrapped to 0 as a u32
        spec.crop = Some([0, 0, 0, 0]);
        assert_eq!(parse_avc_sps(&spec.nal()), Err(SpsError::Invalid("picture size")));
    }

    #[test]
    fn an_emulation_prevention_byte_between_the_level_and_the_first_field_is_removed() {
        // Constraint flags 0 and level 0 followed by an SPS id of 255 (eight zero bits) make `00 00 00`, which the
        // NAL unit escapes as `00 00 03 00` across the end of the header fields: the byte after the level is 03.
        let mut spec = AvcSpsSpec::baseline_vga();
        spec.sps_id = 255;
        spec.constraint_flags = 0;
        spec.level_idc = 0;
        let unescaped = spec.nal();
        let mut escaped = vec![unescaped[0]];
        escaped.extend_from_slice(&escape_rbsp(&unescaped[1..]));
        assert_eq!(&escaped[1..6], &[66, 0, 0, 3, 0], "the escape byte is there");
        let want = parse_avc_sps(&unescaped).unwrap();
        assert_eq!((want.width, want.height), (640, 480));
        assert_eq!(parse_avc_sps(&escaped), Ok(want.clone()));
        // And the rewrite, which always unescaped it, leaves the same picture.
        let SpsRewrite::Patched(patched) = add_bitstream_restriction(&escaped).unwrap() else { panic!("not patched") };
        assert_eq!(parse_avc_sps(&patched), Ok(want));
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
            v.extend(std::iter::repeat_n(fill, 24));
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

    // ---- the SPS rewrite --------------------------------------------------------------------

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len() / 2).map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap()).collect()
    }

    fn patched(nal: &[u8]) -> Vec<u8> {
        match add_bitstream_restriction(nal).unwrap() {
            SpsRewrite::Patched(p) => p,
            other => panic!("expected a rewrite, got {other:?}"),
        }
    }

    /// The same SPS, built with the restriction that the rewrite adds.
    fn built_with_restriction(spec: &AvcSpsSpec) -> Vec<u8> {
        let mut spec = spec.clone();
        let mut vui = spec.vui.take().unwrap_or_default();
        vui.restriction = Some(added_restriction(spec.num_ref_frames));
        spec.vui = Some(vui);
        spec.nal()
    }

    #[test]
    fn the_ipads_sps_gets_its_restriction_bit_for_bit() {
        // The vector verified with ffmpeg's trace_headers and by decoding the whole capture identically.
        let input = unhex("27640033ac131450096034e9b810101010");
        let want = unhex("27640033ac131450096034e9b81010103c201096");
        assert_eq!((input.len(), want.len()), (17, 20));
        assert_eq!(patched(&input), want);
        // It reads back as a restriction with no reordering and four frames of buffer (the SPS has 4 references).
        let r = sps_restriction(&want).unwrap().unwrap();
        assert_eq!(r, added_restriction(4));
        assert_eq!(sps_restriction(&input).unwrap(), None);
        // Nothing the browser is told about the stream changed: profile, level, size, codec string.
        assert_eq!(parse_avc_sps(&want).unwrap(), parse_avc_sps(&input).unwrap());
        assert_eq!(parse_avc_sps(&want).unwrap().codec_string(), "avc1.640033");
        // Rewriting it again is leaving it alone.
        assert!(matches!(add_bitstream_restriction(&want).unwrap(), SpsRewrite::AlreadyRestricted(r2) if r2 == r));
    }

    #[test]
    fn an_sps_with_no_vui_gets_one_with_only_the_restriction() {
        let spec = AvcSpsSpec::high_1080p();
        let out = patched(&spec.nal());
        assert_eq!(out, built_with_restriction(&spec), "the VUI holds the restriction and flags that are off");
        assert_eq!(sps_restriction(&out).unwrap(), Some(added_restriction(4)));
        // Baseline and interlaced streams too.
        for spec in [AvcSpsSpec::baseline_vga(), {
            let mut s = AvcSpsSpec::baseline_vga();
            s.frame_mbs_only = false;
            s.height_map_units = 15;
            s
        }] {
            assert_eq!(patched(&spec.nal()), built_with_restriction(&spec));
        }
    }

    #[test]
    fn the_buffer_is_the_number_of_reference_frames_and_at_least_one() {
        for refs in [0u32, 1, 2, 4, 16] {
            let mut spec = AvcSpsSpec::high_1080p();
            spec.num_ref_frames = refs;
            let out = patched(&spec.nal());
            let r = sps_restriction(&out).unwrap().unwrap();
            assert_eq!(r.max_dec_frame_buffering, refs.max(1), "{refs} reference frames");
            assert_eq!(r.max_num_reorder_frames, 0);
            assert_eq!(out, built_with_restriction(&spec));
        }
    }

    #[test]
    fn every_part_of_a_vui_is_copied_exactly() {
        let hrd = VuiSpec::everything().nal_hrd;
        let variants = [
            ("the iPad's", VuiSpec::ipad()),
            ("everything", VuiSpec::everything()),
            ("aspect idc only", VuiSpec { aspect: Some((1, None)), ..VuiSpec::default() }),
            ("extended SAR", VuiSpec { aspect: Some((255, Some((16, 9)))), ..VuiSpec::default() }),
            ("overscan", VuiSpec { overscan: Some(false), ..VuiSpec::default() }),
            ("colour description", VuiSpec { video_signal: Some((1, false, Some((9, 16, 9)))), ..VuiSpec::default() }),
            ("chroma location", VuiSpec { chroma_loc: Some((2, 3)), ..VuiSpec::default() }),
            ("timing", VuiSpec { timing: Some((1, 120, false)), ..VuiSpec::default() }),
            ("NAL HRD only", VuiSpec { nal_hrd: hrd.clone(), low_delay_hrd: true, ..VuiSpec::default() }),
            ("VCL HRD only", VuiSpec { vcl_hrd: hrd, low_delay_hrd: false, ..VuiSpec::default() }),
            ("pic_struct", VuiSpec { pic_struct: true, ..VuiSpec::default() }),
        ];
        for (name, vui) in variants {
            for base in [AvcSpsSpec::high_1080p(), AvcSpsSpec::baseline_vga()] {
                let mut spec = base;
                spec.vui = Some(vui.clone());
                let input = spec.nal();
                assert_eq!(patched(&input), built_with_restriction(&spec), "{name} (profile {})", spec.profile_idc);
            }
        }
    }

    #[test]
    fn profile_dependent_fields_scaling_lists_and_poc_types_are_copied() {
        let mut spec = AvcSpsSpec::high_1080p();
        spec.vui = Some(VuiSpec::ipad());
        // Scaling lists: one that ends early, a full one, a 64-entry one.
        spec.profile_idc = 110;
        spec.scaling_lists = Some(vec![
            (0, vec![-8]),
            (3, (0..16).map(|i| if i % 2 == 0 { 3 } else { -2 }).collect()),
            (7, (0..64).map(|i| (i % 5) - 2).collect()),
        ]);
        assert_eq!(patched(&spec.nal()), built_with_restriction(&spec));
        // 4:4:4 has twelve lists and a separate-plane flag.
        spec.profile_idc = 244;
        spec.chroma_format_idc = 3;
        spec.scaling_lists = Some(vec![(2, vec![1, -1, 1, -9]), (11, vec![-8])]);
        assert_eq!(patched(&spec.nal()), built_with_restriction(&spec));
        // Picture order count types 0, 1 and 2, cropping, a big picture.
        for poc in 0..3 {
            let mut s = AvcSpsSpec::high_1080p();
            s.poc_type = poc;
            s.vui = Some(VuiSpec::ipad());
            assert_eq!(patched(&s.nal()), built_with_restriction(&s), "poc type {poc}");
        }
        let mut s = AvcSpsSpec::high_1080p();
        s.width_mbs = 150;
        s.height_map_units = 105;
        s.crop = Some([1, 2, 3, 4]);
        assert_eq!(patched(&s.nal()), built_with_restriction(&s));
    }

    #[test]
    fn an_sps_that_has_a_restriction_already_is_left_alone() {
        let own = Restriction {
            motion_vectors_over_pic_boundaries: false,
            max_bytes_per_pic_denom: 2,
            max_bits_per_mb_denom: 1,
            log2_max_mv_length_horizontal: 12,
            log2_max_mv_length_vertical: 11,
            max_num_reorder_frames: 2,
            max_dec_frame_buffering: 3,
        };
        let mut spec = AvcSpsSpec::high_1080p();
        spec.vui = Some(VuiSpec { restriction: Some(own), ..VuiSpec::ipad() });
        let input = spec.nal();
        assert_eq!(add_bitstream_restriction(&input).unwrap(), SpsRewrite::AlreadyRestricted(own));
        assert_eq!(sps_restriction(&input).unwrap(), Some(own));
    }

    #[test]
    fn emulation_prevention_bytes_are_read_and_written_back() {
        // A long run of zero bits in a field makes 00 00 in the bytes, so the NAL unit has 00 00 03.
        let mut escaped = 0;
        for n in 22..=30u32 {
            let mut spec = AvcSpsSpec::high_1080p();
            spec.crop = None;
            spec.poc_lsb = (1u32 << n) - 1;
            spec.vui = Some(VuiSpec::ipad());
            let input = spec.nal();
            let out = patched(&input);
            assert_eq!(out, built_with_restriction(&spec), "poc_lsb {}", spec.poc_lsb);
            escaped += usize::from(input.windows(3).any(|w| w == [0, 0, 3]));
            // The output is a valid NAL unit: no 00 00 00/01/02 inside, and it parses to the same stream.
            assert!(!out.windows(3).any(|w| w[0] == 0 && w[1] == 0 && w[2] <= 2), "{out:02x?}");
            assert_eq!(parse_avc_sps(&out).unwrap(), parse_avc_sps(&input).unwrap());
        }
        assert!(escaped >= 3, "only {escaped} of the inputs had an emulation prevention byte");
        // The NAL header byte is kept as it is (a different NRI is not turned into 0x67).
        let mut input = AvcSpsSpec::baseline_vga().nal();
        input[0] = 0x47;
        assert_eq!(patched(&input)[0], 0x47);
    }

    #[test]
    fn a_truncated_or_odd_sps_is_an_error_and_never_a_panic() {
        let mut spec = AvcSpsSpec::high_1080p();
        spec.vui = Some(VuiSpec::everything());
        let input = spec.nal();
        // Cut anywhere before the last bytes (which hold only the end of the VUI and the trailing bits).
        for n in 0..input.len() - 2 {
            assert!(add_bitstream_restriction(&input[..n]).is_err(), "cut at {n}");
        }
        for n in input.len() - 2..=input.len() {
            let _ = add_bitstream_restriction(&input[..n]);
        }
        // Not an SPS, or with the forbidden bit.
        assert_eq!(add_bitstream_restriction(&[0x68, 0xEE, 0x3C, 0x80, 0x80]), Err(SpsError::NotSps));
        assert_eq!(add_bitstream_restriction(&[0xE7, 100, 0, 51, 0xFF]), Err(SpsError::NotSps));
        // Impossible values: a huge HRD count, a huge POC cycle.
        let mut bad = vec![0x67, 66, 0, 30];
        bad.extend_from_slice(&[0xFF; 20]);
        assert!(add_bitstream_restriction(&bad).is_err());
        for fill in 0..=255u8 {
            let mut v = vec![0x67, 100, 0, 40];
            v.extend(std::iter::repeat_n(fill, 30));
            let _ = add_bitstream_restriction(&v);
        }
    }

    #[test]
    fn slice_types_are_read_from_the_slice_header() {
        use crate::sps::testutil::BitWriter;
        let slice_nal = |nal_header: u8, slice_type: u32| {
            let mut w = BitWriter::default();
            w.ue(0); // first_mb_in_slice
            w.ue(slice_type);
            w.ue(0); // pic_parameter_set_id
            let mut n = vec![nal_header];
            n.extend_from_slice(&crate::sps::testutil::escape_rbsp(&w.finish()));
            n
        };
        for (t, b) in [
            (0, false),
            (1, true),
            (2, false),
            (3, false),
            (4, false),
            (5, false),
            (6, true),
            (7, false),
            (8, false),
            (9, false),
        ] {
            assert_eq!(slice_type(&slice_nal(0x41, t)), Some(t));
            assert_eq!(is_b_slice(&slice_nal(0x41, t)), b, "slice_type {t} in a non-IDR slice");
            assert!(!is_b_slice(&slice_nal(0x65, t)), "an IDR slice is never a B slice (slice_type {t})");
        }
        // Not a slice, or nonsense: no type.
        assert_eq!(slice_type(&[0x67, 0x80]), None);
        assert_eq!(slice_type(&[0x41]), None);
        assert_eq!(slice_type(&[]), None);
        assert_eq!(slice_type(&slice_nal(0x41, 10)), None);
        assert_eq!(slice_type(&[0x41, 0x00, 0x00, 0x00]), None);
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
        packet.extend_from_slice(&avc1_box(640, 480, std::slice::from_ref(&avcc)));
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
            let mut v: Vec<u8> = (0..len).map(|i| u8::try_from(i * 31 % 251).unwrap()).collect();
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
