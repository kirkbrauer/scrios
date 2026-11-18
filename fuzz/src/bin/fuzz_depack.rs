//! Fuzzes the RTP depacketizers (H.264 and HEVC) with packets that are random but shaped like the real ones: the
//! first bytes are NAL headers, aggregation packets, fragmentation units and the iPad's configuration packet;
//! sequence numbers repeat, jump and go back; timestamps and marker bits vary; the state is reset now and then.
//!
//! usage: fuzz_depack [rounds] [seed]
//!
//! Nothing may panic, and every access unit that comes out is Annex-B with 4-byte start codes (and, if it says it
//! carries parameter sets, has an SPS).
use scrios_fuzz::Rng;
use scrios_fuzz::depack::{AccessUnit, Assembler, H264Assembler, Output, PacketMeta};

fn main() {
    let mut args = std::env::args().skip(1);
    let rounds: u64 = args.next().map_or(4000, |a| a.parse().expect("rounds"));
    let seed: u64 = args.next().map_or(0x1234_5678_9ABC_DEF1, |a| a.parse().expect("seed"));
    let mut rng = Rng::new(seed);
    let (mut units, mut drops) = (0u64, 0u64);

    for round in 0..rounds {
        let h264_round = round % 2 == 0;
        let mut h264 = H264Assembler::with_max_unit_bytes(4096);
        let mut hevc = Assembler::with_max_unit_bytes(4096);
        let mut seq = rng.next() as u16;
        let mut ts = rng.next() as u32;
        for _ in 0..500 {
            match rng.below(10) {
                0 => seq = seq.wrapping_add(rng.below(5) as u16),
                1 => seq = seq.wrapping_sub(rng.below(400) as u16),
                2 => seq = rng.next() as u16,
                _ => seq = seq.wrapping_add(1),
            }
            if rng.below(4) == 0 {
                ts = ts.wrapping_add(3000);
            }
            let len = rng.below(300) as usize;
            let mut p: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
            if !p.is_empty() {
                if h264_round {
                    shape_h264(&mut rng, &mut p);
                } else if p.len() > 1 {
                    shape_hevc(&mut rng, &mut p);
                }
            }
            let meta = PacketMeta { seq, timestamp: ts, marker: rng.below(3) == 0 };
            let mut out = Vec::new();
            if h264_round {
                h264.push(meta, &p, &mut out)
            } else {
                hevc.push(meta, &p, &mut out)
            }
            for o in out {
                match o {
                    Output::Unit(u) => {
                        units += 1;
                        check(&u)
                    }
                    Output::Dropped { .. } => drops += 1,
                }
            }
            if rng.below(200) == 0 {
                h264.reset();
                hevc.reset();
            }
        }
    }
    eprintln!("fuzz_depack: {rounds} rounds (seed {seed}): {units} units, {drops} drops");
}

/// Makes the first bytes of `p` an H.264 NAL header of a kind that matters: a STAP-A with plausible sizes inside, an
/// FU-A with a type, an SPS, an IDR, a PPS, or the configuration packet (kind 18) that carries an SPS.
fn shape_h264(rng: &mut Rng, p: &mut Vec<u8>) {
    let k = rng.below(8);
    p[0] = match k {
        0 => 0x7C, // FU-A
        1 => 0x78, // STAP-A
        2 => 0x67, // SPS
        3 => 0x65, // IDR
        4 => 0x92, // the configuration packet (its forbidden bit is set)
        5 => 0x68, // PPS
        _ => p[0] & 0x7f,
    };
    if p[0] == 0x7C && p.len() > 1 {
        p[1] = (p[1] & 0xC0) | [5u8, 7, 8, 1][rng.below(4) as usize];
        if rng.below(3) == 0 {
            p[1] &= 0x7f;
        }
    }
    if p[0] == 0x78 {
        let mut off = 1;
        while off + 2 <= p.len() {
            let size = rng.below(40) as u16;
            p[off] = (size >> 8) as u8;
            p[off + 1] = size as u8;
            if off + 2 < p.len() {
                p[off + 2] = [0x67u8, 0x68, 0x65, 0x41][rng.below(4) as usize];
            }
            off += 2 + size as usize;
        }
    }
    if p[0] == 0x92 && rng.below(2) == 0 {
        let mut sps = vec![0x67, 66, 0xC0, 30];
        sps.extend((0..rng.below(12)).map(|_| rng.next() as u8 | 0x10));
        p.extend_from_slice(&[0, 0, 0, 1]);
        p.extend(sps);
        p.extend_from_slice(&[0, 0, 1, 0x68, 0xEE, 0x3C, 0x80]);
    }
}

/// The same for HEVC: aggregation packets (48), fragmentation units (49), the parameter sets and an IDR slice.
fn shape_hevc(rng: &mut Rng, p: &mut [u8]) {
    let k = rng.below(8);
    p[0] = match k {
        0 => 49 << 1,
        1 => 48 << 1,
        2 => 32 << 1,
        3 => 33 << 1,
        4 => 34 << 1,
        5 => 19 << 1,
        _ => p[0] & 0x7e,
    };
    p[1] = if rng.below(8) == 0 { p[1] } else { 1 };
    if p[0] == 49 << 1 && p.len() > 2 {
        p[2] = (p[2] & 0xC0) | [19u8, 1, 32, 33, 34, 45][rng.below(6) as usize];
        if rng.below(3) == 0 {
            p[2] &= 0x7f;
        }
    }
    if p[0] == 48 << 1 {
        let mut off = 2;
        while off + 2 <= p.len() {
            let size = rng.below(40) as u16;
            p[off] = (size >> 8) as u8;
            p[off + 1] = size as u8;
            if off + 3 < p.len() {
                p[off + 2] = [64u8, 66, 68, 38, 2][rng.below(5) as usize];
                p[off + 3] = 1;
            }
            off += 2 + size as usize;
        }
    }
}

fn check(u: &AccessUnit) {
    assert!(u.payload.starts_with(&[0, 0, 0, 1]), "a unit without a start code");
    if u.params {
        assert!(u.sps.as_ref().is_some_and(|s| !s.is_empty()), "a unit with parameter sets and no SPS");
    }
}
