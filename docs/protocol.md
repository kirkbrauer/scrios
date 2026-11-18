# The framed protocol (version 1)

`scrios --format framed` is the machine interface of scrios: the video and the events of the stream on
standard output (or the file of `--output`), and commands on standard input. It is what a program that drives scrios
reads and writes (Jumpstarter is one consumer); a person who only wants to watch the screen wants the
raw output instead (see [cli.md](cli.md)).

```
device --RTP (UDP in the tunnel)--> scrios --stdout: frames--> program
                                      ^---- stdin: keyframe / stop / touch / key / kb ----+
```

Logs go to stderr, never to stdout. With `--format framed` the process keeps a private duplicate of the original stdout
for the frames and points file descriptor 1 at stderr, so nothing else in the process can write into the frame channel.

## Version

The protocol has a version number, **1**. Every `config` event carries it as its last field, `"protocol":1`, and the first
`config` comes before the first access unit, so a program reads the version before it reads any video. The
number changes only when something incompatible changes: the framing, a flag, an existing event or a command's meaning.
A new event, a new field of an event and a new command do not change it, so a program ignores the events and fields it does
not know. (A failure to start writes an `error` event and no `config`: there is no version to read, and nothing to decode.)

## Frames (all integers big-endian)

```
offset 0   u32  len      length of the payload in bytes (at most 16 MiB)
offset 4   u64  pts_us   microseconds since the first access unit, from the RTP timestamp at the stream's
                         clock rate (24 kHz measured; see clock_source below); 0 for control frames
offset 12  u8   flags    0x01 KEY      the access unit has a key picture: HEVC, an IRAP picture (NAL types 16..=21);
                                       H.264, an IDR slice (NAL type 5)
                         0x02 PARAMS   the parameter sets are at the start of the payload: HEVC VPS, SPS and PPS;
                                       H.264 SPS and PPS
                         0x80 CONTROL  the payload is one UTF-8 JSON object, not an access unit
offset 13  ...  payload  an Annex-B access unit (4-byte start codes 00 00 00 01) of one picture, or the JSON
```

`len` counts the payload only (not the 13 header bytes). There is nothing between frames and no end marker: the end of the
stream is the end of the file, after the last control frame.

### Access units

**Every KEY access unit carries the parameter sets (HEVC: VPS/SPS/PPS; H.264: SPS/PPS) in front.** scrios caches the
sets from the stream and prepends them if the device did not repeat them; if the unit starts with an access unit delimiter,
that stays the first NAL unit and the sets follow it. Delta (non-IRAP) access units are plain. There is one access unit per RTP
timestamp (packets are grouped by timestamp; the marker bit ends a unit). A unit that lost a packet is **dropped, not sent**,
and the next picture is asked to be a keyframe. The first access unit is a key unit: a decoder can start at any KEY unit.
All frames, control and access units alike, go through one queue and one writer, so a control frame never lands inside an access unit.

The presentation time continues without a jump when the device's SSRC changes (one frame later than the last unit sent)
and when the clock rate is corrected.

The codec of the units is the one asked for with `--video-codec` (H.264 by default); the `config` event says it
as `codec`. `--format raw` is the same units without the framing and without the events.

## Events (`flags == 0x80`, payload `{"event": ...}`)

- `{"event":"config","codec":"avc1.640033","width":1206,"height":2622,"coded_width":1216,"coded_height":2656,"fps":60,"clock_hz":24000,"clock_source":"default","ssrc":123,"device_ssrc":456,"rotation":0,"point_scale":3.0,"current_orientation":"rot0","native_orientation":"rot0","protocol":1}`
  sent once **before the first access unit**, and again whenever something it says changes: the SPS (the display resized),
  the display's size or orientation (it turned: the SPS does not change then), the clock rate (corrected from the sender
  reports). An unchanged `config` is never repeated.
  - `codec` is the WebCodecs/RFC 6381 string built from the SPS: for H.264 `avc1.PPCCLL` (profile_idc, the constraint byte and
    level_idc in hex; High, level 5.1 is `avc1.640033`); for HEVC
    `hev1.<profile_space+profile_idc>.<reversed general_profile_compatibility_flags hex>.<L or H><level_idc>.<constraint bytes, B0...>`
    (Main, level_idc 150 is `hev1.1.6.L150.B0`). `coded_*` is the SPS picture size.
  - `width`/`height` is the **framebuffer's visible size** (the encoder pads to the right and bottom). It comes from
    `--visible-size`, else from the device's own display info (the primary display's `nativeSize`), else from the SPS
    conformance window, else the SPS size.
  - `rotation` (0, 90, 180, 270; omitted when the display info is unavailable) is how far the framebuffer must be turned
    **clockwise** to be upright: the degrees of `current_orientation` (`rot90` is 90). The upright picture is
    `width`x`height` for 0 and 180, and `height`x`width` for 90 and 270. `point_scale`, `current_orientation` and
    `native_orientation` are the display info's fields as they are.
  - `clock_source` says where `clock_hz` comes from: `negotiated` (the device's answer named it), `measured` (the device's
    sender reports disagreed by more than 10% with the other sources, so the nearer of 24 and 90 kHz was taken; checked once,
    from two sender reports at least a second apart), or `default` (24 kHz assumed).
  - `ssrc` is ours (the one the offer declares and every RTCP packet carries), `device_ssrc` the device's.
  - `protocol` is the version of this document.
