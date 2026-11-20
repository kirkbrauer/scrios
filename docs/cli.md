# Commands and options

`scrios --help` lists everything with its default. This page explains the parts that need explaining.

## Commands

| Command | What it does |
| --- | --- |
| `scrios` | Open a window with the device's screen. |
| `scrios stream` | Write the video to standard output or a file, with no window. |
| `scrios list` | List connected devices. |
| `scrios doctor` | Check the setup step by step and say how to fix what fails. |
| `scrios developer-mode` | Show whether Developer Mode is on. |
| `scrios ddi-status` | Show whether the developer disk image is installed. |
| `scrios mount-ddi` | Install the developer disk image. |
| `scrios unmount-ddi` | Remove it. |
| `scrios ddi-fetch` | Download the developer disk image without a device. |

A build without the window (the Linux release binaries) runs `scrios stream` when you type `scrios`.

## Options

These work with both `scrios` and `scrios stream`.

| Option | What it does |
| --- | --- |
| `-s`, `--udid UDID` | Pick a device. Not needed when only one is connected. |
| `--usbmuxd ADDR` | Where usbmuxd is: a socket path or `host:port`. See [a device on another machine](#a-device-on-another-machine). |
| `-r`, `--record FILE` | Record the video to a file. |
| `-n`, `--no-control` | View only. Touch and keys are not sent. |
| `--video-codec h264\|h265` | The codec. H.264 is the default. The window only shows H.264. |
| `--display-id N` | Which display to show. 1 is the built-in display. |
| `--no-wait` | Fail right away if the device is not ready, instead of waiting. |
| `--no-preflight` | Skip the setup checks. |
| `--list-devices` | The same as `scrios list`. |

### Window options

For `scrios` only.

| Option | What it does |
| --- | --- |
| `--window-title TITLE` | The window title. |
| `--window-width N`, `--window-height N` | The largest size the window opens at. |
| `--show-stats` | Show the frame rate and delay in the title. |
| `--decoder auto\|platform\|software` | The H.264 decoder. `platform` is [VideoToolbox](https://developer.apple.com/documentation/videotoolbox) on macOS and [Media Foundation](https://learn.microsoft.com/windows/win32/medfound/h-264-video-decoder) on Windows. `software` is [OpenH264](https://www.openh264.org/). `auto` tries the platform decoder first. |
| `--edge-margin N` | Add a band around the picture. A press in the band lands on the very edge of the screen, which makes edge swipes easier. Try 24. |
| `--bottom-swipe keys\|touch` | What a drag up from the bottom edge does. See [the bottom swipe](#the-bottom-swipe). |
| `--lock-shortcut` | Let `MOD+P` lock the device. Off by default so you cannot lock yourself out by accident. |

### Stream options

For `scrios stream` only.

| Option | What it does |
| --- | --- |
| `-o`, `--output -\|FILE` | Where the video goes. `-` is standard output. |
| `--format raw\|framed` | `raw` is plain H.264 that players read. `framed` adds timestamps and events for programs. See [protocol.md](protocol.md). |
| `--no-stdin` | Do not read commands from standard input. |

Where the video ends up:

| Command | Video goes to |
| --- | --- |
| `scrios stream \| player` | The pipe. |
| `scrios stream --output FILE` | The file. |
| `scrios stream --record FILE` | The file only. |
| `scrios stream --record FILE --output -` | The file and standard output. |

scrios refuses to write video to a terminal. Recordings are raw H.264 (`.h264`) or HEVC (`.h265`), not MP4. To get an
MP4, use [FFmpeg](https://ffmpeg.org/):

```
ffmpeg -f h264 -i screen.h264 -c copy screen.mp4
```

For a live view in [ffplay](https://ffmpeg.org/ffplay.html) with low delay:

```
scrios stream | ffplay -f h264 -framerate 60 -probesize 32 -flags low_delay -framedrop -
```

### Other options

`scrios --help` also lists advanced options for touch mapping (`--hid-space`, `--hid-flip-x`, `--hid-flip-y`,
`--hid-swap-xy`), timing intervals, and diagnostics (`--probe-secs`). Most people never need them.
[internals.md](internals.md) explains the touch mapping.

## The window

- **Mouse**: click and drag to touch. One finger only.
- **Keyboard**: keys are sent as a hardware keyboard with a US layout.
- **Rotation**: the window turns when the device turns.
- **Close**: close the window or press Ctrl-C in the terminal.

Shortcuts use **Cmd** on macOS and **Alt** on Linux and Windows. Keys pressed with that modifier are not sent to the
device.

| Shortcut | Action |
| --- | --- |
| `MOD+H` | Home |
| `MOD+S` | App Switcher |
| `MOD+Up`, `MOD+Down` | Volume |
| `MOD+G` | The bottom swipe |
| `MOD+W` | Close the window |
| `MOD+P` | Lock the device (needs `--lock-shortcut`) |

### The bottom swipe

iOS ignores the swipe up from the home indicator when it arrives as injected touch. So scrios turns that gesture into
button presses:

- Drag up from the bottom edge and let go: **Home**.
- Drag up, pause, then let go: **App Switcher**.

Other drags that start at the bottom are sent as normal touch. Use `--bottom-swipe touch` to send everything as touch.

## Getting the device ready

Before it connects, scrios checks each of these and waits for the ones you can fix:

1. [usbmuxd](https://github.com/libimobiledevice/usbmuxd) is running.
2. A device is plugged in.
3. The device trusts the computer. Unlock it and tap **Trust**.
4. [Developer Mode](https://developer.apple.com/documentation/xcode/enabling-developer-mode-on-a-device) is on. scrios shows the switch in Settings and prints the steps. It does not turn it on for you.
5. The developer disk image is installed. scrios installs it. See [developer-disk-image.md](developer-disk-image.md).

`scrios doctor` runs the same checks without waiting and prints `ok` or `FAIL` for each, with the fix:

```
ok    usbmuxd               reachable at /var/run/usbmuxd (lists 1 device(s))
ok    device                00008110-001234567890401E (usb)
FAIL  developer mode        off
                            fix: Turn Developer Mode on: Settings > Privacy & Security > Developer Mode > On ...
```

`scrios stream --format framed` skips these checks, because a program is reading the output. Run `scrios doctor`
first.

`scrios developer-mode --enable --yes` turns Developer Mode on for you. The device restarts, and it only works on a
device with no passcode.

## A device on another machine

scrios reaches devices through usbmuxd, so a device plugged into another machine works if you can reach that machine's
usbmuxd. Forward it over SSH:

```
ssh -N -L 27015:/var/run/usbmuxd you@that-machine &
scrios --usbmuxd 127.0.0.1:27015
```

Do not expose usbmuxd on an open network port. Anyone who can reach it can control every device on that machine.

This has not been tried on a real device yet.

## Windows

- Install the [Apple Devices](https://apps.microsoft.com/detail/9np83lwlpz9k) app or [iTunes](https://www.apple.com/itunes/). scrios talks to its service on `127.0.0.1:27015`.
- Stop with Ctrl-C.
- Windows PowerShell 5.1 corrupts piped video. Use `--record FILE` or `--output FILE`, or run the pipe in `cmd.exe`.
  PowerShell 7.4 and later are fine.

## Environment variables

| Variable | What it does |
| --- | --- |
| `USBMUXD_SOCKET_ADDRESS` | Where usbmuxd is, when `--usbmuxd` is not given. |
| `RUST_LOG` | Log level. `RUST_LOG=debug` shows more. |
| `SSL_CERT_FILE`, `SSL_CERT_DIR` | Extra certificate authorities to trust when downloading. |
| `SCRIOS_DDI_*` | Developer disk image settings. See [developer-disk-image.md](developer-disk-image.md). |

## Exit status

| Status | Meaning |
| --- | --- |
| 0 | Stopped cleanly. |
| 1 | The stream failed while running, or a check found something missing. |
| 2 | The stream could not start, or the options do not go together. |
| 101 | Internal error. |

Logs go to standard error. Stopping can take a few seconds while the device ends the stream.

## Compared with scrcpy

For people who know [scrcpy](https://github.com/Genymobile/scrcpy):

| scrcpy | scrios |
| --- | --- |
| `-s`, `--serial` | `-s`, `--udid` |
| `-r`, `--record` | The same, but raw H.264 instead of MP4. |
| `-n`, `--no-control` | The same. |
| `--video-codec`, `--display-id`, `--window-title`, `--window-width`, `--window-height` | The same. |
| `--max-size`, `--max-fps`, `--video-bit-rate` | Not available. The device chooses these. |
| Audio, clipboard, multi-touch | Not available. |
