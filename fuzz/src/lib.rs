//! The parsing modules of scrios, built on their own for fuzzing: the SPS parsers and rewriter (`sps`, `avc`) and the
//! RTP depacketizers (`depack`). The files are the ones in `../src`, included by path, so what is fuzzed is
//! what is shipped; only `frame` and the `tracing` macros are stand-ins.
#![allow(dead_code, unused_imports)]

#[path = "../../src/sps.rs"]
pub mod sps;

#[path = "../../src/avc.rs"]
pub mod avc;

#[path = "../../src/depack.rs"]
pub mod depack;

/// What `depack` needs of `crate::frame` (the real one pulls in serde); keep in step with `../src/frame.rs`.
pub mod frame {
    pub const MAX_PAYLOAD: usize = 16 * 1024 * 1024;
}

pub mod craft;

/// A xorshift64 generator: the runs are reproducible from the seed, and nothing is needed from a registry.
pub struct Rng(pub u64);

impl Rng {
    /// Seeds are scrambled, so that 4 and 5 are not almost the same run, and never leave xorshift's one bad state (0).
    pub fn new(seed: u64) -> Self {
        let mut rng = Rng(seed.wrapping_add(0x9E37_79B9_7F4A_7C15).wrapping_mul(0xBF58_476D_1CE4_E5B9) | 1);
        for _ in 0..8 {
            rng.next();
        }
        rng
    }

    pub fn next(&mut self) -> u64 {
        let x = &mut self.0;
        *x ^= *x << 13;
        *x ^= *x >> 7;
        *x ^= *x << 17;
        *x
    }

    /// A number below `n` (n > 0).
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}
