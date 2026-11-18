//! The macOS hardware decoder: Apple's `VideoToolbox`, through the `objc2-video-toolbox`, `objc2-core-media` and
//! `objc2-core-video` bindings.
//!
//! One `VTDecompressionSession` per set of parameter sets: a key unit whose SPS and PPS differ from the session's
//! (the display turned or resized) makes a new one. A unit goes in as a `CMSampleBuffer` of its slices, length-prefixed
//! (the parameter sets are in the session's format description), and is decoded **synchronously**: the output callback
//! has run when the call returns, so the picture is handed back at once and nothing queues. The picture comes out as
//! NV12 pixel buffer, whose two planes are copied into a [`Picture`].
//!
//! A session that fails is dropped and the next key unit makes a new one. A decoder that cannot be had at all, or that
//! makes a format other than 8-bit 4:2:0 NV12, is [`DecodeError::Fatal`]: the caller's [`Fallback`](super::Fallback)
//! then decodes in software.

// Every `unsafe` here is a call into CoreFoundation, CoreMedia, CoreVideo or VideoToolbox (the objc2 bindings declare
// them unsafe), each with its safety argument next to it. Nothing else in the viewer is unsafe.
#![allow(unsafe_code)]

use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::{self, NonNull};
use std::sync::{Arc, Mutex, PoisonError};

use objc2_core_foundation::CFRetained;
use objc2_core_media::{
    CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMTime, CMVideoFormatDescriptionCreateFromH264ParameterSets,
    kCMBlockBufferAssureMemoryNowFlag,
};
use objc2_core_video::{
    CVImageBuffer, CVPixelBuffer, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
    CVPixelBufferGetHeightOfPlane, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidthOfPlane,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    kCVImageBufferYCbCrMatrix_ITU_R_601_4, kCVImageBufferYCbCrMatrixKey,
    kCVPixelFormatType_420YpCbCr8BiPlanarFullRange, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
};
use objc2_video_toolbox::{
    VTDecodeFrameFlags, VTDecodeInfoFlags, VTDecompressionOutputCallbackRecord, VTDecompressionSession,
};

use super::annexb::{avcc_slices, parameter_sets};
use super::{ColorMatrix, ColorRange, DecodeError, Decoder, Picture, Plane};
use crate::stream::AccessUnit;

/// `OSStatus`.
type Status = i32;

/// What the output callback leaves for the call that is decoding: a picture or why there is none.
type Slot = Mutex<Option<Result<Picture, String>>>;

/// The decompression session of one pair of parameter sets.
struct Session {
    vt: CFRetained<VTDecompressionSession>,
    format: CFRetained<CMFormatDescription>,
    /// What the output callback writes to; the session's refcon points here, so it outlives the session.
    slot: Arc<Slot>,
    sps: Vec<u8>,
    pps: Vec<u8>,
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: the session is ours and is not used again; invalidating it first stops its callbacks, which point at
        // `slot`, before `slot` can go.
        unsafe { self.vt.invalidate() };
    }
}

/// `VideoToolbox`.
pub struct VideoToolbox {
    session: Option<Session>,
}

impl std::fmt::Debug for VideoToolbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoToolbox").field("session", &self.session.is_some()).finish()
    }
}

// SAFETY: the session and the format description are CoreFoundation objects that are used only through this struct, by
// one thread at a time (the decoder is moved to the thread that decodes and stays there); VideoToolbox sessions may be
// used from any thread as long as the calls do not overlap.
unsafe impl Send for VideoToolbox {}

impl VideoToolbox {
    /// A decoder with no session yet: the first key unit makes it. `VideoToolbox` is part of macOS, so this does not
    /// fail; a Mac that cannot decode shows it at the first unit, as [`DecodeError::Fatal`].
    ///
    /// # Errors
    ///
    /// Never, today.
    pub fn new() -> Result<VideoToolbox, String> {
        Ok(VideoToolbox { session: None })
    }
}

