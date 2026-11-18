# Fuzz and differential harnesses

Harnesses for the code that reads bytes from the device: the SPS parsers and the SPS rewrite (`src/sps.rs`,
`src/avc.rs`) and the RTP depacketizers (`src/depack.rs`).

`fuzz/` is a separate crate (`scrios-fuzz`, `publish = false`, its own `[workspace]`), not a member of the main
build: `cargo build`, `cargo test` and `cargo clippy` in the repository root never see it. It includes the real
sources by path (`#[path = "../../src/sps.rs"]` and so on), so what is fuzzed is what ships. It has **no
dependencies from a registry**: `frame::MAX_PAYLOAD` is repeated in `src/lib.rs` (keep it in step with
`src/frame.rs`) and `stubs/tracing` stands in for the `tracing` macros the included files use. It therefore builds
offline and is quick to build.

```
cd fuzz
cargo build --release          # target/ here is ignored by git; or set CARGO_TARGET_DIR
```

## Fuzzers

They are plain random-mutation fuzzers with a seeded generator (the same seed is the same run), not coverage-guided:
they run for a fixed number of inputs and exit non-zero with the failing input on the first violation.

| Command | What it does |
| --- | --- |
| `cargo run --release --bin fuzz_sps -- [inputs] [seed]` | Random bytes and mutations of hand-made H.264 and HEVC SPSs (`src/craft.rs`: 4:4:4, scaling lists, POC type 1, interlaced, full VUI with both HRDs, with and without a restriction; HEVC with sub-layers and conformance windows) go through every SPS and configuration parser and the rewrite. Defaults: 3 000 000 inputs. |
| `cargo run --release --bin fuzz_depack -- [rounds] [seed]` | 500 packets a round, random but shaped like the real ones (single NAL units, STAP-A and aggregation packets, FU-A and FU, the iPad's configuration packet), with repeated, reordered and jumping sequence numbers, varying timestamps and marker bits and resets. Default: 4000 rounds. |
| `cargo run --release --bin rewrite_sps -- patch in.h264 out.h264` | Rewrites every SPS of an Annex-B file (adds the VUI `bitstream_restriction` with no reordering), for the differential check below. |
| `cargo run --release --bin rewrite_sps -- craft 0\|1\|2 out.h264` | Writes the crafted H.264 SPS (no VUI, a VUI, a VUI with a restriction) with a PPS and a stub slice. A parser stress test, not a decodable stream (ffmpeg refuses its separate colour planes). |

What must hold, besides "no panic":

* `fuzz_sps`: a rewritten H.264 SPS parses to the same picture as the original and is then "already restricted"; a parsed
  H.264 SPS has a coded and a cropped size of at most 32768; a parsed HEVC SPS has a conformance window that leaves a picture.
* `fuzz_depack`: every access unit that comes out starts with a 4-byte Annex-B start code, and one that says it carries
  parameter sets has an SPS.

Run them in a debug build too now and then (`cargo run --bin fuzz_sps -- 1000000 1`, slower): integer overflow checks
are on there, and an arithmetic overflow on a hostile value (the HEVC conformance window once was one) is a panic
instead of a silently wrong size.

```
for seed in 1 2 3 4 5 6 7 8; do target/release/fuzz_sps 5000000 $seed; done
for seed in 1 2 3 4 5; do target/release/fuzz_depack 20000 $seed; done
```

## Differential check of the SPS rewrite against ffmpeg

The rewrite must not change what a decoder makes of a stream. With an encoder that writes no `bitstream_restriction`
(the macOS hardware encoder does not; libx264 always writes one, and the rewrite leaves such an SPS alone) and no B-frames:

```
ffmpeg -f lavfi -i testsrc=size=320x240:rate=30 -frames:v 30 -c:v h264_videotoolbox -f h264 orig.h264
target/release/rewrite_sps patch orig.h264 patched.h264        # "SPS 9 -> 14 bytes"
ffmpeg -v error -i orig.h264    -f framemd5 - | grep -v '^#' | cut -d, -f6 | md5     # (md5sum on Linux)
ffmpeg -v error -i patched.h264 -f framemd5 - | grep -v '^#' | cut -d, -f6 | md5     # the same
ffprobe -v error -show_entries stream=profile,level,width,height,has_b_frames -of default=nw=1 patched.h264
```

The two hashes must be equal. (The rewrite is for streams with no B-frames, which is what the display stream is: it
declares `max_num_reorder_frames = 0`, and the depacketizer counts B slices and warns if one turns up.)
