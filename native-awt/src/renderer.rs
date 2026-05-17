//! Software rasterizer — draws into ARGB pixel buffers.
//!
//! All drawing is done in pure Rust, producing `Vec<u32>` buffers
//! (ARGB format: bits 24-31=alpha, 16-23=red, 8-15=green, 0-7=blue).
//! Platform backends only need to blit the final buffer to the screen.

use std::f64::consts::PI;

// ── Supporting types ──────────────────────────────────────────────────

/// Axis-aligned integer rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self { x, y, width, height }
    }

    /// Returns `true` if the point (px, py) lies inside this rectangle.
    pub fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x
            && py >= self.y
            && px < self.x + self.width as i32
            && py < self.y + self.height as i32
    }

    /// Returns the intersection of two rectangles, or `None` if they don't overlap.
    pub fn intersect(&self, other: &Rect) -> Option<Rect> {
        let x1 = self.x.max(other.x);
        let y1 = self.y.max(other.y);
        let x2 = (self.x + self.width as i32).min(other.x + other.width as i32);
        let y2 = (self.y + self.height as i32).min(other.y + other.height as i32);
        if x2 > x1 && y2 > y1 {
            Some(Rect {
                x: x1,
                y: y1,
                width: (x2 - x1) as u32,
                height: (y2 - y1) as u32,
            })
        } else {
            None
        }
    }
}

/// 2D affine transform represented as a 3x2 matrix.
///
/// Transforms a point (x, y) as:
///   x' = m00*x + m01*y + m02
///   y' = m10*x + m11*y + m12
#[derive(Debug, Clone, Copy)]
pub struct AffineTransform {
    pub m00: f64,
    pub m01: f64,
    pub m02: f64,
    pub m10: f64,
    pub m11: f64,
    pub m12: f64,
}

impl PartialEq for AffineTransform {
    fn eq(&self, other: &Self) -> bool {
        (self.m00 - other.m00).abs() < 1e-10
            && (self.m01 - other.m01).abs() < 1e-10
            && (self.m02 - other.m02).abs() < 1e-10
            && (self.m10 - other.m10).abs() < 1e-10
            && (self.m11 - other.m11).abs() < 1e-10
            && (self.m12 - other.m12).abs() < 1e-10
    }
}

impl AffineTransform {
    pub fn identity() -> Self {
        Self {
            m00: 1.0, m01: 0.0, m02: 0.0,
            m10: 0.0, m11: 1.0, m12: 0.0,
        }
    }

    pub fn translate(dx: f64, dy: f64) -> Self {
        Self {
            m00: 1.0, m01: 0.0, m02: dx,
            m10: 0.0, m11: 1.0, m12: dy,
        }
    }

    pub fn rotate(theta: f64) -> Self {
        let c = theta.cos();
        let s = theta.sin();
        Self {
            m00: c, m01: -s, m02: 0.0,
            m10: s, m11: c,  m12: 0.0,
        }
    }

    pub fn scale(sx: f64, sy: f64) -> Self {
        Self {
            m00: sx, m01: 0.0, m02: 0.0,
            m10: 0.0, m11: sy, m12: 0.0,
        }
    }

    /// `self * other` — applies `other` first, then `self`.
    pub fn concatenate(&self, other: &AffineTransform) -> AffineTransform {
        AffineTransform {
            m00: self.m00 * other.m00 + self.m01 * other.m10,
            m01: self.m00 * other.m01 + self.m01 * other.m11,
            m02: self.m00 * other.m02 + self.m01 * other.m12 + self.m02,
            m10: self.m10 * other.m00 + self.m11 * other.m10,
            m11: self.m10 * other.m01 + self.m11 * other.m11,
            m12: self.m10 * other.m02 + self.m11 * other.m12 + self.m12,
        }
    }

    /// Transform a point.
    pub fn transform_point(&self, x: f64, y: f64) -> (f64, f64) {
        (
            self.m00 * x + self.m01 * y + self.m02,
            self.m10 * x + self.m11 * y + self.m12,
        )
    }

    /// Returns the inverse transform, or `None` if the matrix is singular.
    pub fn invert(&self) -> Option<AffineTransform> {
        let det = self.m00 * self.m11 - self.m01 * self.m10;
        if det.abs() < 1e-15 {
            return None;
        }
        let inv_det = 1.0 / det;
        Some(AffineTransform {
            m00: self.m11 * inv_det,
            m01: -self.m01 * inv_det,
            m02: (self.m01 * self.m12 - self.m02 * self.m11) * inv_det,
            m10: -self.m10 * inv_det,
            m11: self.m00 * inv_det,
            m12: (self.m02 * self.m10 - self.m00 * self.m12) * inv_det,
        })
    }

    pub fn is_identity(&self) -> bool {
        *self == Self::identity()
    }
}

/// Porter-Duff compositing modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompositeMode {
    /// Default: source over destination.
    SrcOver,
    /// Source replaces destination entirely (ignoring dst alpha).
    Src,
    /// Clears destination pixels.
    Clear,
    /// XOR compositing.
    Xor,
}

// ── ARGB helpers ──────────────────────────────────────────────────────

#[inline(always)]
fn argb_a(c: u32) -> u8 {
    (c >> 24) as u8
}
#[inline(always)]
fn argb_r(c: u32) -> u8 {
    (c >> 16) as u8
}
#[inline(always)]
fn argb_g(c: u32) -> u8 {
    (c >> 8) as u8
}
#[inline(always)]
fn argb_b(c: u32) -> u8 {
    c as u8
}
#[inline(always)]
fn make_argb(a: u8, r: u8, g: u8, b: u8) -> u32 {
    (a as u32) << 24 | (r as u32) << 16 | (g as u32) << 8 | b as u32
}

/// Porter-Duff SRC_OVER blend of a single channel.
/// All values in 0..=255.
#[inline(always)]
fn blend_channel(src: u8, dst: u8, src_a: u8, dst_a: u8, out_a: u32) -> u8 {
    if out_a == 0 {
        return 0;
    }
    let sa = src_a as u32;
    let da = dst_a as u32;
    let inv_sa = 255 - sa;
    // Porter-Duff SRC_OVER per-channel (integer 0..255 math):
    // out_c = (src_c * src_a + dst_c * dst_a * (255 - src_a) / 255) / out_a
    let val = (src as u32 * sa + dst as u32 * da * inv_sa / 255) / out_a;
    val.min(255) as u8
}

