# The scrios command line

scrios shows an iOS device's screen as an H.264 (or HEVC) video stream and sends it touch and key events, through
usbmuxd and the device's CoreDevice tunnel. It is a command line in the manner of scrcpy. Without a subcommand it opens a
**window** that shows the screen and takes your mouse and keyboard (see [the viewer](#the-viewer)); `scrios stream`
writes the video where you say, a player or a program reads it, and commands come in on standard input.

The device needs the CoreDevice display service (iOS 27, with Developer Mode on and the developer disk image mounted)
and must trust the host (usbmuxd holds the pair record). scrios gets there the way scrcpy does, with no extra steps: it
waits for the device, for Trust and for Developer Mode, and mounts the developer disk image itself ([what happens before
the stream](#what-scrios-does-before-the-stream)). usbmuxd is its only way to a device (as adb is scrcpy's): it reads
the pair record from usbmuxd, never from a file of the host, and a usbmuxd on another machine works as well
([a remote usbmuxd](#a-remote-usbmuxd)).

```
scrios [OPTIONS]            the viewer window (in a build without the viewer: the same as scrios stream [OPTIONS])
scrios stream [OPTIONS]     stream the screen to a pipe or a file, headless
scrios list [--usbmuxd ADDR]   list the devices usbmuxd knows       (also: scrios --list-devices)
scrios doctor               check everything the stream needs, and say how to fix what is missing
scrios ddi-status           Developer Mode, the mounted developer disk image, the image scrios would mount
scrios mount-ddi            mount the developer disk image (iOS 17 and later)
scrios unmount-ddi          unmount it
scrios developer-mode       is Developer Mode on? reveal its switch; --enable --yes turns it on
```

**`scrios` without a subcommand changed.** It used to be `scrios stream`. In a build with the viewer (the default; the
static Linux release binaries are built without it) it opens the window, and a program that ran `scrios --format framed`
or `scrios | ffplay ...` must now run `scrios stream --format framed` and `scrios stream | ffplay ...`. The window refuses
`--output`, `--format` and `--probe-secs` with a message that says so (exit status 2), so such a program fails at once
instead of opening a window.

## Examples

List the devices (UDID and how each is connected; usbmuxd knows nothing else about a device, so there is no name or model):

```
$ scrios list
UDID                       CONNECTION
00008110-001234567890401E  usb
```

Watch the screen. The default output is a raw Annex-B stream on standard output. For live viewing with low latency,
this `ffplay` command line is the one checked on a real iPhone (macOS, ffmpeg 9):

```
scrios stream --output - | ffplay -f h264 -framerate 60 -probesize 32 -flags low_delay -framedrop -y 1000 -window_title scrios -
```

What each flag is for, because raw Annex-B has no timestamps and no container:

| Flag | Why |
| --- | --- |
| `-framerate 60` | Without it ffplay paces the stream at the demuxer's 25 fps default and builds a backlog of seconds. Use the device's rate (60 for current iPhones and iPads). |
| `-probesize 32` | Without it ffplay reads about 5 MB before it shows anything: seconds on a still screen. |
| `-flags low_delay`, `-framedrop` | Show each picture as soon as it is decoded; drop late ones rather than fall behind. |
| `-y 1000` | The window's height; a phone's screen is about 2,600 pixels tall. |

**Do not add `-fflags nobuffer`**: with a raw H.264 pipe it makes ffplay decode nothing (ffmpeg 9.0.2: 0 frames, no
window). The same applies to HEVC with `--video-codec h265` and `-f hevc`.

(`--udid` is not needed when exactly one device is connected; `--output -` is the default when standard output is a pipe
or a file.)

Record to a file. A recording is a raw Annex-B stream, flushed picture by picture, so a recording that was cut short is
playable up to the cut; `ffmpeg` wraps it in a container:

```
scrios stream --record screen.h264                   # Ctrl-C stops it
ffplay -f h264 screen.h264
ffmpeg -f h264 -i screen.h264 -c copy screen.mp4
scrios stream --record screen.h264 --output - | ffplay -f h264 -framerate 60 -probesize 32 -flags low_delay -framedrop -y 1000 -   # watch and record
```

**On Windows** the same commands work from `cmd.exe` or PowerShell with `scrios.exe`, with two differences. Ctrl-C (or
Ctrl-Break) is the stop, and the video must not go through Windows PowerShell 5.1's pipeline or `>`, which re-encode
it as text: write it with `--record FILE` or `--output FILE`, or pipe it in `cmd.exe` (PowerShell 7.4 and later pass
the bytes through):

```
.\scrios.exe stream --record screen.h264
ffplay -f h264 -framerate 60 -probesize 32 -flags low_delay -framedrop -y 1000 screen.h264
cmd /c ".\scrios.exe stream --output - | ffplay -f h264 -framerate 60 -probesize 32 -flags low_delay -framedrop -y 1000 -window_title scrios -"
```

(Ctrl-C stops the recording. `ffplay` of a growing file is not live; for watching use the `cmd /c` pipe. VLC plays a recording too: Media > Open
File, with the demuxer forced to H.264 if it does not recognise the `.h264`.) None of this has been tried on Windows yet.

For a program: the framed format, which also carries the events (the picture's size and orientation, statistics, what
happened to touch and keys) and has a version number. [protocol.md](protocol.md) specifies it.

```
scrios stream --format framed --output - | my-program
```

Control. Commands arrive on standard input, one per line (the full list is in `scrios --help` and in
[protocol.md](protocol.md)):

```
keyframe                     ask the device for a fresh keyframe
stop                         stop cleanly
touch down|move|up X Y ID    a finger (ID 0), in pixels of the upright picture
key NAME [short|long]        home, app-switcher, lock, power, sleep, volume-up, volume-down, mute, siri
kb down|up USAGE             a key of the HID keyboard page, by its usage in hex
kb type BASE64               type a string
```

```
# tap the middle of the screen, press Home, and stop
{ echo "touch down 603 1311 0"; sleep 0.1; echo "touch up 603 1311 0"; sleep 1; echo "key home"; sleep 1; echo stop; } \
    | scrios stream --record tap.h264
```

The end of standard input is a `stop` only with `--format framed` (a program that closes its end is done). A raw stream
or a recording does not stop at the end of input, so one started with standard input closed (`< /dev/null`, nohup, a
script) runs on; stop it with `stop`, SIGINT or SIGTERM (on Windows: `stop`, Ctrl-C or Ctrl-Break; there is no SIGTERM).
`--no-stdin` does not read standard input at all.

## What scrios does before the stream

Plain `scrios` (and `scrios stream`) makes sure the device is ready before it connects. Each step that is missing is
waited for, and **each wait prints one plain line on standard error saying what it waits for** (once, however long it
takes):

| Step | When it is missing |
| --- | --- |
| usbmuxd (the address of `--usbmuxd`, else `$USBMUXD_SOCKET_ADDRESS`, else the platform's own) | an error that says where it looked and how to start it; no waiting |
| a device (`--udid`, else the only one usbmuxd lists) | `Plug in and unlock your iPhone or iPad`, and it waits. Several devices and no `--udid` is an error that lists their UDIDs |
| trust: usbmuxd holds a pair record for the device | `Unlock the device and tap Trust`, and it waits |
| Developer Mode is on | the Developer Mode switch is revealed in Settings and the steps are printed (`Settings > Privacy & Security > Developer Mode > On`, then restart the device and confirm); it waits through the restart. scrios **never turns Developer Mode on by itself** |
| the developer disk image is mounted | it is mounted from the [DDI folder](#the-developer-disk-image) (Apple's signing server needs internet for it the first time after each restart of the device); no folder is an error that says where to put one |

Then the stream starts on the device that was checked. The waiting is for a person at a terminal:

* `--no-wait` fails at once, with the same words as the error, instead of waiting (exit status 2): for scripts.
* `--format framed` (what a program reads) makes none of these checks and never waits: the stream starts as it always
  has, and what is missing arrives as an `error` event. Run `scrios doctor` or `scrios mount-ddi` before it.
* `--no-preflight` does the same for the other formats.
* Ctrl-C (SIGINT) or SIGTERM while it waits is a clean stop (exit status 0); on Windows, Ctrl-C or Ctrl-Break.

## Commands that set a device up

All of them take `-s, --udid` and `--usbmuxd` like the stream, and none of them waits: what is missing is an error that
says what to do.

* **`scrios doctor`** runs the checks of the list above one by one and prints `ok`, `FAIL` or `skip` for each, with the fix
  under each failure: usbmuxd, device, pairing, Developer Mode, developer disk image (mounted), image source (where one
  would be mounted from), display service (the device's CoreDevice tunnel offers it). Exit status 0 if nothing failed, 1
  if something did.

  ```
  ok    usbmuxd               reachable at /var/run/usbmuxd (lists 1 device(s))
  ok    device                00008110-001234567890401E (usb)
  ok    pairing               usbmuxd holds the pair record of the device
  FAIL  developer mode        off
                              fix: Turn Developer Mode on: Settings > Privacy & Security > Developer Mode > On, then restart the device (...)
  ```

* **`scrios ddi-status`** prints Developer Mode, whether the developer disk image is mounted (and what the device lists
  as mounted), and the folder scrios would mount from. Exit status 0 if Developer Mode is on and the image is mounted, 1
  if one is not, 2 if the device could not be asked.
* **`scrios mount-ddi`** mounts the personalized developer disk image: it reads the image, its trust cache and the build
  manifest from the DDI folder, picks the build identity of the device's board and chip, asks Apple's signing server
  (TSS, plain HTTP to `gs.apple.com`, which needs internet; HTTP proxy variables are not used) for the device's ticket
  unless the device holds one, uploads the image and mounts it. It does nothing if an image is mounted already.
  iOS 17 and later only (the display service needs iOS 27); an older iOS gets an error that says so.
* **`scrios unmount-ddi`** unmounts the developer disk image.
* **`scrios developer-mode`** says whether Developer Mode is on. If it is off it reveals the switch in the device's
  Settings (AMFI) and prints the steps. **`--enable --yes`** is the explicit opt-in that turns it on (AMFI): **the device
  restarts**, and it works only on a device **without a passcode**; `--enable` without `--yes` refuses and says why.
  Exit status 0 if it is on (or is being turned on), 1 if it is off.

## The developer disk image

The display service is only offered when the developer disk image is mounted. scrios mounts the personalized image of iOS
17 and later from a **DDI folder**: the `iOS_DDI` folder that Xcode installs. **scrios does not ship, host or download
Apple's image by default.** It looks, in this order, at:

1. `--ddi-dir DIR`
2. `$SCRIOS_DDI_DIR`
3. on macOS, Xcode's own folders: `/Library/Developer/DeveloperDiskImages/iOS_DDI` and
   `<Xcode.app>/Contents/Resources/CoreDeviceDDIs/iOS_DDI` (the Xcode that `xcode-select -p` names, else
   `/Applications/Xcode.app`). The first was seen on a Mac with Xcode installed; the second is from what Xcode is known
   to do and was not found on that Mac: they are tried, and used only where they exist.
4. scrios's cache folder: `~/Library/Caches/scrios/ddi` (macOS), `$XDG_CACHE_HOME/scrios/ddi` or `~/.cache/scrios/ddi`
   (Linux), `%LOCALAPPDATA%\scrios\ddi` (Windows).

A folder asked for with `--ddi-dir` or `$SCRIOS_DDI_DIR` must be a DDI folder (an error says what is wrong); Xcode's and
the cache are skipped if they are not there. With none found, scrios says so and how to provide one: **copy the `iOS_DDI`
folder from a Mac with Xcode into the cache folder**, so that `<cache>/Restore/BuildManifest.plist` exists:

```
# on a Linux host (from a Mac with Xcode)
mkdir -p ~/.cache/scrios && scp -r mac:/Library/Developer/DeveloperDiskImages/iOS_DDI ~/.cache/scrios/ddi
# on a Windows host, in PowerShell
scp -r mac:/Library/Developer/DeveloperDiskImages/iOS_DDI $env:LOCALAPPDATA\scrios\ddi
```

(Anywhere else works too: `scrios mount-ddi --ddi-dir C:\path\to\iOS_DDI`, or `$SCRIOS_DDI_DIR`.)

Inside a folder, the build manifest is looked for in the folder, in `Restore/`, and under an `iOS_DDI/` folder in either.
The image and trust cache are the files the manifest names for the device's board and chip (Xcode's layout is
`Restore/BuildManifest.plist`, `Restore/<name>.dmg` and `Restore/Firmware/<name>.dmg.trustcache`); a flat folder with
`Image4.dmg` (or `Image.dmg`, `DeveloperDiskImage.dmg`) and its `.trustcache` next to a manifest works too. A manifest
with no build identity for the device (the image is from an Xcode older than the device's iOS) is an error that says so.

**A download**, for a fleet that has no Mac to copy from: if you host a `.tar.gz` of the folder yourself
(`tar czf ddi.tar.gz -C iOS_DDI .`), `--ddi-download-url URL --ddi-sha256 HEX` (or `$SCRIOS_DDI_URL` and
`$SCRIOS_DDI_SHA256`) downloads it into the cache folder when no folder is found. The SHA-256 is required: an archive
that does not match is never unpacked, and an entry that would leave the folder is an error. There is no default URL.

## A remote usbmuxd

usbmuxd is the only transport, so a device plugged into another machine works like scrcpy's remote adb: expose that
machine's usbmuxd and point `--usbmuxd` at it. The pair record is read through it too, so the local machine needs no
pairing.

```
# on the machine the device is plugged into (and paired with)
ssh -N -L 27015:/var/run/usbmuxd you@that-machine        # run where scrios runs: forwards 127.0.0.1:27015 to the socket
scrios --usbmuxd 127.0.0.1:27015
```

or, without ssh, on the machine with the device: `socat TCP-LISTEN:27015,reuseaddr,fork UNIX-CONNECT:/var/run/usbmuxd`
and `scrios --usbmuxd that-machine:27015`. **That socat port is unauthenticated: anyone who can reach it can use
every device the usbmuxd knows and read its pair record. Use it only on a link you trust; prefer ssh.**
The CoreDevice tunnel and the services scrios opens (the display service, touch and keys, the image mounter) are all
connections that usbmuxd makes to the device, so they should work over a remote usbmuxd; this is reasoned from how
they connect and has not been tried on a device.

## The viewer

`scrios` (with no subcommand, in a build with the viewer) opens a window with the device's screen. [README](../README.md#viewer)
has the overview; the details:

* **Before the window.** The window opens when the device is ready: the same set-up as `scrios stream` runs first, on
  the terminal ([what scrios does before the stream](#what-scrios-does-before-the-stream): waiting for the device, Trust
  and Developer Mode, mounting the developer disk image), with `--no-wait`, `--no-preflight` and the image options as
  there. Ctrl-C while it waits is a clean stop. `scrios doctor` also checks that the build has a decoder for the window.
* **Window.** It opens at the device's shape, as large as the screen has room for (`--window-width` and `--window-height`
  limit it, in logical pixels), is resizable, and keeps the picture's shape with black bars. When the device turns, the
  window follows it (the stream's `config` event gives the rotation). Closing the window, `MOD+W`, Ctrl-C and SIGTERM stop
  the stream on the device (up to about 8 seconds, with the window already gone) and exit with the stream's status.
* **Mouse** = finger 0, in pixels of the upright picture (the same `touch down|move|up` as the stdin commands). A press in
  the bars is ignored; a drag that leaves the picture sticks to its edge; losing the focus or a turn of the device lifts
  the finger.
* **Edge margin** (`--edge-margin PIXELS`, default 24 logical pixels, 0 = none). The picture is inset from the window by a
  band a little lighter than the bars. A press that starts in the band is **on the device's edge**: in the bottom band it
  is the last row of the upright picture (the digitizer's own edge, 65535), in the top band row 0, in the side bands
  column 0 or the last column, with the other coordinate as aimed (a corner is the device's corner). The moves after the
  press map as usual and stay on the picture. So an upward drag from the bottom band is a true edge swipe (Home, unlock), a
  downward one from the top band is the top-edge swipe (Control Center, Notification Center), and the window's own resize
  border, outside the band, is not what the drag grabs. While the pointer is in the bottom band a short bar is drawn in it.
  The first window is the picture's size plus the band, and `--window-width`/`--window-height` are for the whole window.
  The band is never more than a sixth of the window's shorter side.
* **Keyboard.** A key goes down and up on the device (`kb down|up`, HID usage by the key's position) as it does on the
  host. Auto-repeat of the host is not forwarded. Losing the focus lifts every key.
* **Shortcuts** with `MOD` = **Cmd** on macOS, **Alt** elsewhere; not sent to the device, and neither is the modifier or a
  key pressed with it that is not a shortcut:

  | | |
  | --- | --- |
  | `MOD+H` | `key home` |
  | `MOD+S` | `key app-switcher` |
  | `MOD+Up`, `MOD+Down` | `key volume-up`, `key volume-down` (repeat while held) |
  | `MOD+G` | the swipe up from the bottom edge: `touch down` on the last row in the middle, a straight run up by 30% of the height in about 150 ms, `touch up` (the gesture that goes Home or unlocks); not made while a finger is down |
  | `MOD+W` | close the window |
  | `MOD+P` | `key lock`, only with `--lock-shortcut` |

* **`--decoder auto|platform|software`.** H.264 only (`--video-codec h265` is refused: use `scrios stream`). `auto` is the
  platform's decoder (VideoToolbox on macOS) with OpenH264 as the fallback if there is none or if it fails; `platform`
  insists on the platform's (an error on Linux, and on Windows until the Media Foundation decoder is written);
  `software` is OpenH264. Pictures are decoded and drawn as they arrive; one that is not drawn before the next is
  decoded is dropped.
* **`--show-stats`** puts `FPS fps, DELAY ms (max MAX), decode D ms` in the title: the pictures drawn a second, and the
  time from the access unit's arrival at the viewer to its drawing (decoding and the wait for the screen), averaged over
  the last second. `skipped N` counts pictures decoded but never drawn, `N dropped` the access units the stream had to
  drop because the viewer could not keep up. The delay is the viewer's own; the device's encoder and the USB link are not in it.
* **`--record FILE`** records the raw stream while the window is open (the same file as for `scrios stream`).
* **`--no-control`** is view only: the mouse and keys do nothing and the title says so. If the device's touch and key
  services cannot be connected, the window still shows the screen and says so in the title.
* The window needs a display, a GPU API (Metal, Vulkan, Direct3D 12 or OpenGL) and, to open at all, a build with the
  `viewer` feature. Without a window system `scrios` exits with status 2 and says why.

## On Windows

What a Windows host needs is in the [README](../README.md#prerequisites): the Apple Devices app or iTunes (their Apple Mobile
Device Service is usbmuxd, on `127.0.0.1:27015`), a trusted device, Developer Mode, and a DDI folder copied from a Mac with
Xcode. A first session, in PowerShell, from the folder `scrios.exe` was unzipped to (nothing here has been tried on a
device yet; `doctor` shows where it stops):

```
.\scrios.exe list                              # the device, as usbmuxd lists it
.\scrios.exe doctor                            # each check, with its fix
.\scrios.exe mount-ddi --ddi-dir C:\path\to\iOS_DDI
.\scrios.exe stream --record test.h264         # Ctrl-C stops it (plain `scrios.exe` opens the window)
ffplay -f h264 test.h264                       # or VLC: Media > Open File
```

A usbmuxd somewhere else: `--usbmuxd host:port` or `$env:USBMUXD_SOCKET_ADDRESS = "host:port"`. Set `$env:RUST_LOG = "debug"`
for the details of a failure. The Apple Mobile Device Service listens on 127.0.0.1 only; to use a device plugged into a
Windows machine from another host, forward its port over ssh (`ssh -N -L 27015:127.0.0.1:27015 you@windows-host`, which
needs an SSH server there).

## Where the output goes (`scrios stream`)

| Command | Standard output | File |
| --- | --- | --- |
| `scrios stream` | raw, if stdout is not a terminal; otherwise refused | |
| `scrios stream --output -` | raw (to a terminal too) | |
| `scrios stream --output FILE` | | raw |
| `scrios stream --record FILE` | nothing | raw |
| `scrios stream --record FILE --output -` | raw | raw |
| `scrios stream --format framed` | framed, if stdout is not a terminal | |
| `scrios stream --format framed --output FILE` | | framed |

* **`--format raw`** (the default) writes the access units one after the other: a plain Annex-B elementary stream with
  4-byte start codes and the parameter sets in front of every key unit. Events are not written: errors and warnings are in
  the log on standard error, and the exit status says how the stream ended.
* **`--format framed`** writes the length-framed machine protocol of [protocol.md](protocol.md), byte for byte as the
  earlier versions did, plus the `protocol` field of the `config` event.
* **`--record FILE`** always writes raw, whatever `--format` says. It does not take a container: a name ending in
  `.mp4`, `.mkv`, `.mov`, `.m4v`, `.webm` or `.avi` is refused with a hint (use `.h264`, or `.h265` for HEVC, and `ffmpeg -c copy`).
* The video is binary: if standard output is a terminal and `--output` is not given, scrios refuses to start instead of
  printing it.
* The stream and the recording start at a key picture. If one of them cannot be written any more (the player closed, the disk
  is full) it is dropped with a warning and the other goes on; when none is left the stream stops (exit status 0).

## Options

`scrios --help` has them all, with the defaults. The ones that matter:

| Option | Meaning |
| --- | --- |
| `-s`, `--udid UDID` | the device (default: the only one usbmuxd lists) |
| `--usbmuxd ADDR` | the usbmuxd to ask: `UNIX:/path` (or a bare `/path`), `host:port` or `TCP:host:port`. Default: `$USBMUXD_SOCKET_ADDRESS`, else `/var/run/usbmuxd` (`127.0.0.1:27015` on Windows, the Apple Mobile Device Service; Windows has no unix sockets, so only the `host:port` forms are taken there, and `--help` shows the host's own default). usbmuxd is the only way scrios reaches a device, and the only place it reads a pair record |
| `--display-id N` | the display to stream (default 1, the built-in display) |
| `--video-codec h264\|h265` | the codec to ask for (default h264, which every player and browser decodes; `hevc` is an alias of `h265`; the older `--codec` still works) |
| `-r`, `--record FILE` | also record the video to FILE (raw Annex-B) |
| `-o`, `--output -\|FILE` | where the stream goes (see above) |
| `--format raw\|framed` | what is written to the output (default raw) |
| `-n`, `--no-control` | do not connect touch and keys: a stream that only watches |
| `--no-wait` | fail at once when the device is not ready (not plugged in, not trusted, Developer Mode off), instead of waiting |
| `--no-preflight` | do not check Developer Mode and the developer disk image first, and never mount one |
| `--ddi-dir DIR` | the developer disk image folder to mount from (see [the developer disk image](#the-developer-disk-image)) |
| `--ddi-download-url URL`, `--ddi-sha256 HEX` | download the image (a `.tar.gz` of the folder, checked against the hash) when no folder is found; no default URL |
| `--decoder auto\|platform\|software`, `--window-title`, `--window-width`, `--window-height`, `--edge-margin`, `--show-stats`, `--lock-shortcut` | the viewer's (see above); `scrios` without a subcommand only |
| `--list-devices` | the same as `scrios list` |

Under **Advanced** in `--help`: `--visible-size`, the RTCP and stall intervals, `--display-poll-ms`, the touch mapping
(`--hid-space`, `--hid-flip-x`, `--hid-flip-y`, `--hid-swap-xy`; see [internals.md](internals.md#the-touch-mapping)), `--no-stdin`, and the
offer experiments (`--rtcp-fb`, `--no-ltr`, `--device-hub-feedback`, `--log-answer`). Under **Diagnostics**: `--probe-secs` and
`--probe-raw`, which measure what the device sends instead of streaming (see [internals.md](internals.md)).

Control is on by default. If the device's touch and key services cannot be connected, the stream runs without them and says
so (a `hid` event with `ready: false` and the reason in `message`, and a warning in the log); a touch or key command for a
service that did not connect is answered with an `ok: false` event.

### Compared with scrcpy

| scrcpy | scrios |
| --- | --- |
| `-s`, `--serial` | `-s`, `--udid` |
| `--video-codec h264\|h265` | the same (`hevc` also) |
| `-r`, `--record FILE` | the same; raw Annex-B only, no mp4/mkv |
| `-n`, `--no-control` | the same (touch and keys; there is no clipboard or app launching) |
| `--list-devices` | `--list-devices`, `scrios list` |
| `--display-id` | the same |
| `--max-fps`, `--video-bit-rate`, `--max-size` | not available: the device's display offer has no frame-rate, bit-rate or size field to set (the stream is the display's own size and rate) |
| `--no-video` | not available: the device only accepts touch while a display stream is open |
| the window, mouse and keyboard forwarding | `scrios` opens a window; mouse = one finger, keys = HID keyboard, `MOD+H` Home, `MOD+S` App Switcher, `MOD+Up`/`Down` volume (no multi-touch, no clipboard) |
| audio | not available |

### What changed from the first command line

* The default output is **raw**; the machine protocol is `--format framed`. A program that read frames from the earlier
  `scrios` must add `--format framed`.
* Touch and keys are on by default (`--no-control` turns them off); `--hid` is still accepted and does nothing.
* `scrios` without a subcommand is the viewer window in a build with the viewer; the old behaviour is `scrios stream`.
* `--codec` is now `--video-codec`; the old name still works, with `hevc` or `h265`.
* `--max-bitrate` is still accepted and ignored (the display offer has no bitrate field).

## Environment

| Variable | Meaning |
| --- | --- |
| `USBMUXD_SOCKET_ADDRESS` | the usbmuxd to ask when `--usbmuxd` is not given (`UNIX:/path`, `/path`, `host:port`, `TCP:host:port`, the first two not on Windows; the variable libimobiledevice and go-ios read) |
| `SCRIOS_DDI_DIR` | the DDI folder, when `--ddi-dir` is not given |
| `SCRIOS_DDI_URL`, `SCRIOS_DDI_SHA256` | the archive to download the DDI from, and its SHA-256 |
| `RUST_LOG` | the log level (the default is `info`) |

## Exit status and logs

Logs go to standard error (`RUST_LOG` selects the level; the default is `info`). The exit status is 0 after a clean stop, 1
if the stream failed while running, 2 if it could not be started or the options do not go together, and 101 after an internal
error. Ctrl-C (SIGINT) and SIGTERM stop the stream cleanly (on Windows: Ctrl-C and Ctrl-Break; closing the console window
or `taskkill /f` ends the process at once), which can take up to about 8 seconds. A device that is not
ready (and `--no-wait`, or a command that does not wait) is exit status 2. `scrios doctor` and `scrios developer-mode`
exit with 1 when something is missing, and `scrios ddi-status` when the image is not mounted or Developer Mode is off.

## As a library

The command line is a thin layer over the `scrios` crate: `scrios::stream::Stream::start` returns the access units and
events of a stream and an `Input` for touch, keys and stop; `scrios::sink` writes them in the formats above; `scrios::list`
lists devices. A viewer written in Rust uses those and does not need a pipe; the window of `scrios` is one, in
`scrios::viewer` (feature `viewer`: `Setup::prepare(options)?.run(feed)` with the `Stream` as the feed, started on the
`ReadyDevice`'s `stream_options`).

What happens before the stream is one function, `scrios::preflight::ensure_ready(&PreflightOptions, &mut progress)`,
which returns a `ReadyDevice` (its `stream_options` gives the `StreamOptions` for the device that was checked) and
tells `progress` what it does and waits for as `PreflightStep`s (each has a plain line). `scrios::preflight::doctor`
is the same checks as a `Report`, to which a program can add its own (`scrios doctor` adds the viewer's decoder).
`scrios::mux::Mux` is the usbmuxd address (`--usbmuxd`, `$USBMUXD_SOCKET_ADDRESS`, the platform's), `scrios::ddi` finds
and unpacks the developer disk image, and `scrios::ops::Device` has the operations on a device (Developer Mode,
mounting).
