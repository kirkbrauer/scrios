//! A hand-made H.264 SPS that uses nearly everything the parser has to skip: 4:4:4 with a separate colour plane,
//! all twelve scaling lists (one that ends early by `useDefault`, one that ends by a zero `nextScale`), picture
//! order count type 1 with a cycle, interlaced coding, cropping, and (by `vui_kind`) no VUI, a full VUI with both
//! HRDs, or the same with a bitstream restriction.
use crate::sps::{BitWriter, escape_rbsp};

pub fn craft(vui_kind: u32) -> Vec<u8> {
    let mut w = BitWriter::default();
    w.u(8, 244);
    w.u(8, 0);
    w.u(8, 51);
    w.ue(5); // sps id
    w.ue(3); // chroma 4:4:4 ...
    w.bit(1); // ... with separate_colour_plane
    w.ue(2);
    w.ue(2);
    w.bit(1); // bit depths, qpprime
    w.bit(1); // seq_scaling_matrix_present
    for i in 0..12 {
        if i % 3 == 2 {
            w.bit(0);
            continue;
        }
        w.bit(1);
        let size = if i < 6 { 16 } else { 64 };
        if i == 4 {
            w.se(-8); // useDefault: next = 8 - 8 = 0 -> stops after one
            continue;
        }
        let (mut last, mut next) = (8i64, 8i64);
        for j in 0..size {
            if next != 0 {
                let target: i64 = if j == size - 3 && i == 7 { 0 } else { ((j * 7 + i * 13) % 200 + 4) as i64 };
                let mut d = target - last;
                if d > 127 {
                    d -= 256
                }
                if d < -128 {
                    d += 256
                }
                w.se(d);
                next = (last + d + 256).rem_euclid(256);
            }
            if next != 0 {
                last = next;
            }
        }
    }
    w.ue(12); // log2_max_frame_num_minus4
    w.ue(1); // poc type 1
    w.bit(0);
    w.se(-3);
    w.se(7);
    w.ue(3);
    w.se(2);
    w.se(-2);
    w.se(100000);
    w.ue(4); // max_num_ref_frames 4
    w.bit(1); // gaps
    w.ue(74);
    w.ue(163);
    w.bit(0);
    w.bit(1);
    w.bit(1); // 1200x2624, field, mbaff, direct8x8
    w.bit(1);
    w.ue(0);
    w.ue(3);
    w.ue(0);
    w.ue(1); // cropping
    if vui_kind == 0 {
        w.bit(0);
    } else {
        w.bit(1);
        w.bit(1);
        w.u(8, 255);
        w.u(16, 3);
        w.u(16, 0xFFFF);
        w.bit(1);
        w.bit(1);
        w.bit(1);
        w.u(3, 5);
        w.bit(1);
        w.bit(1);
        w.u(8, 1);
        w.u(8, 0);
        w.u(8, 0);
        w.bit(1);
        w.ue(5);
        w.ue(5);
        w.bit(1);
        w.u(32, 1);
        w.u(32, 0x30000);
        w.bit(1); // timing, with zeros (emulation prevention)
        for _ in 0..2 {
            // NAL and VCL HRD
            w.bit(1);
            w.ue(2);
            w.u(4, 0);
            w.u(4, 0);
            for _ in 0..3 {
                w.ue(0);
                w.ue(0);
                w.bit(1);
            }
            w.u(5, 0);
            w.u(5, 0);
            w.u(5, 0);
            w.u(5, 0);
        }
        w.bit(1); // low_delay
        w.bit(1); // pic_struct
        if vui_kind == 2 {
            w.bit(1); // bitstream_restriction
            w.bit(0);
            w.ue(2);
            w.ue(1);
            w.ue(13);
            w.ue(11);
            w.ue(3);
            w.ue(5);
        } else {
            w.bit(0);
        }
    }
    let mut nal = vec![0x67];
    nal.extend_from_slice(&escape_rbsp(&w.finish()));
    nal
}

/// A hand-made HEVC SPS (Main, level 5.0, 4:2:0 or as `chroma_format_idc` says, with `max_sub_layers_minus1` sub-layers
/// of profile and level, and a conformance window if given): the fields `sps::parse_sps` reads, then the rest of a
/// typical SPS.
pub fn craft_hevc(
    chroma_format_idc: u32,
    max_sub_layers_minus1: u32,
    width: u32,
    height: u32,
    window: Option<[u32; 4]>,
) -> Vec<u8> {
    let mut w = BitWriter::default();
    w.u(4, 0); // sps_video_parameter_set_id
    w.u(3, u64::from(max_sub_layers_minus1));
    w.u(1, 1); // sps_temporal_id_nesting_flag
    w.u(2, 0); // profile_space
    w.u(1, 0); // tier
    w.u(5, 1); // profile_idc
    w.u(32, 0x6000_0000); // compatibility flags
    for b in [0xB0u8, 0, 0, 0, 0, 0] {
        w.u(8, u64::from(b)); // constraint flags
    }
    w.u(8, 150); // level_idc
    for _ in 0..max_sub_layers_minus1 {
        w.bit(1);
        w.bit(1); // sub_layer_profile_present_flag, sub_layer_level_present_flag
    }
    if max_sub_layers_minus1 > 0 {
        for _ in max_sub_layers_minus1..8 {
            w.u(2, 0);
        }
    }
    for _ in 0..max_sub_layers_minus1 {
        w.u(32, 0x1122_3344);
        w.u(32, 0x5566_7788);
        w.u(24, 0x99AABB);
        w.u(8, 90);
    }
    w.ue(0); // sps_seq_parameter_set_id
    w.ue(chroma_format_idc);
    if chroma_format_idc == 3 {
        w.bit(0); // separate_colour_plane_flag
    }
    w.ue(width);
    w.ue(height);
    match window {
        Some(offsets) => {
            w.bit(1);
            for o in offsets {
                w.ue(o);
            }
        }
        None => w.bit(0),
    }
    // The rest (bit depths, POC, the DPB, block sizes ...) is never read.
    for v in [0, 0, 4] {
        w.ue(v);
    }
    w.bit(1);
    for v in [1, 0, 0, 0, 3, 0, 3, 1, 1] {
        w.ue(v);
    }
    for bit in [0, 0, 1, 0] {
        w.bit(bit);
    }
    w.ue(0);
    for bit in [0, 0, 1, 0, 0] {
        w.bit(bit);
    }
    let mut nal = vec![0x42, 0x01];
    nal.extend_from_slice(&escape_rbsp(&w.finish()));
    nal
}
