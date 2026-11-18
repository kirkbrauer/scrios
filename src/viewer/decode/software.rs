//! The software decoder: Cisco's `OpenH264`, built into the binary from its source (BSD-2-Clause), so that it runs on
//! every host the viewer does. It is the fallback of the platform's decoder and `--decoder software`.
//!
//! `OpenH264` decodes a unit and gives its picture at once (the decoder is run with no output delay); the picture is
//! copied out as NV12, because `OpenH264` reuses its buffers for the next unit.

use openh264::decoder::{DecodedYUV, Decoder as OpenH264, DecoderConfig};
use openh264::formats::YUVSource;

use super::{ColorMatrix, ColorRange, DecodeError, Decoder, Picture, Plane};
use crate::stream::AccessUnit;

/// Where the `OpenH264` code comes from. This is the one place that decides it, and the only line of this module that
/// would change to take Cisco's own binary instead of the built-in source.
///
/// Today `OpenH264` is compiled into scrios from the source in the `openh264-sys2` crate (the `source` feature of the
/// `openh264` dependency, on by default). Cisco's licence for the H.264 patents covers only the binaries Cisco builds and
/// distributes, not a build from source. To use Cisco's binary: in `Cargo.toml` give the `openh264` dependency
/// `default-features = false, features = ["libloading"]` (no C++ compiler is then needed to build scrios), and return
/// `OpenH264API::from_blob_path(path)` here, with the path of the `libopenh264` that was downloaded from Cisco (the crate
/// checks it against the checksums of Cisco's releases); `Software::new` and everything else stay as they are. NOTICE
/// says "built from source" and would change with it.
#[expect(
    clippy::unnecessary_wraps,
    reason = "loading Cisco's binary can fail: the signature is the one that switch needs"
)]
fn api() -> Result<openh264::OpenH264API, String> {
    Ok(openh264::OpenH264API::from_source())
}

/// `OpenH264`, in software.
pub struct Software {
    decoder: OpenH264,
    /// A key unit was seen since the start or the last error: only then do delta units mean anything.
    started: bool,
}

impl std::fmt::Debug for Software {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Software").field("started", &self.started).finish_non_exhaustive()
    }
}

impl Software {
    /// # Errors
    ///
    /// The decoder could not be created.
    pub fn new() -> Result<Software, String> {
        let decoder = OpenH264::with_api_config(api()?, DecoderConfig::new())
            .map_err(|e| format!("cannot create the OpenH264 decoder: {e}"))?;
        Ok(Software { decoder, started: false })
    }
}

impl Decoder for Software {
    fn name(&self) -> &'static str {
        "OpenH264 (software)"
    }

    fn decode(&mut self, unit: &AccessUnit) -> Result<Option<Picture>, DecodeError> {
        if !self.started {
            if !unit.key {
                return Ok(None);
            }
            self.started = true;
        }
        match self.decoder.decode(&unit.data) {
            Ok(Some(yuv)) => Ok(Some(copy_out(&yuv))),
            Ok(None) => Ok(None),
            Err(e) => {
                // Whatever came out of this is not to be built on: wait for the next key unit.
                self.started = false;
                Err(DecodeError::Unit(format!("OpenH264 could not decode the unit: {e}")))
            }
        }
    }
}

/// A row-by-row copy of a decoded picture's three planes into NV12, without the decoder's row padding.
fn copy_out(yuv: &DecodedYUV<'_>) -> Picture {
    let (width, height) = yuv.dimensions();
    let (y_stride, u_stride, v_stride) = yuv.strides();
    let (chroma_w, chroma_h) = (width.div_ceil(2), height.div_ceil(2));

    let mut y = Vec::with_capacity(width * height);
    for row in 0..height {
        y.extend_from_slice(rows(yuv.y(), row, y_stride, width));
    }
    let mut uv = Vec::with_capacity(chroma_w * 2 * chroma_h);
    for row in 0..chroma_h {
        let u = rows(yuv.u(), row, u_stride, chroma_w);
        let v = rows(yuv.v(), row, v_stride, chroma_w);
        for (cb, cr) in u.iter().zip(v) {
            uv.push(*cb);
            uv.push(*cr);
        }
    }
    Picture {
        width: u32::try_from(width).unwrap_or(u32::MAX),
        height: u32::try_from(height).unwrap_or(u32::MAX),
        y: Plane { data: y, stride: width },
        uv: Plane { data: uv, stride: chroma_w * 2 },
        // The stream does not say here; this is what the display stream is (the fixture says it in its VUI).
        range: ColorRange::Limited,
        matrix: ColorMatrix::Bt709,
    }
}

