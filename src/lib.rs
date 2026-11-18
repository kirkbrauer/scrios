//! scrios: the screen of an iOS device as an H.264 or HEVC stream, with touch and keys, over Apple's CoreDevice
//! display and HID services (through usbmuxd and the RemoteXPC tunnel, with the idevice crate).
//!
//! The `scrios` binary is the main consumer of this library. The modules are public so that other programs can embed
//! the pipeline; their API is not stable yet (0.x).

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
pub mod offer;
pub mod outqueue;
pub mod probe;
pub mod rtpstats;
pub mod session;
pub mod sps;
