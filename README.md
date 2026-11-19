# scrios

scrcpy for iOS: see and control an iPhone or iPad from your computer, over USB.

- Works on Linux, macOS and Windows
- No jailbreak, and nothing to install on the device
- Mouse and keyboard control
- Recording
- An H.264 stream for browsers, players and other programs

## Before you start

On the device (iOS 27):

1. Turn on **Developer Mode** in Settings > Privacy & Security.
2. Plug it in and tap **Trust**.

On the computer:

- **macOS**: nothing to install.
- **Linux**: install `usbmuxd`.
- **Windows**: install the Apple Devices app or iTunes.

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

The first run downloads Apple's developer disk image and installs it on the device, so it needs internet access.
If something goes wrong, run `scrios doctor`.

## Examples

```
scrios --record screen.h264            # record while you use the window
scrios --no-control                    # view only
scrios --udid 00008110-001234567890401E   # pick a device (see `scrios list`)
scrios stream | ffplay -f h264 -       # no window: send the video to a player
scrios stream --format framed | my-program   # video, events and control for a program
```

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
- [What has been tested](docs/status.md)
- [How it works](docs/internals.md)
- [Building](BUILD.md)

scrios uses the developer services Xcode uses, through the [idevice](https://github.com/jkcoxson/idevice) library.
Built against **idevice 0.1.68**.

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