impl Decoder for VideoToolbox {
    fn name(&self) -> &'static str {
        "VideoToolbox"
    }

    fn decode(&mut self, unit: &AccessUnit) -> Result<Option<Picture>, DecodeError> {
        if let Some((sps, pps)) = parameter_sets(&unit.data) {
            let current = self.session.as_ref().is_some_and(|s| s.sps == sps && s.pps == pps);
            if !current {
                // A new picture size, or the first unit: a new session (the old one is invalidated as it drops).
                self.session = None;
                self.session = Some(Session::open(sps, pps).map_err(DecodeError::Fatal)?);
            }
        }
        let Some(session) = self.session.as_ref() else {
            // A delta unit before the first key unit.
            return Ok(None);
        };
        match session.decode(&unit.data) {
            Ok(picture) => Ok(picture),
            Err(Failure::Unit(why)) => {
                // Start over at the next key unit: this session may be broken.
                self.session = None;
                Err(DecodeError::Unit(why))
            }
            Err(Failure::Fatal(why)) => {
                self.session = None;
                Err(DecodeError::Fatal(why))
            }
        }
    }
}

/// Why one unit failed.
enum Failure {
    /// This unit: the decoder was fine but could not use it.
    Unit(String),
    /// The decoder: it made something other than NV12.
    Fatal(String),
}

impl Session {
    fn open(sps: &[u8], pps: &[u8]) -> Result<Session, String> {
        let mut pointers = [NonNull::from(sps).cast::<u8>(), NonNull::from(pps).cast::<u8>()];
        let mut sizes = [sps.len(), pps.len()];
        let mut format: *const CMFormatDescription = ptr::null();
        // SAFETY: the pointers and sizes describe two live slices and the out pointer is a live local.
        let status = unsafe {
            CMVideoFormatDescriptionCreateFromH264ParameterSets(
                None,
                2,
                NonNull::from(&mut pointers).cast(),
                NonNull::from(&mut sizes).cast(),
                4,
                NonNull::from(&mut format),
            )
        };
        let format = NonNull::new(format.cast_mut())
            .filter(|_| status == 0)
            // SAFETY: on success the function returns a format description with a retain count of 1, which we own.
            .map(|f| unsafe { CFRetained::from_raw(f) })
            .ok_or_else(|| format!("VideoToolbox does not accept the stream's parameter sets (status {status})"))?;

        let slot: Arc<Slot> = Arc::new(Mutex::new(None));
        let record = VTDecompressionOutputCallbackRecord {
            decompressionOutputCallback: Some(on_picture),
            decompressionOutputRefCon: Arc::as_ptr(&slot).cast_mut().cast::<c_void>(),
        };
        let mut session: *mut VTDecompressionSession = ptr::null_mut();
        // SAFETY: the format description is live, the callback record is read during the call, and the refcon it holds
        // is kept alive by `Session::slot` for as long as the session lives.
        let status = unsafe {
            VTDecompressionSession::create(None, &format, None, None, &raw const record, NonNull::from(&mut session))
        };
        let session = NonNull::new(session)
            .filter(|_| status == 0)
            // SAFETY: on success the function returns a session with a retain count of 1, which we own.
            .map(|s| unsafe { CFRetained::from_raw(s) })
            .ok_or_else(|| format!("VideoToolbox cannot make a decoder for the stream (status {status})"))?;
        Ok(Session { vt: session, format, slot, sps: sps.to_vec(), pps: pps.to_vec() })
    }

