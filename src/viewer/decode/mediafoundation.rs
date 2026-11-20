//! Windows' own decoder: Media Foundation's H.264 decoder transform (`CLSID_MSH264DecoderMFT`), through the `windows`
//! crate's bindings.
//!
//! The transform is given a Direct3D 11 device where there is one, and then decodes on the GPU (DXVA); without one it
//! decodes on the CPU, with Microsoft's own software decoder (several threads, far quicker than `OpenH264`). Either way
//! it is run in low-latency mode (`CODECAPI_AVLowLatencyMode`: a picture comes out for every unit that goes in, nothing
//! is held back for reordering), synchronously: a unit goes in as one Annex-B sample, and what comes out is taken at
//! once. The picture is NV12; its two planes are copied into a [`Picture`] (for a GPU picture, Media Foundation reads
//! the texture back when the buffer is locked), which is what the renderer uploads whatever decoded it.
//!
//! The stream's size is not told beforehand: the transform reads it from the parameter sets, says that its output
//! changed (`MF_E_TRANSFORM_STREAM_CHANGE`), and is given its NV12 output type again. The same happens when the display
//! turns.
//!
//! An error of the transform is [`DecodeError::Fatal`]: the caller's [`Fallback`](super::Fallback) then decodes in
//! software. `SCRIOS_MF_SOFTWARE=1` in the environment keeps the transform off the GPU (to tell a driver's fault from
//! the decoder's).

// Every `unsafe` here is a call into COM, Direct3D 11 or Media Foundation (the `windows` bindings declare them all
// unsafe), each with its safety argument next to it. Nothing else in the viewer is unsafe on Windows.
#![allow(unsafe_code)]

use std::mem::ManuallyDrop;
use std::time::{Duration, Instant};

use tracing::{debug, info};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_QUERY_DESC, D3D11_QUERY_EVENT, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Query, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;
use windows::Win32::Media::MediaFoundation::{
    CLSID_MSH264DecoderMFT, CODECAPI_AVLowLatencyMode, IMF2DBuffer, IMFDXGIBuffer, IMFDXGIDeviceManager,
    IMFMediaBuffer, IMFMediaType, IMFSample, IMFTransform, MF_E_NO_MORE_TYPES, MF_E_NOTACCEPTING,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_SIZE,
    MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_YUV_MATRIX,
    MF_SA_D3D11_AWARE, MF_VERSION, MFCreateDXGIDeviceManager, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
    MFMediaType_Video, MFNominalRange_0_255, MFSTARTUP_LITE, MFShutdown, MFStartup, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFVideoInterlace_MixedInterlaceOrProgressive, MFVideoTransferMatrix_BT601,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CO_MTA_USAGE_COOKIE, CoCreateInstance, CoDecrementMTAUsage, CoIncrementMTAUsage,
};
use windows::core::{BOOL, Interface};

use super::{ColorMatrix, ColorRange, DecodeError, Decoder, Picture, Plane};
use crate::stream::AccessUnit;

/// The environment variable that keeps the transform off the GPU.
const SOFTWARE_ENV: &str = "SCRIOS_MF_SOFTWARE";
/// How often a unit is offered again after the transform said it takes no input and a picture was taken out.
const INPUT_RETRIES: usize = 8;
/// How many times in a row the transform may say its output changed before it is taken to be broken.
const STREAM_CHANGES: usize = 4;
/// How long the GPU may take to finish a picture before it is read anyway.
const GPU_WAIT: Duration = Duration::from_millis(250);

/// COM and Media Foundation, for as long as this lives. It is the last field of [`MediaFoundation`], so that every
/// object of theirs is released before it.
struct Runtime {
    mta: CO_MTA_USAGE_COOKIE,
}

