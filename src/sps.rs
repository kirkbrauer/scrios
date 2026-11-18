//! HEVC sequence parameter set: just enough of it for the WebCodecs/RFC 6381 codec string and the
//! picture size.
//!
//! The fields read are the first ones of the SPS (ITU-T H.265 section 7.3.2.2): the
//! `profile_tier_level` structure, the chroma format, the picture size in luma samples and the
//! conformance window. Nothing after the conformance window is needed.

// The parser modules read untrusted bytes: a malformed input is an error, never a panic, so the lints that find the
// panics are denied here (the tests below may use them).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::fmt;

pub const NAL_TYPE_SPS: u8 = 33;

/// What the SPS says about the stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpsInfo {
    /// `general_profile_space` (0 in every stream met so far).
    pub profile_space: u8,
    /// `general_tier_flag`: false is Main tier (`L`), true is High tier (`H`).
    pub tier_high: bool,
    pub profile_idc: u8,
    /// `general_profile_compatibility_flag[0..32]`, flag 0 in the most significant bit.
    pub compat_flags: u32,
    /// The 48 constraint-indicator bits (`general_progressive_source_flag` first).
    pub constraint: [u8; 6],
    pub level_idc: u8,
    pub chroma_format_idc: u8,
    /// `pic_width_in_luma_samples`.
    pub width: u32,
    /// `pic_height_in_luma_samples`.
    pub height: u32,
    /// Conformance window offsets in chroma units: left, right, top, bottom (all 0 if absent).
    pub conf_window: [u32; 4],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpsError {
    /// Not an SPS NAL unit (or one of a non-base layer).
    NotSps,
    /// The bits ran out.
    Truncated,
    /// A value that cannot be right (an `ue(v)` over 32 bits, a zero or absurd picture size).
    Invalid(&'static str),
}

impl fmt::Display for SpsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SpsError::NotSps => write!(f, "not an SPS NAL unit"),
            SpsError::Truncated => write!(f, "SPS is truncated"),
            SpsError::Invalid(what) => write!(f, "SPS is invalid: {what}"),
        }
    }
}

impl std::error::Error for SpsError {}

impl SpsInfo {
    /// The WebCodecs / RFC 6381 codec string (ISO/IEC 14496-15 annex E):
    /// `<prefix>.<space><profile_idc>.<compat flags, bit order reversed, hex>.<L|H><level_idc>.<constraint bytes, hex>`
    /// with the trailing zero constraint bytes left out (the first is always written).
    pub fn codec_string(&self, prefix: &str) -> String {
        let space = match self.profile_space {
            1 => "A",
            2 => "B",
            3 => "C",
            _ => "",
        };
        let compat = self.compat_flags.reverse_bits();
        let tier = if self.tier_high { 'H' } else { 'L' };
        let used = self.constraint.iter().rposition(|b| *b != 0).map_or(1, |i| i.saturating_add(1));
        let constraints = self.constraint.iter().take(used).map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(".");
        format!("{prefix}.{space}{}.{compat:X}.{tier}{}.{constraints}", self.profile_idc, self.level_idc)
    }

    /// The picture size after the conformance window is applied (what a decoder outputs).
    ///
    /// The offsets are `ue(v)` values read from the stream, so they can be anything up to 2^32-1: the
    /// arithmetic is done in 64 bits, and a window that leaves no picture (a crop of the whole coded
    /// size or more) is an error rather than a zero or wrapped size.
    pub fn cropped_size(&self) -> Result<(u32, u32), SpsError> {
        let (sub_w, sub_h) = match self.chroma_format_idc {
            1 => (2u64, 2u64),
            2 => (2, 1),
            _ => (1, 1), // monochrome and 4:4:4
        };
        let [left, right, top, bottom] = self.conf_window.map(u64::from);
        let crop_w = sub_w.saturating_mul(left.saturating_add(right));
        let crop_h = sub_h.saturating_mul(top.saturating_add(bottom));
        // What is left of one side: nothing (a crop of the whole side or more) is an error, and the difference is
        // below a u32 whenever there is one.
        let left_over = |size: u32, crop: u64| {
            u64::from(size).checked_sub(crop).filter(|rest| *rest > 0).and_then(|rest| u32::try_from(rest).ok())
        };
        match (left_over(self.width, crop_w), left_over(self.height, crop_h)) {
            (Some(width), Some(height)) => Ok((width, height)),
            _ => Err(SpsError::Invalid("conformance window")),
        }
    }
}

