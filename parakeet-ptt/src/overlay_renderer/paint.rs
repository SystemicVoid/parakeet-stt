//! Pixel primitives for the Overlay frame: premultiplied ARGB8888 blending,
//! rounded rectangles, a soft shadow, glyph bitmaps, hairlines, discs and a
//! max-blend coverage canvas for anti-aliased strokes.

/// An RGB colour, 0..=255 per channel.
pub(super) type Rgb = [u8; 3];

/// A mutable view over the surface buffer (little-endian ARGB8888, premultiplied).
pub(super) struct Frame<'a> {
    pub bytes: &'a mut [u8],
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub(super) fn right(&self) -> f32 {
        self.x + self.w
    }

    pub(super) fn bottom(&self) -> f32 {
        self.y + self.h
    }
}

/// Premultiplied pixel in memory order [B, G, R, A].
pub(super) fn argb_pixel_premul(rgb: Rgb, a: u8) -> [u8; 4] {
    let aa = u16::from(a);
    [
        ((u16::from(rgb[2]) * aa) / 255) as u8,
        ((u16::from(rgb[1]) * aa) / 255) as u8,
        ((u16::from(rgb[0]) * aa) / 255) as u8,
        a,
    ]
}

impl Frame<'_> {
    pub(super) fn clear(&mut self) {
        self.bytes.fill(0);
    }

    /// Source-over blend of a premultiplied pixel.
    /// Scales every premultiplied channel by `factor` (0..=1): CSS-style opacity for
    /// the whole frame, applied once after everything is drawn.
    pub(super) fn scale_alpha(&mut self, factor: f32) {
        let f = (factor.clamp(0.0, 1.0) * 255.0).round() as u32;
        if f >= 255 {
            return;
        }
        for byte in self.bytes.iter_mut() {
            *byte = ((*byte as u32 * f + 127) / 255) as u8;
        }
    }

    pub(super) fn blend_premul(&mut self, x: i32, y: i32, color: [u8; 4]) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }
        let sa = color[3];
        if sa == 0 {
            return;
        }
        let idx = ((y as u32 * self.width + x as u32) * 4) as usize;
        if idx + 3 >= self.bytes.len() {
            return;
        }
        let inv = 255 - u16::from(sa);
        for (dst, src) in self.bytes[idx..idx + 4].iter_mut().zip(color) {
            *dst = (u16::from(src) + (u16::from(*dst) * inv) / 255) as u8;
        }
    }

    /// Source-over blend of a straight colour at a fractional alpha.
    pub(super) fn blend(&mut self, x: i32, y: i32, rgb: Rgb, alpha: f32) {
        let a = (alpha.clamp(0.0, 1.0) * 255.0).round() as u8;
        if a == 0 {
            return;
        }
        self.blend_premul(x, y, argb_pixel_premul(rgb, a));
    }

    /// Fills a rounded rectangle with a vertical gradient from `top` to `bottom`.
    pub(super) fn fill_rounded_rect_gradient(
        &mut self,
        rect: Rect,
        radius: f32,
        top: Rgb,
        bottom: Rgb,
        alpha: f32,
    ) {
        let x0 = (rect.x.floor() as i32).max(0);
        let y0 = (rect.y.floor() as i32).max(0);
        let x1 = (rect.right().ceil() as i32).min(self.width as i32);
        let y1 = (rect.bottom().ceil() as i32).min(self.height as i32);
        for py in y0..y1 {
            let t = if rect.h > 1.0 {
                ((py as f32 + 0.5 - rect.y) / rect.h).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let rgb = lerp_rgb(top, bottom, t);
            for px in x0..x1 {
                let cov = rounded_rect_coverage(px as f32 + 0.5, py as f32 + 0.5, rect, radius);
                if cov > 0.0 {
                    self.blend(px, py, rgb, alpha * cov);
                }
            }
        }
    }

    /// Strokes a 1 px inset border just inside a rounded rectangle.
    pub(super) fn stroke_rounded_rect_inset(
        &mut self,
        rect: Rect,
        radius: f32,
        rgb: Rgb,
        alpha: f32,
    ) {
        let x0 = (rect.x.floor() as i32).max(0);
        let y0 = (rect.y.floor() as i32).max(0);
        let x1 = (rect.right().ceil() as i32).min(self.width as i32);
        let y1 = (rect.bottom().ceil() as i32).min(self.height as i32);
        let inner = Rect {
            x: rect.x + 1.0,
            y: rect.y + 1.0,
            w: (rect.w - 2.0).max(0.0),
            h: (rect.h - 2.0).max(0.0),
        };
        for py in y0..y1 {
            for px in x0..x1 {
                let fx = px as f32 + 0.5;
                let fy = py as f32 + 0.5;
                let outer = rounded_rect_coverage(fx, fy, rect, radius);
                let hole = rounded_rect_coverage(fx, fy, inner, (radius - 1.0).max(0.0));
                let cov = (outer - hole).max(0.0);
                if cov > 0.0 {
                    self.blend(px, py, rgb, alpha * cov);
                }
            }
        }
    }

    /// A soft shadow under a rounded rectangle: `offset_y` below it, fading out over
    /// `blur` px from the edge. Pixels covered by the rectangle itself are skipped.
    pub(super) fn draw_shadow(
        &mut self,
        rect: Rect,
        radius: f32,
        offset_y: f32,
        blur: f32,
        rgb: Rgb,
        alpha: f32,
    ) {
        if alpha <= 0.0 {
            return;
        }
        let shadow_rect = Rect {
            y: rect.y + offset_y,
            ..rect
        };
        let half = blur * 0.5;
        let x0 = ((shadow_rect.x - half).floor() as i32).max(0);
        let y0 = ((shadow_rect.y - half).floor() as i32).max(0);
        let x1 = ((shadow_rect.right() + half).ceil() as i32).min(self.width as i32);
        let y1 = ((shadow_rect.bottom() + half).ceil() as i32).min(self.height as i32);
        for py in y0..y1 {
            for px in x0..x1 {
                let fx = px as f32 + 0.5;
                let fy = py as f32 + 0.5;
                if rounded_rect_coverage(fx, fy, rect, radius) >= 1.0 {
                    continue;
                }
                let d = signed_distance_to_rounded_rect(fx, fy, shadow_rect, radius);
                // Smooth falloff centred on the edge, spanning [-half, +half].
                let t = ((d + half) / blur).clamp(0.0, 1.0);
                let falloff = 1.0 - smoothstep(t);
                if falloff <= 0.002 {
                    continue;
                }
                self.blend(px, py, rgb, alpha * falloff);
            }
        }
    }

    /// Blends an 8-bit coverage bitmap (a rasterised glyph) with a colour.
    pub(super) fn blend_bitmap(
        &mut self,
        origin: (i32, i32),
        size: (usize, usize),
        coverage: &[u8],
        rgb: Rgb,
        alpha: f32,
    ) {
        let a = (alpha.clamp(0.0, 1.0) * 255.0).round() as u8;
        if a == 0 {
            return;
        }
        let color = argb_pixel_premul(rgb, a);
        let (origin_x, origin_y) = origin;
        let (width, height) = size;
        for y in 0..height {
            let draw_y = origin_y + y as i32;
            if draw_y < 0 || draw_y >= self.height as i32 {
                continue;
            }
            for x in 0..width {
                let draw_x = origin_x + x as i32;
                if draw_x < 0 || draw_x >= self.width as i32 {
                    continue;
                }
                let cov = u16::from(coverage[y * width + x]);
                if cov == 0 {
                    continue;
                }
                let pixel = [
                    ((cov * u16::from(color[0])) / 255) as u8,
                    ((cov * u16::from(color[1])) / 255) as u8,
                    ((cov * u16::from(color[2])) / 255) as u8,
                    ((cov * u16::from(color[3])) / 255) as u8,
                ];
                self.blend_premul(draw_x, draw_y, pixel);
            }
        }
    }

    /// A 1 px horizontal rule from `x0` to `x1` (fractional ends get partial coverage).
    pub(super) fn hline(&mut self, x0: f32, x1: f32, y: i32, rgb: Rgb, alpha: f32) {
        if x1 <= x0 || alpha <= 0.0 {
            return;
        }
        let start = x0.floor() as i32;
        let end = x1.ceil() as i32;
        for px in start..end {
            let cov = (x1.min(px as f32 + 1.0) - x0.max(px as f32)).clamp(0.0, 1.0);
            self.blend(px, y, rgb, alpha * cov);
        }
    }

    /// A 1 px vertical bar from `y0` to `y1`.
    pub(super) fn vline(&mut self, x: i32, y0: f32, y1: f32, rgb: Rgb, alpha: f32) {
        if y1 <= y0 || alpha <= 0.0 {
            return;
        }
        let start = y0.floor() as i32;
        let end = y1.ceil() as i32;
        for py in start..end {
            let cov = (y1.min(py as f32 + 1.0) - y0.max(py as f32)).clamp(0.0, 1.0);
            self.blend(x, py, rgb, alpha * cov);
        }
    }

    /// An anti-aliased filled disc.
    pub(super) fn fill_disc(&mut self, cx: f32, cy: f32, r: f32, rgb: Rgb, alpha: f32) {
        if r <= 0.0 || alpha <= 0.0 {
            return;
        }
        let x0 = (cx - r - 1.0).floor() as i32;
        let x1 = (cx + r + 1.0).ceil() as i32;
        let y0 = (cy - r - 1.0).floor() as i32;
        let y1 = (cy + r + 1.0).ceil() as i32;
        for py in y0..y1 {
            for px in x0..x1 {
                let dx = px as f32 + 0.5 - cx;
                let dy = py as f32 + 0.5 - cy;
                let cov = (r + 0.5 - (dx * dx + dy * dy).sqrt()).clamp(0.0, 1.0);
                if cov > 0.0 {
                    self.blend(px, py, rgb, alpha * cov);
                }
            }
        }
    }
}

