# Fuzzing

Fuzzers for the code that parses bytes from the device: the SPS parsers and rewrite (`src/sps.rs`, `src/avc.rs`) and
the RTP depacketizers (`src/depack.rs`).

This is a separate crate. The main build and tests do not include it. It compiles the real source files by path, so
it tests the code that ships.

```
cd fuzz
cargo build --release
```

## Running

| Command | What it tests |
| --- | --- |
| `cargo run --release --bin fuzz_sps -- [inputs] [seed]` | Random and mutated SPS data through every SPS parser and the rewrite. |
| `cargo run --release --bin fuzz_depack -- [rounds] [seed]` | Random RTP packets, with loss, reordering and resets, through the depacketizers. |

The fuzzers use a seeded random generator, so the same seed gives the same run. They exit with an error and print the
input on the first failure.

They check that nothing panics, that a rewritten SPS still describes the same picture, and that every picture that
comes out is well formed.

Run a debug build now and then as well. It is slower, but it catches integer overflows.

## Checking the SPS rewrite against ffmpeg

This needs [FFmpeg](https://ffmpeg.org/).

The rewrite must not change how a stream decodes. `rewrite_sps` applies it to a file so you can compare:

```
ffmpeg -f lavfi -i testsrc=size=320x240:rate=30 -frames:v 30 -c:v h264_videotoolbox -f h264 orig.h264
cargo run --release --bin rewrite_sps -- patch orig.h264 patched.h264
ffmpeg -v error -i orig.h264    -f framemd5 - | grep -v '^#' | cut -d, -f6 | md5
ffmpeg -v error -i patched.h264 -f framemd5 - | grep -v '^#' | cut -d, -f6 | md5
```

The two hashes must be equal. Use an encoder that does not already write the restriction, such as the macOS hardware
encoder. libx264 always writes one.