/// Full Porter-Duff compositing of src onto dst with the given mode.
#[inline]
fn composite(src: u32, dst: u32, mode: CompositeMode) -> u32 {
    match mode {
        CompositeMode::Src => src,
        CompositeMode::Clear => 0,
        CompositeMode::SrcOver => {
            let sa = argb_a(src);
            if sa == 255 {
                return src;
            }
            if sa == 0 {
                return dst;
            }
            let da = argb_a(dst);
            let inv_sa = 255u32 - sa as u32;
            let out_a = sa as u32 + da as u32 * inv_sa / 255;
            if out_a == 0 {
                return 0;
            }
            let out_r = blend_channel(argb_r(src), argb_r(dst), sa, da, out_a);
            let out_g = blend_channel(argb_g(src), argb_g(dst), sa, da, out_a);
            let out_b = blend_channel(argb_b(src), argb_b(dst), sa, da, out_a);
            make_argb(out_a.min(255) as u8, out_r, out_g, out_b)
        }
        CompositeMode::Xor => {
            let sa = argb_a(src) as u32;
            let da = argb_a(dst) as u32;
            let inv_sa = 255 - sa;
            let inv_da = 255 - da;
            let out_a = (sa * inv_da / 255 + da * inv_sa / 255).min(255);
            if out_a == 0 {
                return 0;
            }
            let blend_ch = |sc: u8, dc: u8| -> u8 {
                let val = (sc as u32 * sa * inv_da / 255
                    + dc as u32 * da * inv_sa / 255)
                    * 255
                    / out_a
                    / 255;
                val.min(255) as u8
            };
            make_argb(
                out_a as u8,
                blend_ch(argb_r(src), argb_r(dst)),
                blend_ch(argb_g(src), argb_g(dst)),
                blend_ch(argb_b(src), argb_b(dst)),
            )
        }
    }
}

// ── Row-level composite helpers (fast paths) ─────────────────────────
//
// These walk a `&mut [u32]` destination slice and a (solid color or
// source slice) and apply SRC_OVER in a tight scalar loop. Kept simple
// so the optimizer can vectorize / unroll; SIMD intrinsics can be
// dropped in later without touching the call sites.

/// SRC_OVER blend of a single solid `src_argb` into every pixel of `dst`.
/// Caller has already filtered out the fully-opaque and fully-transparent
/// edge cases (those are handled with a memset / no-op).
#[inline]
fn composite_row_src_over_solid(dst: &mut [u32], src_argb: u32) {
    for d in dst.iter_mut() {
        *d = composite(src_argb, *d, CompositeMode::SrcOver);
    }
}

/// SRC_OVER blend of `src` slice into `dst` slice (same length).
#[inline]
fn composite_row_src_over(dst: &mut [u32], src: &[u32]) {
    debug_assert_eq!(dst.len(), src.len());
    for (d, s) in dst.iter_mut().zip(src.iter()) {
        let sv = *s;
        let sa = argb_a(sv);
        // Branch on alpha so the common cases (fully opaque sprite pixel
        // and fully transparent pixel) don't pay the full blend cost.
        if sa == 255 {
            *d = sv;
        } else if sa != 0 {
            *d = composite(sv, *d, CompositeMode::SrcOver);
        }
    }
}

// ── SoftwareRenderer ──────────────────────────────────────────────────

pub struct SoftwareRenderer {
    pixels: Vec<u32>,
    width: u32,
    height: u32,
    clip: Option<Rect>,
    transform: AffineTransform,
    color: u32,
    stroke_width: f32,
    antialias: bool,
    composite_mode: CompositeMode,
}

