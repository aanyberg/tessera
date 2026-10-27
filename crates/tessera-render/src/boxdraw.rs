//! Box drawing (U+2500–U+257F) and block elements (U+2580–U+259F), drawn
//! procedurally at exactly one cell.
//!
//! Font glyphs for these characters are sized to the font's em box, not to
//! the cell (whose width and height are rounded to whole pixels and whose
//! height may include leading), so adjacent cells would show seams. Every
//! bitmap here is `width × height` of the cell, lines are centred with
//! integer thickness, and arms run to the cell edge, so neighbouring cells
//! join without gaps or overlaps.

/// `true` for the characters [`rasterize`] draws.
pub(crate) fn is_builtin(ch: char) -> bool {
    ('\u{2500}'..='\u{259F}').contains(&ch)
}

/// The weight of one arm of a line character.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arm {
    None,
    Light,
    Heavy,
    Double,
}

/// Arms for U+2500..=U+254B and U+2550..=U+256C, U+2574..=U+257F, as
/// `[up, right, down, left]` encoded as `.`, `L`ight, `H`eavy, `D`ouble.
/// Dashed lines (U+2504..=U+250B) are drawn separately and carry `None`.
const LINES: [&[u8; 4]; 76] = [
    b".L.L", b".H.H", b"L.L.", b"H.H.", b"....", b"....", b"....", b"....", // 2500
    b"....", b"....", b"....", b"....", b".LL.", b".HL.", b".LH.", b".HH.", // 2508
    b"..LL", b"..LH", b"..HL", b"..HH", b"LL..", b"LH..", b"HL..", b"HH..", // 2510
    b"L..L", b"L..H", b"H..L", b"H..H", b"LLL.", b"LHL.", b"HLL.", b"LLH.", // 2518
    b"HLH.", b"HHL.", b"LHH.", b"HHH.", b"L.LL", b"L.LH", b"H.LL", b"L.HL", // 2520
    b"H.HL", b"H.LH", b"L.HH", b"H.HH", b".LLL", b".LLH", b".HLL", b".HLH", // 2528
    b".LHL", b".LHH", b".HHL", b".HHH", b"LL.L", b"LL.H", b"LH.L", b"LH.H", // 2530
    b"HL.L", b"HL.H", b"HH.L", b"HH.H", b"LLLL", b"LLLH", b"LHLL", b"LHLH", // 2538
    b"HLLL", b"LLHL", b"HLHL", b"HLLH", b"HHLL", b"LLHH", b"LHHL", b"HHLH", // 2540
    b"LHHH", b"HLHH", b"HHHL", b"HHHH", // 2548..=254B
];

/// U+2550..=U+256C.
const DOUBLES: [&[u8; 4]; 29] = [
    b".D.D", b"D.D.", b".DL.", b".LD.", b".DD.", b"..LD", b"..DL", b"..DD", // 2550
    b"LD..", b"DL..", b"DD..", b"L..D", b"D..L", b"D..D", b"LDL.", b"DLD.", // 2558
    b"DDD.", b"L.LD", b"D.DL", b"D.DD", b".DLD", b".LDL", b".DDD", b"LD.D", // 2560
    b"DL.L", b"DD.D", b"LDLD", b"DLDL", b"DDDD", // 2568..=256C
];

/// U+2574..=U+257F.
const HALVES: [&[u8; 4]; 12] = [
    b"...L", b"L...", b".L..", b"..L.", b"...H", b"H...", b".H..", b"..H.", // 2574
    b".H.L", b"L.H.", b".L.H", b"H.L.", // 257C
];

/// A coverage bitmap with max-blend drawing primitives.
struct Canvas {
    width: u32,
    height: u32,
    data: Vec<u8>,
}