/// `width` samples of row `row` of a plane with `stride` bytes a row (fewer if the plane is short: never a panic).
fn rows(plane: &[u8], row: usize, stride: usize, width: usize) -> &[u8] {
    let start = row.saturating_mul(stride);
    plane.get(start..start.saturating_add(width)).unwrap_or(&[])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::viewer::decode::testing::*;

    #[test]
    fn decodes_the_recorded_stream_picture_by_picture() {
        let units = fixture_units();
        assert_eq!(units.len(), 4, "the fixture has four access units");
        assert!(units[0].key && units[0].params && !units[1].key && !units[2].key && units[3].key);
        let mut decoder = Software::new().expect("decoder");
        for (n, unit) in units.iter().enumerate() {
            let p = decoder.decode(unit).expect("decodes").unwrap_or_else(|| panic!("no picture for unit {n}"));
            assert_eq!((p.width, p.height), FIXTURE_SIZE, "unit {n}");
            assert_eq!(p.y.stride, 64);
            assert_eq!(p.uv.stride, 64);
            assert_eq!(p.y.data.len(), 64 * 96);
            assert_eq!(p.uv.data.len(), 64 * 48);
            assert_eq!((p.range, p.matrix), (ColorRange::Limited, ColorMatrix::Bt709));
            for (left, top) in [(true, true), (false, true), (true, false), (false, false)] {
                let got = sample(&p, left, top);
                let want = quadrant(n, left, top);
                assert!(near(got, want, 4), "unit {n} quadrant left={left} top={top}: got {got:?}, want {want:?}");
            }
        }
    }

    #[test]
    fn a_delta_unit_before_any_key_unit_makes_no_picture_and_no_error() {
        let units = fixture_units();
        let mut decoder = Software::new().expect("decoder");
        assert_eq!(decoder.decode(&units[1]), Ok(None));
        assert_eq!(decoder.decode(&units[2]), Ok(None));
        // The key unit then starts it.
        assert!(decoder.decode(&units[0]).expect("decodes").is_some());
    }

    #[test]
    fn a_new_key_unit_restarts_the_stream() {
        let units = fixture_units();
        let mut decoder = Software::new().expect("decoder");
        for unit in &units[..3] {
            assert!(decoder.decode(unit).expect("decodes").is_some());
        }
        // The fourth unit is a key unit again (as after a lost packet): the picture is the first one's.
        let p = decoder.decode(&units[3]).expect("decodes").expect("a picture");
        assert!(near(sample(&p, false, false), GRAY, 4));
    }

    #[test]
    fn garbage_is_an_error_of_the_unit_and_the_decoder_goes_on_at_the_next_key_unit() {
        let units = fixture_units();
        let mut decoder = Software::new().expect("decoder");
        assert!(decoder.decode(&units[0]).expect("decodes").is_some());
        let junk =
            AccessUnit { data: vec![0, 0, 0, 1, 0x65, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF], key: true, ..units[1].clone() };
        // OpenH264 either rejects it or conceals it; if it rejects it the decoder waits for a key unit, and either
        // way a good key unit afterwards decodes.
        match decoder.decode(&junk) {
            Ok(_) | Err(DecodeError::Unit(_)) => {}
            Err(DecodeError::Fatal(why)) => panic!("not fatal: {why}"),
        }
        let p = decoder.decode(&units[3]).expect("decodes").expect("a picture");
        assert!(near(sample(&p, true, true), RED, 4));
    }

    #[test]
    fn a_picture_that_is_not_a_whole_number_of_macroblocks_is_cropped_to_its_size() {
        use openh264::OpenH264API;
        use openh264::encoder::{Encoder, EncoderConfig};
        use openh264::formats::YUVBuffer;
        // The device's picture is 1206x2622 in a 1216x2656 frame; this is the same shape, small: 70x50 in 80x64.
        let (w, h) = (70usize, 50usize);
        let mut encoder = Encoder::with_api_config(OpenH264API::from_source(), EncoderConfig::new().skip_frames(false))
            .expect("encoder");
        let mut buf = vec![128u8; w * h + 2 * (w / 2) * (h / 2)];
        for v in buf.iter_mut().take(w * h) {
            *v = 90;
        }
        let bits = encoder.encode(&YUVBuffer::from_vec(buf, w, h)).expect("encodes").to_vec();
        let unit = AccessUnit { codec: crate::codec::CodecKind::H264, pts_us: 0, key: true, params: true, data: bits };
        let mut decoder = Software::new().expect("decoder");
        let p = decoder.decode(&unit).expect("decodes").expect("a picture");
        assert_eq!((p.width, p.height), (70, 50));
        assert_eq!(p.y.stride, 70);
        assert_eq!(p.uv.stride, 70);
        assert_eq!(p.uv.data.len(), 70 * 25);
        assert!(p.luma(69, 49).is_some() && p.luma(70, 0).is_none() && p.luma(0, 50).is_none());
    }
}