impl Runtime {
    fn start() -> Result<Runtime, String> {
        // Safety: no arguments. This keeps the process's multithreaded apartment alive without tying it to a thread: a
        // thread that has not chosen an apartment is then in it (the decode thread), and the window's thread is still
        // free to choose its own (winit makes it single-threaded for drag and drop, after this ran).
        let mta = unsafe { CoIncrementMTAUsage() }.map_err(|e| format!("cannot start COM: {e}"))?;
        // Safety: plain arguments; balanced by the `MFShutdown` in `drop`.
        if let Err(e) = unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE) } {
            // Safety: the cookie is the one `CoIncrementMTAUsage` just gave.
            let _ = unsafe { CoDecrementMTAUsage(mta) };
            return Err(format!("cannot start Media Foundation: {e}"));
        }
        Ok(Runtime { mta })
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        // Safety: balances the `MFStartup` and the `CoIncrementMTAUsage` of `start`, with its cookie; the objects made
        // in between are gone (see the field order of `MediaFoundation`).
        unsafe {
            let _ = MFShutdown();
            let _ = CoDecrementMTAUsage(self.mta);
        }
    }
}

/// How the output's samples are laid out: what the transform's current output type says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Layout {
    width: u32,
    height: u32,
    /// Bytes a row, of both planes.
    stride: usize,
    range: ColorRange,
    matrix: ColorMatrix,
}

/// The Media Foundation decoder.
pub struct MediaFoundation {
    transform: IMFTransform,
    /// The transform decodes on the GPU: its device, and what reads its pictures back.
    gpu: Option<Gpu>,
    /// The layout of the output, once the output type is set.
    layout: Option<Layout>,
    /// The sample the transform writes to when it does not bring its own (the CPU decoder): made when first needed and
    /// again when the output changes.
    scratch: Option<IMFSample>,
    /// A key unit was seen since the start: only then do delta units mean anything.
    started: bool,
    // Last: see `Runtime`.
    _runtime: Runtime,
}

// Safety: the transform, the device manager and the samples are free-threaded objects (the transform's threading model
// is "Both", the Direct3D device is made multithread-protected, and they are made in the multithreaded apartment), and
// the decoder is used by one thread at a time (`decode` takes `&mut self`).
unsafe impl Send for MediaFoundation {}

impl std::fmt::Debug for MediaFoundation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaFoundation")
            .field("gpu", &self.gpu.is_some())
            .field("layout", &self.layout)
            .field("started", &self.started)
            .finish_non_exhaustive()
    }
}

impl MediaFoundation {
    /// Makes the decoder: on the GPU if that can be had, else on the CPU.
    ///
    /// # Errors
    ///
    /// COM or Media Foundation cannot be started, or this Windows has no H.264 decoder (an "N" edition without the Media
    /// Feature Pack, a Server without the Media Foundation feature).
    pub fn new() -> Result<MediaFoundation, String> {
        let runtime = Runtime::start()?;
        let on_gpu = if std::env::var_os(SOFTWARE_ENV).is_some_and(|v| !v.is_empty() && v != "0") {
            info!("{SOFTWARE_ENV} is set: Media Foundation decodes on the CPU");
            None
        } else {
            match open(true) {
                Ok(opened) => Some(opened),
                Err(why) => {
                    info!("Media Foundation cannot decode on the GPU ({why}): it decodes on the CPU");
                    None
                }
            }
        };
        let (transform, gpu) = match on_gpu {
            Some(opened) => opened,
            None => open(false)?,
        };
        Ok(MediaFoundation { transform, gpu, layout: None, scratch: None, started: false, _runtime: runtime })
    }

    /// Whether the transform decodes on the GPU.
    #[must_use]
    pub fn on_gpu(&self) -> bool {
        self.gpu.is_some()
    }

    /// Gives the transform its NV12 output type (again, after it said the output changed) and notes its layout.
    fn set_output_type(&mut self) -> Result<(), String> {
        self.layout = None;
        self.scratch = None;
        let ty = set_nv12_output(&self.transform)?;
        let layout = layout_of(&ty)?;
        debug!("Media Foundation output: {layout:?}");
        self.layout = Some(layout);
        Ok(())
    }

    /// Offers one unit to the transform. `Ok(false)`: it takes no input until its output is taken.
    fn offer(&self, sample: &IMFSample) -> Result<bool, String> {
        // Safety: `sample` is a live sample; the transform takes its own reference.
        match unsafe { self.transform.ProcessInput(0, sample, 0) } {
            Ok(()) => Ok(true),
            Err(e) if e.code() == MF_E_NOTACCEPTING => Ok(false),
            Err(e) => Err(format!("the decoder refused a unit: {e}")),
        }
    }