impl Canvas {
    fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            data: vec![0; width as usize * height as usize],
        }
    }

    /// Index of pixel `(x, y)`, computed in `usize` so large cells cannot
    /// overflow `u32`.
    fn index(&self, x: u32, y: u32) -> usize {
        y as usize * self.width as usize + x as usize
    }

    /// Sets `[x0, x1) × [y0, y1)` (clamped to the canvas) to at least `a`.
    fn fill(&mut self, x0: u32, y0: u32, x1: u32, y1: u32, a: u8) {
        let (x1, y1) = (x1.min(self.width), y1.min(self.height));
        for y in y0..y1 {
            for x in x0..x1 {
                let i = self.index(x, y);
                self.data[i] = self.data[i].max(a);
            }
        }
    }

    /// Clears `[x0, x1) × [y0, y1)` (clamped to the canvas).
    fn erase(&mut self, x0: u32, y0: u32, x1: u32, y1: u32) {
        let (x1, y1) = (x1.min(self.width), y1.min(self.height));
        for y in y0..y1 {
            for x in x0..x1 {
                let i = self.index(x, y);
                self.data[i] = 0;
            }
        }
    }

    /// Max-blends `coverage(x, y)` (sampled at pixel centres, `0.0..=1.0`)
    /// into every pixel.
    fn plot(&mut self, coverage: impl Fn(f32, f32) -> f32) {
        for y in 0..self.height {
            for x in 0..self.width {
                #[expect(
                    clippy::cast_precision_loss,
                    reason = "cell dimensions are far below 2^24"
                )]
                let c = coverage(x as f32 + 0.5, y as f32 + 0.5);
                if c > 0.0 {
                    let i = self.index(x, y);
                    self.data[i] = self.data[i].max(unit_to_byte(c));
                }
            }
        }
    }
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the value is clamped to 0..=255 first"
)]
fn unit_to_byte(c: f32) -> u8 {
    (c.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// Rounds a non-negative pixel coordinate.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "callers pass coordinates inside a cell, clamped to be non-negative"
)]
fn px(v: f32) -> u32 {
    v.max(0.0).round() as u32
}

#[expect(
    clippy::cast_precision_loss,
    reason = "cell dimensions are far below 2^24"
)]
fn f(v: u32) -> f32 {
    v as f32
}

/// Light line thickness for a cell: one pixel per 8 pixels of width
/// (rounded), at least one.
fn light_thickness(width: u32) -> u32 {
    (width.saturating_add(4) / 8).max(1)
}

/// `n` eighths of `len`, rounded, at least one pixel.
const fn eighths(len: u32, n: u32) -> u32 {
    let v = (len * n + 4) / 8;
    if v == 0 { 1 } else { v }
}

/// Draws `ch` into a `width × height` coverage bitmap, row-major, one byte
/// per pixel. `None` for characters outside [`is_builtin`] or an empty cell.
pub(crate) fn rasterize(ch: char, width: u32, height: u32) -> Option<Vec<u8>> {
    if !is_builtin(ch) || width == 0 || height == 0 {
        return None;
    }
    let mut c = Canvas::new(width, height);
    let t = light_thickness(width);
    let code = u32::from(ch);
    match code {
        0x2504..=0x250B | 0x254C..=0x254F => dashed(&mut c, code, t),
        0x2500..=0x254B => lines(&mut c, *LINES[(code - 0x2500) as usize], t),
        0x2550..=0x256C => lines(&mut c, *DOUBLES[(code - 0x2550) as usize], t),
        0x256D..=0x2570 => arc(&mut c, code, t),
        0x2571..=0x2573 => diagonal(&mut c, code, t),
        0x2574..=0x257F => lines(&mut c, *HALVES[(code - 0x2574) as usize], t),
        _ => block(&mut c, code),
    }
    Some(c.data)
}

fn arm(code: u8) -> Arm {
    match code {
        b'L' => Arm::Light,
        b'H' => Arm::Heavy,
        b'D' => Arm::Double,
        _ => Arm::None,
    }
}

const fn thickness(arm: Arm, t: u32) -> u32 {
    match arm {
        Arm::None => 0,
        Arm::Light => t,
        Arm::Heavy => t * 2,
        Arm::Double => t * 3,
    }
}

/// Start of a centred band of thickness `k` in a span of `len` pixels.
const fn centred(len: u32, k: u32) -> u32 {
    len.saturating_sub(k) / 2
}

