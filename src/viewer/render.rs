//! Drawing with wgpu: the decoded picture as two textures (luma, and interleaved chroma), one small shader that makes RGB
//! of them, turns the picture upright and crops it, and a viewport that letterboxes it.
//!
//! [`Video`] is the part that needs no window (the pipeline, the textures, the draw), so that it can be tested by
//! drawing to a texture and reading it back; [`Renderer`] adds the surface of a window.

use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use winit::window::Window;

use crate::viewer::decode::{ColorMatrix, ColorRange, Picture};
use crate::viewer::geometry::{PictureGeometry, Rect, Rotation, letterbox};

const SHADER: &str = include_str!("video.wgsl");

/// The uniform of the shader (`Params` in `video.wgsl`).
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Params {
    crop: [f32; 2],
    luma_min: [f32; 2],
    luma_max: [f32; 2],
    chroma_min: [f32; 2],
    chroma_max: [f32; 2],
    rotation: u32,
    flags: u32,
}

/// The textures of one picture size, and what binds them to the shader.
#[derive(Debug)]
struct Textures {
    size: (u32, u32),
    luma: wgpu::Texture,
    chroma: wgpu::Texture,
    bind_group: wgpu::BindGroup,
}

/// What was last uploaded: the picture's size and how to read its colours.
#[derive(Debug, Clone, Copy)]
struct Uploaded {
    size: (u32, u32),
    range: ColorRange,
    matrix: ColorMatrix,
}

/// What a draw needs besides the picture.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Draw {
    /// The framebuffer's visible size (the config's `width` and `height`): the part of the decoded frame to show.
    pub visible: (u32, u32),
    pub rotation: Rotation,
    /// Where in the target the picture goes, in pixels: the letterbox.
    pub rect: Rect,
}

/// The picture's pipeline and textures.
#[derive(Debug)]
pub struct Video {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    params: wgpu::Buffer,
    textures: Option<Textures>,
    uploaded: Option<Uploaded>,
}

impl Video {
    /// A pipeline that draws to targets of `format`.
    #[must_use]
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Video {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("scrios video"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let texture = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("scrios video bindings"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                texture(1),
                texture(2),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("scrios video pipeline layout"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("scrios video"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState { format, blend: None, write_mask: wgpu::ColorWrites::ALL })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("scrios video sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..wgpu::SamplerDescriptor::default()
        });
        let params = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("scrios video params"),
            size: std::mem::size_of::<Params>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Video { pipeline, layout, sampler, params, textures: None, uploaded: None }
    }

    /// Whether a picture was uploaded: until then there is nothing to draw.
    #[must_use]
    pub fn has_picture(&self) -> bool {
        self.uploaded.is_some()
    }

    /// Puts a decoded picture in the textures (made again if its size changed).
    pub fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, picture: &Picture) {
        let size = (picture.width, picture.height);
        if size.0 == 0 || size.1 == 0 {
            return;
        }
        if self.textures.as_ref().is_none_or(|t| t.size != size) {
            self.textures = Some(self.make_textures(device, size));
        }
        let Some(textures) = &self.textures else { return };
        let (cw, ch) = (size.0.div_ceil(2), size.1.div_ceil(2));
        write_plane(queue, &textures.luma, &picture.y.data, picture.y.stride, (size.0, size.1), 1);
        write_plane(queue, &textures.chroma, &picture.uv.data, picture.uv.stride, (cw, ch), 2);
        self.uploaded = Some(Uploaded { size, range: picture.range, matrix: picture.matrix });
    }

    fn make_textures(&self, device: &wgpu::Device, size: (u32, u32)) -> Textures {
        let make = |label, format, (w, h)| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
        };
        let luma = make("scrios luma", wgpu::TextureFormat::R8Unorm, size);
        let chroma = make("scrios chroma", wgpu::TextureFormat::Rg8Unorm, (size.0.div_ceil(2), size.1.div_ceil(2)));
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("scrios video bind group"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: self.params.as_entire_binding() },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(
                        &luma.create_view(&wgpu::TextureViewDescriptor::default()),
                    ),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(
                        &chroma.create_view(&wgpu::TextureViewDescriptor::default()),
                    ),
                },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::Sampler(&self.sampler) },
            ],
        });
        Textures { size, luma, chroma, bind_group }
    }

    /// Draws the last picture into `pass`, in the viewport of `draw.rect`. Nothing is drawn before the first upload.
    pub fn draw(&self, queue: &wgpu::Queue, pass: &mut wgpu::RenderPass<'_>, draw: &Draw) {
        let (Some(textures), Some(uploaded)) = (&self.textures, self.uploaded) else { return };
        if draw.rect.w < 1.0 || draw.rect.h < 1.0 {
            return;
        }
        queue.write_buffer(&self.params, 0, bytemuck::bytes_of(&params_for(draw, uploaded)));
        let rect = draw.rect;
        pass.set_viewport(single(rect.x), single(rect.y), single(rect.w), single(rect.h), 0.0, 1.0);
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &textures.bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
}