    /// Takes one picture out of the transform, if it has one.
    fn take(&mut self) -> Result<Option<Picture>, String> {
        for _ in 0..STREAM_CHANGES {
            // Safety: a call on the live transform.
            let info = unsafe { self.transform.GetOutputStreamInfo(0) }
                .map_err(|e| format!("the decoder does not describe its output: {e}"))?;
            let brings_its_own = info.dwFlags
                & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0).cast_unsigned()
                != 0;
            let ours = if brings_its_own {
                None
            } else {
                if self.scratch.is_none() {
                    self.scratch = Some(memory_sample(info.cbSize, &[])?);
                }
                self.scratch.clone()
            };
            let mut out = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(ours),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            // Safety: one output buffer for the transform's one output stream, and a place for the status; both live
            // through the call.
            let result = unsafe { self.transform.ProcessOutput(0, &mut out, &raw mut status) };
            let [out] = out;
            // What the call left in the buffer is ours to release, whatever it returned.
            let sample = ManuallyDrop::into_inner(out.pSample);
            drop(ManuallyDrop::into_inner(out.pEvents));
            match result {
                Ok(()) => {
                    let sample = sample.ok_or("the decoder gave a picture without a sample")?;
                    let layout = if let Some(layout) = self.layout {
                        layout
                    } else {
                        // Safety: a call on the live transform.
                        let ty = unsafe { self.transform.GetOutputCurrentType(0) }
                            .map_err(|e| format!("the decoder has no output type: {e}"))?;
                        let layout = layout_of(&ty)?;
                        self.layout = Some(layout);
                        layout
                    };
                    let picture = match self.gpu.as_mut() {
                        Some(gpu) => gpu.read(&sample, layout),
                        None => copy_out(&sample, layout),
                    };
                    return picture.map(Some);
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => self.set_output_type()?,
                Err(e) => return Err(format!("the decoder failed: {e}")),
            }
        }
        Err("the decoder keeps changing its output".to_owned())
    }

    fn decode_unit(&mut self, unit: &AccessUnit) -> Result<Option<Picture>, String> {
        // 100 ns units, as Media Foundation counts time.
        let time = i64::try_from(unit.pts_us).unwrap_or(i64::MAX).saturating_mul(10);
        let size = u32::try_from(unit.data.len()).map_err(|_| "the unit is too large".to_owned())?;
        let sample = memory_sample(size, &unit.data)?;
        // Safety: a call on the sample just made.
        unsafe { sample.SetSampleTime(time) }.map_err(|e| format!("cannot stamp the unit: {e}"))?;

        let mut picture = None;
        let mut offered = false;
        for _ in 0..INPUT_RETRIES {
            if self.offer(&sample)? {
                offered = true;
                break;
            }
            // It is full: take a picture out and offer the unit again.
            match self.take()? {
                Some(p) => picture = Some(p),
                None => break,
            }
        }
        if !offered {
            return Err("the decoder takes no input and gives no output".to_owned());
        }
        // Everything it has: in low-latency mode that is this unit's picture (the newest is the one to show).
        while let Some(p) = self.take()? {
            picture = Some(p);
        }
        Ok(picture)
    }
}

impl Decoder for MediaFoundation {
    fn name(&self) -> &'static str {
        if self.gpu.is_some() { "Media Foundation (GPU)" } else { "Media Foundation (CPU)" }
    }

    fn decode(&mut self, unit: &AccessUnit) -> Result<Option<Picture>, DecodeError> {
        if !self.started {
            if !unit.key {
                return Ok(None);
            }
            self.started = true;
        }
        if unit.data.is_empty() {
            return Ok(None);
        }
        self.decode_unit(unit).map_err(DecodeError::Fatal)
    }
}