/// Draws the four arms of a line character.
///
/// Arms are filled as bands running from the cell edge through the
/// junction; a double arm is a band three light lines thick whose middle
/// stripe is then erased up to the junction, which yields correct corners,
/// tees and crosses for every mix of single and double arms.
fn lines(canvas: &mut Canvas, spec: [u8; 4], light: u32) {
    let [up, right, down, left] = spec.map(arm);
    let (w, h) = (canvas.width, canvas.height);
    let kh = thickness(left, light).max(thickness(right, light));
    let kv = thickness(up, light).max(thickness(down, light));
    let (has_h, has_v) = (kh > 0, kv > 0);
    let (hy0, vx0) = (centred(h, kh), centred(w, kv));
    let (hy1, vx1) = (hy0 + kh, vx0 + kv);

    for (weight, is_up) in [(up, true), (down, false)] {
        let thick = thickness(weight, light);
        if thick == 0 {
            continue;
        }
        let x0 = centred(w, thick);
        if is_up {
            let end = if has_h {
                hy1
            } else {
                centred(h, thick) + thick
            };
            canvas.fill(x0, 0, x0 + thick, end, 255);
        } else {
            let start = if has_h { hy0 } else { centred(h, thick) };
            canvas.fill(x0, start, x0 + thick, h, 255);
        }
    }
    for (weight, is_left) in [(left, true), (right, false)] {
        let thick = thickness(weight, light);
        if thick == 0 {
            continue;
        }
        let y0 = centred(h, thick);
        if is_left {
            let end = if has_v {
                vx1
            } else {
                centred(w, thick) + thick
            };
            canvas.fill(0, y0, end, y0 + thick, 255);
        } else {
            let start = if has_v { vx0 } else { centred(w, thick) };
            canvas.fill(start, y0, w, y0 + thick, 255);
        }
    }

    let h_double = left == Arm::Double || right == Arm::Double;
    let v_double = up == Arm::Double || down == Arm::Double;
    let band = 3 * light;
    if up == Arm::Double {
        let x0 = centred(w, band) + light;
        let end = if h_double {
            hy0 + 2 * light
        } else if has_h {
            hy0
        } else {
            h / 2
        };
        canvas.erase(x0, 0, x0 + light, end);
    }
    if down == Arm::Double {
        let x0 = centred(w, band) + light;
        let start = if h_double {
            hy0 + light
        } else if has_h {
            hy1
        } else {
            h / 2
        };
        canvas.erase(x0, start, x0 + light, h);
    }
    if left == Arm::Double {
        let y0 = centred(h, band) + light;
        let end = if v_double {
            vx0 + 2 * light
        } else if has_v {
            vx0
        } else {
            w / 2
        };
        canvas.erase(0, y0, end, y0 + light);
    }
    if right == Arm::Double {
        let y0 = centred(h, band) + light;
        let start = if v_double {
            vx0 + light
        } else if has_v {
            vx1
        } else {
            w / 2
        };
        canvas.erase(start, y0, w, y0 + light);
    }
}

/// U+2504..=U+250B and U+254C..=U+254F: lines broken into 2, 3 or 4
/// dashes, each centred in an equal share of the cell.
fn dashed(c: &mut Canvas, code: u32, t: u32) {
    // (dash count, heavy, horizontal)
    let (n, heavy, horizontal) = match code {
        0x2504 => (3, false, true),
        0x2505 => (3, true, true),
        0x2506 => (3, false, false),
        0x2507 => (3, true, false),
        0x2508 => (4, false, true),
        0x2509 => (4, true, true),
        0x250A => (4, false, false),
        0x250B => (4, true, false),
        0x254C => (2, false, true),
        0x254D => (2, true, true),
        0x254E => (2, false, false),
        _ => (2, true, false),
    };
    let k = if heavy { 2 * t } else { t };
    let len = if horizontal { c.width } else { c.height };
    let gap = (len / (2 * n)).max(1);
    for i in 0..n {
        let start = len * i / n + gap / 2;
        let end = (len * (i + 1) / n)
            .saturating_sub(gap - gap / 2)
            .max(start + 1);
        if horizontal {
            let y0 = centred(c.height, k);
            c.fill(start, y0, end, y0 + k, 255);
        } else {
            let x0 = centred(c.width, k);
            c.fill(x0, start, x0 + k, end, 255);
        }
    }
}

/// U+256D..=U+2570: a quarter circle joining the light vertical and
/// horizontal lines, continued straight to the cell edges, anti-aliased.
fn arc(canvas: &mut Canvas, code: u32, light: u32) {
    // (vertical direction, horizontal direction): +1 = down / right.
    let (vy, hx): (f32, f32) = match code {
        0x256D => (1.0, 1.0),
        0x256E => (1.0, -1.0),
        0x256F => (-1.0, -1.0),
        _ => (-1.0, 1.0),
    };
    let (w, h) = (canvas.width, canvas.height);
    let vx0 = centred(w, light);
    let hy0 = centred(h, light);
    let xc = f(vx0) + f(light) / 2.0;
    let yc = f(hy0) + f(light) / 2.0;
    let room_x = if hx > 0.0 { f(w) - xc } else { xc };
    let room_y = if vy > 0.0 { f(h) - yc } else { yc };
    // Leave at least one pixel of straight line at each edge so the arc
    // meets the neighbouring cell's line at full coverage.
    let radius = (room_x.min(room_y) - 1.0).max(0.5);
    let (cx, cy) = (xc + hx * radius, yc + vy * radius);
    let half = f(light) / 2.0;
    canvas.plot(|x, y| {
        // Only the quadrant of the circle that faces the cell centre.
        if (x - cx) * hx > 0.0 || (y - cy) * vy > 0.0 {
            return 0.0;
        }
        let dist = ((x - cx).hypot(y - cy) - radius).abs();
        half + 0.5 - dist
    });
    // Straight continuations from the arc ends to the edges.
    if vy > 0.0 {
        canvas.fill(vx0, px(cy), vx0 + light, h, 255);
    } else {
        canvas.fill(vx0, 0, vx0 + light, px(cy), 255);
    }
    if hx > 0.0 {
        canvas.fill(px(cx), hy0, w, hy0 + light, 255);
    } else {
        canvas.fill(0, hy0, px(cx), hy0 + light, 255);
    }
}