/// Coverage 0..1 of the pixel centred at (px, py) against a rounded rectangle.
pub(super) fn rounded_rect_coverage(px: f32, py: f32, rect: Rect, radius: f32) -> f32 {
    let r = radius.min(rect.w / 2.0).min(rect.h / 2.0).max(0.0);
    let d = signed_distance_to_rounded_rect(px, py, rect, r);
    (0.5 - d).clamp(0.0, 1.0)
}

/// Signed distance from a point to the edge of a rounded rectangle (negative inside).
pub(super) fn signed_distance_to_rounded_rect(px: f32, py: f32, rect: Rect, radius: f32) -> f32 {
    let r = radius.min(rect.w / 2.0).min(rect.h / 2.0).max(0.0);
    let hw = rect.w / 2.0 - r;
    let hh = rect.h / 2.0 - r;
    let cx = rect.x + rect.w / 2.0;
    let cy = rect.y + rect.h / 2.0;
    let qx = (px - cx).abs() - hw;
    let qy = (py - cy).abs() - hh;
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    let inside = qx.max(qy).min(0.0);
    outside + inside - r
}

fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

pub(super) fn lerp_rgb(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    let mix = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round() as u8;
    [mix(a[0], b[0]), mix(a[1], b[1]), mix(a[2], b[2])]
}