impl SoftwareRenderer {
    /// Creates a new renderer with a buffer filled with transparent black.
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            pixels: vec![0x00000000; (width * height) as usize],
            width,
            height,
            clip: None,
            transform: AffineTransform::identity(),
            color: 0xFF000000, // opaque black
            stroke_width: 1.0,
            antialias: false,
            composite_mode: CompositeMode::SrcOver,
        }
    }

    /// Reallocates the pixel buffer.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        self.pixels = vec![0x00000000; (width * height) as usize];
    }

    pub fn pixels(&self) -> &[u32] {
        &self.pixels
    }
    pub fn pixels_mut(&mut self) -> &mut [u32] {
        &mut self.pixels
    }
    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn set_color(&mut self, argb: u32) {
        self.color = argb;
    }
    pub fn color(&self) -> u32 {
        self.color
    }

    pub fn set_clip(&mut self, rect: Option<Rect>) {
        self.clip = rect;
    }
    pub fn clip(&self) -> Option<Rect> {
        self.clip
    }

    pub fn set_transform(&mut self, t: AffineTransform) {
        self.transform = t;
    }
    pub fn transform(&self) -> &AffineTransform {
        &self.transform
    }

    pub fn set_stroke_width(&mut self, w: f32) {
        self.stroke_width = w;
    }
    pub fn set_antialias(&mut self, on: bool) {
        self.antialias = on;
    }
    pub fn set_composite(&mut self, mode: CompositeMode) {
        self.composite_mode = mode;
    }

    /// Fill the entire buffer with the given color.
    pub fn clear(&mut self, color: u32) {
        self.pixels.fill(color);
    }

    // ── Internal pixel write ──────────────────────────────────────

    /// Returns true if (x, y) passes clipping and bounds checks.
    #[inline]
    fn pixel_visible(&self, x: i32, y: i32) -> bool {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return false;
        }
        if let Some(ref clip) = self.clip {
            if !clip.contains(x, y) {
                return false;
            }
        }
        true
    }

    /// Write a pixel at buffer-space coordinates, applying compositing.
    #[inline]
    fn put_pixel(&mut self, x: i32, y: i32, color: u32) {
        if !self.pixel_visible(x, y) {
            return;
        }
        let idx = (y as u32 * self.width + x as u32) as usize;
        self.pixels[idx] = composite(color, self.pixels[idx], self.composite_mode);
    }

    /// Write a pixel with fractional alpha coverage (for antialiasing).
    #[inline]
    fn put_pixel_aa(&mut self, x: i32, y: i32, color: u32, coverage: f64) {
        if coverage <= 0.0 {
            return;
        }
        let a = argb_a(color) as f64 * coverage.min(1.0);
        let blended = make_argb(a as u8, argb_r(color), argb_g(color), argb_b(color));
        self.put_pixel(x, y, blended);
    }

    /// Transform a logical point to buffer coordinates.
    #[inline]
    fn tx(&self, x: f64, y: f64) -> (i32, i32) {
        if self.transform.is_identity() {
            (x.round() as i32, y.round() as i32)
        } else {
            let (tx, ty) = self.transform.transform_point(x, y);
            (tx.round() as i32, ty.round() as i32)
        }
    }

    // ── Public drawing methods ────────────────────────────────────

    /// Draw a single pixel at (x, y) with the current color.
    pub fn draw_pixel(&mut self, x: i32, y: i32) {
        let (tx, ty) = self.tx(x as f64, y as f64);
        self.put_pixel(tx, ty, self.color);
    }

    /// Draw a line from (x1,y1) to (x2,y2).
    /// Uses Wu's antialiased line when AA is enabled, Bresenham otherwise.
    pub fn draw_line(&mut self, x1: i32, y1: i32, x2: i32, y2: i32) {
        let (tx1, ty1) = self.tx(x1 as f64, y1 as f64);
        let (tx2, ty2) = self.tx(x2 as f64, y2 as f64);

        if self.antialias {
            self.wu_line(tx1, ty1, tx2, ty2);
        } else {
            self.bresenham_line(tx1, ty1, tx2, ty2);
        }
    }

    /// Draw an outline rectangle with the current stroke width.
    pub fn draw_rect(&mut self, x: i32, y: i32, w: u32, h: u32) {
        if w == 0 || h == 0 {
            return;
        }
        let sw = (self.stroke_width as i32).max(1);
        // Top edge
        self.fill_rect_raw(x, y, w, sw as u32);
        // Bottom edge
        self.fill_rect_raw(x, y + h as i32 - sw, w, sw as u32);
        // Left edge (excluding corners already drawn)
        if h as i32 > 2 * sw {
            self.fill_rect_raw(x, y + sw, sw as u32, h - 2 * sw as u32);
            // Right edge
            self.fill_rect_raw(x + w as i32 - sw, y + sw, sw as u32, h - 2 * sw as u32);
        }
    }

    /// Fill a rectangle.
    pub fn fill_rect(&mut self, x: i32, y: i32, w: u32, h: u32) {
        self.fill_rect_raw(x, y, w, h);
    }

    fn fill_rect_raw(&mut self, x: i32, y: i32, w: u32, h: u32) {
        let color = self.color;
        if w == 0 || h == 0 {
            return;
        }

        // Fast path: identity transform + axis-aligned rect.
        // Clip is computed ONCE; inner loop is per-row slice work.
        if self.transform.is_identity() {
            // Compute the destination rect in buffer space (inclusive-exclusive).
            let mut x0 = x;
            let mut y0 = y;
            let mut x1 = x.saturating_add(w as i32);
            let mut y1 = y.saturating_add(h as i32);

            // Clip against the buffer.
            x0 = x0.max(0);
            y0 = y0.max(0);
            x1 = x1.min(self.width as i32);
            y1 = y1.min(self.height as i32);

            // Clip against the user clip rect, if any.
            if let Some(ref clip) = self.clip {
                x0 = x0.max(clip.x);
                y0 = y0.max(clip.y);
                x1 = x1.min(clip.x.saturating_add(clip.width as i32));
                y1 = y1.min(clip.y.saturating_add(clip.height as i32));
            }

            if x0 >= x1 || y0 >= y1 {
                return;
            }

            let stride = self.width as usize;
            let xs = x0 as usize;
            let xe = x1 as usize;
            let mode = self.composite_mode;

            // Specialize on composite mode so the hot loop has no inner branch.
            match mode {
                CompositeMode::Src => {
                    for row in (y0 as usize)..(y1 as usize) {
                        let start = row * stride + xs;
                        let end = row * stride + xe;
                        let slice = &mut self.pixels[start..end];
                        slice.iter_mut().for_each(|p| *p = color);
                    }
                }
                CompositeMode::Clear => {
                    for row in (y0 as usize)..(y1 as usize) {
                        let start = row * stride + xs;
                        let end = row * stride + xe;
                        let slice = &mut self.pixels[start..end];
                        slice.iter_mut().for_each(|p| *p = 0);
                    }
                }
                CompositeMode::SrcOver => {
                    // Hoist alpha-shortcut decisions out of the inner loop.
                    let sa = argb_a(color);
                    if sa == 0 {
                        return; // fully transparent source — no-op
                    }
                    if sa == 255 {
                        // Opaque SRC_OVER == SRC; use the memset-style fast path.
                        for row in (y0 as usize)..(y1 as usize) {
                            let start = row * stride + xs;
                            let end = row * stride + xe;
                            let slice = &mut self.pixels[start..end];
                            slice.iter_mut().for_each(|p| *p = color);
                        }
                    } else {
                        for row in (y0 as usize)..(y1 as usize) {
                            let start = row * stride + xs;
                            let end = row * stride + xe;
                            composite_row_src_over_solid(&mut self.pixels[start..end], color);
                        }
                    }
                }
                CompositeMode::Xor => {
                    for row in (y0 as usize)..(y1 as usize) {
                        let start = row * stride + xs;
                        let end = row * stride + xe;
                        let slice = &mut self.pixels[start..end];
                        for p in slice.iter_mut() {
                            *p = composite(color, *p, CompositeMode::Xor);
                        }
                    }
                }
            }
            return;
        }

        // Slow path: arbitrary affine transform — defer to per-pixel writes.
        for dy in 0..h as i32 {
            for dx in 0..w as i32 {
                let (tx, ty) = self.tx((x + dx) as f64, (y + dy) as f64);
                self.put_pixel(tx, ty, color);
            }
        }
    }

    /// Draw an ellipse outline using the midpoint ellipse algorithm.
    pub fn draw_ellipse(&mut self, cx: i32, cy: i32, rx: u32, ry: u32) {
        if rx == 0 && ry == 0 {
            self.draw_pixel(cx, cy);
            return;
        }
        if rx == 0 {
            self.draw_line(cx, cy - ry as i32, cx, cy + ry as i32);
            return;
        }
        if ry == 0 {
            self.draw_line(cx - rx as i32, cy, cx + rx as i32, cy);
            return;
        }

        let a = rx as i64;
        let b = ry as i64;
        let a2 = a * a;
        let b2 = b * b;

        // Region 1
        let mut x: i64 = 0;
        let mut y: i64 = b;
        let mut d1 = b2 - a2 * b + a2 / 4;
        let mut dx = 2 * b2 * x;
        let mut dy = 2 * a2 * y;

        while dx < dy {
            self.plot_ellipse_points(cx, cy, x as i32, y as i32);
            x += 1;
            dx += 2 * b2;
            if d1 < 0 {
                d1 += dx + b2;
            } else {
                y -= 1;
                dy -= 2 * a2;
                d1 += dx - dy + b2;
            }
        }

        // Region 2
        let mut d2 = b2 * (2 * x + 1) * (2 * x + 1) / 4 + a2 * (y - 1) * (y - 1) - a2 * b2;
        while y >= 0 {
            self.plot_ellipse_points(cx, cy, x as i32, y as i32);
            y -= 1;
            dy -= 2 * a2;
            if d2 > 0 {
                d2 += a2 - dy;
            } else {
                x += 1;
                dx += 2 * b2;
                d2 += dx - dy + a2;
            }
        }
    }

    fn plot_ellipse_points(&mut self, cx: i32, cy: i32, x: i32, y: i32) {
        let color = self.color;
        let (tx1, ty1) = self.tx((cx + x) as f64, (cy + y) as f64);
        self.put_pixel(tx1, ty1, color);
        let (tx2, ty2) = self.tx((cx - x) as f64, (cy + y) as f64);
        self.put_pixel(tx2, ty2, color);
        let (tx3, ty3) = self.tx((cx + x) as f64, (cy - y) as f64);
        self.put_pixel(tx3, ty3, color);
        let (tx4, ty4) = self.tx((cx - x) as f64, (cy - y) as f64);
        self.put_pixel(tx4, ty4, color);
    }

    /// Fill an ellipse using scanline fill.
    pub fn fill_ellipse(&mut self, cx: i32, cy: i32, rx: u32, ry: u32) {
        if rx == 0 && ry == 0 {
            self.draw_pixel(cx, cy);
            return;
        }
        let a = rx as f64;
        let b = ry as f64;
        let color = self.color;

        for dy in -(ry as i32)..=(ry as i32) {
            // x^2/a^2 + y^2/b^2 = 1  =>  x = a * sqrt(1 - y^2/b^2)
            let fy = dy as f64;
            let ratio = 1.0 - (fy * fy) / (b * b);
            if ratio < 0.0 {
                continue;
            }
            let half_w = (a * ratio.sqrt()).round() as i32;
            for dx in -half_w..=half_w {
                let (tx, ty) = self.tx((cx + dx) as f64, (cy + dy) as f64);
                self.put_pixel(tx, ty, color);
            }
        }
    }

    /// Draw a parametric arc.  Angles in degrees.
    pub fn draw_arc(
        &mut self,
        cx: i32, cy: i32,
        rx: u32, ry: u32,
        start_angle: f32, arc_angle: f32,
    ) {
        let steps = ((rx.max(ry) as f32 * arc_angle.abs() / 90.0).ceil() as usize).max(16);
        let start_rad = (start_angle as f64) * PI / 180.0;
        let arc_rad = (arc_angle as f64) * PI / 180.0;
        let dt = arc_rad / steps as f64;

        let mut prev_x = cx as f64 + rx as f64 * start_rad.cos();
        let mut prev_y = cy as f64 - ry as f64 * start_rad.sin();

        for i in 1..=steps {
            let t = start_rad + i as f64 * dt;
            let cur_x = cx as f64 + rx as f64 * t.cos();
            let cur_y = cy as f64 - ry as f64 * t.sin();
            // Draw line segment in logical coords, then transform
            let (tx1, ty1) = self.tx(prev_x, prev_y);
            let (tx2, ty2) = self.tx(cur_x, cur_y);
            if self.antialias {
                self.wu_line(tx1, ty1, tx2, ty2);
            } else {
                self.bresenham_line(tx1, ty1, tx2, ty2);
            }
            prev_x = cur_x;
            prev_y = cur_y;
        }
    }

    /// Fill a pie-shaped arc.  Angles in degrees.
    pub fn fill_arc(
        &mut self,
        cx: i32, cy: i32,
        rx: u32, ry: u32,
        start_angle: f32, arc_angle: f32,
    ) {
        // Build polygon: center -> arc points -> center
        let steps = ((rx.max(ry) as f32 * arc_angle.abs() / 90.0).ceil() as usize).max(16);
        let start_rad = (start_angle as f64) * PI / 180.0;
        let arc_rad = (arc_angle as f64) * PI / 180.0;
        let dt = arc_rad / steps as f64;

        let mut points = Vec::with_capacity(steps + 2);
        points.push((cx, cy));
        for i in 0..=steps {
            let t = start_rad + i as f64 * dt;
            let px = (cx as f64 + rx as f64 * t.cos()).round() as i32;
            let py = (cy as f64 - ry as f64 * t.sin()).round() as i32;
            points.push((px, py));
        }

        self.fill_polygon(&points);
    }

    /// Draw a closed polygon outline.
    pub fn draw_polygon(&mut self, points: &[(i32, i32)]) {
        if points.len() < 2 {
            return;
        }
        for i in 0..points.len() {
            let j = (i + 1) % points.len();
            self.draw_line(points[i].0, points[i].1, points[j].0, points[j].1);
        }
    }

    /// Fill a polygon using scanline fill with the even-odd rule.
    pub fn fill_polygon(&mut self, points: &[(i32, i32)]) {
        if points.len() < 3 {
            return;
        }

        // Find bounding box
        let min_y = points.iter().map(|p| p.1).min().unwrap();
        let max_y = points.iter().map(|p| p.1).max().unwrap();
        let color = self.color;
        let n = points.len();

        // Hoisted out of the y-loop: one allocation reused via clear() per scanline.
        // Pre-sized to the edge count to avoid early growth reallocations
        // (a scanline can intersect at most every edge).
        let mut intersections: Vec<f64> = Vec::with_capacity(n);

        for y in min_y..=max_y {
            // Reuse buffer; clear() keeps the capacity.
            intersections.clear();
            for i in 0..n {
                let j = (i + 1) % n;
                let (x0, y0) = points[i];
                let (x1, y1) = points[j];

                // Check if this edge crosses the scanline
                if (y0 <= y && y1 > y) || (y1 <= y && y0 > y) {
                    let dy = y1 as f64 - y0 as f64;
                    if dy.abs() > 0.0 {
                        let t = (y as f64 - y0 as f64) / dy;
                        let ix = x0 as f64 + t * (x1 as f64 - x0 as f64);
                        intersections.push(ix);
                    }
                }
            }

            intersections.sort_by(|a, b| a.partial_cmp(b).unwrap());

            // Fill between pairs (even-odd rule)
            let mut i = 0;
            while i + 1 < intersections.len() {
                let x_start = intersections[i].ceil() as i32;
                let x_end = intersections[i + 1].floor() as i32;
                for x in x_start..=x_end {
                    let (tx, ty) = self.tx(x as f64, y as f64);
                    self.put_pixel(tx, ty, color);
                }
                i += 2;
            }
        }
    }

    /// Draw connected line segments (not closed).
    pub fn draw_polyline(&mut self, points: &[(i32, i32)]) {
        if points.len() < 2 {
            return;
        }
        for i in 0..points.len() - 1 {
            self.draw_line(points[i].0, points[i].1, points[i + 1].0, points[i + 1].1);
        }
    }

    /// Alpha-composited blit of source image onto this buffer.
    pub fn blit_image(&mut self, src: &[u32], src_w: u32, src_h: u32, dx: i32, dy: i32) {
        if src_w == 0 || src_h == 0 {
            return;
        }

        // Fast path: identity transform — clip ONCE and do per-row slice work.
        if self.transform.is_identity() {
            // Effective source dimensions (don't read past end of supplied slice).
            let eff_src_h = (src.len() / src_w as usize).min(src_h as usize) as i32;
            if eff_src_h <= 0 {
                return;
            }

            // Destination rect in buffer space.
            let mut dst_x0 = dx;
            let mut dst_y0 = dy;
            let mut dst_x1 = dx.saturating_add(src_w as i32);
            let mut dst_y1 = dy.saturating_add(eff_src_h);

            // Clip to buffer.
            dst_x0 = dst_x0.max(0);
            dst_y0 = dst_y0.max(0);
            dst_x1 = dst_x1.min(self.width as i32);
            dst_y1 = dst_y1.min(self.height as i32);

            // Clip to user clip rect.
            if let Some(ref clip) = self.clip {
                dst_x0 = dst_x0.max(clip.x);
                dst_y0 = dst_y0.max(clip.y);
                dst_x1 = dst_x1.min(clip.x.saturating_add(clip.width as i32));
                dst_y1 = dst_y1.min(clip.y.saturating_add(clip.height as i32));
            }

            if dst_x0 >= dst_x1 || dst_y0 >= dst_y1 {
                return;
            }

            // Source origin offset corresponding to (dst_x0, dst_y0).
            let src_off_x = dst_x0 - dx;
            let src_off_y = dst_y0 - dy;
            let row_w = (dst_x1 - dst_x0) as usize;
            let dst_stride = self.width as usize;
            let src_stride = src_w as usize;
            let mode = self.composite_mode;

            for row in 0..(dst_y1 - dst_y0) as usize {
                let dst_row = (dst_y0 as usize + row) * dst_stride + dst_x0 as usize;
                let src_row = (src_off_y as usize + row) * src_stride + src_off_x as usize;
                let dst_slice = &mut self.pixels[dst_row..dst_row + row_w];
                let src_slice = &src[src_row..src_row + row_w];

                match mode {
                    CompositeMode::Src => {
                        dst_slice.copy_from_slice(src_slice);
                    }
                    CompositeMode::Clear => {
                        dst_slice.iter_mut().for_each(|p| *p = 0);
                    }
                    CompositeMode::SrcOver => {
                        composite_row_src_over(dst_slice, src_slice);
                    }
                    CompositeMode::Xor => {
                        for (d, s) in dst_slice.iter_mut().zip(src_slice.iter()) {
                            *d = composite(*s, *d, CompositeMode::Xor);
                        }
                    }
                }
            }
            return;
        }

        // Slow path: arbitrary transform — go through put_pixel per pixel.
        for sy in 0..src_h as i32 {
            for sx in 0..src_w as i32 {
                let src_idx = (sy as u32 * src_w + sx as u32) as usize;
                if src_idx >= src.len() {
                    continue;
                }
                let color = src[src_idx];
                let (tx, ty) = self.tx((dx + sx) as f64, (dy + sy) as f64);
                self.put_pixel(tx, ty, color);
            }
        }
    }

    /// Scaled blit with optional bilinear interpolation.
    pub fn blit_image_scaled(
        &mut self,
        src: &[u32], src_w: u32, src_h: u32,
        dx: i32, dy: i32, dw: u32, dh: u32,
        bilinear: bool,
    ) {
        if dw == 0 || dh == 0 || src_w == 0 || src_h == 0 {
            return;
        }

        for out_y in 0..dh as i32 {
            for out_x in 0..dw as i32 {
                let src_xf = out_x as f64 * (src_w as f64 - 1.0) / (dw as f64 - 1.0).max(1.0);
                let src_yf = out_y as f64 * (src_h as f64 - 1.0) / (dh as f64 - 1.0).max(1.0);

                let color = if bilinear {
                    bilinear_sample(src, src_w, src_h, src_xf, src_yf)
                } else {
                    let sx = src_xf.round() as u32;
                    let sy = src_yf.round() as u32;
                    let sx = sx.min(src_w - 1);
                    let sy = sy.min(src_h - 1);
                    src[(sy * src_w + sx) as usize]
                };

                let (tx, ty) = self.tx((dx + out_x) as f64, (dy + out_y) as f64);
                self.put_pixel(tx, ty, color);
            }
        }
    }

    /// Copy a rectangular region within the buffer.
    pub fn copy_area(&mut self, x: i32, y: i32, w: u32, h: u32, dx: i32, dy: i32) {
        if w == 0 || h == 0 || (dx == 0 && dy == 0) {
            return;
        }

        // Compute the intersection of the source rect with the buffer.
        let src_x0 = x.max(0);
        let src_y0 = y.max(0);
        let src_x1 = x.saturating_add(w as i32).min(self.width as i32);
        let src_y1 = y.saturating_add(h as i32).min(self.height as i32);
        if src_x0 >= src_x1 || src_y0 >= src_y1 {
            return;
        }

        // The "valid" sub-rect of the source — those reads that yielded real
        // pixel data. The previous implementation also wrote 0 for the
        // out-of-source area; to preserve identical behavior we keep that path
        // available, but the fast path is only used when the *entire* source
        // rect is in-bounds (the common case).
        let full_src_in_bounds = src_x0 == x
            && src_y0 == y
            && src_x1 == x + w as i32
            && src_y1 == y + h as i32;

        if full_src_in_bounds {
            // Compute destination rect (matching the source offset).
            let dst_x0 = x.saturating_add(dx);
            let dst_y0 = y.saturating_add(dy);
            let dst_x1 = dst_x0.saturating_add(w as i32);
            let dst_y1 = dst_y0.saturating_add(h as i32);

            // Clip destination to the buffer; compute how much to shave off each
            // side and apply the same shave to the source so they stay aligned.
            let clip_left = (-dst_x0).max(0);
            let clip_top = (-dst_y0).max(0);
            let clip_right = (dst_x1 - self.width as i32).max(0);
            let clip_bottom = (dst_y1 - self.height as i32).max(0);

            let copy_w = (w as i32 - clip_left - clip_right).max(0);
            let copy_h = (h as i32 - clip_top - clip_bottom).max(0);
            if copy_w <= 0 || copy_h <= 0 {
                return;
            }

            let s_x = (x + clip_left) as usize;
            let s_y = (y + clip_top) as usize;
            let d_x = (dst_x0 + clip_left) as usize;
            let d_y = (dst_y0 + clip_top) as usize;
            let copy_w = copy_w as usize;
            let copy_h = copy_h as usize;
            let stride = self.width as usize;

            // Non-overlap detection: the source and destination rects do not
            // share any pixels. We can copy rows in any order without aliasing.
            let s_x_end = s_x + copy_w;
            let s_y_end = s_y + copy_h;
            let d_x_end = d_x + copy_w;
            let d_y_end = d_y + copy_h;
            let non_overlap = s_x_end <= d_x || d_x_end <= s_x
                || s_y_end <= d_y || d_y_end <= s_y;

            if non_overlap {
                // True memcpy per row — no aliasing, no temporary buffer.
                // copy_within handles disjoint slices safely.
                for r in 0..copy_h {
                    let src_start = (s_y + r) * stride + s_x;
                    let dst_start = (d_y + r) * stride + d_x;
                    self.pixels.copy_within(src_start..src_start + copy_w, dst_start);
                }
                return;
            }

            // Overlapping in the same buffer. If they only overlap vertically
            // we can still avoid a temp buffer by iterating rows in the right
            // direction (memmove-style) — but only when X ranges are equal
            // or rows don't alias each other within a single row.
            // Within a row, copy_within handles overlap correctly (memmove
            // semantics). Across rows we just need to pick the safe iteration
            // direction so we don't clobber yet-to-be-read source rows.
            let rows_iter: Box<dyn Iterator<Item = usize>> = if d_y > s_y {
                Box::new((0..copy_h).rev())
            } else {
                Box::new(0..copy_h)
            };

            for r in rows_iter {
                let src_start = (s_y + r) * stride + s_x;
                let dst_start = (d_y + r) * stride + d_x;
                self.pixels.copy_within(src_start..src_start + copy_w, dst_start);
            }
            return;
        }

        // Fallback (out-of-bounds source rect): preserve original behavior of
        // reading 0 for out-of-source pixels via a temp buffer.
        let mut temp = Vec::with_capacity((w * h) as usize);
        for sy in 0..h as i32 {
            for sx in 0..w as i32 {
                let src_x = x + sx;
                let src_y = y + sy;
                if src_x >= 0 && src_y >= 0
                    && src_x < self.width as i32 && src_y < self.height as i32
                {
                    temp.push(self.pixels[(src_y as u32 * self.width + src_x as u32) as usize]);
                } else {
                    temp.push(0);
                }
            }
        }

        // Write to destination
        for sy in 0..h as i32 {
            for sx in 0..w as i32 {
                let dst_x = x + sx + dx;
                let dst_y = y + sy + dy;
                if dst_x >= 0 && dst_y >= 0
                    && dst_x < self.width as i32 && dst_y < self.height as i32
                {
                    let idx = (dst_y as u32 * self.width + dst_x as u32) as usize;
                    self.pixels[idx] = temp[(sy as u32 * w + sx as u32) as usize];
                }
            }
        }
    }

    // ── Line algorithms ───────────────────────────────────────────

    /// Bresenham's line algorithm.
    fn bresenham_line(&mut self, mut x0: i32, mut y0: i32, x1: i32, y1: i32) {
        let color = self.color;
        let dx = (x1 - x0).abs();
        let dy = -(y1 - y0).abs();
        let sx: i32 = if x0 < x1 { 1 } else { -1 };
        let sy: i32 = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;

        loop {
            self.put_pixel(x0, y0, color);
            if x0 == x1 && y0 == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                if x0 == x1 {
                    break;
                }
                err += dy;
                x0 += sx;
            }
            if e2 <= dx {
                if y0 == y1 {
                    break;
                }
                err += dx;
                y0 += sy;
            }
        }
    }

    /// Wu's antialiased line algorithm.
    fn wu_line(&mut self, mut x0: i32, mut y0: i32, mut x1: i32, mut y1: i32) {
        let color = self.color;
        let steep = (y1 - y0).abs() > (x1 - x0).abs();

        if steep {
            std::mem::swap(&mut x0, &mut y0);
            std::mem::swap(&mut x1, &mut y1);
        }
        if x0 > x1 {
            std::mem::swap(&mut x0, &mut x1);
            std::mem::swap(&mut y0, &mut y1);
        }

        let dx = (x1 - x0) as f64;
        let dy = (y1 - y0) as f64;
        let gradient = if dx == 0.0 { 1.0 } else { dy / dx };

        // First endpoint
        let xend = x0 as f64;
        let yend = y0 as f64;
        let xpxl1 = xend as i32;
        let ypxl1 = yend.floor() as i32;

        if steep {
            self.put_pixel_aa(ypxl1, xpxl1, color, 1.0);
        } else {
            self.put_pixel_aa(xpxl1, ypxl1, color, 1.0);
        }

        let mut intery = yend + gradient;

        // Second endpoint
        let xend2 = x1 as f64;
        let xpxl2 = xend2 as i32;

        // Main loop
        for x in (xpxl1 + 1)..xpxl2 {
            let fpart = intery - intery.floor();
            let y = intery.floor() as i32;
            if steep {
                self.put_pixel_aa(y, x, color, 1.0 - fpart);
                self.put_pixel_aa(y + 1, x, color, fpart);
            } else {
                self.put_pixel_aa(x, y, color, 1.0 - fpart);
                self.put_pixel_aa(x, y + 1, color, fpart);
            }
            intery += gradient;
        }

        // Last endpoint
        if steep {
            self.put_pixel_aa(intery.floor() as i32, xpxl2, color, 1.0);
        } else {
            self.put_pixel_aa(xpxl2, intery.floor() as i32, color, 1.0);
        }
    }
}

