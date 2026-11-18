# The scrios command line

scrios shows an iOS device's screen as an H.264 (or HEVC) video stream and sends it touch and key events, through
usbmuxd and the device's CoreDevice tunnel. It is a command line in the manner of scrcpy: it writes the video where you
say, a player or a program reads it, and commands come in on standard input. It has no window of its own.

The device needs the CoreDevice display service (iOS 27 with a mounted developer image) and must already be paired with
the host (usbmuxd holds the pair record; scrios never pairs or mounts anything).

```
scrios [OPTIONS]            the same as: scrios stream [OPTIONS]
scrios stream [OPTIONS]     stream the screen
scrios list [--usbmuxd ADDR]   list the devices usbmuxd knows       (also: scrios --list-devices)
```

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
scrios --output - | ffplay -f h264 -framerate 60 -probesize 32 -flags low_delay -framedrop -y 1000 -window_title scrios -
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
scrios --record screen.h264                          # Ctrl-C stops it
ffplay -f h264 screen.h264
ffmpeg -f h264 -i screen.h264 -c copy screen.mp4
scrios --record screen.h264 --output - | ffplay -f h264 -framerate 60 -probesize 32 -flags low_delay -framedrop -y 1000 -   # watch and record
```

For a program: the framed format, which also carries the events (the picture's size and orientation, statistics, what
happened to touch and keys) and has a version number. [protocol.md](protocol.md) specifies it.

```
scrios --format framed --output - | my-program
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
# tap the middle of the screen, press Home, and stop (the end of input is a stop)
{ echo "touch down 603 1311 0"; sleep 0.1; echo "touch up 603 1311 0"; sleep 1; echo "key home"; sleep 1; } \
    | scrios --record tap.h264
```

Without a terminal on standard input (`< /dev/null`, nohup, a script that closes it) the end of input would stop the
stream at once: add `--no-stdin`, and stop it with SIGINT or SIGTERM.

## Where the output goes

| Command | Standard output | File |
| --- | --- | --- |
| `scrios` | raw, if stdout is not a terminal; otherwise refused | |
| `scrios --output -` | raw (to a terminal too) | |
| `scrios --output FILE` | | raw |
| `scrios --record FILE` | nothing | raw |
| `scrios --record FILE --output -` | raw | raw |
| `scrios --format framed` | framed, if stdout is not a terminal | |
| `scrios --format framed --output FILE` | | framed |

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
| `--usbmuxd ADDR` | the usbmuxd to ask: `UNIX:/path` (or a bare `/path`), `host:port` or `TCP:host:port`. Default: `$USBMUXD_SOCKET_ADDRESS`, else `/var/run/usbmuxd` (`127.0.0.1:27015` on Windows). usbmuxd is the only way scrios reaches a device, and the only place it reads a pair record |
| `--display-id N` | the display to stream (default 1, the built-in display) |
| `--video-codec h264\|h265` | the codec to ask for (default h264, which every player and browser decodes; `hevc` is an alias of `h265`; the older `--codec` still works) |
| `-r`, `--record FILE` | also record the video to FILE (raw Annex-B) |
| `-o`, `--output -\|FILE` | where the stream goes (see above) |
| `--format raw\|framed` | what is written to the output (default raw) |
| `-n`, `--no-control` | do not connect touch and keys: a stream that only watches |
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
| audio, window, keyboard and mouse forwarding | not available: there is no viewer yet |

### What changed from the first command line

* The default output is **raw**; the machine protocol is `--format framed`. A program that read frames from the earlier
  `scrios` must add `--format framed`.
* Touch and keys are on by default (`--no-control` turns them off); `--hid` is still accepted and does nothing.
* `--codec` is now `--video-codec`; the old name still works, with `hevc` or `h265`.
* `--max-bitrate` is still accepted and ignored (the display offer has no bitrate field).

## Exit status and logs

Logs go to standard error (`RUST_LOG` selects the level; the default is `info`). The exit status is 0 after a clean stop, 1
if the stream failed while running, 2 if it could not be started or the options do not go together, and 101 after an internal
error. Ctrl-C (SIGINT) and SIGTERM stop the stream cleanly, which can take up to about 8 seconds.

## As a library

The command line is a thin layer over the `scrios` crate: `scrios::stream::Stream::start` returns the access units and
events of a stream and an `Input` for touch, keys and stop; `scrios::sink` writes them in the formats above; `scrios::list`
lists devices. A viewer written in Rust uses those and does not need a pipe.