/// U+2571..=U+2573: corner-to-corner diagonals, anti-aliased.
fn diagonal(c: &mut Canvas, code: u32, t: u32) {
    let (w, h) = (f(c.width), f(c.height));
    let half = f(t) / 2.0;
    let len = w.hypot(h);
    // Distance from (x, y) to the line through (0, 0)-(w, h) or (w, 0)-(0, h).
    let falling = move |x: f32, y: f32| (h * x - w * y).abs() / len;
    let rising = move |x: f32, y: f32| (h * x + w * y - w * h).abs() / len;
    let draw_rising = code != 0x2572;
    let draw_falling = code != 0x2571;
    c.plot(|x, y| {
        let mut cov: f32 = 0.0;
        if draw_rising {
            cov = cov.max(half + 0.5 - rising(x, y));
        }
        if draw_falling {
            cov = cov.max(half + 0.5 - falling(x, y));
        }
        cov
    });
}

/// U+2580..=U+259F: eighths, halves, quadrants and shades.
fn block(c: &mut Canvas, code: u32) {
    let (w, h) = (c.width, c.height);
    let eighth_h = |n: u32| eighths(h, n);
    let eighth_w = |n: u32| eighths(w, n);
    let (mx, my) = (w / 2, h / 2);
    match code {
        0x2580 => c.fill(0, 0, w, my, 255),
        0x2581..=0x2588 => c.fill(0, h - eighth_h(code - 0x2580), w, h, 255),
        0x2589..=0x258F => c.fill(0, 0, eighth_w(0x2590 - code), h, 255),
        0x2590 => c.fill(mx, 0, w, h, 255),
        0x2591 => c.fill(0, 0, w, h, 64),
        0x2592 => c.fill(0, 0, w, h, 128),
        0x2593 => c.fill(0, 0, w, h, 191),
        0x2594 => c.fill(0, 0, w, eighth_h(1), 255),
        0x2595 => c.fill(w - eighth_w(1), 0, w, h, 255),
        _ => {
            // Quadrants: bit 0 upper-left, 1 upper-right, 2 lower-left,
            // 3 lower-right.
            let quads: u8 = match code {
                0x2596 => 0b0100,
                0x2597 => 0b1000,
                0x2598 => 0b0001,
                0x2599 => 0b1101,
                0x259A => 0b1001,
                0x259B => 0b0111,
                0x259C => 0b1011,
                0x259D => 0b0010,
                0x259E => 0b0110,
                _ => 0b1110,
            };
            let rects = [
                (0, 0, mx, my),
                (mx, 0, w, my),
                (0, my, mx, h),
                (mx, my, w, h),
            ];
            for (bit, (x0, y0, x1, y1)) in rects.into_iter().enumerate() {
                if quads & (1 << bit) != 0 {
                    c.fill(x0, y0, x1, y1, 255);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full(data: &[u8]) -> usize {
        data.iter().map(|&a| usize::from(a == 255)).sum()
    }

    fn draw(ch: char, w: u32, h: u32) -> Vec<u8> {
        rasterize(ch, w, h).expect("builtin")
    }

    fn row(data: &[u8], w: u32, y: u32) -> &[u8] {
        &data[(y * w) as usize..((y + 1) * w) as usize]
    }

    fn col(data: &[u8], w: u32, h: u32, x: u32) -> Vec<u8> {
        (0..h).map(|y| data[(y * w + x) as usize]).collect()
    }

    #[test]
    fn only_box_and_block_ranges_are_builtin() {
        assert!(is_builtin('\u{2500}'));
        assert!(is_builtin('\u{259F}'));
        assert!(!is_builtin('\u{24FF}'));
        assert!(!is_builtin('\u{25A0}'));
        assert!(rasterize('a', 8, 16).is_none());
        assert!(rasterize('\u{2500}', 0, 16).is_none());
    }

    #[test]
    fn horizontal_line_spans_the_full_width_in_every_cell_size() {
        for (w, h) in [(8, 15), (12, 24), (16, 30), (7, 13), (1, 1)] {
            let data = draw('\u{2500}', w, h);
            let t = light_thickness(w);
            let y0 = centred(h, t);
            for y in y0..y0 + t {
                assert!(
                    row(&data, w, y).iter().all(|&a| a == 255),
                    "{w}x{h} row {y}"
                );
            }
            let ink: usize = data.iter().filter(|&&a| a > 0).count();
            assert_eq!(ink, (w * t) as usize, "{w}x{h}: only the line is inked");
        }
    }

    #[test]
    fn vertical_line_spans_the_full_height() {
        for (w, h) in [(8, 15), (12, 24), (16, 30)] {
            let data = draw('\u{2502}', w, h);
            let x0 = centred(w, light_thickness(w));
            assert!(col(&data, w, h, x0).iter().all(|&a| a == 255), "{w}x{h}");
        }
    }

    #[test]
    fn corner_arms_reach_their_edges_and_meet() {
        let (w, h) = (8, 16);
        let data = draw('\u{250C}', w, h); // ┌
        let t = light_thickness(w);
        let (x0, y0) = (centred(w, t), centred(h, t));
        // Right arm reaches the right edge, down arm the bottom edge.
        assert_eq!(data[(y0 * w + w - 1) as usize], 255);
        assert_eq!(data[((h - 1) * w + x0) as usize], 255);
        // Nothing left of the vertical line or above the horizontal one.
        assert!(row(&data, w, 0).iter().all(|&a| a == 0));
        assert!(col(&data, w, h, 0).iter().all(|&a| a == 0));
        // The junction pixel is inked.
        assert_eq!(data[(y0 * w + x0) as usize], 255);
    }

    #[test]
    fn double_horizontal_is_two_full_width_lines() {
        let (w, h) = (16, 30);
        let t = light_thickness(w);
        let data = draw('\u{2550}', w, h); // ═
        let y0 = centred(h, 3 * t);
        for y in (y0..y0 + t).chain(y0 + 2 * t..y0 + 3 * t) {
            assert!(row(&data, w, y).iter().all(|&a| a == 255), "row {y}");
        }
        for y in y0 + t..y0 + 2 * t {
            assert!(row(&data, w, y).iter().all(|&a| a == 0), "gap row {y}");
        }
    }

    #[test]
    fn double_corner_has_an_open_inside() {
        let (w, h) = (16, 30);
        let t = light_thickness(w);
        let data = draw('\u{2554}', w, h); // ╔
        let (x0, y0) = (centred(w, 3 * t), centred(h, 3 * t));
        // The outer corner pixel and the inner corner pixel are inked; the
        // gap between them is not.
        assert_eq!(data[(y0 * w + x0) as usize], 255);
        assert_eq!(data[((y0 + 2 * t) * w + x0 + 2 * t) as usize], 255);
        assert_eq!(data[((y0 + t) * w + x0 + t) as usize], 0);
        assert_eq!(data[((y0 + t) * w + w - 1) as usize], 0);
    }

    #[test]
    fn full_block_and_halves_cover_exact_areas() {
        let (w, h) = (9, 17);
        assert!(draw('\u{2588}', w, h).iter().all(|&a| a == 255));
        let upper = draw('\u{2580}', w, h);
        assert_eq!(full(&upper), (w * (h / 2)) as usize);
        let lower = draw('\u{2584}', w, h);
        let left = draw('\u{258C}', w, h);
        assert_eq!(full(&lower), (w * eighths(h, 4)) as usize);
        assert_eq!(full(&left), (h * eighths(w, 4)) as usize);
    }

    #[test]
    fn every_builtin_character_draws_something() {
        for code in 0x2500..=0x259F {
            let ch = char::from_u32(code).expect("valid");
            for (w, h) in [(8, 15), (16, 30), (3, 5)] {
                let data = draw(ch, w, h);
                assert_eq!(data.len(), (w * h) as usize);
                assert!(
                    data.iter().any(|&a| a > 0),
                    "U+{code:04X} at {w}x{h} is blank"
                );
            }
        }
    }

    #[test]
    fn rounded_corner_meets_the_straight_lines_at_the_edges() {
        let (w, h) = (8, 16);
        let t = light_thickness(w);
        let data = draw('\u{256D}', w, h); // ╭
        let (x0, y0) = (centred(w, t), centred(h, t));
        assert_eq!(data[((h - 1) * w + x0) as usize], 255, "bottom edge");
        assert_eq!(data[(y0 * w + w - 1) as usize], 255, "right edge");
        assert_eq!(data[0], 0, "top-left corner stays empty");
    }
}
