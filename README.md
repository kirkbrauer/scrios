# scrios

scrcpy for iOS. scrios shows an iPhone or iPad's screen on your computer and lets you control it with the mouse and
keyboard. It can also write the screen as an H.264 stream for a browser, a player, or another program.

It works over USB on Linux, macOS and Windows. The device does not need a jailbreak or an app installed.

```
scrios                               # open a window with the device's screen
scrios stream | ffplay -f h264 -     # the same video, headless, into a player
scrios stream --record screen.h264   # record it
```

## Requirements

**On the device**

- iOS 27. Older versions are untested and may not send the stream.
- Developer Mode turned on (Settings > Privacy & Security > Developer Mode).
- The computer trusted: tap **Trust** when the device asks.

**On the computer**

| | What you need |
| --- | --- |
| macOS | Nothing extra. |
| Linux | The `usbmuxd` package. |
| Windows | The Apple Devices app (Microsoft Store) or iTunes. |

The first run needs internet access. scrios installs Apple's developer disk image on the device, and downloads it if
you do not have Xcode. See [where the image comes from](docs/cli.md#the-developer-disk-image).

## Install

Download a binary from the [releases page](https://github.com/kirkbrauer/scrios/releases), or build from source with
Rust 1.89 or newer:

```
cargo build --release --locked
```

The Linux release binaries are headless: they have `scrios stream` but no window. Build from source for the window.
More build options are in [BUILD.md](BUILD.md).

## Usage

Plug in the device and run:

```
scrios
```

scrios waits for anything that is missing and tells you what to do: plug in the device, tap Trust, turn on Developer
Mode. If something does not work, `scrios doctor` checks each step and says how to fix it.

| Command | What it does |
| --- | --- |
| `scrios` | Open the window. |
| `scrios stream` | Write the video to standard output or a file, with no window. |
| `scrios list` | List connected devices. |
| `scrios doctor` | Check the setup and explain what is wrong. |

Common options:

| Option | What it does |
| --- | --- |
| `-s`, `--udid UDID` | Pick a device when more than one is connected. |
| `-r`, `--record FILE` | Record the video to a file (raw H.264). |
| `-n`, `--no-control` | View only: send no touch or keys. |
| `--video-codec h264\|h265` | Choose the codec. The default is H.264. |
| `--usbmuxd ADDR` | Use a device plugged into [another machine](docs/cli.md#a-remote-usbmuxd). |

Every command and option is in [docs/cli.md](docs/cli.md), or run `scrios --help`.

### The window

Click and drag to touch. Type to type. Close the window to stop.

Shortcuts use **Cmd** on macOS and **Alt** on Linux and Windows:

| Shortcut | Action |
| --- | --- |
| `MOD+H` | Home |
| `MOD+S` | App Switcher |
| `MOD+Up`, `MOD+Down` | Volume |
| `MOD+W` | Close the window |

Dragging up from the bottom edge goes Home. Drag up and hold to open the App Switcher.
Details are in [docs/cli.md](docs/cli.md#the-viewer).

### Streaming to another program

`scrios stream` writes plain H.264 (Annex-B) that `ffplay`, `ffmpeg` and `mpv` read directly:

```
scrios stream --record screen.h264
ffmpeg -f h264 -i screen.h264 -c copy screen.mp4
```

For a low-latency live view, use the `ffplay` flags in [docs/cli.md](docs/cli.md#examples).

Programs that need timestamps, screen size and rotation, or want to send touch and keys, use the framed format:

```
scrios stream --format framed | my-program
```

Touch and key commands go in on standard input, one per line (`touch down 600 1300 0`, `key home`, `stop`).
The format and the commands are specified in [docs/protocol.md](docs/protocol.md).
[Jumpstarter](https://github.com/jumpstarter-dev/jumpstarter) uses this to show and control the iOS devices it leases
out.

### As a Rust library

The `scrios` crate has the same stream and controls as the command line. Start with `scrios::stream::Stream`. The API
is not stable yet.

## Status

scrios is new, and not everything has been tried on a real device.

| | Tested on a device |
| --- | --- |
| Streaming and recording on macOS | Yes (iPhone 16 Pro on iOS 27.2, iPad on iOS 27.0.1) |
| Touch and keys through `scrios stream` | Yes, on an earlier build |
| Touch and keys through the window | No |
| Installing the developer disk image | No. The download is tested; the install is tested against a fake device only |
| The window on Linux and Windows | No. It builds but has not been run |
| Anything on Windows | No |
| iOS versions before 27 | No |

Not supported yet: audio, multi-touch, clipboard, and HEVC in the window.

## How it works

scrios talks to the same developer services Xcode uses: the CoreDevice display service for video, and the HID service
for touch and keys. It reaches them through usbmuxd using the [idevice](https://github.com/jkcoxson/idevice) library.
Apple does not document these services, so a new iOS version can break them.

Built against **idevice 0.1.68**.

The device sends video as RTP. scrios reassembles it into H.264 and passes it on. More detail is in
[docs/internals.md](docs/internals.md).

## Development

```
cargo fmt --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-features --locked
```

The tests need no device and no network.

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