/// A length in pixels as the shader's float: pictures are far below the 2^24 pixels a float counts exactly.
#[expect(clippy::cast_precision_loss, reason = "a side of a picture is at most 32768 pixels")]
fn pixels(n: u32) -> f32 {
    n as f32
}

/// A position or size of the window as the viewport's float.
#[expect(clippy::cast_possible_truncation, reason = "whole pixels of a window, far below the range of a float")]
fn single(v: f64) -> f32 {
    v as f32
}

/// The uniform for a draw of a picture.
fn params_for(draw: &Draw, picture: Uploaded) -> Params {
    let (dw, dh) = (pixels(picture.size.0), pixels(picture.size.1));
    // The visible part can not be larger than the frame.
    let (vw, vh) = (pixels(draw.visible.0.min(picture.size.0)), pixels(draw.visible.1.min(picture.size.1)));
    let mut flags = 0;
    if picture.range == ColorRange::Full {
        flags |= 1;
    }
    if picture.matrix == ColorMatrix::Bt709 {
        flags |= 2;
    }
    // Half a texel in from the visible edge, in the luma plane's texels and in the chroma plane's (which are twice as big:
    // half a chroma texel is a whole luma texel).
    Params {
        crop: [vw / dw, vh / dh],
        luma_min: [0.5 / dw, 0.5 / dh],
        luma_max: [(vw - 0.5).max(0.5) / dw, (vh - 0.5).max(0.5) / dh],
        chroma_min: [1.0 / dw, 1.0 / dh],
        chroma_max: [(vw - 1.0).max(1.0) / dw, (vh - 1.0).max(1.0) / dh],
        rotation: draw.rotation.quarter_turns(),
        flags,
    }
}

/// Writes one plane of a picture (`texel` bytes per texel of the texture) from rows of `stride` bytes.
fn write_plane(
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    data: &[u8],
    stride: usize,
    size: (u32, u32),
    texel: usize,
) {
    // A plane shorter than its rows say is a decoder's bug; the upload then leaves the texture as it was.
    let row = size.0 as usize * texel;
    let rows = size.1 as usize;
    let Ok(bytes_per_row) = u32::try_from(stride) else { return };
    if stride < row || rows == 0 || data.len() < stride * (rows - 1) + row {
        return;
    }
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        data,
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(bytes_per_row), rows_per_image: Some(size.1) },
        wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
    );
}

/// Why a frame was not drawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderError {
    /// The surface is gone (the GPU was reset, the display went away): the viewer cannot go on.
    Lost(String),
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self::Lost(why) = self;
        f.write_str(why)
    }
}

impl std::error::Error for RenderError {}

/// A window's surface and the GPU behind it.
#[derive(Debug)]
pub struct Renderer {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    video: Video,
}

