//! Decoding H.264 access units into the pictures the renderer shows.
//!
//! A [`Decoder`] takes one access unit at a time (what [`Stream::recv`](crate::stream::Stream::recv) yields: Annex-B,
//! the parameter sets in front of every key unit) and gives back the picture it makes, as soon as it exists: no
//! queue, no reordering (the stream has no B-frames; its SPS says so). What it gives back is a [`Picture`]: 8-bit 4:2:0
//! as a luma plane and one interleaved chroma plane (NV12), which is what the renderer uploads whatever decoded it.
//!
//! * `videotoolbox` (macOS only, so not linked to): Apple's `VideoToolbox`, the hardware decoder.
//! * [`software`]: `OpenH264`, Cisco's decoder, built into the binary; it runs anywhere.
//! * `mediafoundation` (Windows only, so not linked to): Media Foundation's H.264 decoder, on the GPU where it can.
//!
//! [`create`] chooses: the platform's decoder where there is one, the software one otherwise, and the software one
//! again if the platform's fails.

use std::fmt;

use crate::codec::CodecKind;
use crate::stream::AccessUnit;

pub mod annexb;
#[cfg(windows)]
pub mod mediafoundation;
pub mod software;
#[cfg(target_os = "macos")]
pub mod videotoolbox;

/// The levels of a plane: studio swing (16..=235 for luma) or the whole 0..=255.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorRange {
    #[default]
    Limited,
    Full,
}

/// The matrix that takes `Y'CbCr` to `R'G'B'`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorMatrix {
    Bt601,
    /// What HD video and the display stream use, and what a stream that does not say is taken to be.
    #[default]
    Bt709,
}

/// One plane of samples: `stride` bytes a row, at least the row's width.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plane {
    pub data: Vec<u8>,
    pub stride: usize,
}

/// A decoded picture: 8-bit 4:2:0, a luma plane and an interleaved Cb/Cr plane at half the width and height (rounded up).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picture {
    /// The size of the luma plane in samples: the picture as the decoder made it (the stream's padding is still in it:
    /// the renderer crops to the config's visible size).
    pub width: u32,
    pub height: u32,
    pub y: Plane,
    pub uv: Plane,
    pub range: ColorRange,
    pub matrix: ColorMatrix,
}

impl Picture {
    /// A picture of the given size with every sample 0 (what a test builds on).
    #[must_use]
    pub fn blank(width: u32, height: u32) -> Picture {
        let (w, h) = (width as usize, height as usize);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        Picture {
            width,
            height,
            y: Plane { data: vec![0; w * h], stride: w },
            uv: Plane { data: vec![0; cw * 2 * ch], stride: cw * 2 },
            range: ColorRange::Limited,
            matrix: ColorMatrix::Bt709,
        }
    }

    /// The luma sample at `(x, y)`, if it is inside the picture.
    #[must_use]
    pub fn luma(&self, x: u32, y: u32) -> Option<u8> {
        if x >= self.width || y >= self.height {
            return None;
        }
        self.y.data.get(y as usize * self.y.stride + x as usize).copied()
    }

    /// The chroma samples `(Cb, Cr)` that cover the luma sample at `(x, y)`.
    #[must_use]
    pub fn chroma(&self, x: u32, y: u32) -> Option<(u8, u8)> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let at = (y / 2) as usize * self.uv.stride + (x / 2) as usize * 2;
        Some((*self.uv.data.get(at)?, *self.uv.data.get(at + 1)?))
    }
}

/// Why a decoder gave no picture for an access unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// This unit could not be decoded (or does not follow the ones before it). The decoder is still usable, and wants
    /// a key unit before it makes pictures again: the caller asks the device for one.
    Unit(String),
    /// The decoder cannot go on at all (the session broke, the hardware is gone).
    Fatal(String),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Unit(m) | DecodeError::Fatal(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for DecodeError {}

/// A decoder of H.264 access units.
pub trait Decoder: Send {
    /// What the decoder is called in the log and the statistics.
    fn name(&self) -> &'static str;

    /// Decodes one access unit and returns its picture, or `None` when the unit makes none (the parameter sets alone,
    /// or a delta unit before the first key unit).
    ///
    /// # Errors
    ///
    /// [`DecodeError::Unit`] for a unit that cannot be decoded, [`DecodeError::Fatal`] when the decoder is broken.
    fn decode(&mut self, unit: &AccessUnit) -> Result<Option<Picture>, DecodeError>;
}

/// Which decoder to use (`--decoder`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DecoderChoice {
    /// The platform's decoder where there is one (`VideoToolbox` on macOS, Media Foundation on Windows), the software
    /// decoder otherwise or if the platform's fails.
    #[default]
    Auto,
    /// The platform's decoder, and no other: it is an error if there is none.
    Platform,
    /// `OpenH264`, in software.
    Software,
}

