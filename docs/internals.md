# How scrios works

Notes for people working on scrios. Users do not need this page.

## The path

```
device --RTP over the CoreDevice tunnel--> scrios --H.264--> window, file or program
       <------------- HID touch and keys ---------------
```

1. scrios finds the device through [usbmuxd](https://github.com/libimobiledevice/usbmuxd) and opens the CoreDevice tunnel. The
   [idevice](https://github.com/jkcoxson/idevice) crate does this.
2. It asks the display service to start a video stream. The device sends RTP packets.
3. scrios reassembles the packets into pictures and passes them on.
4. Touch and keys go back through the HID service. The device only accepts them while a video stream is open.

## Why H.264

The device prefers HEVC. scrios asks for H.264 by leaving HEVC out of its offer, because every browser and player
decodes H.264. `--video-codec h265` sends the standard offer and gets HEVC.

## Parameter sets

The device does not send the H.264 SPS and PPS the usual way. Its first packet holds an MP4 `avc1` box, and scrios
reads the SPS and PPS out of it. scrios then puts them in front of every keyframe, so a decoder can start at any
keyframe.

## The SPS rewrite

scrios adds a `bitstream_restriction` to every H.264 SPS that says pictures are not reordered.

Without it, a browser's hardware decoder waits for several pictures before showing the first one, which adds a few
hundred milliseconds of delay. The device's stream has no B-frames, so the statement is true. If a B-frame ever shows
up, scrios logs a warning and counts it in `stats`.

An SPS that cannot be parsed is passed through unchanged.

## Lost packets

- A picture with a missing packet is dropped.
- scrios then holds back pictures until the next keyframe, and asks the device for one.
- Keyframe requests are limited to one every two seconds.
- If no video arrives for 5 seconds, scrios asks for a keyframe. After 15 seconds it gives up with an error.
- Malformed packets are ignored. They never crash the stream.

## Keeping the stream alive

The device stops sending if it hears nothing for 20 seconds. scrios sends an RTCP receiver report every 5 seconds.

## Screen size and rotation

The video does not change when the device rotates, so scrios asks the device for its display size and orientation
once a second. A change produces a new `config` event, and the window turns to match.

## Touch mapping

Touch commands use pixels in the upright picture. scrios converts them in two steps:

1. Undo the rotation, to get a position in the encoded picture.
2. Convert that to the touch panel's coordinates, which run from 0 to 65535 on each axis.

The first and last pixel map to exactly 0 and 65535. iOS edge gestures only trigger when a touch starts on the very
edge.

The panel's axes depend on how it is mounted in the device. scrios reads this from the device's display info. This
has only been checked on one iPad. If touches land in the wrong place, these options override it: `--hid-space`,
`--hid-swap-xy`, `--hid-flip-x`, `--hid-flip-y`.

## Limits

- One finger. No pinch or other multi-touch gestures.
- No audio. The device needs an audio stream to start screen sharing, so scrios opens one and discards it.
- Text is typed through a US keyboard layout.
- The bit rate, frame rate and size are chosen by the device and cannot be set.
- scrios does not pair with a device or turn on Developer Mode by itself.
