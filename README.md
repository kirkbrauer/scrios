# jumpstarter-ios-display

A small Linux/macOS binary that streams an iPhone's or iPad's screen as **HEVC** to its standard output, and
(optionally) takes touch and hardware-key commands on its standard input.

It connects to the device through `usbmuxd` (using the pair record usbmuxd already holds), opens the
iOS 17+ CoreDevice tunnel, starts the display stream of `com.apple.coredevice.displayservice` (RTP/HEVC
over UDP inside the tunnel), depacketizes it (RFC 7798: single NAL unit, aggregation, fragmentation)
into **Annex-B access units**, and writes one length-framed record per picture to stdout. A driver
reads the frames and fans them out to viewers; commands come back on stdin. With `--hid` the same process
and the same tunnel also carry the touch and button services (one process against the device).

The tunnel and the CoreDevice services come from the MIT-licensed [`idevice`](https://github.com/jkcoxson/idevice)
crate (pinned by commit in `Cargo.toml`). Everything else (depacketizer, SPS parsing, RTCP behaviour,
rate limiting, framing, display info, touch mapping) lives in this crate.

```
device --RTP/HEVC (UDP in the tunnel)--> jumpstarter-ios-display --stdout: frames--> driver
                                                  ^---- stdin: keyframe / stop / touch / key ----+
```

## Usage

```
jumpstarter-ios-display [OPTIONS]

  --udid <UDID>                    the device (default: the only device usbmuxd lists)
  --usbmuxd <PATH|HOST:PORT>       usbmuxd address: a unix socket path or host:port
                                   (default: $USBMUXD_SOCKET_ADDRESS, else /var/run/usbmuxd)
  --display-id <N>                 display to stream (default 1, the built-in display)
  --max-bitrate <BPS>              a ceiling for the bitrate; the display offer has no such field,
                                   so it is logged and ignored
  --visible-size <WxH>             override of the picture's visible size, e.g. 1206x2622
  --rtcp-interval-ms <MS>          receiver report interval (default 5000, 0 = never)
  --keyframe-min-interval-ms <MS>  never send two keyframe requests closer than this (default 2000)
  --stats-interval-ms <MS>         stats event interval (default 5000, 0 = never)
  --rtp-stall-keyframe-ms <MS>     no RTP for this long: ask for a keyframe (default 5000, 0 = never)
  --rtp-stall-timeout-ms <MS>      no RTP for this long: fail with an error (default 15000, 0 = never)
  --display-poll-ms <MS>           ask the device for its display's size and orientation this often
                                   (default 1000; 0 = only at start and when the SPS changes)
  --hid                            accept touch and key commands (connects the HID services)
  --hid-space <auto|panel|ui>      where the digitizer's axes lie (default auto, see The touch mapping)
  --hid-flip-x / --hid-flip-y / --hid-swap-xy   mirror or exchange the axes on top of the chosen space
  --rtcp-fb                        offer allowRTCPFB = 1 (opt-in, see Offer)
  --no-ltr                         offer without long-term reference pictures (opt-in, see Offer)
  --device-hub-feedback            also send Device Hub's per-frame acks and RCTL reports (experimental)
  --log-answer                     log the device's answer and media support info (diagnostics)
  -h, --help, -V, --version
```

Millisecond options above a day are clamped to a day.

**Logs go to stderr only** (`RUST_LOG` selects the level; the default is `info`; the touch mapping of every
touch is logged at `debug`). At start-up the process points file descriptor 1 at stderr and keeps a private
duplicate of the original stdout for the frames, so nothing else in the process can write into the frame channel.

Exit status: `0` after a clean stop, `2` if the stream could not be started, `1` if it failed while
running, `101` after an internal error. Before a non-zero exit the `error` event is written (when
stdout still works).

**A clean stop can take up to about 8 seconds**: `stopMediaStream` has 5 seconds to answer, and the writer
has 3 seconds to flush its last frames to a reader that may not be reading.

## stdout: frames (all integers big-endian)

```
offset 0   u32  len      length of the payload in bytes (at most 16 MiB)
offset 4   u64  pts_us   microseconds since the first access unit, from the RTP timestamp at the stream's
                         clock rate (24 kHz measured; see clock_source below); 0 for control frames
offset 12  u8   flags    0x01 KEY      the access unit has an IRAP picture (NAL types 16..=21)
                         0x02 PARAMS   VPS, SPS and PPS are at the start of the payload
                         0x80 CONTROL  the payload is one UTF-8 JSON object, not an access unit
offset 13  ...  payload  an Annex-B access unit (4-byte start codes 00 00 00 01) of one picture, or the JSON
```

**Every KEY access unit carries VPS/SPS/PPS in front** (the sidecar caches the sets from the stream and
prepends them if the device did not repeat them; if the unit starts with an access unit delimiter, that
stays the first NAL unit and the sets follow it). Delta (non-IRAP) access units are plain. One access
unit per RTP timestamp (packets are grouped by timestamp; the marker bit ends a unit). A unit that lost
a packet is **dropped, not sent**, and the next picture is asked to be a keyframe, rate-limited as below.
All frames, control and access units alike, go through one queue and one writer thread, so a control frame
never lands inside an access unit.

The presentation time continues without a jump when the device's SSRC changes (one frame later than the last
unit sent) and when the clock rate is corrected.

