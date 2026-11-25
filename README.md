# scrios

[scrcpy](https://github.com/Genymobile/scrcpy) for iOS: see and control an iPhone or iPad from your computer, over USB.

- Works on Linux, macOS (Apple silicon) and Windows
- No jailbreak, and nothing to install on the device
- Mouse and keyboard control
- Recording
- An H.264 stream for browsers, players and other programs

## Before you start

On the device (iOS 27):

1. Turn on [Developer Mode](https://developer.apple.com/documentation/xcode/enabling-developer-mode-on-a-device) in Settings > Privacy & Security.
2. Plug it in and tap **Trust**.

On the computer:

- **macOS**: nothing to install.
- **Linux**: install [usbmuxd](https://github.com/libimobiledevice/usbmuxd). Most distributions have a `usbmuxd` package.
- **Windows**: install the [Apple Devices](https://apps.microsoft.com/detail/9np83lwlpz9k) app or [iTunes](https://www.apple.com/itunes/).

## Get scrios

Download a binary from the [releases page](https://github.com/kirkbrauer/scrios/releases), or build it:

```
cargo build --release --locked
```

The Linux release binaries have no window, only `scrios stream`. See [BUILD.md](BUILD.md).

## Run it

```
scrios
```

A window opens with the device's screen. Click and drag to touch, type to type, close the window to stop.

The first run downloads Apple's [developer disk image](docs/developer-disk-image.md) and installs it on the device, so it needs internet access.
If something goes wrong, run `scrios doctor`.

## Examples

```
scrios --record screen.h264            # record while you use the window
scrios --no-control                    # view only
scrios --udid 00008110-001234567890401E   # pick a device (see `scrios list`)
scrios stream | ffplay -f h264 -       # no window: send the video to a player
scrios stream --format framed | my-program   # video, events and control for a program
```

`ffplay` is part of [FFmpeg](https://ffmpeg.org/).

## Shortcuts

`MOD` is **Cmd** on macOS and **Alt** on Linux and Windows.

| Shortcut | Action |
| --- | --- |
| `MOD+H` | Home |
| `MOD+S` | App Switcher |
| `MOD+Up`, `MOD+Down` | Volume |
| `MOD+W` | Close the window |

## Documentation

- [Commands and options](docs/cli.md)
- [Stream format and control commands](docs/protocol.md), for programs such as
  [Jumpstarter](https://github.com/jumpstarter-dev/jumpstarter)
- [The developer disk image](docs/developer-disk-image.md): where it comes from, offline use, proxies
- [What has been tested](docs/status.md)
- [How it works](docs/internals.md)
- [Building](BUILD.md)

scrios uses the developer services [Xcode](https://developer.apple.com/xcode/) uses, through the [idevice](https://github.com/jkcoxson/idevice) library.
Built against **idevice 0.1.68**.

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