- `{"event":"stats","packets":..,"access_units":..,"bytes":..,"lost":..,"dropped_units":..,"keyframes":..,"plis":..,"rtcp":..,"b_slices":..,"edge_downs":..}`
  every `--stats-interval-ms`, counts since start: RTP packets accepted; access units **handed to the output**; their payload
  bytes; RTP packets declared lost (sequence gaps); access units not written (incomplete, withheld until a keyframe, or
  dropped because the output could not keep up); KEY units written; keyframe requests (PLI + FIR) sent; RTCP datagrams
  sent (receiver reports, keyframe requests, and Device Hub feedback when enabled); `b_slices` the H.264 B slices seen
  (always 0 for HEVC); `edge_downs` the touch downs that landed on the very edge of the digitizer (see [internals.md](internals.md#the-touch-mapping)).
- `{"event":"hid","ready":true,"keyboard":true,"mapping":"auto",...}` or `{"event":"hid","ready":false,"message":"..."}`
  once, after the touch and key services were asked for (control is on by default; `--no-control` sends none): `ready` is
  true when touch works; `keyboard` is true once the virtual keyboard was created and `kb` commands can be used, false with the
  reason in `message` otherwise: touch and keys keep working; a `message` may also say that the buttons did not connect. Later
  `{"event":"hid","ok":false,"message":"..."}` for a command that failed (nothing for a command that worked; at most one such
  event a second), and `{"event":"hid","warning":"..."}` for a surprise that is not an error. The mapping is announced again when it changes.
- `{"event":"error","message":"..."}` once, just before a non-zero exit.
- `{"event":"stopped"}` once, last, on a clean stop.

## Commands on stdin (one per line)

Control is on by default: the `touch`, `key` and `kb` commands need it (with `--no-control` they are answered with an
`ok:false` `hid` event).

- `keyframe`: send an RTCP **PLI** (RFC 4585: V=2, FMT=1, PT=206, length 2, sender SSRC = ours, media SSRC = the device's) and a
  **FIR** (RFC 5104: PT=206, FMT=4) to the device's RTCP address (its IP and the RTP source port, through the tunnel's UDP
  socket). They go out in one compound datagram behind a receiver report and SDES. **Rate-limited: at most one every
  `--keyframe-min-interval-ms`**; a request inside the window is coalesced into one sent when the window ends (never sent twice in
  the window). A held request is **cancelled when a keyframe is written**.
- `stop`: stop cleanly: lift a finger still down, `stopMediaStream` to the device, close the tunnel, send the `stopped` event,
  exit 0. **End of stdin is `stop`** (unless `--no-stdin`). SIGTERM and SIGINT are `stop` too, and a stop that arrives while the
  stream is still starting ends the start (and stops whatever was already started on the device). A clean stop can take up to
  about 8 seconds.
- `touch down|move|up <x> <y> <id>`: a finger. `x` and `y` are pixels of the **upright picture**, the picture the viewer sees
  after turning the framebuffer by the `config` event's `rotation`: `width`x`height` of the config for rotation 0 and 180,
  `height`x`width` for 90 and 270. Numbers may be decimals; positions outside the picture are clamped to its edge; values
  beyond +-1,000,000 are rejected. Only finger `0` is handled (other ids are logged and ignored). `down` sends a contact sample at
  the position, `move` another one (only while a finger is down; moves are coalesced: the newest position goes out at most every
  8 ms), `up` the release. A finger still down at `stop`, end of stdin or a failure is lifted where it is. The mapping uses the
  rotation and size of the **latest** `config` at the moment of each touch.
- `key <name> [short|long]`: a hardware button, pressed and released after its hold time; `short` is the default. `home`,
  `volume-up`, `volume-down`, `mute`: 80 ms short, 600 ms long. `lock`, `power`, `sleep` (the same button): 200 ms short, 1200 ms
  long. `siri`: 1200 ms short, 2500 ms long. A long press is one held press, not a repeated one. At most 8 presses wait for
  their release; further ones are refused. `app-switcher` (alias `switcher`) opens the App Switcher, which is the **Home button
  pressed twice** (Home down, 80 ms, up, a 120 ms gap, Home down, 80 ms, up); `short` and `long` are accepted and ignored for it,
  and a second `app-switcher` while one is going on is refused (an `ok:false` `hid` event).
- `kb down <usage>` and `kb up <usage>`: a key of the HID keyboard page, by its usage in hex (two digits, `0x` allowed): `04` is A,
  `28` Enter, `29` Escape, `2a` Backspace, `2b` Tab, `4f`..`52` Right, Left, Down, Up, and `e0`..`e7` the modifiers (Left Control,
  Shift, Alt, GUI, then the right ones). The usages 01 to e7 are accepted. Keys and modifiers **accumulate in the virtual
  keyboard's pressed set, as on a physical keyboard**: each `down` or `up` sends the complete pressed set. A `down` of a key that
  is down already and an `up` of a key that is not down do nothing.
- `kb type <base64>`: type a UTF-8 string (base64, so that a newline or any byte is safe on one line; at most 4096 characters per
  command, so a line can be about 22 KB; the line limit is 64 KiB). Each character goes through the **US layout**: letters, digits,
  the printable ASCII punctuation (Shift where the layout needs it), space, `\n` as Return and `\t` as Tab. A character that is
  not in that table (`\r`, accented letters, emoji) is skipped, and the skipped ones of one command are counted in one
  `{"event":"hid","warning":...}`. A character's key is down for 8 ms, and the next character follows 4 ms after its release. The
  typing runs in the background, so a long string does not hold up `touch`, `key`, `kb down`, a `keyframe` or a `stop`; strings
  sent while one is being typed wait behind it (at most 4096 characters waiting in all).
- **Keyboard safety.** On `stop`, end of stdin or a failure of the stream, typing is aborted and every key is released, and the
  virtual keyboard is taken down. If any `kb` action fails, everything is released at once and an `ok:false` reply says why. Keys
  that are down for 30 seconds without any other keyboard command are released with a warning.
- A `touch`, `key` or `kb` that does not parse (a bad number, an unknown key name, a press that is not short or long, the wrong
  number of words, bad hex or base64) is answered with a `hid` event with `ok:false`. Any other unknown command is logged and
  ignored. Commands that send reports are rate-limited (a burst of 100, then 250 a second; lifts are never refused).

## Start, end and exit status

A stream starts when the process starts: usbmuxd, the CoreDevice tunnel, the display stream, the display info and (with control)
the HID services; the first `config` and the first access unit follow. It ends with `stopped` (and exit status 0) after a
`stop`, or with `error` (and a non-zero exit status) when it cannot go on.

| Exit status | Meaning |
| --- | --- |
| 0 | a clean stop (also: nobody reads the output any more) |
| 1 | the stream failed while running (no RTP for 15 s, no datagram for 30 s, the socket failed, ...) |
| 2 | the stream could not be started, or the options do not go together |
| 101 | an internal error (a panic); the stream is stopped on the device if possible |

Before a non-zero exit the `error` event is written (when the output still works).

**The output too slow.** The queue in front of the writer holds at most 32 access units (16 MiB). When it is full the queued
access units are dropped (control frames are kept), the stream waits for a keyframe and asks for one; `stats` counts them in
`dropped_units`. Memory stays bounded. **The output closed:** the stream is stopped cleanly and the process exits 0.

## Embedding

The same frames and events are what the Rust library yields (`scrios::stream::Stream`, items of access units and
events), and `scrios::sink` writes them in this format; a program written in Rust needs no pipe.