/// The H.264 decoder transform, set up and streaming: with a Direct3D 11 device of its own if `gpu`.
fn open(gpu: bool) -> Result<(IMFTransform, Option<Gpu>), String> {
    // Safety: the class id is a constant of the bindings, there is no outer object, and the interface asked for is the
    // one the result is typed as.
    let transform: IMFTransform = unsafe { CoCreateInstance(&CLSID_MSH264DecoderMFT, None, CLSCTX_INPROC_SERVER) }
        .map_err(|e| format!("this Windows has no H.264 decoder (Media Foundation): {e}"))?;
    // Safety (all of this block): calls on the live transform and on objects it or Media Foundation just gave; every
    // pointer passed is to a constant of the bindings or to a local that outlives the call.
    unsafe {
        let attributes = transform.GetAttributes().map_err(|e| format!("the decoder has no attributes: {e}"))?;
        let manager = if gpu {
            if attributes.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) == 0 {
                return Err("the decoder does not work with Direct3D 11".to_owned());
            }
            let gpu = Gpu::new()?;
            // The message's parameter is the manager's `IUnknown` pointer, which the transform takes a reference to.
            transform
                .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, gpu.manager.as_raw() as usize)
                .map_err(|e| format!("the decoder does not take the graphics device: {e}"))?;
            Some(gpu)
        } else {
            None
        };
        // A picture out for every unit in. Without it the decoder holds pictures back (it is made for players).
        attributes
            .SetUINT32(&CODECAPI_AVLowLatencyMode, 1)
            .map_err(|e| format!("the decoder has no low-latency mode: {e}"))?;

        // The input: H.264, Annex-B. Its size is in the stream.
        let input = MFCreateMediaType().map_err(|e| format!("cannot describe the stream: {e}"))?;
        input
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .and_then(|()| input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264))
            .and_then(|()| {
                input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_MixedInterlaceOrProgressive.0.cast_unsigned())
            })
            .map_err(|e| format!("cannot describe the stream: {e}"))?;
        transform.SetInputType(0, &input, 0).map_err(|e| format!("the decoder does not take H.264: {e}"))?;
        // The output: NV12, at a size the decoder makes up until the stream says its own.
        set_nv12_output(&transform)?;
        // Both are hints (the decoder may not know them): a decoder that does not start says so at the first unit.
        let _ = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
        let _ = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
        Ok((transform, manager))
    }
}

/// The Direct3D 11 device the transform decodes on, and what reads a decoded picture back from it.
///
/// A decoded picture is one slice of a texture array of the decoder's. It is copied to a staging texture (one the CPU
/// can map), the GPU is waited for (some drivers hand a picture over before it is decoded: read then, it is all zeros,
/// which is green), and the staging texture is mapped and copied out.
struct Gpu {
    /// Kept for as long as the transform uses it.
    manager: IMFDXGIDeviceManager,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    /// The staging texture, and the size it was made for.
    staging: Option<(ID3D11Texture2D, (u32, u32))>,
    /// The query that says the GPU has done everything it was given.
    query: Option<ID3D11Query>,
}