/// MSB-first bit reader over RBSP bytes.
pub(crate) struct Bits<'a> {
    data: &'a [u8],
    pos: usize, // in bits
}

impl<'a> Bits<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Bits { data, pos: 0 }
    }

    pub(crate) fn bit(&mut self) -> Result<u32, SpsError> {
        let byte = self.data.get(self.pos / 8).ok_or(SpsError::Truncated)?;
        let bit = byte & bit_mask(self.pos) != 0;
        self.pos = self.pos.saturating_add(1);
        Ok(u32::from(bit))
    }

    /// The low 32 bits of the next `n` bits (`n` is at most 32), most significant bit first.
    pub(crate) fn u(&mut self, n: u32) -> Result<u32, SpsError> {
        debug_assert!(n <= 32);
        let mut v: u32 = 0;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Ok(v)
    }

    /// The next `n` bits (`n` is at most 8) as a byte.
    pub(crate) fn byte(&mut self, n: u32) -> Result<u8, SpsError> {
        debug_assert!(n <= 8);
        u8::try_from(self.u(n)?).map_err(|_| SpsError::Invalid("a field wider than a byte"))
    }

    pub(crate) fn skip(&mut self, n: usize) -> Result<(), SpsError> {
        let end = self.pos.checked_add(n).ok_or(SpsError::Truncated)?;
        if end > self.data.len().saturating_mul(8) {
            return Err(SpsError::Truncated);
        }
        self.pos = end;
        Ok(())
    }

    /// Signed Exp-Golomb `se(v)`: 1, -1, 2, -2, ...
    pub(crate) fn se(&mut self) -> Result<i64, SpsError> {
        let k = i64::from(self.ue()?);
        Ok(if k % 2 == 1 { k.saturating_add(1) / 2 } else { (k / 2).saturating_neg() })
    }

    /// Exp-Golomb `ue(v)`: the count of leading zero bits, a one, and that many more bits. The value is
    /// `2^zeros - 1` plus those bits, that is the whole code word (the one and the bits) minus 1.
    pub(crate) fn ue(&mut self) -> Result<u32, SpsError> {
        for zeros in 0..32 {
            if self.bit()? == 1 {
                let mut word = 1u64;
                for _ in 0..zeros {
                    word = (word << 1) | u64::from(self.bit()?);
                }
                // At most 31 zeros: the code word is below 2^32, and so is its value.
                return u32::try_from(word.saturating_sub(1)).map_err(|_| SpsError::Invalid("ue(v) over 32 bits"));
            }
        }
        Err(SpsError::Invalid("ue(v) longer than 32 bits"))
    }
}

/// The mask of the bit at bit position `pos` (counted from the start) within its byte: the first bit of a byte is the
/// most significant one.
fn bit_mask(pos: usize) -> u8 {
    // `pos % 8` is below 8, so the conversion cannot fail, and rotating the top bit by it cannot wrap past the byte.
    0x80u8.rotate_right(u32::try_from(pos % 8).unwrap_or(0))
}

/// Removes the emulation prevention bytes (`00 00 03` becomes `00 00`) from a NAL unit's payload.
pub fn unescape_rbsp(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0u32;
    for &b in data {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros.saturating_add(1) } else { 0 };
        out.push(b);
    }
    out
}

/// MSB-first bit writer for RBSP bytes.
#[derive(Default)]
pub(crate) struct BitWriter {
    pub bytes: Vec<u8>,
    nbits: usize,
}

impl BitWriter {
    pub fn bit(&mut self, v: u32) {
        if self.nbits.is_multiple_of(8) {
            self.bytes.push(0);
        }
        // A byte was pushed above if this bit starts one, so there is always a last byte to set the bit in.
        if let Some(last) = self.bytes.last_mut().filter(|_| v & 1 == 1) {
            *last |= bit_mask(self.nbits);
        }
        self.nbits = self.nbits.saturating_add(1);
    }

    /// The `n` low bits of `v`, most significant first (bits above bit 63 are zeros).
    pub fn u(&mut self, n: u32, v: u64) {
        for i in (0..n).rev() {
            self.bit(u32::from(v.checked_shr(i).unwrap_or(0) & 1 == 1));
        }
    }

    pub fn ue(&mut self, v: u32) {
        let x = u64::from(v).saturating_add(1);
        let len = 64u32.saturating_sub(x.leading_zeros()); // bits in x
        self.u(len.saturating_sub(1), 0);
        self.u(len, x);
    }