/// A floating-point coverage canvas. Strokes accumulate with a per-pixel maximum, so
/// overlapping segments of one polyline never bead at the joints and a back stroke
/// stays under a front stroke instead of darkening it.
pub(super) struct Coverage {
    pub width: usize,
    pub height: usize,
    data: Vec<f32>,
}

impl Coverage {
    pub(super) fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            data: vec![0.0; width * height],
        }
    }

    pub(super) fn clear(&mut self) {
        self.data.fill(0.0);
    }

    fn max_at(&mut self, x: i32, y: i32, value: f32) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }
        let idx = y as usize * self.width + x as usize;
        if value > self.data[idx] {
            self.data[idx] = value;
        }
    }

    /// Anti-aliased line segment with round caps, `width` px wide, at `alpha`.
    pub(super) fn stroke_segment(
        &mut self,
        (x0, y0): (f32, f32),
        (x1, y1): (f32, f32),
        width: f32,
        alpha: f32,
    ) {
        if alpha <= 0.0 {
            return;
        }
        let half = width * 0.5;
        let pad = half + 1.0;
        let min_x = (x0.min(x1) - pad).floor() as i32;
        let max_x = (x0.max(x1) + pad).ceil() as i32;
        let min_y = (y0.min(y1) - pad).floor() as i32;
        let max_y = (y0.max(y1) + pad).ceil() as i32;
        let dx = x1 - x0;
        let dy = y1 - y0;
        let len_sq = dx * dx + dy * dy;
        for py in min_y..=max_y {
            for px in min_x..=max_x {
                let fx = px as f32 + 0.5;
                let fy = py as f32 + 0.5;
                let t = if len_sq > 0.0 {
                    (((fx - x0) * dx + (fy - y0) * dy) / len_sq).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let cx = x0 + dx * t;
                let cy = y0 + dy * t;
                let dist = ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt();
                let cov = (half + 0.5 - dist).clamp(0.0, 1.0);
                if cov > 0.0 {
                    self.max_at(px, py, cov * alpha);
                }
            }
        }
    }

    /// Composites the canvas onto the frame at `(dst_x, dst_y)`, box-downsampling by
    /// `scale` (a 120x120 canvas at scale 2 lands in a 60x60 cell).
    pub(super) fn composite(
        &self,
        frame: &mut Frame,
        dst_x: i32,
        dst_y: i32,
        scale: usize,
        rgb: Rgb,
        alpha: f32,
    ) {
        if alpha <= 0.0 || scale == 0 {
            return;
        }
        let out_w = self.width / scale;
        let out_h = self.height / scale;
        let norm = 1.0 / (scale * scale) as f32;
        for oy in 0..out_h {
            for ox in 0..out_w {
                let mut sum = 0.0;
                for sy in 0..scale {
                    let row = (oy * scale + sy) * self.width + ox * scale;
                    sum += self.data[row..row + scale].iter().sum::<f32>();
                }
                let cov = sum * norm;
                if cov > 0.002 {
                    frame.blend(dst_x + ox as i32, dst_y + oy as i32, rgb, alpha * cov);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_of(width: u32, height: u32) -> Vec<u8> {
        vec![0u8; (width * height * 4) as usize]
    }

    fn pixel(bytes: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
        let idx = ((y * width + x) * 4) as usize;
        [bytes[idx], bytes[idx + 1], bytes[idx + 2], bytes[idx + 3]]
    }

    #[test]
    fn premultiplied_pixel_is_bgra_in_memory() {
        assert_eq!(argb_pixel_premul([255, 0, 0], 255), [0, 0, 255, 255]);
        assert_eq!(
            argb_pixel_premul([255, 255, 255], 128),
            [128, 128, 128, 128]
        );
    }

    #[test]
    fn rounded_rect_coverage_is_full_inside_and_zero_outside() {
        let rect = Rect {
            x: 10.0,
            y: 10.0,
            w: 40.0,
            h: 20.0,
        };
        assert_eq!(rounded_rect_coverage(30.0, 20.0, rect, 3.0), 1.0);
        assert_eq!(rounded_rect_coverage(5.0, 20.0, rect, 3.0), 0.0);
        let corner = rounded_rect_coverage(10.5, 10.5, rect, 3.0);
        assert!(
            corner < 0.5,
            "corner pixel should be mostly uncovered: {corner}"
        );
    }

    #[test]
    fn gradient_fill_interpolates_top_to_bottom() {
        let (w, h) = (4u32, 10u32);
        let mut bytes = frame_of(w, h);
        let mut frame = Frame {
            bytes: &mut bytes,
            width: w,
            height: h,
        };
        frame.fill_rounded_rect_gradient(
            Rect {
                x: 0.0,
                y: 0.0,
                w: 4.0,
                h: 10.0,
            },
            0.0,
            [200, 0, 0],
            [0, 0, 200],
            1.0,
        );
        let top = pixel(&bytes, w, 2, 0);
        let bottom = pixel(&bytes, w, 2, 9);
        assert!(top[2] > top[0], "top row should be red-dominant: {top:?}");
        assert!(
            bottom[0] > bottom[2],
            "bottom row should be blue-dominant: {bottom:?}"
        );
    }

    #[test]
    fn hline_gives_partial_coverage_to_fractional_ends() {
        let (w, h) = (10u32, 1u32);
        let mut bytes = frame_of(w, h);
        let mut frame = Frame {
            bytes: &mut bytes,
            width: w,
            height: h,
        };
        frame.hline(2.0, 5.5, 0, [255, 255, 255], 1.0);
        assert_eq!(pixel(&bytes, w, 1, 0)[3], 0);
        assert_eq!(pixel(&bytes, w, 3, 0)[3], 255);
        let end = pixel(&bytes, w, 5, 0)[3];
        assert!((120..=136).contains(&end), "half-covered end pixel: {end}");
        assert_eq!(pixel(&bytes, w, 6, 0)[3], 0);
    }

    #[test]
    fn stroke_segments_max_blend_instead_of_accumulating() {
        let mut canvas = Coverage::new(8, 8);
        canvas.stroke_segment((1.0, 4.5), (7.0, 4.5), 1.0, 0.5);
        let single = canvas.data[4 * 8 + 4];
        canvas.stroke_segment((1.0, 4.5), (7.0, 4.5), 1.0, 0.5);
        let twice = canvas.data[4 * 8 + 4];
        assert!(
            single > 0.4,
            "stroke should cover its centre pixel: {single}"
        );
        assert_eq!(single, twice, "overlapping strokes must not darken");
    }

    #[test]
    fn composite_downsamples_by_scale() {
        let mut canvas = Coverage::new(4, 4);
        canvas.stroke_segment((0.0, 1.0), (4.0, 1.0), 2.0, 1.0);
        let (w, h) = (2u32, 2u32);
        let mut bytes = frame_of(w, h);
        let mut frame = Frame {
            bytes: &mut bytes,
            width: w,
            height: h,
        };
        canvas.composite(&mut frame, 0, 0, 2, [0, 0, 0], 1.0);
        assert!(pixel(&bytes, w, 0, 0)[3] > 200, "top row is fully stroked");
        assert!(pixel(&bytes, w, 0, 1)[3] < 60, "bottom row is mostly empty");
    }

    #[test]
    fn shadow_is_darker_below_than_above() {
        let (w, h) = (60u32, 100u32);
        let mut bytes = frame_of(w, h);
        let mut frame = Frame {
            bytes: &mut bytes,
            width: w,
            height: h,
        };
        let rect = Rect {
            x: 20.0,
            y: 30.0,
            w: 20.0,
            h: 20.0,
        };
        frame.draw_shadow(rect, 3.0, 10.0, 20.0, [0, 0, 0], 0.5);
        let above = pixel(&bytes, w, 30, 26)[3];
        let below = pixel(&bytes, w, 30, 54)[3];
        assert!(
            below > above,
            "below {below} should be darker than above {above}"
        );
        assert_eq!(
            pixel(&bytes, w, 30, 40)[3],
            0,
            "inside the sheet stays clear"
        );
    }
}