/// Whether this build has a decoder of the platform's own.
#[must_use]
pub fn has_platform_decoder() -> bool {
    cfg!(any(target_os = "macos", windows))
}

/// The platform's decoder.
fn platform() -> Result<Box<dyn Decoder>, String> {
    #[cfg(target_os = "macos")]
    {
        videotoolbox::VideoToolbox::new().map(|d| Box::new(d) as Box<dyn Decoder>)
    }
    #[cfg(windows)]
    {
        mediafoundation::MediaFoundation::new().map(|d| Box::new(d) as Box<dyn Decoder>)
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        Err("this platform has no hardware decoder in scrios".to_owned())
    }
}

/// The decoder for `choice`, for a stream of `codec`.
///
/// # Errors
///
/// The codec is not H.264 (the viewer decodes nothing else yet), or the decoder asked for cannot be had.
pub fn create(choice: DecoderChoice, codec: CodecKind) -> Result<Box<dyn Decoder>, String> {
    if codec != CodecKind::H264 {
        return Err("the viewer decodes H.264 only: use the default --video-codec, or `scrios stream` for HEVC".into());
    }
    match choice {
        DecoderChoice::Software => software::Software::new().map(|d| Box::new(d) as Box<dyn Decoder>),
        DecoderChoice::Platform => platform(),
        DecoderChoice::Auto => match platform() {
            Ok(primary) => Ok(Box::new(Fallback::new(primary))),
            Err(why) => {
                tracing::info!("no platform decoder ({why}): decoding in software");
                software::Software::new().map(|d| Box::new(d) as Box<dyn Decoder>)
            }
        },
    }
}

/// A platform decoder that gives way to the software decoder if it breaks: one bad session does not end the view.
pub struct Fallback {
    primary: Option<Box<dyn Decoder>>,
    software: Option<software::Software>,
}

impl fmt::Debug for Fallback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Fallback").field("decoder", &self.name()).finish_non_exhaustive()
    }
}

impl Fallback {
    #[must_use]
    pub fn new(primary: Box<dyn Decoder>) -> Fallback {
        Fallback { primary: Some(primary), software: None }
    }
}

