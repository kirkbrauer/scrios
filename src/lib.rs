//! scrios: the screen of an iOS device as an H.264 or HEVC stream, with touch and keys, over Apple's `CoreDevice`
//! display and HID services (through usbmuxd and the `RemoteXPC` tunnel, with the idevice crate).
//!
//! # Using it
//!
//! * [`stream`] is the entry point: [`stream::Stream::start`] begins a session and yields access units and
//!   events ([`stream::Item`]); [`stream::Input`] sends touch, keys, text, keyframe requests and stop.
//! * [`sink`] writes what a stream yields as a raw Annex-B file or as the framed machine protocol
//!   (`docs/protocol.md`), which is what the `scrios` binary does.
//! * [`list`] lists the devices usbmuxd knows.
//!
//! The `scrios` binary is a thin command line over these. The other modules are public too, so that other programs
//! can use the pieces (depacketizers, SPS parsers, the HID controller, ...); their API is not stable yet (0.x).

pub mod avc;
pub mod codec;
pub mod commands;
pub mod depack;
pub mod device;
pub mod displayinfo;
pub mod engine;
pub mod frame;
pub mod hid;
pub mod keyframe;
pub mod list;
pub mod offer;
pub mod outqueue;
pub mod probe;
pub mod rtpstats;
pub mod session;
pub mod sink;
pub mod sps;
pub mod stream;
