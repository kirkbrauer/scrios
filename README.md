# jumpstarter-ios-display

A small Linux/macOS binary that streams an iPhone's or iPad's screen as **H.264** (or, with `--codec hevc`, HEVC) to its standard output, and
(optionally) takes touch and hardware-key commands on its standard input.

It connects to the device through `usbmuxd` (using the pair record usbmuxd already holds), opens the
iOS 17+ CoreDevice tunnel, starts the display stream of `com.apple.coredevice.displayservice` (RTP/HEVC
over UDP inside the tunnel), depacketizes it (RFC 6184 for H.264, RFC 7798 for HEVC: single NAL unit, aggregation, fragmentation)
into **Annex-B access units**, and writes one length-framed record per picture to stdout. A driver
reads the frames and fans them out to viewers; commands come back on stdin. With `--hid` the same process
and the same tunnel also carry the touch and button services (one process against the device).

The tunnel and the CoreDevice services come from the MIT-licensed [`idevice`](https://github.com/jkcoxson/idevice)
crate (pinned by commit in `Cargo.toml`). Everything else (depacketizer, SPS parsing, RTCP behaviour,
rate limiting, framing, display info, touch mapping) lives in this crate.

```
device --RTP/H.264 (UDP in the tunnel)--> jumpstarter-ios-display --stdout: frames--> driver
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
  --codec <h264|hevc>              the codec to ask for (default h264; hevc is the opt-in: the stock offer; see H.264)
  --probe-secs <SECS>              with the h264 codec (the default): count what arrives for this long, report, stop, exit (default off)
  --probe-raw <PATH>               with the probe: write every datagram received to this file (see Codec probe)
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
offset 12  u8   flags    0x01 KEY      the access unit has a key picture: HEVC, an IRAP picture (NAL types 16..=21);
                                       H.264, an IDR slice (NAL type 5)
                         0x02 PARAMS   the parameter sets are at the start of the payload: HEVC VPS, SPS and PPS;
                                       H.264 SPS and PPS
                         0x80 CONTROL  the payload is one UTF-8 JSON object, not an access unit
offset 13  ...  payload  an Annex-B access unit (4-byte start codes 00 00 00 01) of one picture, or the JSON
```

**Every KEY access unit carries the parameter sets (HEVC: VPS/SPS/PPS; H.264: SPS/PPS) in front** (the sidecar caches the sets from the stream and
prepends them if the device did not repeat them; if the unit starts with an access unit delimiter, that
stays the first NAL unit and the sets follow it). Delta (non-IRAP) access units are plain. One access
unit per RTP timestamp (packets are grouped by timestamp; the marker bit ends a unit). A unit that lost
a packet is **dropped, not sent**, and the next picture is asked to be a keyframe, rate-limited as below.
All frames, control and access units alike, go through one queue and one writer thread, so a control frame
never lands inside an access unit.

The presentation time continues without a jump when the device's SSRC changes (one frame later than the last
unit sent) and when the clock rate is corrected.

### Control frames (`flags == 0x80`, payload `{"event": ...}`)

