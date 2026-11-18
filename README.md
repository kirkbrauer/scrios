# scrios

**scrios** ("screen iOS") shows and controls the screen of an iPhone or iPad from a Linux or macOS host: it streams
the screen as **H.264** (or HEVC) and sends **touch, hardware keys and keyboard text** back to the device. Think of
it as scrcpy, but for iOS devices: a small command-line program, and a Rust library to embed.

- The video is plain Annex-B H.264/HEVC: write it to a file or pipe it into `ffplay` (see [docs/cli.md](docs/cli.md) for the flags that keep it live), or use the documented
  framed mode (`--format framed`), with timestamps, key-unit markers and control events, for programs that consume
  it.
- Control is a line protocol on standard input: touch down/move/up, keys (home, app switcher, lock, volume, …),
  keyboard keys and text, a keyframe request, and stop.
- No viewer window yet: see the [roadmap](#roadmap).

## How it works, and what the device needs

scrios uses Apple's own developer services: the **CoreDevice display service and HID service** that Xcode uses,
reached over the standard usbmuxd/RemoteXPC tunnel. These are official Apple developer services, but they are not
publicly documented. **No jailbreak is needed**; the device only needs:

- **Developer Mode** enabled (Settings > Privacy & Security > Developer Mode);
- to be **paired/trusted** with the host (usbmuxd must already hold the pair record; scrios never pairs);
- the **developer disk image** mounted. Xcode does that on a Mac; on Linux, a tool such as
  [go-ios](https://github.com/danielpaulus/go-ios) or [pymobiledevice3](https://github.com/doronz88/pymobiledevice3)
  can mount the personalized DDI.

scrios never changes a setting of the device, writes a pair record or mounts an image itself.

The CoreDevice, RemoteXPC, usbmuxd and lockdown protocols come from the [idevice](https://github.com/jkcoxson/idevice)
crate by Jackson Coxson (MIT), pinned to one release. scrios adds the display stream on top: the offer, the RTP
depacketizer (RFC 6184 for H.264, RFC 7798 for HEVC), parameter-set handling, an H.264 SPS rewrite so that
browsers' hardware decoders do not buffer frames, RTCP, keyframe requests, display size/orientation tracking, and
the touch mapping onto the device's digitizer. Details: [docs/internals.md](docs/internals.md).

```
device --RTP H.264/HEVC over the CoreDevice tunnel--> scrios --Annex-B or framed--> stdout / file / your program
       <-------------- HID touch / keys / text -------------- stdin: one command per line
```

### Compatibility

Built against **idevice 0.1.68** (pinned exactly; see [BUILD.md](BUILD.md#idevice)).

### Tested on

The display stream and touch were tested, on an earlier build of scrios, with the devices below. This build (idevice
0.1.68) has streamed and recorded an iPhone's screen on macOS; touch on it is not yet checked.

- iPhone 16 Pro, iOS 27.2
- iPad (iPad14,3), iOS 27.0.1

The display service needs iOS 17 or later (the CoreDevice tunnel); **iOS versions older than 27 are untested.**
Host: Linux (x86_64, aarch64) and macOS (Apple silicon); Unix only.

## Install

Release binaries (from the GitHub releases of this repository): `scrios-x86_64-linux-musl` and
`scrios-aarch64-linux-musl` (static, any Linux distribution) and `scrios-aarch64-apple-darwin`, with `SHA256SUMS`.

From source, with Rust 1.89 or newer and a C compiler (for `ring`, the TLS backend):

```
cargo build --release --locked      # -> target/release/scrios
```

Cross-builds and static musl binaries: [BUILD.md](BUILD.md).

## Usage

```
scrios list                                          # the devices usbmuxd knows
scrios --output - | ffplay -f h264 -framerate 60 -probesize 32 -flags low_delay -framedrop -y 1000 -   # watch it
scrios --record screen.h264                          # record raw H.264
scrios --format framed                               # the machine protocol, for programs
```

Every option, with examples: [docs/cli.md](docs/cli.md). The framed output and the stdin control protocol are
specified, with their version, in [docs/protocol.md](docs/protocol.md).

Logs go to standard error (`RUST_LOG` sets the level). Exit status: `0` after a clean stop, `2` if the stream could
not be started, `1` if it failed while running, `101` after an internal error.

## Library

The crate is a library (`scrios`) and the `scrios` binary on top of it. The modules are public so that other
programs can embed the stream; the API is not stable yet (0.x). See the crate documentation (`cargo doc --open`).

## Users

- [Jumpstarter](https://github.com/jumpstarter-dev/jumpstarter) runs `scrios --format framed` for the screen of the
  iOS devices it leases out, with touch and keys.

## Roadmap

- **Phase 2: a viewer window**, like scrcpy's main mode: decode and show the stream, mouse and touch input mapped to
  the device, hardware keys on shortcuts. The library already separates the stream (access units) from the input
  (commands), which is what a viewer needs.
- Multi-touch (today: one finger), audio (the device's audio stream is started but discarded), more devices and
  iOS versions tested.

## Development

```
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked                  # no device and no network needed at run time
cargo deny check                     # licences, bans, sources, advisories (deny.toml)
scripts/smoke.sh target/release/scrios   # a release binary without a device
```

CI (GitHub Actions) runs all of these; a `v*` tag builds the release binaries.

## License

Apache-2.0: see [LICENSE](LICENSE). Third-party crates, including idevice (MIT), are listed with their licences in
[NOTICE](NOTICE).
