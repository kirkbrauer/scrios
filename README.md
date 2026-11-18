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
- to be **paired/trusted** with the host (the pair record is in usbmuxd, which is the only place scrios reads it);
- the **developer disk image** mounted, which scrios does itself from the `iOS_DDI` folder of Xcode (see
  [Prerequisites](#prerequisites)); no other tool (go-ios, pymobiledevice3) is needed.

scrios works like scrcpy, with no extra steps: `scrios` waits for the device ("Plug in and unlock your iPhone or iPad"),
for Trust ("Unlock the device and tap Trust") and for Developer Mode (it reveals the switch and prints the steps), and
mounts the developer disk image if it is missing. It never turns Developer Mode on by itself and never pairs: the device's
owner does that on the device (`scrios developer-mode --enable --yes` turns Developer Mode on when you ask for it by name).
`scrios doctor` checks everything and says how to fix what is missing.

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

The set-up described under [Prerequisites](#prerequisites) (waiting for the device, Trust and Developer Mode, mounting
the developer disk image, `scrios doctor`) is tested against scripted fakes of usbmuxd and the device, not yet on a
device.

The display service needs iOS 17 or later (the CoreDevice tunnel); **iOS versions older than 27 are untested.**
Host: Linux (x86_64, aarch64) and macOS (Apple silicon); Unix only.

## Prerequisites

usbmuxd is scrios's only way to a device, as adb is scrcpy's. What the host needs, by platform:

| Host | usbmuxd | The developer disk image folder |
| --- | --- | --- |
| **macOS** | built in (`/var/run/usbmuxd`) | **Xcode**, or a copy of its `iOS_DDI` folder: scrios finds `/Library/Developer/DeveloperDiskImages/iOS_DDI` itself |
| **Linux** | the `usbmuxd` daemon (a package of every distribution; it starts on its own when a device is plugged in, or `sudo systemctl start usbmuxd`) | a copy of `iOS_DDI` from a Mac with Xcode |
| **Windows** (not supported yet) | the Apple Devices app or iTunes, whose Apple Mobile Device Service is usbmuxd on `127.0.0.1:27015` | a copy of `iOS_DDI` from a Mac with Xcode |

On every host:

- **Developer Mode** on the device (Settings > Privacy & Security > Developer Mode; scrios reveals the switch and says the
  steps, then waits through the restart);
- the device **paired and trusted** with the host (tap Trust on the device; scrios itself does not pair, so if the host
  never asked, pair once with your system's tool: Finder on macOS, `idevicepair pair` from libimobiledevice on Linux);
- **internet for the first mount after each restart of the device**: mounting the personalized image asks Apple's signing
  server (`gs.apple.com`, plain HTTP, not through a proxy) for a ticket. Without internet scrios says so; once the image
  is mounted nothing else is needed until the device restarts.

**The developer disk image is Apple's, and scrios does not ship, host or download it by default.** scrios looks for the
`iOS_DDI` folder (it holds `Restore/BuildManifest.plist`, an image and its trust cache) in `--ddi-dir`, `$SCRIOS_DDI_DIR`,
Xcode's folders on macOS, and its cache folder (`~/Library/Caches/scrios/ddi` on macOS, `~/.cache/scrios/ddi` on Linux,
`%LOCALAPPDATA%\scrios\ddi` on Windows). On Linux or Windows, copy the folder from a Mac with Xcode into the cache folder
once:

```
mkdir -p ~/.cache/scrios && scp -r mac:/Library/Developer/DeveloperDiskImages/iOS_DDI ~/.cache/scrios/ddi
```

(The paths of Xcode's folders are from what Xcode is known to do; one was seen on a Mac with Xcode, the other was not.
Details, the archive download for fleets, and the manifest layout: [docs/cli.md](docs/cli.md#the-developer-disk-image).)

### A device on another machine

Point `--usbmuxd` at the usbmuxd of the machine the device is plugged into, as scrcpy does with a remote adb:

```
# where scrios runs: forward a local port to the usbmuxd socket of the machine the device is plugged into
ssh -N -L 27015:/var/run/usbmuxd you@that-machine &
scrios --usbmuxd 127.0.0.1:27015

# or, without ssh, on that machine: serve the socket on TCP, and give scrios its address
socat TCP-LISTEN:27015,reuseaddr,fork UNIX-CONNECT:/var/run/usbmuxd
scrios --usbmuxd that-machine:27015
```

**The socat port is unauthenticated** (anyone who reaches it can use every device and read its pair record): use it only
over a link you trust, and prefer the ssh forward. The CoreDevice tunnel and the services all ride usbmuxd connections, so
they should work remotely; that is reasoned, not tried on a device. See [docs/cli.md](docs/cli.md#a-remote-usbmuxd).

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
scrios doctor                                        # check everything, with the fix for what is missing
scrios --output - | ffplay -f h264 -framerate 60 -probesize 32 -flags low_delay -framedrop -y 1000 -   # watch it
scrios --record screen.h264                          # record raw H.264
scrios --format framed                               # the machine protocol, for programs
```

`scrios` waits for what is missing (`--no-wait` fails at once) and mounts the developer disk image if needed; `scrios
mount-ddi`, `ddi-status`, `unmount-ddi` and `developer-mode` do those steps by hand. Every option, with examples:
[docs/cli.md](docs/cli.md). The framed output and the stdin control protocol are
specified, with their version, in [docs/protocol.md](docs/protocol.md).

Logs go to standard error (`RUST_LOG` sets the level). Exit status: `0` after a clean stop, `2` if the stream could
not be started, `1` if it failed while running, `101` after an internal error. The end of standard input stops a stream
only with `--format framed`; a recording started with standard input closed (nohup, a script) runs until SIGINT or SIGTERM.

## Library

The crate is a library (`scrios`) and the `scrios` binary on top of it. The modules are public so that other
programs can embed the stream; the API is not stable yet (0.x). See the crate documentation (`cargo doc --open`).

## Users

- [Jumpstarter](https://github.com/jumpstarter-dev/jumpstarter) runs `scrios --format framed` for the screen of the
  iOS devices it leases out, with touch and keys. The framed format makes none of the checks above and never waits (a
  program reads what is missing as an `error` event): run `scrios doctor` or `scrios mount-ddi` before.

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