impl Renderer {
    /// The GPU and the surface of `window`. `display` is the event loop's display handle, which OpenGL on Wayland
    /// needs.
    ///
    /// # Errors
    ///
    /// No surface, no suitable adapter, or no device.
    pub fn new(window: Arc<Window>, display: winit::event_loop::OwnedDisplayHandle) -> Result<Renderer, String> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle(Box::new(display)));
        let surface = instance
            .create_surface(window.clone())
            .map_err(|e| format!("cannot make a surface for the window: {e}"))?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
            apply_limit_buckets: false,
        }))
        .map_err(|e| format!("no graphics adapter can draw to the window: {e}"))?;
        tracing::info!("graphics: {} ({:?})", adapter.get_info().name, adapter.get_info().backend);
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("scrios"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
            ..wgpu::DeviceDescriptor::default()
        }))
        .map_err(|e| format!("cannot open the graphics device: {e}"))?;

        let size = window.inner_size();
        let capabilities = surface.get_capabilities(&adapter);
        // Video is gamma-encoded already: write it as it is, to a format that does not encode again.
        let format = capabilities
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .or_else(|| capabilities.formats.first().copied())
            .ok_or("the window's surface has no texture format")?;
        // The newest picture replaces the one waiting (Mailbox) where the platform can; else one refresh of latency.
        let present_mode = if capabilities.present_modes.contains(&wgpu::PresentMode::Mailbox) {
            wgpu::PresentMode::Mailbox
        } else {
            wgpu::PresentMode::Fifo
        };
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode,
            desired_maximum_frame_latency: 1,
            alpha_mode: capabilities.alpha_modes.first().copied().unwrap_or(wgpu::CompositeAlphaMode::Auto),
            view_formats: vec![],
            color_space: wgpu::SurfaceColorSpace::Auto,
        };
        surface.configure(&device, &config);
        let video = Video::new(&device, format);
        Ok(Renderer { window, surface, device, queue, config, video })
    }

    /// The window changed size (in physical pixels).
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
    }

    /// Makes the next draw show `picture`.
    pub fn upload(&mut self, picture: &Picture) {
        self.video.upload(&self.device, &self.queue, picture);
    }

    /// Draws a frame: black, and the last picture letterboxed in it if the stream has said what it looks like
    /// (`geometry`) and sent one.
    ///
    /// # Errors
    ///
    /// The surface was lost.
    pub fn render(&mut self, geometry: Option<&PictureGeometry>) -> Result<(), RenderError> {
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame) | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.device, &self.config);
                return Ok(());
            }
            // Nothing to draw to now (minimised, a missed frame): the next frame will.
            wgpu::CurrentSurfaceTexture::Timeout
            | wgpu::CurrentSurfaceTexture::Occluded
            | wgpu::CurrentSurfaceTexture::Validation => return Ok(()),
            wgpu::CurrentSurfaceTexture::Lost => return Err(RenderError::Lost("the window's surface was lost".into())),
        };
        let target = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("scrios") });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("scrios"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if let Some(g) = geometry
                && let Some(rect) = letterbox((self.config.width, self.config.height), g.upright())
            {
                self.video.draw(
                    &self.queue,
                    &mut pass,
                    &Draw { visible: (g.width, g.height), rotation: g.rotation, rect },
                );
            }
        }
        self.queue.submit([encoder.finish()]);
        self.window.pre_present_notify();
        self.queue.present(frame);
        Ok(())
    }

    /// The window's size in physical pixels, as the surface is configured.
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        (self.config.width, self.config.height)
    }

    /// Whether a picture has been uploaded.
    #[must_use]
    pub fn has_picture(&self) -> bool {
        self.video.has_picture()
    }
}

#[cfg(test)]
mod tests {
    //! The shader, drawn to a texture and read back: the colours, the rotation, the crop and the letterbox. They need
    //! a graphics adapter; on a machine without one the tests say so and pass.

    use super::*;
    use crate::viewer::decode::testing::*;
    use crate::viewer::decode::{Decoder, software::Software};

    const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

    struct Gpu {
        device: wgpu::Device,
        queue: wgpu::Queue,
    }

