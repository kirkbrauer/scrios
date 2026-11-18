//! Fuzzes the SPS parsers and the SPS rewrite with random bytes and with mutations of a hand-made SPS.
//!
//! usage: fuzz_sps [iterations] [seed]
//!
//! Nothing may panic, and what the code promises must hold: a rewritten H.264 SPS parses to the same picture as the
//! one it came from and is then "already restricted"; a parsed H.264 SPS has a coded and a cropped size within
//! 32768; a parsed HEVC SPS has a conformance window that leaves a picture.
use scrios_fuzz::{Rng, avc, craft, sps};

fn main() {
    let mut args = std::env::args().skip(1);
    let iterations: u64 = args.next().map_or(3_000_000, |a| a.parse().expect("iterations"));
    let seed: u64 = args.next().map_or(0x9E37_79B9_7F4A_7C15, |a| a.parse().expect("seed"));
    let mut rng = Rng::new(seed);
    // Three H.264 SPSs (no VUI, a VUI, a VUI with a restriction) and HEVC ones with and without a window.
    let mut seeds: Vec<Vec<u8>> = (0..3).map(craft::craft).collect();
    seeds.push(craft::craft_hevc(1, 0, 1216, 2656, None));
    seeds.push(craft::craft_hevc(1, 2, 1920, 1088, Some([0, 0, 0, 4])));
    seeds.push(craft::craft_hevc(2, 1, 64, 64, Some([1, 1, 1, 1])));
    let n_seeds = seeds.len() as u64;
    let (mut hevc_ok, mut avc_ok, mut patched) = (0u64, 0u64, 0u64);

    for it in 0..iterations {
        let (mut v, from_hevc_seed) = if it % 2 == 0 {
            let i = (it / 2 % n_seeds) as usize;
            (seeds[i].clone(), i >= 3)
        } else {
            ((0..rng.below(64)).map(|_| rng.next() as u8).collect(), false)
        };
        // A few random bytes changed, then maybe cut short, then (a third of the time) made to look like an H.264 SPS.
        for _ in 0..rng.below(6) {
            if v.is_empty() {
                break;
            }
            let i = rng.next() as usize % v.len();
            v[i] = rng.next() as u8;
        }
        if rng.below(4) == 0 {
            let len = rng.next() as usize % (v.len() + 1);
            v.truncate(len);
        }
        if !v.is_empty() && it % 3 == 0 && !from_hevc_seed {
            v[0] = 0x67; // an H.264 SPS header on the bytes
        }

        let rewrite = avc::add_bitstream_restriction(&v);
        let parsed = avc::parse_avc_sps(&v);
        let _ = avc::find_avc_config(&v);
        let _ = avc::scan_payload(&v);
        let _ = avc::slice_type(&v);
        if let Ok(s) = &parsed {
            avc_ok += 1;
            assert!(s.width <= 32768 && s.height <= 32768, "{}x{}: {v:02x?}", s.width, s.height);
            assert!(
                s.coded_width <= 32768 && s.coded_height <= 32768,
                "{}x{}: {v:02x?}",
                s.coded_width,
                s.coded_height
            );
        }
        if let Ok(s) = sps::parse_sps(&v) {
            hevc_ok += 1;
            assert!(s.cropped_size().is_ok(), "a parsed SPS must leave a picture: {v:02x?}");
            let _ = s.codec_string("hev1");
        }
        if let Ok(avc::SpsRewrite::Patched(p)) = rewrite {
            patched += 1;
            assert_eq!(avc::parse_avc_sps(&p).ok(), parsed.ok(), "the picture changed: {v:02x?}");
            assert!(
                matches!(avc::add_bitstream_restriction(&p), Ok(avc::SpsRewrite::AlreadyRestricted(_))),
                "not restricted after the rewrite: {v:02x?}"
            );
        }
    }
    eprintln!(
        "fuzz_sps: {iterations} inputs (seed {seed}): {avc_ok} H.264 SPSs and {hevc_ok} HEVC SPSs read, {patched} rewritten"
    );
}
