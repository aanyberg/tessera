// Instanced quads for every grid pass (ADR 0010).
//
// One instance is one rectangle in physical pixels. `kind` selects the
// fragment: 0 fills with `color`, 1 multiplies `color` by coverage from the
// alpha atlas, 2 takes premultiplied colour from the colour atlas times
// `color.a`, 3 fills with `color` in dashes along x (`texel` holds the
// period and the dash length), 4 draws a wave along x (`texel` holds the
// period and the amplitude; the stroke is the rect's height minus twice
// the amplitude). Patterns are phased from the x in `extra` (the grid's
// left edge), not from their own rect, so they continue across runs.
// Colours are linear and premultiplied; the pipeline blends with
// (One, OneMinusSrcAlpha) and an sRGB target encodes on write.
//
// Glyphs drawn at their own size (`extra` 1) sit on whole pixels and map
// one texel to one pixel, so the atlas is read with `textureLoad`: exact,
// no filtering at glyph edges. Superscript and subscript glyphs are drawn
// smaller (`extra` = atlas texels per pixel, above 1) and filtered
// bilinearly by hand: four `textureLoad`s inside the glyph's own bitmap
// (so a neighbour in the atlas never bleeds in), weighted by the position
// inside the bitmap alone. The weights therefore do not depend on where in
// the atlas the glyph sits, and are exact, unlike a sampler's (whose
// fixed-point weights, from absolute coordinates, made the same glyph
// differ by a step between atlas slots).

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
    // Atlas texel of the rectangle's top-left corner (glyphs), or the
    // pattern's parameters (kinds 3 and 4).
    @location(3) texel: vec2<f32>,
    @location(4) kind: u32,
    // Glyphs: atlas texels per pixel. Patterns: the x their phase starts at.
    @location(5) extra: f32,
};

struct VsOut {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
    // Glyphs: position inside the glyph's bitmap, in texels.
    @location(1) texel: vec2<f32>,
    @location(2) @interpolate(flat) kind: u32,
    @location(3) @interpolate(flat) clip: vec4<f32>,
    @location(4) @interpolate(flat) rect: vec4<f32>,
    // Glyphs: the bitmap's first texel and its size in texels. Patterns:
    // their two parameters in `xy`.
    @location(5) @interpolate(flat) params: vec4<f32>,
    @location(6) @interpolate(flat) extra: f32,
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
    out.texel = corner * size * instance.extra;
    out.kind = instance.kind;
    out.clip = instance.clip;
    out.rect = instance.rect;
    out.params = vec4<f32>(instance.texel, size * instance.extra);
    out.extra = instance.extra;
    return out;
}

const TAU: f32 = 6.283185307179586;

// A scaled glyph's sample: bilinear between the four texel centres around
// the position inside its bitmap, clamped to the bitmap's edge texels.
fn sampled(atlas: texture_2d<f32>, in: VsOut) -> vec4<f32> {
    let size = max(round(in.params.zw), vec2<f32>(1.0));
    let p = clamp(in.texel, vec2<f32>(0.5), max(size - vec2<f32>(0.5), vec2<f32>(0.5)))
        - vec2<f32>(0.5);
    let cell = floor(p);
    let f = p - cell;
    let base = vec2<i32>(round(in.params.xy));
    let last = vec2<i32>(size) - vec2<i32>(1);
    let i0 = min(vec2<i32>(cell), last);
    let i1 = min(i0 + vec2<i32>(1), last);
    let a = textureLoad(atlas, base + i0, 0);
    let b = textureLoad(atlas, base + vec2<i32>(i1.x, i0.y), 0);
    let c = textureLoad(atlas, base + vec2<i32>(i0.x, i1.y), 0);
    let d = textureLoad(atlas, base + i1, 0);
    return mix(mix(a, b, f.x), mix(c, d, f.x), f.y);
}

fn glyph(atlas: texture_2d<f32>, in: VsOut) -> vec4<f32> {
    if in.extra == 1.0 {
        return textureLoad(atlas, vec2<i32>(floor(in.params.xy + in.texel)), 0);
    }
    return sampled(atlas, in);
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // `position.xy` is the pixel centre.
    let p = in.position.xy;
    if p.x < in.clip.x || p.y < in.clip.y || p.x >= in.clip.z || p.y >= in.clip.w {
        discard;
    }
    switch in.kind {
        case 1u: {
            return in.color * glyph(alpha_atlas, in).r;
        }
        case 2u: {
            return glyph(color_atlas, in) * in.color.a;
        }
        case 3u: {
            // Dashes: `params.x` long periods counted from `extra`, the
            // first `params.y` pixels of each drawn.
            let period = max(in.params.x, 1.0);
            let along = p.x - in.extra;
            if along - floor(along / period) * period >= in.params.y {
                discard;
            }
            return in.color;
        }
        case 4u: {
            // A sine of period `params.x` and amplitude `params.y` around
            // the rect's middle, stroked as thick as the rect's height
            // minus both amplitudes, with a one-pixel anti-aliased edge
            // measured across the curve.
            let period = max(in.params.x, 1.0);
            let amplitude = in.params.y;
            let stroke = max(in.rect.w - 2.0 * amplitude, 1.0);
            let phase = (p.x - in.extra) / period * TAU;
            let centre = in.rect.y + in.rect.w * 0.5 - amplitude * sin(phase);
            let slope = amplitude * TAU / period * cos(phase);
            let distance = abs(p.y - centre) / sqrt(1.0 + slope * slope);
            let coverage = clamp(stroke * 0.5 + 0.5 - distance, 0.0, 1.0);
            if coverage <= 0.0 {
                discard;
            }
            return in.color * coverage;
        }
        default: {
            return in.color;
        }
    }
}