impl Gpu {
    /// A Direct3D 11 device that can decode video, in the device manager the transform takes it from.
    fn new() -> Result<Gpu, String> {
        let mut device: Option<ID3D11Device> = None;
        // Safety: the default adapter, no software rasteriser, the default feature levels; `device` outlives the call and
        // is the only output asked for.
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&raw mut device),
                None,
                None,
            )
        }
        .map_err(|e| format!("no graphics device that decodes video: {e}"))?;
        let device = device.ok_or("no graphics device")?;
        // The decoder uses the device from threads of its own.
        let multithread: ID3D11Multithread =
            device.cast().map_err(|e| format!("the graphics device cannot be shared between threads: {e}"))?;
        // Safety: a call on the live device; what it returns is the setting it had before.
        let _ = unsafe { multithread.SetMultithreadProtected(true) };

        let mut token = 0u32;
        let mut manager: Option<IMFDXGIDeviceManager> = None;
        // Safety: both outputs are locals that outlive the call.
        unsafe { MFCreateDXGIDeviceManager(&raw mut token, &raw mut manager) }
            .map_err(|e| format!("cannot make a device manager: {e}"))?;
        let manager = manager.ok_or("no device manager")?;
        // Safety: the device is live and the token is the one the manager was made with.
        unsafe { manager.ResetDevice(&device, token) }
            .map_err(|e| format!("the device manager refused the device: {e}"))?;
        // Safety: a call on the live device.
        let context =
            unsafe { device.GetImmediateContext() }.map_err(|e| format!("the graphics device has no context: {e}"))?;
        Ok(Gpu { manager, device, context, staging: None, query: None })
    }

    /// Copies the picture of a decoded sample into a [`Picture`].
    fn read(&mut self, sample: &IMFSample, layout: Layout) -> Result<Picture, String> {
        let failed = |e: windows::core::Error| format!("cannot read the picture from the GPU: {e}");
        // Safety: a call on the live sample.
        let buffer = unsafe { sample.GetBufferByIndex(0) }.map_err(failed)?;
        // A picture that is in memory after all (the transform decoded it on the CPU).
        let Ok(dxgi) = buffer.cast::<IMFDXGIBuffer>() else { return copy_out(sample, layout) };
        let mut raw = std::ptr::null_mut();
        // Safety: the interface asked for is the one the pointer is then taken as, and `raw` outlives the call; the
        // reference the call gives is owned by `texture`.
        let texture = unsafe {
            dxgi.GetResource(&ID3D11Texture2D::IID, &raw mut raw).map_err(failed)?;
            ID3D11Texture2D::from_raw(raw)
        };
        // Safety: a call on the live buffer.
        let slice = unsafe { dxgi.GetSubresourceIndex() }.map_err(failed)?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // Safety: `desc` outlives the call.
        unsafe { texture.GetDesc(&raw mut desc) };
        if desc.Format != DXGI_FORMAT_NV12 || desc.Width == 0 || desc.Height == 0 {
            return Err(format!(
                "the GPU's picture is {}x{} of format {}, not NV12",
                desc.Width, desc.Height, desc.Format.0
            ));
        }
        let size = (desc.Width, desc.Height);
        if self.staging.as_ref().is_none_or(|(_, made_for)| *made_for != size) {
            let staging_desc = D3D11_TEXTURE2D_DESC {
                MipLevels: 1,
                ArraySize: 1,
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0.cast_unsigned(),
                MiscFlags: 0,
                ..desc
            };
            let mut staging = None;
            // Safety: the description and the output are locals that outlive the call.
            unsafe { self.device.CreateTexture2D(&raw const staging_desc, None, Some(&raw mut staging)) }
                .map_err(failed)?;
            self.staging = Some((staging.ok_or("no staging texture")?, size));
            debug!(
                "Media Foundation reads back {}x{} pictures (the decoder's textures have {} slices)",
                size.0, size.1, desc.ArraySize
            );
        }
        let Some((staging, _)) = self.staging.as_ref() else { return Err("no staging texture".to_owned()) };
        if self.query.is_none() {
            let query_desc = D3D11_QUERY_DESC { Query: D3D11_QUERY_EVENT, MiscFlags: 0 };
            let mut query = None;
            // Safety: the description and the output are locals that outlive the call.
            // Without the query the picture is read without the wait: not an error.
            let _ = unsafe { self.device.CreateQuery(&raw const query_desc, Some(&raw mut query)) };
            self.query = query;
        }
        // Safety: both textures are live, of the same device, format and size; the slice is the buffer's own.
        unsafe { self.context.CopySubresourceRegion(staging, 0, 0, 0, 0, &texture, slice, None) };
        if let Some(query) = &self.query {
            // Safety: calls on the live context with the live query; `done` outlives each call and is of the size given.
            unsafe {
                self.context.End(query);
                self.context.Flush();
                let started = Instant::now();
                let mut done = BOOL(0);
                while self.context.GetData(query, Some((&raw mut done).cast()), 4, 0).is_ok()
                    && !done.as_bool()
                    && started.elapsed() < GPU_WAIT
                {
                    std::thread::yield_now();
                }
            }
        }
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // Safety: the staging texture is CPU-readable; `mapped` outlives the call; unmapped below on every path.
        unsafe { self.context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&raw mut mapped)) }.map_err(failed)?;
        let stride = mapped.RowPitch as usize;
        let (width, height) = (desc.Width as usize, desc.Height as usize);
        let chroma_w = width.div_ceil(2) * 2;
        let result = if mapped.pData.is_null() || stride < chroma_w {
            Err(format!("the GPU's picture is mapped with rows of {stride} bytes for a width of {width}"))
        } else {
            // The luma rows, then the chroma rows, each `stride` apart; the last row is taken to end at the picture's
            // edge.
            let length = stride * (height + height.div_ceil(2) - 1) + chroma_w;
            // Safety: a mapped NV12 staging texture is that many readable bytes, which nothing writes while it is
            // mapped; the slice does not outlive the mapping.
            let bytes = unsafe { std::slice::from_raw_parts(mapped.pData.cast::<u8>().cast_const(), length) };
            planes(bytes, Layout { width: desc.Width, height: desc.Height, stride, ..layout })
        };
        // Safety: balances the `Map` above.
        unsafe { self.context.Unmap(staging, 0) };
        result
    }
}