    /// Decodes one access unit. The callback has run, with the picture, when this returns.
    fn decode(&self, unit: &[u8]) -> Result<Option<Picture>, Failure> {
        let avcc = avcc_slices(unit);
        if avcc.is_empty() {
            return Ok(None);
        }
        let sample = self.sample(&avcc)?;
        *self.slot.lock().unwrap_or_else(PoisonError::into_inner) = None;
        let mut info = VTDecodeInfoFlags::empty();
        // SAFETY: the session and the sample buffer are live; no frame refcon is used; `info` is a live local. With no
        // asynchronous flag, the output callback has run before this returns.
        let status =
            unsafe { self.vt.decode_frame(&sample, VTDecodeFrameFlags::empty(), ptr::null_mut(), &raw mut info) };
        if status != 0 {
            return Err(Failure::Unit(format!("VideoToolbox could not decode the unit (status {status})")));
        }
        // SAFETY: the session is live. A decoder that did go asynchronous is waited for: nothing is left behind.
        let _ = unsafe { self.vt.wait_for_asynchronous_frames() };
        match self.slot.lock().unwrap_or_else(PoisonError::into_inner).take() {
            Some(Ok(picture)) => Ok(Some(picture)),
            Some(Err(why)) if why.starts_with(FORMAT_PREFIX) => Err(Failure::Fatal(why)),
            Some(Err(why)) => Err(Failure::Unit(why)),
            // A frame the decoder dropped.
            None => Ok(None),
        }
    }

    /// A `CMSampleBuffer` of the one frame in `avcc`.
    fn sample(&self, avcc: &[u8]) -> Result<CFRetained<CMSampleBuffer>, Failure> {
        let fail = |what: &str, status: Status| {
            Failure::Unit(format!("cannot build the sample for VideoToolbox: {what} (status {status})"))
        };
        let mut block: *mut CMBlockBuffer = ptr::null_mut();
        // SAFETY: a null memory block with the "assure memory now" flag asks CoreMedia to allocate `avcc.len()` bytes;
        // the out pointer is a live local.
        let status = unsafe {
            CMBlockBuffer::create_with_memory_block(
                None,
                ptr::null_mut(),
                avcc.len(),
                None,
                ptr::null(),
                0,
                avcc.len(),
                kCMBlockBufferAssureMemoryNowFlag,
                NonNull::from(&mut block),
            )
        };
        let block = NonNull::new(block)
            .filter(|_| status == 0)
            // SAFETY: on success the function returns a block buffer with a retain count of 1, which we own.
            .map(|b| unsafe { CFRetained::from_raw(b) })
            .ok_or_else(|| fail("block buffer", status))?;
        // SAFETY: the source is `avcc.len()` live bytes and the block buffer was made that long.
        let status = unsafe { CMBlockBuffer::replace_data_bytes(NonNull::from(avcc).cast(), &block, 0, avcc.len()) };
        if status != 0 {
            return Err(fail("copying the data", status));
        }
        let mut sample: *mut CMSampleBuffer = ptr::null_mut();
        let size = avcc.len();
        // SAFETY: one sample of `size` bytes in the block buffer, with no timing (the output is not reordered).
        let status = unsafe {
            CMSampleBuffer::create_ready(
                None,
                Some(&block),
                Some(&self.format),
                1,
                0,
                ptr::null(),
                1,
                &raw const size,
                NonNull::from(&mut sample),
            )
        };
        NonNull::new(sample)
            .filter(|_| status == 0)
            // SAFETY: on success the function returns a sample buffer with a retain count of 1, which we own.
            .map(|s| unsafe { CFRetained::from_raw(s) })
            .ok_or_else(|| fail("sample buffer", status))
    }
}

/// What starts the message of a picture that is not NV12.
const FORMAT_PREFIX: &str = "VideoToolbox made a picture in a format other than NV12";

