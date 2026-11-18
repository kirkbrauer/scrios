# scrios

**scrios** ("screen iOS") shows and controls the screen of an iPhone or iPad from a Linux, macOS or Windows host: it streams
the screen as **H.264** (or HEVC) and sends **touch, hardware keys and keyboard text** back to the device. Think of
it as scrcpy, but for iOS devices: a small command-line program with a [viewer window](#viewer), and a Rust library to
embed.

- `scrios` opens a **window** with the device's screen: click and drag to touch, type to type, Cmd+H (Alt+H on Linux
  and Windows) for Home. `scrios stream` is the same stream without a window, for players and programs.
- The video is plain Annex-B H.264/HEVC: write it to a file or pipe it into `ffplay` (see [docs/cli.md](docs/cli.md) for the flags that keep it live), or use the documented
  framed mode (`--format framed`), with timestamps, key-unit markers and control events, for programs that consume
  it.
- Control is a line protocol on standard input: touch down/move/up, keys (home, app switcher, lock, volume, …),
  keyboard keys and text, a keyframe request, and stop.

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

### Platforms

| Host | Built and tested in CI | Release binary | Checked with a device |
| --- | --- | --- | --- |
| **Linux** (x86_64, aarch64) | yes: clippy, tests, doc, smoke test | static musl (`scrios-x86_64-linux-musl`, `scrios-aarch64-linux-musl`) | see [Tested on](#tested-on) (earlier build) |
| **macOS** (Apple silicon) | yes, the same | `scrios-aarch64-apple-darwin` | yes: this build streamed and recorded an iPhone |
| **Windows** (x86_64; arm64 is a best-effort cross-build) | yes (`windows-latest`, MSVC): clippy, tests, doc, a smoke test of the release `scrios.exe` | `scrios-x86_64-pc-windows-msvc.zip` | **no: nothing has run against a device on Windows yet** |

On Windows the code path is **reasoned from idevice's sources, not tried**: idevice already talks to a usbmuxd on TCP
`127.0.0.1:27015` (what the Apple Mobile Device Service listens on) and has no unix socket there, scrios takes its own
copy of the standard output handle for the stream the way it takes the descriptor on unix, and Ctrl-C and Ctrl-Break
are its stop signals. What the Apple Mobile Device Service answers to the requests scrios makes (the device list, the
pair record) has not been seen. If something differs, `scrios doctor` shows where it stops, and `RUST_LOG=debug` says more.

The **viewer window** is tested on macOS. On Linux and Windows it is built (the Windows zip includes it) but not run: it is the same code on winit, wgpu and OpenH264, with software decoding there (Windows' Media Foundation decoder is a stub).

## Prerequisites

usbmuxd is scrios's only way to a device, as adb is scrcpy's. What the host needs, by platform:

| Host | usbmuxd | The developer disk image folder |
| --- | --- | --- |
| **macOS** | built in (`/var/run/usbmuxd`) | **Xcode**, or a copy of its `iOS_DDI` folder: scrios finds `/Library/Developer/DeveloperDiskImages/iOS_DDI` itself |
| **Linux** | the `usbmuxd` daemon (a package of every distribution; it starts on its own when a device is plugged in, or `sudo systemctl start usbmuxd`) | a copy of `iOS_DDI` from a Mac with Xcode |
| **Windows** (see [Platforms](#platforms)) | the Apple Devices app (Microsoft Store) or iTunes: its Apple Mobile Device Service is usbmuxd, on `127.0.0.1:27015`, and must be running | a copy of `iOS_DDI` from a Mac with Xcode, in `%LOCALAPPDATA%\scrios\ddi` or given with `--ddi-dir` |

On every host:

- **Developer Mode** on the device (Settings > Privacy & Security > Developer Mode; scrios reveals the switch and says the
  steps, then waits through the restart);
- the device **paired and trusted** with the host (tap Trust on the device; scrios itself does not pair, so if the host
  never asked, pair once with your system's tool: Finder on macOS, `idevicepair pair` from libimobiledevice on Linux, the
  Apple Devices app or iTunes on Windows);
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

On Windows, in PowerShell (the folder from the Mac, copied by any means: `scp`, a USB stick, a share):

```
scp -r mac:/Library/Developer/DeveloperDiskImages/iOS_DDI $env:LOCALAPPDATA\scrios\ddi
# or leave it anywhere and say where:  .\scrios.exe --ddi-dir C:\path\to\iOS_DDI
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

### On Windows

`scrios.exe` is a console program with no installer: unzip it and run it from a terminal. The address of usbmuxd is
`127.0.0.1:27015` unless `--usbmuxd host:port` or `USBMUXD_SOCKET_ADDRESS` says otherwise (there are no unix sockets:
`UNIX:` addresses are refused). Ctrl-C and Ctrl-Break are a clean stop; closing the console window or `taskkill /f`
ends the process at once, without telling the device.

The video is binary, and **Windows PowerShell 5.1 re-encodes what a program writes when it is piped or redirected with
`>`, which corrupts it**. Use `--output FILE` or `--record FILE`, or do the pipe in `cmd.exe`
(`cmd /c ".\scrios.exe stream --output - | ffplay -f h264 -"`); PowerShell 7.4 and later pass the bytes through. The same
holds for `--format framed` into another program.

## Install

Release binaries (from the GitHub releases of this repository): `scrios-x86_64-linux-musl` and
`scrios-aarch64-linux-musl` (static, any Linux distribution; **headless**: no viewer window), `scrios-aarch64-apple-darwin`
(with the viewer), and for Windows `scrios-x86_64-pc-windows-msvc.zip` (`scrios.exe`, with the viewer, with the README,
LICENSE and NOTICE; an arm64 zip when its cross-build worked), with `SHA256SUMS`. Every build of `main` also uploads the
Windows zip as a workflow artifact (30 days).

From source, with Rust 1.89 or newer, a C compiler (for `ring`, the TLS backend) and, for the viewer, a C++ compiler
(OpenH264 is built from source):

```
cargo build --release --locked                          # with the viewer window -> target/release/scrios
cargo build --release --locked --no-default-features    # headless: no window, no GPU, no decoder; much smaller
```

Cross-builds and static musl binaries: [BUILD.md](BUILD.md).

## Usage

```
scrios list                                          # the devices usbmuxd knows
scrios doctor                                        # check everything, with the fix for what is missing
scrios                                               # the window: see and use the screen
scrios stream --output - | ffplay -f h264 -framerate 60 -probesize 32 -flags low_delay -framedrop -y 1000 -   # watch it in ffplay
scrios stream --record screen.h264                   # record raw H.264, no window
scrios stream --format framed                        # the machine protocol, for programs
```

`scrios` waits for what is missing (`--no-wait` fails at once) and mounts the developer disk image if needed; `scrios
mount-ddi`, `ddi-status`, `unmount-ddi` and `developer-mode` do those steps by hand. Every option, with examples:
[docs/cli.md](docs/cli.md). The framed output and the stdin control protocol are
specified, with their version, in [docs/protocol.md](docs/protocol.md).

Logs go to standard error (`RUST_LOG` sets the level). Exit status: `0` after a clean stop, `2` if the stream could
not be started, `1` if it failed while running, `101` after an internal error. The end of standard input stops a stream
only with `--format framed`; a recording started with standard input closed (nohup, a script) runs until SIGINT or SIGTERM
(Ctrl-C on Windows).

## Viewer

`scrios` with no subcommand opens a window that shows the device's screen and lets you use it with the mouse and the
keyboard, like scrcpy:

```
scrios                       # the only device usbmuxd lists
scrios --udid 00008110-001234567890401E --show-stats
scrios --record screen.h264  # record raw H.264 while the window is open
scrios --no-control          # view only: no touch, no keys
```

Close the window (or press Ctrl-C in the terminal) to stop: the window goes at once and the stream is stopped on the
device, which can take a few seconds. If the stream fails the window closes and the reason is on standard error.

**The window.** It opens shaped like the device's screen and as large as the screen of your computer has room for, and it
can be resized; the picture keeps its shape, with black bars if the window's shape differs. It follows the device's
orientation: when the device turns, the window turns with it. The frame rate and delay can be shown in the title
with `--show-stats` (the delay is the viewer's own: from the moment the stream hands over a picture to the moment it is
drawn; it does not include the device's encoder or the USB link).

**Mouse.** The left button is one finger (touch 0): press, drag and release on the picture are `touch down`, `move`
and `up` at the matching point of the device's screen. A press in the black bars does nothing; a drag that leaves the
picture stays on its edge. One finger only (there is no multi-touch yet).

**Keyboard.** Keys are sent to the device as a hardware keyboard by their position (letters, digits, arrows, function
keys, modifiers), so the device interprets them with its own layout (US). A key is down on the device while it is down
on the host (the host's auto-repeat is not forwarded). The window losing the focus lifts every key and the finger.

**Shortcuts.** Held with the *shortcut modifier*, **Cmd** on macOS and **Alt** on Linux and Windows (as in scrcpy's
`MOD`). The shortcuts and the modifier itself are not sent to the device, and neither is any other key pressed
with the modifier held.

| Shortcut | Does |
| --- | --- |
| `MOD+H` | Home |
| `MOD+S` | App Switcher |
| `MOD+Up`, `MOD+Down` | Volume up, down (repeat while held) |
| `MOD+W` | close the window |
| `MOD+P` | lock the device, **only with `--lock-shortcut`** |

There is no lock or power shortcut by default: with the screen as the only way in, a lock from the keyboard is a way to
lock yourself out.

**Decoders.** The viewer decodes H.264 only (HEVC: use `scrios stream`). `--decoder` chooses:

| `--decoder` | |
| --- | --- |
| `auto` (default) | the platform's decoder, and OpenH264 where there is none or if the platform's fails |
| `platform` | the platform's decoder only: VideoToolbox on macOS; an error elsewhere |
| `software` | OpenH264 (Cisco), built into scrios from its source (BSD-2-Clause) |

macOS uses **VideoToolbox**, the hardware decoder. Linux decodes with OpenH264 (VA-API and others may come later).
Windows decodes with OpenH264 too: a Media Foundation decoder is a stub that says it is not written yet. Each picture
is decoded and shown as soon as it arrives, with no queue and no pacing: a picture that has not been drawn when the next
one is decoded is dropped. Pictures are drawn with wgpu (Metal, Vulkan, Direct3D 12 or OpenGL) from the decoder's own
NV12 planes.

| Option | |
| --- | --- |
| `--window-title TITLE` | the window's title (default `scrios`) |
| `--window-width PIXELS`, `--window-height PIXELS` | the first window is at most this wide / high (logical pixels) |
| `--show-stats` | frame rate and delay in the title |
| `--lock-shortcut` | `MOD+P` locks the device |
| `--decoder auto\|platform\|software` | the H.264 decoder |
| `--record FILE`, `--udid`, `--no-control`, ... | as for `scrios stream` |

`--output`, `--format` and the probe belong to `scrios stream` and are refused here. A build without the viewer (the
static Linux release binaries, `cargo build --no-default-features`) has no window: `scrios` is then `scrios stream`.

**Where it is tested.** On macOS, without a device: the window, the picture's drawing (each rotation, the crop, the
letterbox), VideoToolbox and OpenH264 decoding recorded video, the mouse and key mapping. Touch and keys through the window
have not been tried on a device yet. Linux and Windows are the same code on winit, wgpu and OpenH264, but are not tested
(and scrios drives the device through idevice, which is Unix-only for now).

## Library

The crate is a library (`scrios`) and the `scrios` binary on top of it. The modules are public so that other
programs can embed the stream; the API is not stable yet (0.x). See the crate documentation (`cargo doc --open`).

## Users

- [Jumpstarter](https://github.com/jumpstarter-dev/jumpstarter) runs `scrios stream --format framed` for the screen of
  the iOS devices it leases out, with touch and keys. The framed format makes none of the checks above and never waits (a
  program reads what is missing as an `error` event): run `scrios doctor` or `scrios mount-ddi` before. (Before the
  viewer, `scrios --format framed` meant the same; with the viewer in a build, `scrios` without a subcommand is the
  window, and refuses `--format` and `--output`.)

## Roadmap

- The viewer: HEVC, a Media Foundation decoder on Windows (and a hardware decoder on Linux), pinch and two-finger
  gestures from the mouse and keyboard, clipboard, Linux and Windows tested.
- Multi-touch (today: one finger), audio (the device's audio stream is started but discarded), more devices and
  iOS versions tested.

## Development

```
cargo fmt --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-features --locked   # no device and no network needed at run time
cargo test --no-default-features --locked    # the headless build
cargo deny check                     # licences, bans, sources, advisories (deny.toml)
scripts/smoke.sh target/release/scrios   # a release binary without a device (unix: needs python3)
scripts/smoke-minimal.sh target/release/scrios   # the same, minimal: runs in Git Bash on Windows too (scrios.exe)
```

CI (GitHub Actions) runs all of these on Linux, macOS and Windows (what does not depend on the host, once); a `v*` tag
builds the release binaries.

## License

Apache-2.0: see [LICENSE](LICENSE). Third-party crates, including idevice (MIT), are listed with their licences in
[NOTICE](NOTICE).