impl Decoder for Fallback {
    fn name(&self) -> &'static str {
        match (&self.primary, &self.software) {
            (Some(p), _) => p.name(),
            (None, Some(s)) => s.name(),
            (None, None) => "none",
        }
    }

    fn decode(&mut self, unit: &AccessUnit) -> Result<Option<Picture>, DecodeError> {
        let Some(primary) = self.primary.as_mut() else {
            return match self.software.as_mut() {
                Some(software) => software.decode(unit),
                None => Err(DecodeError::Fatal("no decoder".into())),
            };
        };
        match primary.decode(unit) {
            Err(DecodeError::Fatal(why)) => {
                tracing::warn!(
                    "the {} decoder failed ({why}): decoding in software from the next key unit",
                    primary.name()
                );
                self.primary = None;
                match software::Software::new() {
                    Ok(software) => {
                        self.software = Some(software);
                        // The software decoder starts at a key unit: this one may be it.
                        self.decode(unit)
                    }
                    Err(why) => Err(DecodeError::Fatal(why)),
                }
            }
            other => other,
        }
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! The fixture the decoder tests share, and what they expect of it.

    use super::*;
    use crate::frame::{Frame, decode_all};

    /// The recorded stream: four access units of a 64x96 picture of four flat quadrants, in the framed format of
    /// `docs/protocol.md` (a key unit with its parameter sets, then three delta units that change the bottom right
    /// quadrant). Made by [`regenerate`] with the software encoder; it holds no content of anyone's.
    pub const FIXTURE: &[u8] = include_bytes!("../testdata/quadrants-64x96.framed");
    pub const FIXTURE_SIZE: (u32, u32) = (64, 96);

    /// The access units of the fixture.
    pub fn fixture_units() -> Vec<AccessUnit> {
        decode_all(FIXTURE)
            .expect("the fixture is framed")
            .into_iter()
            .filter(|f| !f.is_control())
            .map(|f| AccessUnit {
                codec: CodecKind::H264,
                pts_us: f.pts_us,
                key: f.is_key(),
                params: f.flags & crate::frame::FLAG_PARAMS != 0,
                data: f.payload,
            })
            .collect()
    }

    /// The studio-swing BT.709 `Y'CbCr` of a quadrant, as the fixture was made from.
    pub const RED: (u8, u8, u8) = (63, 102, 240);
    pub const GREEN: (u8, u8, u8) = (173, 42, 26);
    pub const BLUE: (u8, u8, u8) = (32, 240, 118);
    pub const GRAY: (u8, u8, u8) = (126, 128, 128);
    pub const WHITE: (u8, u8, u8) = (235, 128, 128);
    pub const BLACK: (u8, u8, u8) = (16, 128, 128);

    /// The bottom right quadrant's colour in each access unit of the fixture.
    pub const BOTTOM_RIGHT: [(u8, u8, u8); 4] = [GRAY, WHITE, BLACK, GRAY];

    /// The colour of a quadrant of the fixture's picture `n`.
    pub fn quadrant(n: usize, left: bool, top: bool) -> (u8, u8, u8) {
        match (left, top) {
            (true, true) => RED,
            (false, true) => GREEN,
            (true, false) => BLUE,
            (false, false) => BOTTOM_RIGHT[n],
        }
    }

    /// Whether `a` is within `tolerance` of `b` in each of Y, Cb and Cr.
    pub fn near(a: (u8, u8, u8), b: (u8, u8, u8), tolerance: u8) -> bool {
        a.0.abs_diff(b.0) <= tolerance && a.1.abs_diff(b.1) <= tolerance && a.2.abs_diff(b.2) <= tolerance
    }

    /// The `Y'CbCr` of the picture at the middle of a quadrant.
    pub fn sample(p: &Picture, left: bool, top: bool) -> (u8, u8, u8) {
        let x = if left { p.width / 4 } else { p.width * 3 / 4 };
        let y = if top { p.height / 4 } else { p.height * 3 / 4 };
        let (cb, cr) = p.chroma(x, y).expect("inside");
        (p.luma(x, y).expect("inside"), cb, cr)
    }

    /// An I420 buffer of the fixture's picture `n`: four flat quadrants.
    pub fn quadrants_i420(n: usize) -> Vec<u8> {
        let (width, height) = (FIXTURE_SIZE.0 as usize, FIXTURE_SIZE.1 as usize);
        let (cw, ch) = (width / 2, height / 2);
        let mut buf = vec![0u8; width * height + 2 * cw * ch];
        let (luma, chroma) = buf.split_at_mut(width * height);
        let (cb, cr) = chroma.split_at_mut(cw * ch);
        for row in 0..height {
            for col in 0..width {
                luma[row * width + col] = quadrant(n, col < cw, row < ch).0;
            }
        }
        for row in 0..ch {
            for col in 0..cw {
                let (_, blue_diff, red_diff) = quadrant(n, col < cw / 2, row < ch / 2);
                cb[row * cw + col] = blue_diff;
                cr[row * cw + col] = red_diff;
            }
        }
        buf
    }

    /// Writes the fixture: `cargo test --all-features regenerate_the_decoder_fixture -- --ignored`. The four
    /// pictures are encoded by `OpenH264` (a key unit with its parameter sets, then three delta units, BT.709 studio swing
    /// in the VUI).
    #[test]
    #[ignore = "writes src/viewer/testdata/quadrants-64x96.framed: run it only to remake the fixture"]
    fn regenerate_the_decoder_fixture() {
        use openh264::OpenH264API;
        use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate, RateControlMode, VuiConfig};
        use openh264::formats::YUVBuffer;

        let (w, h) = (FIXTURE_SIZE.0 as usize, FIXTURE_SIZE.1 as usize);
        let config = EncoderConfig::new()
            .bitrate(BitRate::from_bps(4_000_000))
            .max_frame_rate(FrameRate::from_hz(60.0))
            .rate_control_mode(RateControlMode::Quality)
            .skip_frames(false)
            .vui(VuiConfig::bt709());
        let mut encoder = Encoder::with_api_config(OpenH264API::from_source(), config).expect("encoder");
        let mut out = Vec::new();
        for n in 0..4 {
            let yuv = YUVBuffer::from_vec(quadrants_i420(n), w, h);
            if n == 3 {
                encoder.force_intra_frame();
            }
            let data = encoder.encode(&yuv).expect("encode").to_vec();
            let key = n == 0 || n == 3;
            Frame::unit(n as u64 * 16_666, key, key, data).write_to(&mut out).expect("frame");
        }
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/viewer/testdata/quadrants-64x96.framed");
        std::fs::write(&path, &out).expect("write");
        eprintln!("wrote {} ({} bytes)", path.display(), out.len());
    }
}