/// The output callback: copies the picture out and leaves it in the slot.
///
/// # Safety
///
/// Called by `VideoToolbox` with the refcon of the session (the pointer to its [`Slot`], which is alive) and an image
/// buffer that is alive for the call.
unsafe extern "C-unwind" fn on_picture(
    refcon: *mut c_void,
    _frame_refcon: *mut c_void,
    status: Status,
    _flags: VTDecodeInfoFlags,
    image: *mut CVImageBuffer,
    _pts: CMTime,
    _duration: CMTime,
) {
    // A panic must not unwind into VideoToolbox.
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: the refcon is `Arc::as_ptr` of the session's slot, which the session keeps alive.
        let Some(slot) = (unsafe { refcon.cast::<Slot>().as_ref() }) else { return };
        // SAFETY: the image buffer, if any, is alive for the duration of the callback.
        let result = match unsafe { image.as_ref() } {
            Some(image) if status == 0 => copy_picture(image),
            _ => Err(format!("VideoToolbox decoded no picture (status {status})")),
        };
        *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(result);
    }));
}

/// Copies an NV12 pixel buffer's two planes into a [`Picture`].
fn copy_picture(buffer: &CVPixelBuffer) -> Result<Picture, String> {
    let format = CVPixelBufferGetPixelFormatType(buffer);
    let range = if format == kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange {
        ColorRange::Limited
    } else if format == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange {
        ColorRange::Full
    } else {
        return Err(format!("{FORMAT_PREFIX} ({format:#010x})"));
    };
    let matrix = matrix_of(buffer);
    let lock = CVPixelBufferLockFlags::ReadOnly;
    // SAFETY: the buffer is live; it is unlocked below on every path.
    let status = unsafe { CVPixelBufferLockBaseAddress(buffer, lock) };
    if status != 0 {
        return Err(format!("cannot lock the picture's memory (status {status})"));
    }
    let planes = (|| {
        let y = copy_plane(buffer, 0, 1)?;
        let uv = copy_plane(buffer, 1, 2)?;
        Ok::<_, String>((y, uv))
    })();
    // SAFETY: the buffer was locked above.
    let _ = unsafe { CVPixelBufferUnlockBaseAddress(buffer, lock) };
    let (y, uv) = planes?;
    let width =
        u32::try_from(CVPixelBufferGetWidthOfPlane(buffer, 0)).map_err(|_| "the picture is too wide".to_owned())?;
    let height =
        u32::try_from(CVPixelBufferGetHeightOfPlane(buffer, 0)).map_err(|_| "the picture is too tall".to_owned())?;
    Ok(Picture { width, height, y, uv, range, matrix })
}

/// A plane of a locked pixel buffer, without the row padding; `bytes` is the size of one sample of it.
fn copy_plane(buffer: &CVPixelBuffer, index: usize, bytes: usize) -> Result<Plane, String> {
    let base = CVPixelBufferGetBaseAddressOfPlane(buffer, index).cast::<u8>().cast_const();
    let (width, height) = (CVPixelBufferGetWidthOfPlane(buffer, index), CVPixelBufferGetHeightOfPlane(buffer, index));
    let stride = CVPixelBufferGetBytesPerRowOfPlane(buffer, index);
    let row = width * bytes;
    if base.is_null() || stride < row || height == 0 {
        return Err(format!("the picture's plane {index} is not readable"));
    }
    // SAFETY: the buffer is locked, so `base` addresses `height` rows of `stride` bytes (the last row may be short of
    // its padding, hence `row` for it).
    let all = unsafe { std::slice::from_raw_parts(base, stride * (height - 1) + row) };
    let mut data = Vec::with_capacity(row * height);
    for r in 0..height {
        data.extend_from_slice(
            all.get(r * stride..r * stride + row).ok_or("a row of the picture is out of its plane")?,
        );
    }
    Ok(Plane { data, stride: row })
}

