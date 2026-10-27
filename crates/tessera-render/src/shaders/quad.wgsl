// Instanced quads for every grid pass (ADR 0010).
//
// One instance is one rectangle in physical pixels. `kind` selects the
// fragment: 0 fills with `color`, 1 multiplies `color` by coverage from the
// alpha atlas, 2 takes premultiplied colour from the colour atlas times
// `color.a`. Colours are linear and premultiplied; the pipeline blends with
// (One, OneMinusSrcAlpha) and an sRGB target encodes on write.
//
// Atlas texels are fetched with `textureLoad`, not sampled: glyphs sit on
// whole pixels and map one texel to one pixel, so a fetch is exact and
// needs no sampler.

struct Globals {
    // Target size in pixels.
    viewport: vec2<f32>,
    _pad: vec2<f32>,
};

@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var alpha_atlas: texture_2d<f32>;
@group(0) @binding(2) var color_atlas: texture_2d<f32>;

struct InstanceIn {
    // x, y, width, height in pixels.
    @location(0) rect: vec4<f32>,
    // x0, y0, x1, y1: fragments outside are discarded.
    @location(1) clip: vec4<f32>,
    @location(2) color: vec4<f32>,
    // Atlas texel of the rectangle's top-left corner.
    @location(3) texel: vec2<f32>,
    @location(4) kind: u32,
};

struct VsOut {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) texel: vec2<f32>,
    @location(2) @interpolate(flat) kind: u32,
    @location(3) @interpolate(flat) clip: vec4<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vertex: u32, instance: InstanceIn) -> VsOut {
    // Triangle strip: (0,0) (1,0) (0,1) (1,1).
    let corner = vec2<f32>(f32(vertex & 1u), f32(vertex >> 1u));
    let size = instance.rect.zw;
    let pixel = instance.rect.xy + corner * size;
    let ndc = vec2<f32>(
        pixel.x / globals.viewport.x * 2.0 - 1.0,
        1.0 - pixel.y / globals.viewport.y * 2.0,
    );
    var out: VsOut;
    out.position = vec4<f32>(ndc, 0.0, 1.0);
    out.color = instance.color;
    out.texel = instance.texel + corner * size;
    out.kind = instance.kind;
    out.clip = instance.clip;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // `position.xy` is the pixel centre.
    let p = in.position.xy;
    if p.x < in.clip.x || p.y < in.clip.y || p.x >= in.clip.z || p.y >= in.clip.w {
        discard;
    }
    let texel = vec2<i32>(floor(in.texel));
    switch in.kind {
        case 1u: {
            return in.color * textureLoad(alpha_atlas, texel, 0).r;
        }
        case 2u: {
            return textureLoad(color_atlas, texel, 0) * in.color.a;
        }
        default: {
            return in.color;
        }
    }
}