    fn gpu() -> Option<Gpu> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())).ok()?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).ok()?;
        Some(Gpu { device, queue })
    }

    /// Draws `picture` into a target of `size` and returns its RGBA pixels, row by row.
    fn render(gpu: &Gpu, picture: &Picture, size: (u32, u32), draw: &Draw) -> Vec<[u8; 4]> {
        let mut video = Video::new(&gpu.device, FORMAT);
        video.upload(&gpu.device, &gpu.queue, picture);
        assert!(video.has_picture());
        let target = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("test target"),
            size: wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let row_bytes = (size.0 * 4).next_multiple_of(256);
        let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("test readback"),
            size: u64::from(row_bytes) * u64::from(size.1),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let view = target.create_view(&wgpu::TextureViewDescriptor::default());
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("test"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            video.draw(&gpu.queue, &mut pass, draw);
        }
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row_bytes),
                    rows_per_image: Some(size.1),
                },
            },
            wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 },
        );
        gpu.queue.submit([encoder.finish()]);
        let slice = readback.slice(..);
        slice.map_async(wgpu::MapMode::Read, |r| r.expect("map"));
        gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
        let data = slice.get_mapped_range().expect("mapped");
        let mut pixels = Vec::new();
        for y in 0..size.1 as usize {
            for x in 0..size.0 as usize {
                let at = y * row_bytes as usize + x * 4;
                pixels.push([data[at], data[at + 1], data[at + 2], data[at + 3]]);
            }
        }
        pixels
    }

    fn picture(n: usize) -> Picture {
        let units = fixture_units();
        let mut decoder = Software::new().expect("decoder");
        let mut last = None;
        for unit in &units[..=n] {
            last = decoder.decode(unit).expect("decodes");
        }
        last.expect("a picture")
    }

    fn at(pixels: &[[u8; 4]], width: u32, x: u32, y: u32) -> [u8; 4] {
        pixels[(y * width + x) as usize]
    }

    fn close(got: [u8; 4], want: [u8; 3], tolerance: u8) -> bool {
        (0..3).all(|i| got[i].abs_diff(want[i]) <= tolerance)
    }

    const RED_RGB: [u8; 3] = [255, 0, 0];
    const GREEN_RGB: [u8; 3] = [0, 255, 0];
    const BLUE_RGB: [u8; 3] = [0, 0, 255];
    const GRAY_RGB: [u8; 3] = [128, 128, 128];

    fn whole(size: (u32, u32), visible: (u32, u32), rotation: Rotation) -> Draw {
        Draw { visible, rotation, rect: Rect { x: 0.0, y: 0.0, w: f64::from(size.0), h: f64::from(size.1) } }
    }

    #[test]
    fn the_four_quadrants_are_the_four_colours_upright() {
        let Some(gpu) = gpu() else {
            eprintln!("no graphics adapter: skipped");
            return;
        };
        let size = (64, 96);
        let pixels = render(&gpu, &picture(0), size, &whole(size, size, Rotation::R0));
        for (x, y, want) in [(16, 24, RED_RGB), (48, 24, GREEN_RGB), (16, 72, BLUE_RGB), (48, 72, GRAY_RGB)] {
            let got = at(&pixels, size.0, x, y);
            assert!(close(got, want, 10), "({x}, {y}): got {got:?}, want {want:?}");
            assert_eq!(got[3], 255);
        }
    }

    #[test]
    fn each_rotation_turns_the_picture_clockwise() {
        let Some(gpu) = gpu() else {
            eprintln!("no graphics adapter: skipped");
            return;
        };
        // The corners of the upright picture (top left, top right, bottom right, bottom left) show these colours of
        // the framebuffer (red, green, gray, blue at its corners) for each quarter turn.
        let cases = [
            (Rotation::R0, (64u32, 96u32), [RED_RGB, GREEN_RGB, GRAY_RGB, BLUE_RGB]),
            (Rotation::R90, (96, 64), [BLUE_RGB, RED_RGB, GREEN_RGB, GRAY_RGB]),
            (Rotation::R180, (64, 96), [GRAY_RGB, BLUE_RGB, RED_RGB, GREEN_RGB]),
            (Rotation::R270, (96, 64), [GREEN_RGB, GRAY_RGB, BLUE_RGB, RED_RGB]),
        ];
        for (rotation, size, want) in cases {
            let pixels = render(&gpu, &picture(0), size, &whole(size, (64, 96), rotation));
            let (w, h) = size;
            let probes = [(w / 4, h / 4), (w * 3 / 4, h / 4), (w * 3 / 4, h * 3 / 4), (w / 4, h * 3 / 4)];
            for (i, (x, y)) in probes.into_iter().enumerate() {
                let got = at(&pixels, w, x, y);
                assert!(
                    close(got, want[i], 10),
                    "{rotation:?} corner {i} at ({x}, {y}): got {got:?}, want {:?}",
                    want[i]
                );
            }
        }
    }

    #[test]
    fn a_later_picture_replaces_the_earlier_one() {
        let Some(gpu) = gpu() else {
            eprintln!("no graphics adapter: skipped");
            return;
        };
        let size = (64, 96);
        // The third picture of the fixture has a black bottom right.
        let pixels = render(&gpu, &picture(2), size, &whole(size, size, Rotation::R0));
        assert!(close(at(&pixels, size.0, 48, 72), [0, 0, 0], 10));
        assert!(close(at(&pixels, size.0, 16, 24), RED_RGB, 10));
    }

    #[test]
    fn only_the_visible_part_of_the_frame_is_shown() {
        let Some(gpu) = gpu() else {
            eprintln!("no graphics adapter: skipped");
            return;
        };
        // The left half of the frame (red over blue), over the whole target: no green and no gray anywhere.
        let size = (64, 96);
        let pixels = render(&gpu, &picture(0), size, &whole(size, (32, 96), Rotation::R0));
        assert!(close(at(&pixels, size.0, 60, 20), RED_RGB, 10), "{:?}", at(&pixels, size.0, 60, 20));
        assert!(close(at(&pixels, size.0, 60, 80), BLUE_RGB, 10), "{:?}", at(&pixels, size.0, 60, 80));
        assert!(close(at(&pixels, size.0, 63, 95), BLUE_RGB, 10));
    }

    #[test]
    fn the_picture_is_letterboxed_and_the_bars_are_black() {
        let Some(gpu) = gpu() else {
            eprintln!("no graphics adapter: skipped");
            return;
        };
        // A 64x96 picture in a 192x96 target: 64 pixels wide in the middle, bars of 64 left and right.
        let size = (192, 96);
        let rect = letterbox(size, (64, 96)).expect("a letterbox");
        assert_eq!((rect.x, rect.w), (64.0, 64.0));
        let pixels = render(&gpu, &picture(0), size, &Draw { visible: (64, 96), rotation: Rotation::R0, rect });
        assert!(close(at(&pixels, size.0, 10, 50), [0, 0, 0], 0), "left bar");
        assert!(close(at(&pixels, size.0, 180, 50), [0, 0, 0], 0), "right bar");
        assert!(close(at(&pixels, size.0, 64 + 16, 24), RED_RGB, 10));
        assert!(close(at(&pixels, size.0, 64 + 48, 72), GRAY_RGB, 10));
    }

    #[test]
    #[expect(clippy::float_cmp, reason = "a visible size larger than the frame is exactly the frame")]
    fn the_uniform_for_a_draw_matches_the_shader() {
        let picture = Uploaded { size: (1216, 2656), range: ColorRange::Limited, matrix: ColorMatrix::Bt709 };
        let draw =
            Draw { visible: (1206, 2622), rotation: Rotation::R90, rect: Rect { x: 0.0, y: 0.0, w: 10.0, h: 10.0 } };
        let p = params_for(&draw, picture);
        assert_eq!(p.rotation, 1);
        assert_eq!(p.flags, 2);
        assert!((p.crop[0] - 1206.0 / 1216.0).abs() < 1e-6 && (p.crop[1] - 2622.0 / 2656.0).abs() < 1e-6);
        assert!((p.luma_max[0] - 1205.5 / 1216.0).abs() < 1e-6 && (p.luma_max[1] - 2621.5 / 2656.0).abs() < 1e-6);
        assert!((p.luma_min[0] - 0.5 / 1216.0).abs() < 1e-9);
        // The chroma plane's last visible texel is the one of the visible edge's last pair of luma pixels.
        assert!((p.chroma_max[0] - 1205.0 / 1216.0).abs() < 1e-6 && (p.chroma_min[1] - 1.0 / 2656.0).abs() < 1e-9);
        // A visible size larger than the frame is the frame.
        let big = Draw { visible: (5000, 5000), ..draw };
        assert_eq!(params_for(&big, picture).crop, [1.0, 1.0]);
        let full = Uploaded { range: ColorRange::Full, matrix: ColorMatrix::Bt601, ..picture };
        assert_eq!(params_for(&draw, full).flags, 1);
        assert_eq!(std::mem::size_of::<Params>(), 48, "the WGSL struct is 48 bytes");
    }
}