/// Sets the transform's output to the NV12 type it offers, and returns that type.
fn set_nv12_output(transform: &IMFTransform) -> Result<IMFMediaType, String> {
    for index in 0.. {
        // Safety: a call on the live transform; the index runs until it says there are no more types.
        let ty = match unsafe { transform.GetOutputAvailableType(0, index) } {
            Ok(ty) => ty,
            Err(e) if e.code() == MF_E_NO_MORE_TYPES => break,
            Err(e) => return Err(format!("the decoder offers no output: {e}")),
        };
        // Safety: a call on the type just given.
        if unsafe { ty.GetGUID(&MF_MT_SUBTYPE) }.is_ok_and(|subtype| subtype == MFVideoFormat_NV12) {
            // Safety: the type is the transform's own offer.
            unsafe { transform.SetOutputType(0, &ty, 0) }
                .map_err(|e| format!("the decoder does not make NV12: {e}"))?;
            return Ok(ty);
        }
    }
    Err("the decoder does not offer NV12".to_owned())
}

/// What an output type says of its samples.
fn layout_of(ty: &IMFMediaType) -> Result<Layout, String> {
    // Safety (the four reads): calls on a live media type with keys that are constants of the bindings.
    let size = unsafe { ty.GetUINT64(&MF_MT_FRAME_SIZE) }.map_err(|e| format!("the output has no size: {e}"))?;
    // The width is the high half, the height the low one.
    let (width, height) = ((size >> 32) as u32, (size & 0xFFFF_FFFF) as u32);
    if width == 0 || height == 0 || width > 16384 || height > 16384 {
        return Err(format!("the output's size is {width}x{height}"));
    }
    // A stride that is missing is the width; one that is negative (a bottom-up picture) is not NV12's.
    let stride = unsafe { ty.GetUINT32(&MF_MT_DEFAULT_STRIDE) }.map_or(width, |s| s);
    if stride < width || stride > 4 * width {
        return Err(format!("the output's stride is {} for a width of {width}", stride.cast_signed()));
    }
    let range = match unsafe { ty.GetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE) } {
        Ok(r) if r == MFNominalRange_0_255.0.cast_unsigned() => ColorRange::Full,
        _ => ColorRange::Limited,
    };
    let matrix = match unsafe { ty.GetUINT32(&MF_MT_YUV_MATRIX) } {
        Ok(m) if m == MFVideoTransferMatrix_BT601.0.cast_unsigned() => ColorMatrix::Bt601,
        _ => ColorMatrix::Bt709,
    };
    Ok(Layout { width, height, stride: stride as usize, range, matrix })
}

/// A sample with one memory buffer of `capacity` bytes that holds `data` (which must fit).
fn memory_sample(capacity: u32, data: &[u8]) -> Result<IMFSample, String> {
    let length = u32::try_from(data.len()).ok().filter(|l| *l <= capacity).ok_or("the data does not fit its buffer")?;
    let failed = |e: windows::core::Error| format!("cannot make a buffer of {capacity} bytes: {e}");
    // Safety: plain arguments; the results are owned objects.
    let (sample, buffer) =
        unsafe { (MFCreateSample().map_err(failed)?, MFCreateMemoryBuffer(capacity.max(1)).map_err(failed)?) };
    if !data.is_empty() {
        with_bytes(&buffer, |bytes| {
            bytes.get_mut(..data.len()).ok_or("the buffer is smaller than was asked for")?.copy_from_slice(data);
            Ok(())
        })?;
    }
    // Safety: calls on the two objects just made; the length is within the buffer's capacity.
    unsafe { buffer.SetCurrentLength(length).and_then(|()| sample.AddBuffer(&buffer)) }.map_err(failed)?;
    Ok(sample)
}