// ── Bilinear interpolation ────────────────────────────────────────────

fn bilinear_sample(src: &[u32], w: u32, h: u32, x: f64, y: f64) -> u32 {
    let x0 = x.floor() as i32;
    let y0 = y.floor() as i32;
    let x1 = (x0 + 1).min(w as i32 - 1);
    let y1 = (y0 + 1).min(h as i32 - 1);
    let x0 = x0.max(0).min(w as i32 - 1);
    let y0 = y0.max(0).min(h as i32 - 1);

    let fx = x - x.floor();
    let fy = y - y.floor();

    let c00 = src[(y0 as u32 * w + x0 as u32) as usize];
    let c10 = src[(y0 as u32 * w + x1 as u32) as usize];
    let c01 = src[(y1 as u32 * w + x0 as u32) as usize];
    let c11 = src[(y1 as u32 * w + x1 as u32) as usize];

    let lerp_ch = |shift: u32| -> u8 {
        let v00 = ((c00 >> shift) & 0xFF) as f64;
        let v10 = ((c10 >> shift) & 0xFF) as f64;
        let v01 = ((c01 >> shift) & 0xFF) as f64;
        let v11 = ((c11 >> shift) & 0xFF) as f64;
        let top = v00 * (1.0 - fx) + v10 * fx;
        let bot = v01 * (1.0 - fx) + v11 * fx;
        let val = top * (1.0 - fy) + bot * fy;
        val.round().min(255.0).max(0.0) as u8
    };

    make_argb(lerp_ch(24), lerp_ch(16), lerp_ch(8), lerp_ch(0))
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rect_contains() {
        let r = Rect::new(10, 20, 30, 40);
        assert!(r.contains(10, 20));
        assert!(r.contains(39, 59));
        assert!(!r.contains(40, 60));
        assert!(!r.contains(9, 20));
    }

    #[test]
    fn test_rect_intersect() {
        let a = Rect::new(0, 0, 10, 10);
        let b = Rect::new(5, 5, 10, 10);
        let i = a.intersect(&b).unwrap();
        assert_eq!(i, Rect::new(5, 5, 5, 5));

        let c = Rect::new(20, 20, 5, 5);
        assert!(a.intersect(&c).is_none());
    }

    #[test]
    fn test_affine_identity() {
        let t = AffineTransform::identity();
        assert!(t.is_identity());
        let (x, y) = t.transform_point(3.0, 7.0);
        assert!((x - 3.0).abs() < 1e-10);
        assert!((y - 7.0).abs() < 1e-10);
    }

    #[test]
    fn test_affine_translate() {
        let t = AffineTransform::translate(10.0, 20.0);
        let (x, y) = t.transform_point(5.0, 5.0);
        assert!((x - 15.0).abs() < 1e-10);
        assert!((y - 25.0).abs() < 1e-10);
    }

    #[test]
    fn test_affine_rotate() {
        let t = AffineTransform::rotate(PI / 2.0);
        let (x, y) = t.transform_point(1.0, 0.0);
        assert!((x - 0.0).abs() < 1e-10);
        assert!((y - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_affine_scale() {
        let t = AffineTransform::scale(2.0, 3.0);
        let (x, y) = t.transform_point(5.0, 10.0);
        assert!((x - 10.0).abs() < 1e-10);
        assert!((y - 30.0).abs() < 1e-10);
    }

    #[test]
    fn test_affine_concatenate() {
        // translate then scale: scale(2,2) * translate(10,20)
        let t = AffineTransform::scale(2.0, 2.0);
        let u = AffineTransform::translate(10.0, 20.0);
        let c = t.concatenate(&u);
        let (x, y) = c.transform_point(0.0, 0.0);
        assert!((x - 20.0).abs() < 1e-10);
        assert!((y - 40.0).abs() < 1e-10);
    }

    #[test]
    fn test_affine_invert() {
        let t = AffineTransform::translate(10.0, 20.0);
        let inv = t.invert().unwrap();
        let composed = t.concatenate(&inv);
        assert!(composed.is_identity());

        // Singular matrix
        let s = AffineTransform { m00: 0.0, m01: 0.0, m02: 0.0, m10: 0.0, m11: 0.0, m12: 0.0 };
        assert!(s.invert().is_none());
    }

    #[test]
    fn test_horizontal_line() {
        let mut r = SoftwareRenderer::new(10, 10);
        r.set_color(0xFF_FF0000);
        r.draw_line(2, 5, 7, 5);
        for x in 2..=7 {
            assert_eq!(r.pixels()[(5 * 10 + x) as usize], 0xFF_FF0000);
        }
        // Pixel outside should be untouched
        assert_eq!(r.pixels()[(5 * 10 + 1) as usize], 0);
    }

    #[test]
    fn test_vertical_line() {
        let mut r = SoftwareRenderer::new(10, 10);
        r.set_color(0xFF_00FF00);
        r.draw_line(3, 1, 3, 8);
        for y in 1..=8 {
            assert_eq!(r.pixels()[(y * 10 + 3) as usize], 0xFF_00FF00);
        }
    }

    #[test]
    fn test_diagonal_line() {
        let mut r = SoftwareRenderer::new(10, 10);
        r.set_color(0xFF_0000FF);
        r.draw_line(0, 0, 9, 9);
        // Diagonal should have pixels set along the diagonal
        for i in 0..10 {
            assert_eq!(r.pixels()[(i * 10 + i) as usize], 0xFF_0000FF);
        }
    }

    #[test]
    fn test_fill_rect() {
        let mut r = SoftwareRenderer::new(20, 20);
        r.set_color(0xFF_AABBCC);
        r.fill_rect(5, 5, 3, 3);
        for y in 5..8 {
            for x in 5..8 {
                assert_eq!(r.pixels()[(y * 20 + x) as usize], 0xFF_AABBCC);
            }
        }
        // Outside
        assert_eq!(r.pixels()[(4 * 20 + 5) as usize], 0);
        assert_eq!(r.pixels()[(5 * 20 + 4) as usize], 0);
    }

    #[test]
    fn test_draw_rect_outline() {
        let mut r = SoftwareRenderer::new(20, 20);
        r.set_color(0xFF_112233);
        r.draw_rect(2, 2, 6, 6);
        // Top edge
        for x in 2..8 {
            assert_eq!(r.pixels()[(2 * 20 + x) as usize], 0xFF_112233);
        }
        // Interior should be empty
        assert_eq!(r.pixels()[(4 * 20 + 4) as usize], 0);
    }

    #[test]
    fn test_fill_ellipse() {
        let mut r = SoftwareRenderer::new(30, 30);
        r.set_color(0xFF_FF0000);
        r.fill_ellipse(15, 15, 5, 5);
        // Center should be filled
        assert_eq!(r.pixels()[(15 * 30 + 15) as usize], 0xFF_FF0000);
        // A point far outside the ellipse should not be
        assert_eq!(r.pixels()[(0 * 30 + 0) as usize], 0);
    }

    #[test]
    fn test_draw_ellipse() {
        let mut r = SoftwareRenderer::new(30, 30);
        r.set_color(0xFF_00FF00);
        r.draw_ellipse(15, 15, 8, 5);
        // Right-most point of ellipse should be drawn
        assert_eq!(r.pixels()[(15 * 30 + 23) as usize], 0xFF_00FF00);
        // Top-most
        assert_eq!(r.pixels()[(10 * 30 + 15) as usize], 0xFF_00FF00);
    }

    #[test]
    fn test_polygon_fill_triangle() {
        let mut r = SoftwareRenderer::new(20, 20);
        r.set_color(0xFF_FFFFFF);
        let tri = [(10, 2), (2, 18), (18, 18)];
        r.fill_polygon(&tri);
        // Centroid should be filled
        assert_ne!(r.pixels()[(10 * 20 + 10) as usize], 0);
        // Top-left corner should not be
        assert_eq!(r.pixels()[(0 * 20 + 0) as usize], 0);
    }

    #[test]
    fn test_polygon_fill_convex() {
        let mut r = SoftwareRenderer::new(20, 20);
        r.set_color(0xFF_ABCDEF);
        // Square as polygon
        let sq = [(5, 5), (15, 5), (15, 15), (5, 15)];
        r.fill_polygon(&sq);
        // Interior should be filled
        assert_eq!(r.pixels()[(10 * 20 + 10) as usize], 0xFF_ABCDEF);
        // Outside
        assert_eq!(r.pixels()[(0 * 20 + 0) as usize], 0);
    }

    #[test]
    fn test_polygon_fill_concave() {
        // L-shaped polygon (concave)
        let mut r = SoftwareRenderer::new(20, 20);
        r.set_color(0xFF_123456);
        let l_shape = [
            (2, 2), (10, 2), (10, 10), (6, 10), (6, 6), (2, 6),
        ];
        r.fill_polygon(&l_shape);
        // Inside the L
        assert_ne!(r.pixels()[(4 * 20 + 4) as usize], 0);
        // In the notch (outside the L)
        assert_eq!(r.pixels()[(8 * 20 + 4) as usize], 0);
    }

    #[test]
    fn test_alpha_blending_src_over() {
        let mut r = SoftwareRenderer::new(1, 1);
        // Draw opaque red
        r.set_color(0xFF_FF0000);
        r.draw_pixel(0, 0);
        assert_eq!(r.pixels()[0], 0xFF_FF0000);

        // Draw 50% alpha green on top
        r.set_color(0x80_00FF00);
        r.draw_pixel(0, 0);
        let result = r.pixels()[0];
        let a = argb_a(result);
        let red = argb_r(result);
        let green = argb_g(result);
        // Alpha should be high (near 255)
        assert!(a > 240);
        // Green should dominate over red
        assert!(green > red);
    }

    #[test]
    fn test_composite_src() {
        let mut r = SoftwareRenderer::new(1, 1);
        r.set_color(0xFF_FF0000);
        r.draw_pixel(0, 0);

        r.set_composite(CompositeMode::Src);
        r.set_color(0x80_00FF00);
        r.draw_pixel(0, 0);
        // SRC replaces entirely
        assert_eq!(r.pixels()[0], 0x80_00FF00);
    }

    #[test]
    fn test_composite_clear() {
        let mut r = SoftwareRenderer::new(1, 1);
        r.set_color(0xFF_FF0000);
        r.draw_pixel(0, 0);

        r.set_composite(CompositeMode::Clear);
        r.set_color(0xFF_00FF00);
        r.draw_pixel(0, 0);
        assert_eq!(r.pixels()[0], 0);
    }

    #[test]
    fn test_clip_rect_enforcement() {
        let mut r = SoftwareRenderer::new(20, 20);
        r.set_clip(Some(Rect::new(5, 5, 5, 5)));
        r.set_color(0xFF_FFFFFF);

        // Draw a big fill rect
        r.fill_rect(0, 0, 20, 20);

        // Only the clip region should be filled
        for y in 0..20 {
            for x in 0..20 {
                let inside_clip = x >= 5 && x < 10 && y >= 5 && y < 10;
                let val = r.pixels()[(y * 20 + x) as usize];
                if inside_clip {
                    assert_eq!(val, 0xFF_FFFFFF, "pixel ({},{}) should be set", x, y);
                } else {
                    assert_eq!(val, 0, "pixel ({},{}) should be clear", x, y);
                }
            }
        }
    }

    #[test]
    fn test_blit_image() {
        let mut r = SoftwareRenderer::new(10, 10);
        let src = vec![0xFF_FF0000; 4]; // 2x2 red
        r.blit_image(&src, 2, 2, 3, 3);
        assert_eq!(r.pixels()[(3 * 10 + 3) as usize], 0xFF_FF0000);
        assert_eq!(r.pixels()[(3 * 10 + 4) as usize], 0xFF_FF0000);
        assert_eq!(r.pixels()[(4 * 10 + 3) as usize], 0xFF_FF0000);
        assert_eq!(r.pixels()[(4 * 10 + 4) as usize], 0xFF_FF0000);
    }

    #[test]
    fn test_blit_image_scaled_bilinear() {
        // 2x2 source: top-left red, top-right green, bottom-left blue, bottom-right white
        let src = vec![
            0xFF_FF0000, 0xFF_00FF00,
            0xFF_0000FF, 0xFF_FFFFFF,
        ];
        let mut r = SoftwareRenderer::new(10, 10);
        r.blit_image_scaled(&src, 2, 2, 0, 0, 4, 4, true);

        // Corners should match source
        assert_eq!(r.pixels()[0], 0xFF_FF0000); // top-left
        assert_eq!(r.pixels()[3], 0xFF_00FF00); // top-right

        // Mid-point should be blended
        let mid = r.pixels()[(1 * 10 + 1) as usize];
        let a = argb_a(mid);
        assert_eq!(a, 255); // fully opaque
        // The middle should have contributions from all four source pixels
    }

    #[test]
    fn test_copy_area() {
        let mut r = SoftwareRenderer::new(10, 10);
        r.set_color(0xFF_AABB00);
        r.fill_rect(0, 0, 3, 3);
        r.copy_area(0, 0, 3, 3, 5, 5);
        // Original still there
        assert_eq!(r.pixels()[(0 * 10 + 0) as usize], 0xFF_AABB00);
        // Copy at offset
        assert_eq!(r.pixels()[(5 * 10 + 5) as usize], 0xFF_AABB00);
        assert_eq!(r.pixels()[(7 * 10 + 7) as usize], 0xFF_AABB00);
    }

    #[test]
    fn test_clear() {
        let mut r = SoftwareRenderer::new(5, 5);
        r.clear(0xFF_112233);
        for px in r.pixels() {
            assert_eq!(*px, 0xFF_112233);
        }
    }

    #[test]
    fn test_resize() {
        let mut r = SoftwareRenderer::new(5, 5);
        r.set_color(0xFF_FF0000);
        r.fill_rect(0, 0, 5, 5);
        r.resize(10, 10);
        assert_eq!(r.width(), 10);
        assert_eq!(r.height(), 10);
        assert_eq!(r.pixels().len(), 100);
        // Should be cleared
        assert_eq!(r.pixels()[0], 0);
    }

    #[test]
    fn test_wu_line_antialias() {
        let mut r = SoftwareRenderer::new(10, 10);
        r.set_antialias(true);
        r.set_color(0xFF_FFFFFF);
        r.draw_line(0, 0, 9, 4);
        // Some pixels along the path should be set
        let mut has_pixels = false;
        for p in r.pixels() {
            if *p != 0 {
                has_pixels = true;
                break;
            }
        }
        assert!(has_pixels, "Wu's line should draw some pixels");
    }

    #[test]
    fn test_draw_arc() {
        let mut r = SoftwareRenderer::new(30, 30);
        r.set_color(0xFF_FF0000);
        r.draw_arc(15, 15, 10, 10, 0.0, 90.0);
        // Should have drawn something
        let drawn = r.pixels().iter().filter(|&&p| p != 0).count();
        assert!(drawn > 0, "draw_arc should produce pixels");
    }

    #[test]
    fn test_fill_arc() {
        let mut r = SoftwareRenderer::new(30, 30);
        r.set_color(0xFF_00FF00);
        r.fill_arc(15, 15, 10, 10, 0.0, 90.0);
        // A point inside the pie sector (above and right of center) should be filled
        // The arc spans 0..90 degrees (right to top), so (20, 12) should be inside.
        assert_ne!(r.pixels()[(12 * 30 + 20) as usize], 0);
    }

    #[test]
    fn test_draw_pixel_out_of_bounds() {
        let mut r = SoftwareRenderer::new(5, 5);
        r.set_color(0xFF_FFFFFF);
        // These should not panic
        r.draw_pixel(-1, 0);
        r.draw_pixel(0, -1);
        r.draw_pixel(5, 0);
        r.draw_pixel(0, 5);
        r.draw_pixel(100, 100);
        // Buffer untouched
        for p in r.pixels() {
            assert_eq!(*p, 0);
        }
    }

    #[test]
    fn test_transform_draw_pixel() {
        let mut r = SoftwareRenderer::new(20, 20);
        r.set_transform(AffineTransform::translate(5.0, 5.0));
        r.set_color(0xFF_FF0000);
        r.draw_pixel(0, 0);
        // Should appear at (5, 5)
        assert_eq!(r.pixels()[(5 * 20 + 5) as usize], 0xFF_FF0000);
        assert_eq!(r.pixels()[0], 0);
    }
}