### Control frames (`flags == 0x80`, payload `{"event": ...}`)

- `{"event":"config","codec":"hev1.1.6.L150.B0","width":1206,"height":2622,"coded_width":1216,"coded_height":2656,"fps":60,"clock_hz":24000,"clock_source":"default","ssrc":123,"device_ssrc":456,"rotation":0,"point_scale":3.0,"current_orientation":"rot0","native_orientation":"rot0"}`
  sent once **before the first access unit**, and again whenever something it says changes: the SPS (the
  display resized), the display's size or orientation (it turned: the SPS does not change then), the
  clock rate (corrected from the sender reports). An unchanged `config` is never repeated.
  - `codec` is the WebCodecs/RFC 6381 string built from the SPS
    (`hev1.<profile_space+profile_idc>.<reversed general_profile_compatibility_flags hex>.<L or H><level_idc>.<constraint bytes, B0...>`;
    Main, level_idc 150 is `hev1.1.6.L150.B0`). `coded_*` is the SPS picture size.
  - `width`/`height` is the **framebuffer's visible size** (the encoder pads to the right and bottom). It comes
    from `--visible-size`, else from the device's own display info (the primary display's `nativeSize`),
    else from the SPS conformance window, else the SPS size.
  - `rotation` (0, 90, 180, 270; omitted when the display info is unavailable) is how far the framebuffer
    must be turned **clockwise** to be upright: the degrees of `current_orientation` (`rot90` is 90). The
    upright picture is `width`x`height` for 0 and 180, and `height`x`width` for 90 and 270. (The integrator checked
    on an iPad that `rot90` means 90 degrees clockwise; the native orientation does not enter into it.)
    `point_scale`, `current_orientation` and `native_orientation` are the display info's fields as they are.
  - `clock_source` says where `clock_hz` comes from: `negotiated` (the device's answer named it),
    `measured` (the device's sender reports disagreed by more than 10% with the other sources, so the
    nearer of 24 and 90 kHz was taken; checked once, from two sender reports at least a second apart), or
    `default` (24 kHz assumed).
  - `ssrc` is ours (the one the offer declares and every RTCP packet carries), `device_ssrc` the device's.
- `{"event":"stats","packets":..,"access_units":..,"bytes":..,"lost":..,"dropped_units":..,"keyframes":..,"plis":..,"rtcp":..}`
  every `--stats-interval-ms`, counts since start: RTP packets accepted; access units **written to stdout**; their
  payload bytes; RTP packets declared lost (sequence gaps); access units not written (incomplete,
  withheld until a keyframe, or dropped because stdout could not keep up); KEY units written; keyframe
  requests (PLI + FIR) sent; RTCP datagrams sent (receiver reports, keyframe requests, and Device Hub
  feedback when enabled).
- `{"event":"hid","ready":true}` or `{"event":"hid","ready":false,"message":"..."}` once, after `--hid` asked
  for the HID services (`ready` is true when touch works; a `message` may say that the buttons did not
  connect); later `{"event":"hid","ok":false,"message":"..."}` for a command that failed (nothing for a
  command that worked; at most one such event a second, with a count of those held back).
- `{"event":"error","message":"..."}` once, just before a non-zero exit.
- `{"event":"stopped"}` once, last, on a clean stop.

## stdin: commands (one per line)

- `keyframe`: send an RTCP **PLI** (RFC 4585: V=2, FMT=1, PT=206, length 2, sender SSRC = ours, media SSRC =
  the device's) and a **FIR** (RFC 5104: PT=206, FMT=4) to the device's RTCP address (its IP and the RTP
  source port, through the tunnel's UDP socket). They go out in one compound datagram behind a receiver
  report and SDES. **Rate-limited: at most one every `--keyframe-min-interval-ms`**; a request inside the
  window is coalesced into one sent when the window ends (never sent twice in the window). A held request
  is **cancelled when a keyframe is written** (no second request after an IDR already arrived).
- `stop`: stop cleanly: lift a finger still down, `stopMediaStream` to the device, close the tunnel, send the
  `stopped` event, exit 0. **End of stdin is `stop`.** SIGTERM and SIGINT are `stop` too, and a stop that arrives
  while the stream is still starting ends the start (and stops whatever was already started on the device).
- `touch down|move|up <x> <y> <id>` (with `--hid`): a finger. `x` and `y` are pixels of the **upright picture**, the
  picture the viewer sees after turning the framebuffer by the `config` event's `rotation`: `width`x`height` of the
  config for rotation 0 and 180, `height`x`width` for 90 and 270. Numbers may be decimals; positions outside the
  picture are clamped to its edge; values beyond +-1,000,000 are rejected. Only finger `0` is handled (other ids are logged
  and ignored). `down` sends a contact sample at the position, `move` another one (only while a finger is down;
  moves are coalesced: the newest position goes out at most every 8 ms), `up` the release. A finger still down at `stop`,
  end of stdin or a failure is lifted where it is. The mapping uses the rotation and size of the **latest** `config`
  at the moment of each touch.
- `key <name> [short|long]` (with `--hid`): a hardware button, pressed and released after its hold time; `short` is the
  default. `home`, `volume-up`, `volume-down`, `mute`: 80 ms short, 600 ms long. `lock`, `power`, `sleep` (the same button):
  200 ms short, 1200 ms long. `siri`: 1200 ms short, 2500 ms long. The short times are the reference tool's; a long press is
  one held press, not a repeated one. At most 8 presses wait for their release; further ones are refused.
- A `touch` or `key` that does not parse (a bad number, an unknown key name, a press that is not short or long, the wrong number of words) is answered
  with a `hid` event with `ok:false`. Any other unknown command is logged and ignored. Commands that send reports are
  rate-limited (a burst of 100, then 250 a second; lifts are never refused).

### The touch mapping

Three steps: the upright picture position is turned back by the config's `rotation` to a position in the **framebuffer**
(normalized to 0..1 on both axes: `(fx, fy)`); the framebuffer point is turned into the **digitizer's** point; the
overrides are applied. The first step is `map_touch(x, y, upright_w, upright_h, rotation)`: for a landscape framebuffer turned
90 degrees (a portrait picture) the upright top-left corner is the framebuffer's bottom-left, the upright top-right is its
top-left, and so on.

The second step is the default, `--hid-space auto`. The model: the digitizer's axes are the **panel's native axes**, and the
display stream's framebuffer is the display raster in `nativeSize`. So the framebuffer point is rotated clockwise by
`(360 - native) % 360`, where `native` is the primary display's `nativeOrientation` from the display info:

| `nativeOrientation` | digitizer point |
| --- | --- |
| `rot0` | `(fx, fy)` |
| `rot270` (the iPad) | `(1 - fy, fx)` (swap the axes, then mirror x) |
| `rot180` | `(1 - fx, 1 - fy)` |
| `rot90` | `(fy, 1 - fx)` (swap the axes, then mirror y) |

**Verified on one device only: an iPad14,3 on iOS 27.0.1** (native `rot270`, `rotation` 0: this is what landed, as
`--hid-space panel --hid-swap-xy --hid-flip-x`). Other devices are unverified. Without display info (the service could not be
used, or its answer has not arrived yet) auto falls back to the identity and says so (`"mapping":"identity (no display info)"`);
when the display info arrives later, the next touch announces the new mapping.

`--hid-space panel` is the framebuffer's own axes (no native-orientation step); `--hid-space ui` uses the upright picture's axes
as they are (no rotation undone either). `--hid-swap-xy`, `--hid-flip-x` and `--hid-flip-y` are applied **after** the chosen
space, including after the automatic mapping (swap first, then x, then y), so they work as overrides on top of auto. The `hid`
`ready` event carries the effective mapping, with the automatic mapping and the overrides combined:
`{"event":"hid","ready":true,"mapping":"auto","swap_xy":true,"flip_x":true,"flip_y":false}` (`mapping` is `auto`,
`identity (no display info)`, `panel` or `ui`). Every mapped touch is logged at debug level.

At `--hid` start the device's list of HID surfaces (`list_connected_services`: id, product, primary usage) is logged at INFO. The
listing carries no orientation or size (the crate reads only those three fields), so the mapping cannot be checked against it;
a device that lists no touchscreen surface (id 257, the one touch reports go to) gets a `{"event":"hid","warning":"..."}`.

How the report goes out: a `mainTouchscreen` report (id `0x09`, 58 bytes) with the contact state `0xC2` (a finger on the
screen: both `down` and `move`) or `0x02` (the lift), unsigned 16-bit little-endian x and y, and the wall clock as the 48-bit
timestamp. HID events are one-way messages (no reply), so only transport failures are ever reported. The device drops
HID events unless a display stream is open: the stream this process holds is that gate.

## Offer

The offer is **idevice's stock screen-sharing offer** (the one the system's own client sends), with our SSRC in it.
That offer, with a PLI sent behind receiver reports, was answered with an IDR in about 50 ms on a real iPad.
Two settings of its video settings message can be changed for comparison on a device, each by an opt-in flag:
`--rtcp-fb` sets `allowRTCPFB` (field 2) to 1, `--no-ltr` sets `ltrpEnabled` (field 7) to 0 and removes `LTR;` from the
HEVC feature string. Everything else in the blob is left byte for byte as the builder wrote it (a test decodes the offer
and checks the fields, and that the default is identical to the builder's).

## RTCP

The device's stream config has `RTCPTimeoutInterval` 20 s: a receiver report is due at least that often
or the encoder may stall. Every `--rtcp-interval-ms` the sidecar sends an **RR + SDES (CNAME)** compound
packet with **our SSRC** (the one the offer declared) and one report block about the device's SSRC (taken
from the answer, confirmed by the first RTP packet) carrying the real loss (cumulative and per interval),
the extended highest sequence number, the interarrival jitter and LSR/DLSR from the device's last sender
report. A keyframe request carries the same report, so it also restarts the report timer. Each datagram
must go out within 500 ms, or it is given up on (a stalled tunnel cannot hold a stop).

`--device-hub-feedback` (experimental, default off) additionally sends what Device Hub sends: an `APP` packet
acknowledging each picture (its RTP timestamp) and an `RCTL` report every 50 ms (pictures received, milliseconds
since start, highest sequence number). It is there to be compared on a device.

## Display info (size and orientation)

At start the process asks the CoreDevice `deviceinfo` service (`getdisplayinfo`, read-only) for the displays and
takes the primary one's `nativeSize`, `pointScale` and orientations. The stream does not wait for it for more than
3 seconds: a slow answer arrives later and makes a second `config`. The query repeats every `--display-poll-ms`
(1 s by default), and at once when the SPS changes, because the orientation changes while streaming without the SPS
changing. Any change of the size, rotation, scale or orientations makes one new `config`; an unchanged answer makes
none. If the service cannot be used, the size comes from `--visible-size`, else the SPS, and the reason is logged.

## What it does when things go wrong

- **A lost packet** (a gap in the RTP sequence numbers) drops the unit it falls in, and the unit after it
  if the gap sits at a unit boundary (its head may be missing). The stream then **withholds delta units
  until the next keyframe** (a delta without its reference is no use, and the driver's cache of the last
  keyframe would otherwise get a hole), and a keyframe is requested through the rate limit. The request
  repeats with every withheld unit until a keyframe arrives, never faster than the limit. Parameter sets are
  cached only from units that arrived whole.
- **A source that jumps** (three packets in a row, each following the last, far behind the sequence cursor: it
  restarted with a new sequence base) resyncs the depacketizer, as after a loss.
- **stdout too slow:** the writer queue holds at most 32 access units (16 MiB). When it is full the queued
  access units are dropped (control frames are kept), the stream waits for a keyframe and asks for one.
  Memory stays bounded.
- **No RTP** (sender reports and junk do not count) for `--rtp-stall-keyframe-ms` (5 s): a keyframe is requested
  (rate-limited, repeating); for `--rtp-stall-timeout-ms` (15 s): `error`, exit 1. **No datagram at all** for 30 s: `error`, exit 1.
- **stdout closed:** the stream is stopped cleanly and the process exits 0.
- A new device SSRC mid-stream starts the stream over (parameter sets forgotten, config sent again).
- Bad packets (too short, truncated aggregation, orphan fragments, forbidden bit, foreign payload types)
  never panic: the unit is dropped or the packet ignored, with a throttled warning.
- A panic still tries `stopMediaStream`; a panic of the stdin reader counts as a stop.
- A failing or stalled HID service never stops the stream; each report has 500 ms.

## What it does not do

- It does **not** pair, write a pair record, mount an image or change any setting of the device.
- It keeps **one** VPS, one SPS and one PPS (the latest of each); streams that switch between several
  parameter-set ids are not supported.
- No reorder buffer: the tunnel preserves order, so a gap is a loss at once and a late packet is ignored.
- Only the base layer, no DONL/DOND (not negotiated), no PACI packets.
- It does not decode, scale, rotate or crop the picture; the size and rotation are only reported.
- Touch is a single finger; no keyboard text, scrolling wheel or gestures beyond what a finger path makes.
- `--max-bitrate` is not applied: the display offer of the `idevice` crate has no bitrate field to set.
- No audio (an audio stream is started because the device needs one to establish the screen-sharing
  session; its packets are discarded).
- Unix hosts only.

## Building and testing

No Rust is needed on the host: see [BUILD.md](BUILD.md) (`./build.sh`, `./test.sh`, a container build).

## License

Apache-2.0 (this crate). `idevice` is MIT.