/// Runs `f` on the whole memory of a buffer, locked for the time of the call.
fn with_bytes<T>(buffer: &IMFMediaBuffer, f: impl FnOnce(&mut [u8]) -> Result<T, String>) -> Result<T, String> {
    let mut data: *mut u8 = std::ptr::null_mut();
    let mut capacity = 0u32;
    // Safety: the outputs are locals that outlive the call; the lock is released below, on every path.
    unsafe { buffer.Lock(&raw mut data, Some(&raw mut capacity), None) }
        .map_err(|e| format!("cannot read a buffer: {e}"))?;
    let result = if data.is_null() {
        Err("a buffer has no memory".to_owned())
    } else {
        // Safety: while the buffer is locked, `data` points to `capacity` bytes that nothing else reads or writes; the
        // slice does not outlive the lock (`f` cannot keep it).
        f(unsafe { std::slice::from_raw_parts_mut(data, capacity as usize) })
    };
    // Safety: balances the `Lock` above.
    let _ = unsafe { buffer.Unlock() };
    result
}

/// Copies a decoded sample (NV12, laid out as `layout` says) into a [`Picture`], without the rows' padding.
fn copy_out(sample: &IMFSample, layout: Layout) -> Result<Picture, String> {
    // One buffer with the whole picture, luma rows then chroma rows: for a picture on the GPU this is where it is read
    // back.
    // Safety: a call on the live sample.
    let buffer = unsafe { sample.ConvertToContiguousBuffer() }.map_err(|e| format!("cannot read the picture: {e}"))?;
    // How long the picture is. A memory buffer says (the decoder set it); a buffer that is a texture says 0, and its
    // length is that of the copy the lock makes: what it says as a 2-D buffer, or else all of what the lock gives.
    // Safety (both): calls on the live buffer.
    let current = unsafe { buffer.GetCurrentLength() }.unwrap_or(0) as usize;
    let contiguous =
        buffer.cast::<IMF2DBuffer>().ok().and_then(|b| unsafe { b.GetContiguousLength() }.ok()).unwrap_or(0) as usize;
    let picture = with_bytes(&buffer, |bytes| {
        let length = [current, contiguous, bytes.len()].into_iter().find(|l| *l > 0).unwrap_or(0);
        let bytes = bytes.get(..length).ok_or_else(|| {
            format!(
                "the picture is {length} bytes ({current} current, {contiguous} contiguous) in a buffer of {}",
                bytes.len()
            )
        })?;
        planes(bytes, layout)
    })?;
    // The sample is used again for the next picture if it is ours: it is empty again.
    // Safety: a call on the live buffer; zero is within any capacity.
    let _ = unsafe { buffer.SetCurrentLength(0) };
    Ok(picture)
}