- `{"event":"config","codec":"avc1.640033","width":1206,"height":2622,"coded_width":1216,"coded_height":2656,"fps":60,"clock_hz":24000,"clock_source":"default","ssrc":123,"device_ssrc":456,"rotation":0,"point_scale":3.0,"current_orientation":"rot0","native_orientation":"rot0"}`
  sent once **before the first access unit**, and again whenever something it says changes: the SPS (the
  display resized), the display's size or orientation (it turned: the SPS does not change then), the
  clock rate (corrected from the sender reports). An unchanged `config` is never repeated.
  - `codec` is the WebCodecs/RFC 6381 string built from the SPS: for H.264 (the default) `avc1.PPCCLL` (profile_idc, the constraint byte and
    level_idc in hex; High, level 5.1 is `avc1.640033`); for HEVC
    `hev1.<profile_space+profile_idc>.<reversed general_profile_compatibility_flags hex>.<L or H><level_idc>.<constraint bytes, B0...>`
    (Main, level_idc 150 is `hev1.1.6.L150.B0`). `coded_*` is the SPS picture size.
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
- `{"event":"stats","packets":..,"access_units":..,"bytes":..,"lost":..,"dropped_units":..,"keyframes":..,"plis":..,"rtcp":..,"b_slices":..}`
  every `--stats-interval-ms`, counts since start: RTP packets accepted; access units **written to stdout**; their
  payload bytes; RTP packets declared lost (sequence gaps); access units not written (incomplete,
  withheld until a keyframe, or dropped because stdout could not keep up); KEY units written; keyframe
  requests (PLI + FIR) sent; RTCP datagrams sent (receiver reports, keyframe requests, and Device Hub
  feedback when enabled); `b_slices` the H.264 B slices seen (always 0 for HEVC; see "The SPS gets a bitstream restriction").
- `{"event":"hid","ready":true,"keyboard":true,"mapping":"auto",...}` or `{"event":"hid","ready":false,"message":"..."}` once, after `--hid` asked
  for the HID services (`ready` is true when touch works; `keyboard` is true once the virtual keyboard was created and `kb` commands can be used, false with
  the reason in `message` otherwise: touch and keys keep working; a `message` may also say that the buttons did not connect); later `{"event":"hid","ok":false,"message":"..."}` for a command that failed (nothing for a
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
- `kb down <usage>` and `kb up <usage>` (with `--hid`): a key of the HID keyboard page, by its usage in hex (two digits, `0x` allowed): `04` is A,
  `28` Enter, `29` Escape, `2a` Backspace, `2b` Tab, `4f`..`52` Right, Left, Down, Up, and `e0`..`e7` the modifiers (Left Control, Shift, Alt, GUI, then
  the right ones). The usages 01 to e7 are accepted. Keys and modifiers **accumulate in the virtual keyboard's pressed set, as on a physical keyboard**: each
  `down` or `up` sends the complete pressed set. A `down` of a key that is down already (a browser repeats `keydown` while a key is held) and an `up` of a key that is
  not down do nothing.
- `kb type <base64>` (with `--hid`): type a UTF-8 string (base64, so that a newline or any byte is safe on one line; at most 4096 characters per
  command, so a line can be about 22 KB; the line limit is 64 KiB). Each character goes through the **US layout**: letters, digits, the printable ASCII
  punctuation (Shift where the layout needs it), space, `\n` as Return and `\t` as Tab. A character that is not in that table (`\r`, accented letters, emoji)
  is skipped, and the skipped ones of one command are counted in one `{"event":"hid","warning":...}`. A character's key is down for 8 ms, and the next
  character follows 4 ms after its release. The typing runs in the background of the stream loop (a schedule of timed steps, never a loop that waits), so
  a long string does not hold up `touch`, `key`, `kb down`, a `keyframe` or a `stop`; strings sent while one is being typed wait behind it (at most 4096
  characters waiting in all). A Shift that the viewer holds down is not let go by the typing.
- **Keyboard safety.** On `stop`, end of stdin or a failure of the stream, typing is aborted and every key is released (an empty report, then the device's
  reset), and the virtual keyboard is taken down. If any `kb` action fails, everything is released at once and an `ok:false` reply says why. Keys that are
  down for 30 seconds without any other keyboard command (the repeat of a held key counts as one) are released with a warning, so a stuck key cannot outlive
  the viewer. A `kb` line that does not parse (bad hex, a usage outside 01..e7, bad base64, text that is not UTF-8, too long) is answered with an `ok:false` `hid`
  event like the other malformed commands.
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

### The SPS gets a bitstream restriction (H.264)

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
jumpstarter-ios-display --probe-secs 8 [--udid <UDID>] [--usbmuxd <PATH|HOST:PORT>]
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
