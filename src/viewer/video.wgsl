// Draws one decoded picture: NV12 (a luma plane and an interleaved Cb/Cr plane) to RGB, turned upright, cropped to the
// visible part of the frame. The rectangle it covers is the viewport the pass sets (the letterbox); the rest of the
// target is whatever the pass cleared it to.

struct Params {
    // The visible part of the decoded frame, as fractions of its size (the encoder pads to whole macroblocks).
    crop: vec2<f32>,
    // The smallest and largest texture coordinates that are inside the visible part, half a texel in from its edge, so
    // that the filter never blends in the padding. The chroma plane has half the texels, so its limits are not the
    // luma plane's.
    luma_min: vec2<f32>,
    luma_max: vec2<f32>,
    chroma_min: vec2<f32>,
    chroma_max: vec2<f32>,
    // Quarter turns clockwise that make the framebuffer upright: 0 to 3.
    rotation: u32,
    // Bit 0: full range (else studio swing). Bit 1: BT.709 (else BT.601).
    flags: u32,
}

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var luma: texture_2d<f32>;
@group(0) @binding(2) var chroma: texture_2d<f32>;
@group(0) @binding(3) var smp: sampler;

struct Out {
    @builtin(position) position: vec4<f32>,
    // The place in the upright picture, 0 to 1 from the top left.
    @location(0) upright: vec2<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> Out {
    // One triangle that covers the viewport: (0, 0), (2, 0) and (0, 2) of the upright picture.
    let u = f32((index << 1u) & 2u);
    let v = f32(index & 2u);
    var out: Out;
    out.position = vec4<f32>(u * 2.0 - 1.0, 1.0 - v * 2.0, 0.0, 1.0);
    out.upright = vec2<f32>(u, v);
    return out;
}

// The point of the framebuffer that shows at a point of the upright picture (Rotation::sample in geometry.rs).
fn framebuffer_point(p: vec2<f32>) -> vec2<f32> {
    switch params.rotation {
        case 1u: {
            return vec2<f32>(p.y, 1.0 - p.x);
        }
        case 2u: {
            return vec2<f32>(1.0 - p.x, 1.0 - p.y);
        }
        case 3u: {
            return vec2<f32>(1.0 - p.y, p.x);
        }
        default: {
            return p;
        }
    }
}

@fragment
fn fs_main(in: Out) -> @location(0) vec4<f32> {
    let fb = framebuffer_point(in.upright);
    let at = fb * params.crop;
    let y = textureSampleLevel(luma, smp, clamp(at, params.luma_min, params.luma_max), 0.0).r;
    let c = textureSampleLevel(chroma, smp, clamp(at, params.chroma_min, params.chroma_max), 0.0).rg;

    var yy = y;
    var cb = c.r - 0.5;
    var cr = c.g - 0.5;
    if (params.flags & 1u) == 0u {
        yy = (y - 16.0 / 255.0) * (255.0 / 219.0);
        cb = (c.r - 128.0 / 255.0) * (255.0 / 224.0);
        cr = (c.g - 128.0 / 255.0) * (255.0 / 224.0);
    }

    var rgb: vec3<f32>;
    if (params.flags & 2u) != 0u {
        rgb = vec3<f32>(yy + 1.5748 * cr, yy - 0.1873 * cb - 0.4681 * cr, yy + 1.8556 * cb);
    } else {
        rgb = vec3<f32>(yy + 1.402 * cr, yy - 0.344136 * cb - 0.714136 * cr, yy + 1.772 * cb);
    }
    return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