    /// Signed Exp-Golomb: the inverse of `Bits::se`, which never reads more than 2^32 - 1 as the code number.
    pub fn se(&mut self, v: i64) {
        let twice = v.unsigned_abs().saturating_mul(2);
        let code = if v > 0 { twice.saturating_sub(1) } else { twice };
        self.ue(u32::try_from(code).unwrap_or(u32::MAX));
    }

    /// rbsp_trailing_bits: a one, then zeros to the byte boundary.
    pub fn finish(mut self) -> Vec<u8> {
        self.bit(1);
        while !self.nbits.is_multiple_of(8) {
            self.bit(0);
        }
        self.bytes
    }
}

/// Inserts emulation prevention bytes: a `03` after any `00 00` followed by a byte <= 3.
pub fn escape_rbsp(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len().saturating_add(8));
    let mut zeros = 0u32;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros.saturating_add(1) } else { 0 };
    }
    out
}

/// Parses an SPS NAL unit (including its two-byte NAL unit header, without a start code).
pub fn parse_sps(nal: &[u8]) -> Result<SpsInfo, SpsError> {
    let [b0, b1, payload @ ..] = nal else {
        return Err(SpsError::NotSps);
    };
    if payload.is_empty() {
        return Err(SpsError::NotSps); // at least three bytes: the header and one of the payload
    }
    let forbidden = b0 >> 7;
    let nal_type = (b0 >> 1) & 0x3f;
    let layer_id = ((b0 & 1) << 5) | (b1 >> 3);
    if forbidden != 0 || nal_type != NAL_TYPE_SPS || layer_id != 0 {
        return Err(SpsError::NotSps);
    }
    let rbsp = unescape_rbsp(payload);
    let mut b = Bits::new(&rbsp);

    b.skip(4)?; // sps_video_parameter_set_id
    let max_sub_layers_minus1 = b.byte(3)?;
    b.skip(1)?; // sps_temporal_id_nesting_flag

    // profile_tier_level(1, sps_max_sub_layers_minus1)
    let profile_space = b.byte(2)?;
    let tier_high = b.bit()? == 1;
    let profile_idc = b.byte(5)?;
    let compat_flags = b.u(32)?;
    let mut constraint = [0u8; 6];
    for byte in &mut constraint {
        *byte = b.byte(8)?;
    }
    let level_idc = b.byte(8)?;
    // (sub_layer_profile_present_flag, sub_layer_level_present_flag) of each sub-layer below the last
    let mut sub_layers = [(false, false); 8];
    let below = usize::from(max_sub_layers_minus1);
    for slot in sub_layers.iter_mut().take(below) {
        *slot = (b.bit()? == 1, b.bit()? == 1);
    }
    if max_sub_layers_minus1 > 0 {
        for _ in max_sub_layers_minus1..8 {
            b.skip(2)?; // reserved_zero_2bits
        }
    }
    for &(profile_present, level_present) in sub_layers.iter().take(below) {
        if profile_present {
            b.skip(88)?;
        }
        if level_present {
            b.skip(8)?;
        }
    }

    b.ue()?; // sps_seq_parameter_set_id
    let mut chroma_format_idc =
        u8::try_from(b.ue()?).ok().filter(|c| *c <= 3).ok_or(SpsError::Invalid("chroma_format_idc"))?;
    if chroma_format_idc == 3 && b.bit()? == 1 {
        chroma_format_idc = 0; // separate_colour_plane_flag: coded as three monochrome planes
    }
    let width = b.ue()?;
    let height = b.ue()?;
    if !(1..=32768).contains(&width) || !(1..=32768).contains(&height) {
        return Err(SpsError::Invalid("picture size"));
    }
    let mut conf_window = [0u32; 4];
    if b.bit()? == 1 {
        for o in &mut conf_window {
            *o = b.ue()?;
        }
    }

    let info = SpsInfo {
        profile_space,
        tier_high,
        profile_idc,
        compat_flags,
        constraint,
        level_idc,
        chroma_format_idc,
        width,
        height,
        conf_window,
    };
    // A conformance window that leaves no picture is not an SPS to trust (and its size must never wrap).
    info.cropped_size()?;
    Ok(info)
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
    //! Builds SPS NAL units for tests: a bit writer plus the fields `parse_sps` reads, followed by
    //! the fields of a plain low-delay Main stream so the result looks like a real SPS.

    pub(crate) use super::{BitWriter, escape_rbsp};

    #[derive(Debug)]
    pub struct SpsSpec {
        pub profile_idc: u8,
        pub compat_flags: u32,
        pub constraint: [u8; 6],
        pub level_idc: u8,
        pub tier_high: bool,
        pub width: u32,
        pub height: u32,
        pub conf_window: Option<[u32; 4]>,
        pub max_sub_layers_minus1: u32,
        pub chroma_format_idc: u32,
    }

    impl SpsSpec {
        /// Main profile, level 5.0, 4:2:0, the shape of the display stream's SPS.
        pub fn main_l150(width: u32, height: u32) -> SpsSpec {
            SpsSpec {
                profile_idc: 1,
                // Main: flags 1 (Main) and 2 (Main 10) set, flag 0 in the MSB.
                compat_flags: 0x6000_0000,
                // progressive_source, non_packed_constraint, frame_only_constraint.
                constraint: [0xB0, 0, 0, 0, 0, 0],
                level_idc: 150,
                tier_high: false,
                width,
                height,
                conf_window: None,
                max_sub_layers_minus1: 0,
                chroma_format_idc: 1,
            }
        }

        /// The NAL unit: header `42 01`, the escaped RBSP.
        pub fn nal(&self) -> Vec<u8> {
            let mut w = BitWriter::default();
            w.u(4, 0); // sps_video_parameter_set_id
            w.u(3, self.max_sub_layers_minus1 as u64);
            w.u(1, 1); // sps_temporal_id_nesting_flag
            // profile_tier_level
            w.u(2, 0);
            w.u(1, self.tier_high as u64);
            w.u(5, self.profile_idc as u64);
            w.u(32, self.compat_flags as u64);
            for b in self.constraint {
                w.u(8, b as u64);
            }
            w.u(8, self.level_idc as u64);
            for _ in 0..self.max_sub_layers_minus1 {
                w.bit(1); // sub_layer_profile_present_flag
                w.bit(1); // sub_layer_level_present_flag
            }
            if self.max_sub_layers_minus1 > 0 {
                for _ in self.max_sub_layers_minus1..8 {
                    w.u(2, 0);
                }
            }
            for _ in 0..self.max_sub_layers_minus1 {
                w.u(32, 0x1122_3344); // 88 bits of sub-layer profile
                w.u(32, 0x5566_7788);
                w.u(24, 0x99AABB);
                w.u(8, 90); // sub-layer level
            }
            w.ue(0); // sps_seq_parameter_set_id
            w.ue(self.chroma_format_idc);
            if self.chroma_format_idc == 3 {
                w.bit(0); // separate_colour_plane_flag
            }
            w.ue(self.width);
            w.ue(self.height);
            match self.conf_window {
                Some(o) => {
                    w.bit(1);
                    for v in o {
                        w.ue(v);
                    }
                }
                None => w.bit(0),
            }
            // What follows in a real SPS (bit depths, POC, the DPB, CTB sizes ...): values of a
            // typical stream, never read by parse_sps.
            w.ue(0); // bit_depth_luma_minus8
            w.ue(0); // bit_depth_chroma_minus8
            w.ue(4); // log2_max_pic_order_cnt_lsb_minus4
            w.bit(1); // sps_sub_layer_ordering_info_present_flag
            w.ue(1); // sps_max_dec_pic_buffering_minus1
            w.ue(0); // sps_max_num_reorder_pics
            w.ue(0); // sps_max_latency_increase_plus1
            w.ue(0); // log2_min_luma_coding_block_size_minus3
            w.ue(3); // log2_diff_max_min_luma_coding_block_size
            w.ue(0); // log2_min_luma_transform_block_size_minus2
            w.ue(3); // log2_diff_max_min_luma_transform_block_size
            w.ue(1); // max_transform_hierarchy_depth_inter
            w.ue(1); // max_transform_hierarchy_depth_intra
            w.bit(0); // scaling_list_enabled_flag
            w.bit(0); // amp_enabled_flag
            w.bit(1); // sample_adaptive_offset_enabled_flag
            w.bit(0); // pcm_enabled_flag
            w.ue(0); // num_short_term_ref_pic_sets
            w.bit(0); // long_term_ref_pics_present_flag
            w.bit(0); // sps_temporal_mvp_enabled_flag
            w.bit(1); // strong_intra_smoothing_enabled_flag
            w.bit(0); // vui_parameters_present_flag
            w.bit(0); // sps_extension_present_flag
            let rbsp = w.finish();
            let mut nal = vec![0x42, 0x01];
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
    fn display_stream_sps_gives_the_contracts_codec_and_size() {
        // Main, level 5.0, 1216x2656: what the iPhone's display stream measured.
        let nal = SpsSpec::main_l150(1216, 2656).nal();
        let sps = parse_sps(&nal).unwrap();
        assert_eq!(sps.profile_idc, 1);
        assert_eq!(sps.level_idc, 150);
        assert_eq!((sps.width, sps.height), (1216, 2656));
        assert_eq!(sps.conf_window, [0; 4]);
        assert_eq!(sps.codec_string("hev1"), "hev1.1.6.L150.B0");
        assert_eq!(sps.cropped_size(), Ok((1216, 2656)));
    }

    #[test]
    fn the_builders_profile_tier_level_matches_the_bytes_real_encoders_write() {
        // The front of a Main L5.0 SPS as encoders emit it, emulation prevention included:
        //   42 01            NAL header (type 33)
        //   01               vps id 0, max_sub_layers_minus1 0, temporal_id_nesting 1
        //   01               profile_space 0, tier 0, profile_idc 1
        //   60 00 00 03 00   compatibility flags 0x60000000 (a 03 follows 00 00)
        //   B0 00 00 03 00 00 03 00   six constraint bytes B0 00 00 00 00 00
        //   96               level_idc 150
        let nal = SpsSpec::main_l150(1216, 2656).nal();
        let golden = [
            0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0xB0, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x96,
        ];
        assert_eq!(&nal[..golden.len()], &golden);
    }

    #[test]
    fn a_conformance_window_crops_the_output_size() {
        // The classic 1080p stream: coded 1920x1088, bottom offset 4 (chroma units of 2 rows).
        let mut spec = SpsSpec::main_l150(1920, 1088);
        spec.level_idc = 120;
        spec.conf_window = Some([0, 0, 0, 4]);
        let sps = parse_sps(&spec.nal()).unwrap();
        assert_eq!((sps.width, sps.height), (1920, 1088));
        assert_eq!(sps.conf_window, [0, 0, 0, 4]);
        assert_eq!(sps.cropped_size(), Ok((1920, 1080)));
        assert_eq!(sps.codec_string("hvc1"), "hvc1.1.6.L120.B0");

        // Padding on the right and bottom, as the phone's encoder does, if it declared it.
        let mut spec = SpsSpec::main_l150(1216, 2656);
        spec.conf_window = Some([0, 5, 0, 17]);
        let sps = parse_sps(&spec.nal()).unwrap();
        assert_eq!(sps.cropped_size(), Ok((1216 - 10, 2656 - 34)));
    }

    #[test]
    fn chroma_format_sets_the_crop_units() {
        let mut spec = SpsSpec::main_l150(64, 64);
        spec.profile_idc = 4;
        spec.chroma_format_idc = 2; // 4:2:2: SubWidthC 2, SubHeightC 1
        spec.conf_window = Some([1, 1, 1, 1]);
        let sps = parse_sps(&spec.nal()).unwrap();
        assert_eq!(sps.cropped_size(), Ok((60, 62)));
        spec.chroma_format_idc = 3; // 4:4:4: 1 and 1
        let sps = parse_sps(&spec.nal()).unwrap();
        assert_eq!(sps.cropped_size(), Ok((62, 62)));
        spec.chroma_format_idc = 0; // monochrome: 1 and 1
        let sps = parse_sps(&spec.nal()).unwrap();
        assert_eq!(sps.cropped_size(), Ok((62, 62)));
    }

    #[test]
    fn a_conformance_window_that_overflows_or_leaves_no_picture_is_an_error_not_a_wrapped_size() {
        // ue(v) offsets of up to 2^32-2 (the longest the reader accepts), times a chroma unit of 2, overflowed
        // a u32 (a panic in a debug build, a wrapped size in a release one); the SPS must be rejected instead.
        const BIG: u32 = u32::MAX - 1;
        let mut spec = SpsSpec::main_l150(1216, 2656);
        for window in [
            [BIG, BIG, 0, 0],
            [0, 0, BIG, BIG],
            [0x8000_0000, 0, 0, 0],
            [0x4000_0000, 0x4000_0000, 0, 0],
            [0, 0, 0x7FFF_FFFF, 1],
        ] {
            spec.conf_window = Some(window);
            assert_eq!(parse_sps(&spec.nal()), Err(SpsError::Invalid("conformance window")), "window {window:?}");
        }
        // A crop of exactly the coded size (or one chroma unit more) leaves nothing.
        let mut spec = SpsSpec::main_l150(64, 64);
        spec.conf_window = Some([16, 16, 0, 0]); // 2 * 32 = 64 columns
        assert_eq!(parse_sps(&spec.nal()), Err(SpsError::Invalid("conformance window")));
        spec.conf_window = Some([0, 0, 16, 16]);
        assert_eq!(parse_sps(&spec.nal()), Err(SpsError::Invalid("conformance window")));
        spec.conf_window = Some([16, 17, 0, 0]);
        assert_eq!(parse_sps(&spec.nal()), Err(SpsError::Invalid("conformance window")));
        // One unit less is a picture of 2 columns.
        spec.conf_window = Some([16, 15, 0, 0]);
        assert_eq!(parse_sps(&spec.nal()).unwrap().cropped_size(), Ok((2, 64)));

        // A hand-built SpsInfo (its fields are public) gets the same answer, without a panic.
        let mut info = parse_sps(&SpsSpec::main_l150(64, 64).nal()).unwrap();
        info.conf_window = [u32::MAX, u32::MAX, u32::MAX, u32::MAX];
        assert_eq!(info.cropped_size(), Err(SpsError::Invalid("conformance window")));
        // And through the codec layer, which reports it as a string error.
        assert!(
            crate::codec::CodecKind::Hevc
                .describe_sps(&{
                    let mut spec = SpsSpec::main_l150(1216, 2656);
                    spec.conf_window = Some([u32::MAX, 0, 0, 0]);
                    spec.nal()
                })
                .is_err()
        );
    }

    #[test]
    fn sub_layers_are_skipped_correctly() {
        let mut spec = SpsSpec::main_l150(1280, 720);
        spec.max_sub_layers_minus1 = 2;
        let sps = parse_sps(&spec.nal()).unwrap();
        assert_eq!((sps.width, sps.height), (1280, 720));
        assert_eq!(sps.level_idc, 150);
    }

    #[test]
    fn codec_string_variants() {
        let base = parse_sps(&SpsSpec::main_l150(640, 480).nal()).unwrap();

        // High tier, Main 10 (profile 2, compat flags 2 and ... ), level 5.1.
        let mut s = base.clone();
        s.tier_high = true;
        s.profile_idc = 2;
        s.compat_flags = 0x2000_0000; // flag 2 only: reversed = 0x4
        s.level_idc = 153;
        s.constraint = [0x90, 0, 0, 0, 0, 0];
        assert_eq!(s.codec_string("hev1"), "hev1.2.4.H153.90");

        // A non-zero profile space becomes a letter.
        s.profile_space = 1;
        assert_eq!(s.codec_string("hvc1"), "hvc1.A2.4.H153.90");

        // Trailing zero constraint bytes are dropped, the others kept in order.
        s.constraint = [0x90, 0x01, 0, 0x10, 0, 0];
        assert_eq!(s.codec_string("hev1"), "hev1.A2.4.H153.90.01.00.10");

        // All-zero constraint bytes still write the first one.
        s.constraint = [0; 6];
        assert_eq!(s.codec_string("hev1"), "hev1.A2.4.H153.00");

        // Compatibility flags are reversed, with no leading zeros: flag 0 alone is 0x1, flag 31 alone is 0x80000000.
        s.compat_flags = 0x8000_0000;
        assert!(s.codec_string("hev1").contains(".1."));
        s.compat_flags = 0x0000_0001;
        assert!(s.codec_string("hev1").contains(".80000000."));
    }

    #[test]
    fn bad_input_is_an_error_not_a_panic() {
        assert_eq!(parse_sps(&[]), Err(SpsError::NotSps));
        assert_eq!(parse_sps(&[0x42, 0x01]), Err(SpsError::NotSps));
        // A PPS (type 34) is not an SPS.
        assert_eq!(parse_sps(&[0x44, 0x01, 0xC1, 0x72]), Err(SpsError::NotSps));
        // Forbidden bit set.
        assert_eq!(parse_sps(&[0xC2, 0x01, 0x01, 0x01]), Err(SpsError::NotSps));
        // A non-base layer.
        assert_eq!(parse_sps(&[0x42, 0x09, 0x01, 0x01]), Err(SpsError::NotSps));
        // Truncated at every length: the fields read end 25 bytes in (18 bytes through the level,
        // then 49 bits).
        let nal = SpsSpec::main_l150(1216, 2656).nal();
        for n in 3..25 {
            assert!(parse_sps(&nal[..n]).is_err(), "prefix of {n} bytes");
        }
        assert!(parse_sps(&nal[..25]).is_ok());
        // Garbage: ue(v) of all zeros.
        let mut junk = vec![0x42, 0x01, 0x01, 0x01];
        junk.extend_from_slice(&[0u8; 40]);
        assert!(parse_sps(&junk).is_err());
        // A zero picture size.
        assert_eq!(parse_sps(&SpsSpec::main_l150(0, 100).nal()), Err(SpsError::Invalid("picture size")));
        // Every single byte pattern in the dimension area: never a panic.
        for fill in 0..=255u8 {
            let mut v = vec![0x42, 0x01];
            v.extend(std::iter::repeat_n(fill, 32));
            let _ = parse_sps(&v);
        }
    }

    #[test]
    fn unescape_removes_only_emulation_prevention_bytes() {
        assert_eq!(unescape_rbsp(&[0, 0, 3, 1]), vec![0, 0, 1]);
        assert_eq!(unescape_rbsp(&[0, 0, 3, 0, 0, 3, 0]), vec![0, 0, 0, 0, 0]);
        assert_eq!(unescape_rbsp(&[1, 3, 3]), vec![1, 3, 3]);
        assert_eq!(unescape_rbsp(&[0, 3, 0]), vec![0, 3, 0]);
        let raw = [0u8, 0, 1, 0, 0, 0, 0, 2, 5];
        assert_eq!(unescape_rbsp(&escape_rbsp(&raw)), raw);
    }

    #[test]
    fn exp_golomb_round_trip() {
        for v in [0u32, 1, 2, 3, 6, 7, 254, 255, 1215, 2655, 65535, 1 << 20] {
            let mut w = BitWriter::default();
            w.ue(v);
            let bytes = w.finish();
            assert_eq!(Bits::new(&bytes).ue().unwrap(), v, "ue({v})");
        }
    }

    #[test]
    fn exp_golomb_limits() {
        // The largest value the reader takes (31 zeros), and the signed values around it.
        for v in [u32::MAX - 1, u32::MAX / 2, (u32::MAX / 2) + 1] {
            let mut w = BitWriter::default();
            w.ue(v);
            let bytes = w.finish();
            assert_eq!(Bits::new(&bytes).ue().unwrap(), v, "ue({v})");
        }
        for v in [0i64, 1, -1, 2, -2, 255, -255, i64::from(i32::MAX), -i64::from(i32::MAX)] {
            let mut w = BitWriter::default();
            w.se(v);
            let bytes = w.finish();
            assert_eq!(Bits::new(&bytes).se().unwrap(), v, "se({v})");
        }
        // 32 zeros are too many whatever follows; 31 zeros and a one with the bits cut off are a short input.
        let mut w = BitWriter::default();
        w.u(32, 0);
        w.u(1, 1);
        w.u(32, 0);
        let bytes = w.finish();
        assert_eq!(Bits::new(&bytes).ue(), Err(SpsError::Invalid("ue(v) longer than 32 bits")));
        let mut w = BitWriter::default();
        w.u(31, 0);
        w.u(1, 1);
        w.u(8, 0xFF); // the code word needs 31 more bits after the one, and 8 are left
        assert_eq!(Bits::new(&w.bytes).ue(), Err(SpsError::Truncated));
        assert_eq!(Bits::new(&[]).ue(), Err(SpsError::Truncated));
    }

    #[test]
    fn skipping_past_the_end_is_an_error_whatever_the_count() {
        let data = [0u8; 4];
        let mut b = Bits::new(&data);
        assert_eq!(b.skip(32), Ok(()));
        assert_eq!(b.skip(1), Err(SpsError::Truncated));
        assert_eq!(Bits::new(&data).skip(usize::MAX), Err(SpsError::Truncated));
        let mut b = Bits::new(&data);
        b.skip(1).unwrap();
        assert_eq!(b.skip(usize::MAX), Err(SpsError::Truncated), "pos + n would wrap");
        assert_eq!(b.u(8), Ok(0), "a refused skip does not move the reader");
    }
}