/// The two planes of an NV12 picture in `bytes`: `height` rows of luma, then half as many of interleaved chroma, each
/// `stride` bytes.
fn planes(bytes: &[u8], layout: Layout) -> Result<Picture, String> {
    let Layout { width, height, stride, range, matrix } = layout;
    let (w, h) = (width as usize, height as usize);
    let (chroma_w, chroma_h) = (w.div_ceil(2) * 2, h.div_ceil(2));
    // The copy of a texture has rows of its own length, whatever the output type's default is: a buffer that is a whole
    // number of rows of exactly the picture's width, or of more than the type says, is read by those rows.
    let rows = h + chroma_h;
    let own = bytes.len() / rows;
    let stride = if bytes.len().is_multiple_of(rows) && (own == chroma_w || own > stride) { own } else { stride };
    let short = || format!("the picture has {} bytes, too few for {width}x{height} at {stride} a row", bytes.len());
    let (luma, chroma) = bytes.split_at_checked(stride * h).ok_or_else(short)?;
    if stride < chroma_w || chroma.len() < stride * (chroma_h.saturating_sub(1)) + chroma_w {
        return Err(short());
    }
    let mut y = Vec::with_capacity(w * h);
    for row in luma.chunks_exact(stride) {
        y.extend_from_slice(row.get(..w).unwrap_or(row));
    }
    let mut uv = Vec::with_capacity(chroma_w * chroma_h);
    for row in chroma.chunks(stride).take(chroma_h) {
        uv.extend_from_slice(row.get(..chroma_w).unwrap_or(row));
    }
    Ok(Picture {
        width,
        height,
        y: Plane { data: y, stride: w },
        uv: Plane { data: uv, stride: chroma_w },
        range,
        matrix,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::viewer::decode::testing::*;

    /// The decoder, or `None` on a Windows without one (a Server without the Media Foundation feature): the tests then
    /// say so and pass, as there is nothing to test.
    fn decoder() -> Option<MediaFoundation> {
        match MediaFoundation::new() {
            Ok(decoder) => {
                eprintln!("decoding with {}", decoder.name());
                Some(decoder)
            }
            Err(why) => {
                eprintln!("no Media Foundation decoder here, nothing tested: {why}");
                None
            }
        }
    }

    #[test]
    fn decodes_the_recorded_stream_picture_by_picture() {
        let Some(mut decoder) = decoder() else { return };
        let units = fixture_units();
        for (n, unit) in units.iter().enumerate() {
            let p = decoder.decode(unit).expect("decodes").unwrap_or_else(|| panic!("no picture for unit {n}"));
            // The frame may be larger than the picture (the decoder's own padding), never smaller.
            assert!(p.width >= FIXTURE_SIZE.0 && p.height >= FIXTURE_SIZE.1, "unit {n}: {}x{}", p.width, p.height);
            assert_eq!(p.y.data.len(), p.y.stride * p.height as usize);
            assert_eq!(p.uv.data.len(), p.uv.stride * (p.height as usize).div_ceil(2));
            // Sampled where the fixture's quadrants are, whatever the frame's size.
            for (left, top) in [(true, true), (false, true), (true, false), (false, false)] {
                let x = if left { FIXTURE_SIZE.0 / 4 } else { FIXTURE_SIZE.0 * 3 / 4 };
                let y = if top { FIXTURE_SIZE.1 / 4 } else { FIXTURE_SIZE.1 * 3 / 4 };
                let (cb, cr) = p.chroma(x, y).expect("inside");
                let got = (p.luma(x, y).expect("inside"), cb, cr);
                let want = quadrant(n, left, top);
                assert!(near(got, want, 6), "unit {n} quadrant left={left} top={top}: got {got:?}, want {want:?}");
            }
        }
    }

    #[test]
    fn a_delta_unit_before_any_key_unit_makes_no_picture_and_no_error() {
        let Some(mut decoder) = decoder() else { return };
        let units = fixture_units();
        assert_eq!(decoder.decode(&units[1]), Ok(None));
        assert_eq!(decoder.decode(&units[2]), Ok(None));
        assert!(decoder.decode(&units[0]).expect("decodes").is_some());
    }

    #[test]
    fn two_decoders_one_after_the_other_and_on_another_thread() {
        // The runtime is started and shut down per decoder, and a decoder made on one thread decodes on another (as
        // the viewer does: made on the main thread, used by the decode thread).
        let Some(first) = decoder() else { return };
        drop(first);
        let Some(mut decoder) = decoder() else { return };
        let units = fixture_units();
        let picture = std::thread::spawn(move || decoder.decode(&units[0])).join().expect("no panic");
        assert!(picture.expect("decodes").is_some());
    }

    #[test]
    fn the_planes_are_cut_out_of_rows_with_padding() {
        // 4x2 at 6 bytes a row: two luma rows, one chroma row, the padding marked 9.
        let bytes = [1, 2, 3, 4, 9, 9, 5, 6, 7, 8, 9, 9, 10, 11, 12, 13, 9, 9];
        let layout = Layout { width: 4, height: 2, stride: 6, range: ColorRange::Limited, matrix: ColorMatrix::Bt709 };
        let p = planes(&bytes, layout).expect("fits");
        assert_eq!((p.y.data, p.y.stride), (vec![1, 2, 3, 4, 5, 6, 7, 8], 4));
        assert_eq!((p.uv.data, p.uv.stride), (vec![10, 11, 12, 13], 4));
        // The last chroma row may stop at the picture's edge; anything shorter is refused, not read past.
        assert!(planes(&bytes[..16], layout).is_ok());
        assert!(planes(&bytes[..15], layout).is_err());
        assert!(planes(&bytes[..11], layout).is_err());
        // A buffer of whole rows of another length than the type says is read by its own rows (a texture's copy).
        let tight = [1, 2, 3, 4, 5, 6, 7, 8, 10, 11, 12, 13];
        let p = planes(&tight, layout).expect("fits");
        assert_eq!((p.y.data, p.uv.data), (vec![1, 2, 3, 4, 5, 6, 7, 8], vec![10, 11, 12, 13]));
    }
}
