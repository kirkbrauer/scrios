//! The viewer: a window that shows the device's screen and sends it touch and keys, like scrcpy's.
//!
//! The parts so far: the picture's geometry in the window, what the mouse and keyboard mean for the device, and the
//! H.264 decoders.

pub mod decode;
pub mod geometry;
pub mod input;
