# How scrios works

Implementation notes: the touch mapping, the offer, the codecs, RTCP, display info, failure handling and limits. The
command-line interface is in [cli.md](cli.md) and the machine protocols in [protocol.md](protocol.md).

## The touch mapping

Three steps: the upright picture position is turned back by the config's `rotation` to a position in the **framebuffer**
(normalized to 0..1 on both axes: `(fx, fy)`); the framebuffer point is turned into the **digitizer's** point; the
overrides are applied. The first step is `map_touch(x, y, upright_w, upright_h, rotation)`: for a landscape framebuffer turned
90 degrees (a portrait picture) the upright top-left corner is the framebuffer's bottom-left, the upright top-right is its
top-left, and so on.

**The outermost pixels are the digitizer's limits.** A position along an axis of `size` pixels is `p / (size - 1)` of the way, so pixel 0 is
exactly 0.0 and the last pixel, `size - 1`, exactly 1.0: HID 0 and 65535 on that axis once the rotation and the axes are applied (a position at or beyond
the size is 1.0 too, one before the first pixel is 0.0). iPadOS's edge recognisers (the home bar's swipe up, the side swipes) want the first contact on the
very edge; the earlier `p / size` put the last pixel row at 2387/2388, HID 65508, 27 units short. This is a slight rescale of every position: a touch moves by
`p / (size * (size - 1))` of the axis, nothing at pixel 0, about half a pixel (14 units on the iPad's 2388-pixel axis) in the middle and one pixel (27 units)
at the last pixel. A touch `down` whose mapped x or y is 0 or 65535 is logged at INFO (`edge touch down at HID (x, y) from upright pixel (px, py), edge: left|right|top|bottom`;
a corner names both sides, such as `right+bottom`; the side is the picture's, whatever the mapping does to the axes) and counted in the `stats` event as `edge_downs`.

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

The keyboard is a virtual HID service that the sidecar creates on the same universal HID connection (`createService`, fixed service id) and takes down at the end
of the session; it is confirmed by listing the device's HID surfaces. Its reports carry the complete pressed set (a 29-byte bitmap of usages up to 0xE7), so a
`set_main_keyboard_usages` before the first key is not needed (the set starts empty), the crate refuses a `down` of a key that is down and an `up` of one
that is not (which the controller filters out), and `reset_main_keyboard` only sends `resetGestureState` (the sidecar sends the empty report first).

How the report goes out: a `mainTouchscreen` report (id `0x09`, 58 bytes) with the contact state `0xC2` (a finger on the
screen: both `down` and `move`) or `0x02` (the lift), unsigned 16-bit little-endian x and y, and the wall clock as the 48-bit
timestamp. HID events are one-way messages (no reply), so only transport failures are ever reported. The device drops
HID events unless a display stream is open: the stream this process holds is that gate.

## Offer

With `--codec hevc` the offer is **idevice's stock screen-sharing offer** (the one the system's own client sends), with our SSRC in it; by default
(`--codec h264`) it is that offer without the HEVC bank (see below). The stock offer, with a PLI sent behind receiver reports, was answered with an IDR in
about 50 ms on a real iPad, and so was the H.264 one (57 to 80 ms).
Two settings of its video settings message can be changed for comparison on a device, each by an opt-in flag:
`--rtcp-fb` sets `allowRTCPFB` (field 2) to 1, `--no-ltr` sets `ltrpEnabled` (field 7) to 0 and removes `LTR;` from the
HEVC feature string. Everything else in the blob is left byte for byte as the builder wrote it (a test decodes the offer
and checks the fields, and that the default is identical to the builder's).

## H.264 (the default)

**The default codec is `h264`**; `--codec hevc` is the opt-in (the stock offer, HEVC, which a browser can only decode in hardware). The default is a single constant
(`DEFAULT_CODEC` in `src/main.rs`), named by the tests of the default and by this paragraph. H.264 was run on a real iPad (iPad14,3, iOS 27.0.1): the configuration
packet was read (1 SPS and 1 PPS, profile 100, level 51), the codec string was `avc1.640033`, every IDR carried SPS and PPS, a keyframe request was answered by an IDR in 57 to
80 ms, and the stream decoded cleanly in ffmpeg and in Chrome.

`--codec h264` (the default) sends the offer without the HEVC bank (see below) and runs the stream with an H.264 depacketizer (RFC 6184, packetization mode 1:
single NAL units, STAP-A, FU-A; the interleaved types are refused). It is the same machinery as for HEVC, with the same loss rules, the same
keyframe handling and the same frames: `config.codec` is `avc1.PPCCLL` (profile_idc, the constraint byte and level_idc of the SPS, in hex), `width` and
`height` are the cropped picture (the display's size if known) and `coded_width`/`coded_height` the macroblock-aligned size, KEY is an IDR slice,
PARAMS says that SPS and PPS are the first NAL units, and every KEY unit has them in front: from the configuration packet **and** from in-band NAL units
(the latest of each wins; if the stream repeats them, the unit's own copies are the ones used), and prepended from the cache when the stream does not repeat
them. SEI and access unit delimiter NAL units are **kept** in the unit, in order; an access unit delimiter stays first, so in a key unit the sets follow it.
Reserved and extension NAL types (14..=23 and above) are left out. A lost packet inside a fragmented NAL unit (FU-A) drops the unit and asks for a keyframe, as for HEVC.

**Where the SPS and PPS come from.** The iPad's H.264 stream carries no in-band sets; its first packet is of a
proprietary NAL type 18 (its first byte is `0x92`, which has the forbidden bit set): a 4-byte header followed by an MP4 `avc1` box (134 bytes, with a 44-byte `avcC`
child). The
depacketizer reads such a packet as a configuration packet: it finds an `avc1` box (or a bare `avcC` box) in it, reads the `avcC` record
(`AVCDecoderConfigurationRecord`: the SPS list and the PPS list) and caches the first SPS and PPS, which then go in front of every key unit. Sets that
come in-band (a STAP-A or single NAL units) are used as well, and the latest of each wins. The shape of the
packet was read off one capture, not specified, so the reading is lenient and degrades instead of failing: first an `avc1` box with an `avcC` child, then a bare `avcC`
box, then a bare `AVCDecoderConfigurationRecord` at one of the first nine offsets, then SPS and PPS NAL units in Annex-B form. If none of them fits, the packet is ignored, the
sets are taken from the stream's own NAL units, and the log says so once (`--probe-raw` records the real packet). What is read is also said once, at INFO.

## The SPS gets a bitstream restriction (H.264)

**Every H.264 SPS the sidecar writes carries a VUI `bitstream_restriction` with `max_num_reorder_frames = 0` and
`max_dec_frame_buffering = max(max_num_ref_frames, 1)`.** The iPad's SPS has no such restriction, and without it a browser's hardware decoder
(Chrome's) cannot know that pictures come out in the order they went in, so it holds back a full decoded picture buffer before it
shows anything: 11 frames in the measurement, 391 ms of latency against 2 ms with the restriction. The stream itself is the same; only the SPS is
rewritten, once, where NAL units enter the depacketizer, so the SPS inside a key unit and the one cached from the configuration packet (the `avcC` record) are the same bytes.

- The SPS is read in full: profile-dependent chroma and scaling lists, picture order count types 0, 1 and 2, cropping, and the whole VUI (aspect ratio, overscan, video
  signal type, colour description, chroma location, timing, NAL and VCL HRD, low delay, `pic_struct`). Everything is **copied bit for bit** up to the restriction, which is written
  with `motion_vectors_over_pic_boundaries_flag` 1, `max_bytes_per_pic_denom` 0, `max_bits_per_mb_denom` 0, `log2_max_mv_length_horizontal` and `_vertical` 15,
  `max_num_reorder_frames` 0 and `max_dec_frame_buffering` as above. An SPS **without a VUI** gets one with only the restriction. An SPS that **has a restriction already** is
  left as it is. The emulation prevention bytes are taken out to read and put back in to write; the NAL header byte is kept.
  The iPad's `27640033ac131450096034e9b810101010` (17 bytes) becomes `27640033ac131450096034e9b81010103c201096` (20 bytes).
- Nothing else changes: `config.codec` (`avc1.PPCCLL`, from the profile, constraint and level bytes), the picture sizes, the NAL units of the slices.
- **An SPS that cannot be read** (truncated, or not a valid SPS) **goes on exactly as it came**, with one `WARN` in the log for the process; the stream keeps working, only with the
  decoder's own delay.
- **Pictures may come out in the wrong order if the stream has B slices:** `max_num_reorder_frames = 0` promises there is no reordering. The iPad's stream has none (all 2188 access units of one capture were IDR I slices and P slices), and the
  sidecar only watches: a non-IDR slice with `slice_type` 1 or 6 (an IDR picture has only I slices) is counted in `b_slices` of the `stats` event, and the first one logs one `WARN` ("B slices with
  max_num_reorder_frames=0: pictures may be out of order"). The stream is not changed because of it.

## Codec probe (what does the device send for an H.264-only offer?)

The stock offer names two codec banks in its screen media blob: H.264 (RTP payload type 123, features `FLS;SW:1;`) and HEVC
(payload type 100); the device picks HEVC. The H.264 offer takes the HEVC bank out of the offer (the repeated bank whose payload type is
100) and leaves everything else as the builder wrote it, so a device that can do H.264 has to answer with it (an iPad did: payload type 123).
`--probe-secs N` (which needs the h264 codec, the default) turns the run into a measurement instead of a stream.

```
scrios --probe-secs 8 [--udid <UDID>] [--usbmuxd <PATH|HOST:PORT>]
```

The probe does the normal start (and logs the device's answer, the server status, the negotiation blob's payloads and streams, and the
values in them that name a payload type, codec, SSRC, port, frame rate, RTCP timeout, size, profile or level, as `--log-answer` does), then
for N seconds only counts: RTP packets per payload type, the first 12 payload bytes of the first 10 packets, the payload kinds and
the NAL unit types by the H.264 rules of RFC 6184 (type = first byte & 0x1f: 1..23 a single NAL unit, 24 STAP-A, 28 FU-A with the inner
type in the FU header's low 5 bits, counted where the fragment starts; 5 IDR, 7 SPS, 8 PPS), and, for packets of payload type 100, the HEVC
NAL types (to read an HEVC answer). From the first SPS seen whole it logs `profile_idc`, the constraint flags, `level_idc`, the picture size
(cropping applied) and the `avc1.PPCCLL` codec string. Then it stops the stream (`stopMediaStream`) and exits.

Nothing is written to stdout but the closing event: `stopped` and exit 0 if most packets had payload type 123 (or another payload type that
carries H.264 SPS or IDR NAL units); an `error` event and exit 1 if the device answered with HEVC anyway (the message says how many packets of
which payload type), sent nothing, or sent something that is neither. If the device refuses the offer, the start fails like any other
start (exit 2, an `error` event with what the device said). A `stop`, end of stdin or a signal ends the probe early with exit 0. The probe
sends no receiver reports, so keep N well under the device's 20 seconds.

`--probe-raw <PATH>` (with the probe) also writes **every datagram** received during the window, RTP and RTCP, to a file: for each, its length
as 4 bytes big-endian, then its bytes. The probe's log lists every RTP packet whose payload kind (first payload byte & 0x1f) is not 1, 5 or 28
(a non-IDR slice, an IDR slice, an FU-A fragment) with its header fields and the payload in hex, up to 400 bytes (200 such packets at most), so a
configuration packet or an SEI can be read exactly. The file is opened before anything touches the device, so a bad path costs no run.

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
  until the next keyframe** (a delta without its reference is no use, and a consumer's cache of the last
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
- Touch is a single finger (touch id 0; other ids are ignored), so no pinch; no scrolling wheel or gestures beyond
  what a finger path makes. The keyboard takes HID key usages and text typed through the US layout.
- `--max-bitrate` is not applied: the display offer of the `idevice` crate has no bitrate field to set.
- No audio (an audio stream is started because the device needs one to establish the screen-sharing
  session; its packets are discarded).
- Unix hosts only.