/// BT.601 if the buffer says so, BT.709 otherwise (what HD video, and so the display stream, is).
fn matrix_of(buffer: &CVPixelBuffer) -> ColorMatrix {
    // SAFETY: the buffer is live and no attachment mode is asked for; the two keys are constants of CoreVideo.
    let is_601 = unsafe {
        buffer
            .attachment(kCVImageBufferYCbCrMatrixKey, ptr::null_mut())
            .is_some_and(|value| *value == *kCVImageBufferYCbCrMatrix_ITU_R_601_4.as_ref())
    };
    if is_601 { ColorMatrix::Bt601 } else { ColorMatrix::Bt709 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::viewer::decode::software::Software;
    use crate::viewer::decode::testing::*;

    #[test]
    fn decodes_the_recorded_stream_picture_by_picture() {
        let units = fixture_units();
        let mut decoder = VideoToolbox::new().expect("decoder");
        for (n, unit) in units.iter().enumerate() {
            let p = decoder.decode(unit).expect("decodes").unwrap_or_else(|| panic!("no picture for unit {n}"));
            assert_eq!((p.width, p.height), FIXTURE_SIZE, "unit {n}");
            assert_eq!(p.y.data.len(), 64 * 96);
            assert_eq!(p.uv.data.len(), 64 * 48);
            for (left, top) in [(true, true), (false, true), (true, false), (false, false)] {
                let got = sample(&p, left, top);
                let want = quadrant(n, left, top);
                assert!(near(got, want, 4), "unit {n} quadrant left={left} top={top}: got {got:?}, want {want:?}");
            }
        }
    }

    #[test]
    fn agrees_with_the_software_decoder() {
        let units = fixture_units();
        let mut hardware = VideoToolbox::new().expect("decoder");
        let mut software = Software::new().expect("decoder");
        for (n, unit) in units.iter().enumerate() {
            let a = hardware.decode(unit).expect("decodes").expect("a picture");
            let b = software.decode(unit).expect("decodes").expect("a picture");
            assert_eq!((a.width, a.height), (b.width, b.height));
            let worst = a.y.data.iter().zip(&b.y.data).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0);
            assert!(worst <= 3, "unit {n}: the luma differs by {worst}");
            let worst = a.uv.data.iter().zip(&b.uv.data).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0);
            assert!(worst <= 3, "unit {n}: the chroma differs by {worst}");
        }
    }

    #[test]
    fn a_delta_unit_before_any_key_unit_makes_no_picture() {
        let units = fixture_units();
        let mut decoder = VideoToolbox::new().expect("decoder");
        assert_eq!(decoder.decode(&units[1]), Ok(None));
        assert!(decoder.decode(&units[0]).expect("decodes").is_some());
    }

    #[test]
    fn a_new_key_unit_with_the_same_parameter_sets_keeps_the_session() {
        let units = fixture_units();
        let mut decoder = VideoToolbox::new().expect("decoder");
        for unit in &units {
            assert!(decoder.decode(unit).expect("decodes").is_some());
        }
        assert!(decoder.session.is_some());
    }

    #[test]
    fn parameter_sets_that_are_not_h264_are_refused_as_fatal_not_as_a_crash() {
        let units = fixture_units();
        let mut decoder = VideoToolbox::new().expect("decoder");
        let junk = AccessUnit {
            data: [&[0, 0, 0, 1, 0x67, 1, 2, 3][..], &[0, 0, 0, 1, 0x68, 4], &[0, 0, 0, 1, 0x65, 5, 6]].concat(),
            ..units[0].clone()
        };
        assert!(matches!(decoder.decode(&junk), Err(DecodeError::Fatal(_))));
        // And the decoder is usable again with the real stream.
        assert!(decoder.decode(&units[0]).expect("decodes").is_some());
    }

    #[test]
    fn the_real_ipad_parameter_sets_make_a_session() {
        // The SPS and PPS of an iPad's display stream (High profile, level 5.1, 2400x1680): the format description and the
        // session accept what the device sends.
        let sps =
            [0x27, 0x64, 0x00, 0x33, 0xAC, 0x13, 0x14, 0x50, 0x09, 0x60, 0x34, 0xE9, 0xB8, 0x10, 0x10, 0x10, 0x10];
        let pps = [0x28, 0xEE, 0x3C, 0xB0];
        assert!(Session::open(&sps, &pps).is_ok());
    }
}
