# The framed protocol (version 1)

`scrios stream --format framed` is for programs. It writes video and events to standard output and reads commands
from standard input.

```
scrios stream --format framed | my-program
```

Logs go to standard error, never to standard output.

## Frames

The output is a sequence of frames with nothing between them. The stream ends at end of file. Integers are big-endian.

| Offset | Type | Field | Meaning |
| --- | --- | --- | --- |
| 0 | u32 | `len` | Length of the payload in bytes. At most 16 MiB. |
| 4 | u64 | `pts_us` | Microseconds since the first picture. 0 for events. |
| 12 | u8 | `flags` | See below. |
| 13 | | payload | One picture, or one JSON event. |

Flags:

| Bit | Name | Meaning |
| --- | --- | --- |
| `0x01` | KEY | The picture is a keyframe. A decoder can start here. |
| `0x02` | PARAMS | The payload starts with the parameter sets (SPS and PPS, plus VPS for HEVC). |
| `0x80` | CONTROL | The payload is a JSON event, not a picture. |

## Pictures

Each picture is one Annex-B access unit with 4-byte start codes.

- The first picture is a keyframe.
- Every keyframe carries its parameter sets.
- A picture that lost a packet is dropped, not sent. scrios then asks the device for a new keyframe and holds back
  pictures until it arrives.
- If your program reads too slowly, scrios drops queued pictures and asks for a keyframe. Events are never dropped.

## Events

An event is a JSON object with an `event` field. Ignore events and fields you do not know. New ones may be added
without changing the version.

### `config`

Sent before the first picture, and again whenever something in it changes, for example when the device rotates.

```json
{"event":"config","codec":"avc1.640033","width":1206,"height":2622,"coded_width":1216,"coded_height":2656,
 "fps":60,"clock_hz":24000,"clock_source":"default","ssrc":123,"device_ssrc":456,"rotation":0,
 "point_scale":3.0,"current_orientation":"rot0","native_orientation":"rot0","protocol":1}
```

| Field | Meaning |
| --- | --- |
| `codec` | A WebCodecs codec string, such as `avc1.640033` or `hev1.1.6.L150.B0`. |
| `width`, `height` | The visible size of the picture as encoded. |
| `coded_width`, `coded_height` | The encoded size, including padding. |
| `rotation` | Degrees to turn the picture clockwise to make it upright: 0, 90, 180 or 270. Missing if unknown. |
| `protocol` | The version of this protocol. |

The upright picture is `width` x `height` when `rotation` is 0 or 180, and `height` x `width` otherwise.

The other fields are informational: the frame rate, the RTP clock, the stream identifiers, and the device's own
display values.

### `stats`

Sent every few seconds. Counts since the start.

```json
{"event":"stats","packets":0,"access_units":0,"bytes":0,"lost":0,"dropped_units":0,"keyframes":0,"plis":0,
 "rtcp":0,"b_slices":0,"edge_downs":0}
```

| Field | Meaning |
| --- | --- |
| `access_units`, `bytes` | Pictures written, and their size. |
| `lost` | Network packets lost. |
| `dropped_units` | Pictures not written. |
| `keyframes` | Keyframes written. |
| `plis` | Keyframe requests sent. |

### `hid`

Reports on touch and keys.

| Event | Meaning |
| --- | --- |
| `{"event":"hid","ready":true,"keyboard":true,...}` | Sent once. Touch works. `keyboard` says whether `kb` commands work. |
| `{"event":"hid","ready":false,"message":"..."}` | Sent once. Touch and keys are not available. The video still runs. |
| `{"event":"hid","ok":false,"message":"..."}` | A command failed. Commands that work get no reply. |
| `{"event":"hid","warning":"..."}` | Something unexpected that is not an error. |

### `error` and `stopped`

`{"event":"error","message":"..."}` is sent once, just before a failed exit.
`{"event":"stopped"}` is the last frame after a clean stop.

If scrios cannot start, it writes an `error` and no `config`.

## Commands

One command per line on standard input. Closing standard input stops the stream.

| Command | What it does |
| --- | --- |
| `touch down X Y 0` | Put a finger down. |
| `touch move X Y 0` | Move it. |
| `touch up X Y 0` | Lift it. |
| `key NAME [short\|long]` | Press a hardware button. |
| `kb down USAGE`, `kb up USAGE` | Press or release a keyboard key. |
| `kb type BASE64` | Type text. |
| `keyframe` | Ask the device for a keyframe. |
| `stop` | Stop cleanly. |

**Touch.** `X` and `Y` are pixels in the upright picture, using the latest `config`. Positions outside the picture
are moved to its edge. Only finger `0` is supported.

**Buttons.** The names are `home`, `app-switcher`, `lock`, `power`, `sleep`, `volume-up`, `volume-down`, `mute` and
`siri`. `long` holds the button down longer.

**Keyboard keys.** `USAGE` is a USB HID keyboard usage in hex: `04` is A, `28` is Enter, `2a` is Backspace, `e1` is
Left Shift. Keys stay down until you release them. Keys left down for 30 seconds are released.

**Text.** `kb type` takes base64-encoded UTF-8, at most 4096 characters. It types through a US keyboard layout, so it
handles ASCII letters, digits, punctuation, space, newline and tab. Other characters are skipped and reported in a
`warning`.

**Keyframes.** Requests are limited to one every two seconds. Extra requests are merged.

**Stop.** Stopping lifts any finger, releases all keys, and ends the stream on the device. It can take a few seconds.
SIGINT and SIGTERM do the same.

A command that cannot be parsed gets an `ok:false` reply. With `--no-control`, touch and key commands get an
`ok:false` reply.

## Exit status

| Status | Meaning |
| --- | --- |
| 0 | Stopped cleanly. Also when the program reading the output has gone away. |
| 1 | The stream failed while running. |
| 2 | The stream could not start. |
| 101 | Internal error. |

## Versioning

The version is 1. It changes only if something incompatible changes: the frame layout, a flag, or the meaning of an
existing event or command.

## From Rust

A Rust program can skip the pipe. `scrios::stream::Stream` yields the same pictures and events, and `scrios::sink`
writes this format.
