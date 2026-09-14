// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Java Graphics2D state machine.
//!
//! Wraps [`SoftwareRenderer`](crate::renderer::SoftwareRenderer) and maps
//! Java's `Graphics2D` API to renderer calls.  Maintains the full graphics
//! state: paint, font, stroke, rendering hints, clip, transform, and a
//! save/restore stack.

use crate::renderer::{AffineTransform, CompositeMode, InterpolationKind, Rect, SoftwareRenderer};

// ── Supporting types ──────────────────────────────────────────────────

/// Paint used for filling shapes.
#[derive(Debug, Clone)]
pub enum Paint {
    /// Solid ARGB colour.
    Solid(u32),
    /// Linear gradient between two points.
    LinearGradient {
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        color1: u32,
        color2: u32,
        cyclic: bool,
    },
}

/// Font specification.
#[derive(Debug, Clone)]
pub struct FontSpec {
    pub family: String,
    /// 0 = PLAIN, 1 = BOLD, 2 = ITALIC, 3 = BOLD|ITALIC.
    pub style: i32,
    pub size: i32,
}

impl Default for FontSpec {
    fn default() -> Self {
        Self {
            family: "Dialog".to_string(),
            style: 0,
            size: 12,
        }
    }
}

/// Cap style for BasicStroke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapStyle {
    Butt,
    Round,
    Square,
}

/// Join style for BasicStroke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinStyle {
    Miter,
    Round,
    Bevel,
}

/// Stroke specification matching `java.awt.BasicStroke`.
#[derive(Debug, Clone, Copy)]
pub struct StrokeSpec {
    pub width: f32,
    pub cap: CapStyle,
    pub join: JoinStyle,
}

impl Default for StrokeSpec {
    fn default() -> Self {
        Self {
            width: 1.0,
            cap: CapStyle::Square,
            join: JoinStyle::Miter,
        }
    }
}

/// Interpolation mode for image scaling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interpolation {
    NearestNeighbor,
    Bilinear,
    Bicubic,
}

impl Interpolation {
    /// Project the Java-side hint onto the renderer's interpolation
    /// kind. Kept here so the natives layer doesn't need to know about
    /// the renderer's internal type.
    fn to_kind(self) -> InterpolationKind {
        match self {
            Interpolation::NearestNeighbor => InterpolationKind::Nearest,
            Interpolation::Bilinear => InterpolationKind::Bilinear,
            Interpolation::Bicubic => InterpolationKind::Bicubic,
        }
    }
}

/// Rendering hints (maps to `java.awt.RenderingHints`).
#[derive(Debug, Clone, Copy)]
pub struct RenderingHints {
    pub antialias: bool,
    pub text_antialias: bool,
    pub interpolation: Interpolation,
}

impl Default for RenderingHints {
    fn default() -> Self {
        Self {
            antialias: false,
            text_antialias: false,
            interpolation: Interpolation::NearestNeighbor,
        }
    }
}

/// Keys for `setRenderingHint`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderingHintKey {
    Antialiasing,
    TextAntialiasing,
    Interpolation,
}

/// Values for `setRenderingHint`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderingHintValue {
    On,
    Off,
    Default,
    BilinearInterpolation,
    NearestNeighborInterpolation,
    BicubicInterpolation,
}

/// Snapshot of graphics state for save/restore.
#[derive(Debug, Clone)]
struct SavedState {
    color: u32,
    paint: Paint,
    font: FontSpec,
    stroke: StrokeSpec,
    hints: RenderingHints,
    transform: AffineTransform,
    clip: Option<Rect>,
    background: u32,
    composite: CompositeMode,
}

// ── Graphics2DState ───────────────────────────────────────────────────

/// Full Graphics2D state machine wrapping a software renderer.
pub struct Graphics2DState {
    renderer: SoftwareRenderer,
    paint: Paint,
    font: FontSpec,
    stroke: StrokeSpec,
    rendering_hints: RenderingHints,
    background: u32,
    saved_states: Vec<SavedState>,
    disposed: bool,
}

impl Graphics2DState {
    /// Create a new Graphics2D context with the given buffer dimensions.
    pub fn create(width: u32, height: u32) -> Self {
        let mut renderer = SoftwareRenderer::new(width, height);
        renderer.set_color(0xFF_000000); // default opaque black
        Self {
            renderer,
            paint: Paint::Solid(0xFF_000000),
            font: FontSpec::default(),
            stroke: StrokeSpec::default(),
            rendering_hints: RenderingHints::default(),
            background: 0xFF_FFFFFF,
            saved_states: Vec::new(),
            disposed: false,
        }
    }

    // ── Colour / Paint ────────────────────────────────────────────

    pub fn set_color(&mut self, r: u8, g: u8, b: u8, a: u8) {
        if self.disposed {
            return;
        }
        let argb = (a as u32) << 24 | (r as u32) << 16 | (g as u32) << 8 | b as u32;
        self.paint = Paint::Solid(argb);
        self.renderer.set_color(argb);
    }

    /// The current colour as packed ARGB — what `Graphics.getColor()` answers.
    ///
    /// Read from `paint` rather than the renderer, so that a `Paint` which is
    /// not a solid colour still reports the last solid colour set, which is
    /// what the JDK does (`getColor` is documented in terms of the colour, and
    /// a gradient paint leaves it unchanged).
    pub fn color(&self) -> u32 {
        match self.paint {
            Paint::Solid(argb) => argb,
            _ => 0xFF_000000,
        }
    }

    /// The background colour used by `clearRect`.
    pub fn background(&self) -> u32 {
        self.background
    }

    pub fn set_background(&mut self, argb: u32) {
        if self.disposed {
            return;
        }
        self.background = argb;
    }

    pub fn set_paint(&mut self, paint: Paint) {
        if self.disposed {
            return;
        }
        match &paint {
            Paint::Solid(c) => self.renderer.set_color(*c),
            Paint::LinearGradient { color1, .. } => {
                // Use color1 as the renderer's current color (gradients
                // require per-pixel computation handled in fill methods).
                self.renderer.set_color(*color1);
            }
        }
        self.paint = paint;
    }

    // ── Font ──────────────────────────────────────────────────────

    pub fn set_font(&mut self, name: &str, style: i32, size: i32) {
        if self.disposed {
            return;
        }
        self.font = FontSpec {
            family: name.to_string(),
            style,
            size,
        };
    }

    // ── Stroke ────────────────────────────────────────────────────

    pub fn set_stroke(&mut self, stroke: StrokeSpec) {
        if self.disposed {
            return;
        }
        self.stroke = stroke;
        self.renderer.set_stroke_width(stroke.width);
    }

    // ── Rendering hints ───────────────────────────────────────────

    pub fn set_rendering_hint(&mut self, key: RenderingHintKey, value: RenderingHintValue) {
        if self.disposed {
            return;
        }
        match key {
            RenderingHintKey::Antialiasing => {
                let on = matches!(value, RenderingHintValue::On);
                self.rendering_hints.antialias = on;
                self.renderer.set_antialias(on);
            }
            RenderingHintKey::TextAntialiasing => {
                self.rendering_hints.text_antialias = matches!(value, RenderingHintValue::On);
            }
            RenderingHintKey::Interpolation => {
                self.rendering_hints.interpolation = match value {
                    RenderingHintValue::BilinearInterpolation => Interpolation::Bilinear,
                    RenderingHintValue::BicubicInterpolation => Interpolation::Bicubic,
                    _ => Interpolation::NearestNeighbor,
                };
            }
        }
    }

    // ── Clip ──────────────────────────────────────────────────────

    pub fn set_clip_rect(&mut self, x: i32, y: i32, w: u32, h: u32) {
        if self.disposed {
            return;
        }
        self.renderer.set_clip(Some(Rect::new(x, y, w, h)));
    }

    pub fn get_clip_bounds(&self) -> Option<Rect> {
        self.renderer.clip()
    }

    // ── Transform ─────────────────────────────────────────────────

    pub fn set_transform(&mut self, t: AffineTransform) {
        if self.disposed {
            return;
        }
        self.renderer.set_transform(t);
    }

    pub fn get_transform(&self) -> AffineTransform {
        *self.renderer.transform()
    }

    pub fn translate(&mut self, dx: f64, dy: f64) {
        if self.disposed {
            return;
        }
        let cur = *self.renderer.transform();
        let t = cur.concatenate(&AffineTransform::translate(dx, dy));
        self.renderer.set_transform(t);
    }

    pub fn rotate(&mut self, theta: f64) {
        if self.disposed {
            return;
        }
        let cur = *self.renderer.transform();
        let t = cur.concatenate(&AffineTransform::rotate(theta));
        self.renderer.set_transform(t);
    }

    pub fn scale(&mut self, sx: f64, sy: f64) {
        if self.disposed {
            return;
        }
        let cur = *self.renderer.transform();
        let t = cur.concatenate(&AffineTransform::scale(sx, sy));
        self.renderer.set_transform(t);
    }

    // ── Drawing primitives ────────────────────────────────────────

    pub fn draw_line(&mut self, x1: i32, y1: i32, x2: i32, y2: i32) {
        if self.disposed {
            return;
        }
        self.sync_paint_color_line(x1, y1, x2, y2);
        self.renderer.draw_line(x1, y1, x2, y2);
    }

    pub fn draw_rect(&mut self, x: i32, y: i32, w: i32, h: i32) {
        if self.disposed || w < 0 || h < 0 {
            return;
        }
        self.renderer.draw_rect(x, y, w as u32, h as u32);
    }

    pub fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32) {
        if self.disposed || w < 0 || h < 0 {
            return;
        }
        match &self.paint {
            Paint::Solid(_) => {
                self.renderer.fill_rect(x, y, w as u32, h as u32);
            }
            Paint::LinearGradient {
                x1: gx1,
                y1: gy1,
                x2: gx2,
                y2: gy2,
                color1,
                color2,
                cyclic,
            } => {
                self.fill_rect_gradient(
                    x, y, w as u32, h as u32, *gx1, *gy1, *gx2, *gy2, *color1, *color2, *cyclic,
                );
            }
        }
    }

    pub fn draw_oval(&mut self, x: i32, y: i32, w: i32, h: i32) {
        if self.disposed || w < 0 || h < 0 {
            return;
        }
        let cx = x.saturating_add(w / 2);
        let cy = y.saturating_add(h / 2);
        let rx = (w / 2) as u32;
        let ry = (h / 2) as u32;
        self.renderer.draw_ellipse(cx, cy, rx, ry);
    }

    pub fn fill_oval(&mut self, x: i32, y: i32, w: i32, h: i32) {
        if self.disposed || w < 0 || h < 0 {
            return;
        }
        let cx = x.saturating_add(w / 2);
        let cy = y.saturating_add(h / 2);
        let rx = (w / 2) as u32;
        let ry = (h / 2) as u32;
        self.renderer.fill_ellipse(cx, cy, rx, ry);
    }

    pub fn draw_arc(&mut self, x: i32, y: i32, w: i32, h: i32, start: i32, extent: i32) {
        if self.disposed || w < 0 || h < 0 {
            return;
        }
        let cx = x.saturating_add(w / 2);
        let cy = y.saturating_add(h / 2);
        let rx = (w / 2) as u32;
        let ry = (h / 2) as u32;
        self.renderer
            .draw_arc(cx, cy, rx, ry, start as f32, extent as f32);
    }

    pub fn fill_arc(&mut self, x: i32, y: i32, w: i32, h: i32, start: i32, extent: i32) {
        if self.disposed || w < 0 || h < 0 {
            return;
        }
        let cx = x.saturating_add(w / 2);
        let cy = y.saturating_add(h / 2);
        let rx = (w / 2) as u32;
        let ry = (h / 2) as u32;
        self.renderer
            .fill_arc(cx, cy, rx, ry, start as f32, extent as f32);
    }

    pub fn draw_polygon(&mut self, xs: &[i32], ys: &[i32]) {
        if self.disposed {
            return;
        }
        let points = Self::zip_points(xs, ys);
        self.renderer.draw_polygon(&points);
    }

    pub fn fill_polygon(&mut self, xs: &[i32], ys: &[i32]) {
        if self.disposed {
            return;
        }
        let points = Self::zip_points(xs, ys);
        self.renderer.fill_polygon(&points);
    }

    pub fn draw_polyline(&mut self, xs: &[i32], ys: &[i32]) {
        if self.disposed {
            return;
        }
        let points = Self::zip_points(xs, ys);
        self.renderer.draw_polyline(&points);
    }

    /// Draw a string at (x, y).
    ///
    /// Minimal text renderer that draws each character as a small block glyph.
    /// Full font rasterization (DirectWrite / fontdue) is handled by the
    /// platform layer; this fallback is always available for headless
    /// rendering and tests.
    pub fn draw_string(&mut self, text: &str, x: i32, y: i32) {
        if self.disposed {
            return;
        }
        let color = self.renderer.color();
        let glyph_h = self.font.size;

        // Bug awt-font-image #1: advance the pen by the SAME shared per-glyph
        // advance model that FontMetrics.stringWidth / FontEngine use, so the
        // headless software render lands each glyph exactly where Swing's text
        // layout measured it. The block-glyph mark is drawn narrower than the
        // advance (a real glyph leaves side bearing), but the pen advance —
        // the thing layout depends on — now matches the metrics by
        // construction. `cx` is tracked as a float and floored per glyph to
        // avoid per-character rounding drift accumulating across a long string.
        let mut pen = x as f64;
        for ch in text.chars() {
            let advance =
                crate::font::glyph_advance(&self.font.family, self.font.style, self.font.size, ch);
            let cx = pen.floor() as i32;
            if ch != ' ' {
                // Draw the visible mark within the glyph's advance box. Width
                // is the advance minus a 1px right-side bearing (clamped >= 1).
                let mark_w = (advance.floor() as i32 - 1).max(1);
                if mark_w >= 3 && glyph_h >= 3 {
                    if ch.is_uppercase() || ch.is_ascii_digit() {
                        self.renderer
                            .draw_rect(cx, y - glyph_h + 1, mark_w as u32, glyph_h as u32);
                    } else {
                        let half = glyph_h / 2;
                        self.renderer
                            .fill_rect(cx, y - half + 1, mark_w as u32, half as u32);
                    }
                } else {
                    self.renderer
                        .fill_rect(cx, y - glyph_h + 1, mark_w as u32, glyph_h as u32);
                }
            }
            pen += advance;
        }
        self.renderer.set_color(color);
    }

    // ── Image operations ──────────────────────────────────────────

    pub fn draw_image(&mut self, pixels: &[u32], w: u32, h: u32, x: i32, y: i32) {
        if self.disposed {
            return;
        }
        self.renderer.blit_image(pixels, w, h, x, y);
    }

    pub fn draw_image_scaled(
        &mut self,
        pixels: &[u32],
        w: u32,
        h: u32,
        x: i32,
        y: i32,
        dw: u32,
        dh: u32,
    ) {
        if self.disposed {
            return;
        }
        let kind = self.rendering_hints.interpolation.to_kind();
        self.renderer
            .blit_image_scaled(pixels, w, h, x, y, dw, dh, kind);
    }

    // ── Clear / copy ──────────────────────────────────────────────

    /// Fill a rectangle with the background colour (ignoring current paint).
    pub fn clear_rect(&mut self, x: i32, y: i32, w: u32, h: u32) {
        if self.disposed {
            return;
        }
        let saved = self.renderer.color();
        self.renderer.set_color(self.background);
        self.renderer.set_composite(CompositeMode::Src);
        self.renderer.fill_rect(x, y, w, h);
        self.renderer.set_composite(CompositeMode::SrcOver);
        self.renderer.set_color(saved);
    }

    pub fn copy_area(&mut self, x: i32, y: i32, w: u32, h: u32, dx: i32, dy: i32) {
        if self.disposed {
            return;
        }
        self.renderer.copy_area(x, y, w, h, dx, dy);
    }

    // ── State save/restore ────────────────────────────────────────

    pub fn save(&mut self) {
        if self.disposed {
            return;
        }
        let state = SavedState {
            color: self.renderer.color(),
            paint: self.paint.clone(),
            font: self.font.clone(),
            stroke: self.stroke,
            hints: self.rendering_hints,
            transform: *self.renderer.transform(),
            clip: self.renderer.clip(),
            background: self.background,
            composite: self.renderer.composite(),
        };
        self.saved_states.push(state);
    }

    pub fn restore(&mut self) {
        if self.disposed {
            return;
        }
        if let Some(state) = self.saved_states.pop() {
            self.renderer.set_color(state.color);
            self.paint = state.paint;
            self.font = state.font;
            self.stroke = state.stroke;
            self.rendering_hints = state.hints;
            self.renderer.set_transform(state.transform);
            self.renderer.set_clip(state.clip);
            self.renderer.set_antialias(state.hints.antialias);
            self.renderer.set_stroke_width(state.stroke.width);
            self.background = state.background;
            self.renderer.set_composite(state.composite);
        }
    }

    /// Mark this context as disposed.  All subsequent drawing ops become no-ops.
    pub fn dispose(&mut self) {
        self.disposed = true;
    }

    // ── Buffer access ─────────────────────────────────────────────

    pub fn pixels(&self) -> &[u32] {
        self.renderer.pixels()
    }

    pub fn width(&self) -> u32 {
        self.renderer.width()
    }

    pub fn height(&self) -> u32 {
        self.renderer.height()
    }

    // ── Internals ─────────────────────────────────────────────────

    fn zip_points(xs: &[i32], ys: &[i32]) -> Vec<(i32, i32)> {
        xs.iter().zip(ys.iter()).map(|(&x, &y)| (x, y)).collect()
    }

    /// If the current paint is a gradient, compute the colour at the midpoint
    /// of a line and set it as the renderer colour.
    fn sync_paint_color_line(&mut self, x1: i32, y1: i32, x2: i32, y2: i32) {
        if let Paint::LinearGradient {
            x1: gx1,
            y1: gy1,
            x2: gx2,
            y2: gy2,
            color1,
            color2,
            cyclic,
        } = &self.paint
        {
            // Widen to i64 before summing so extreme coordinates cannot
            // overflow i32 (which would panic in debug / wrap in release).
            let mid_x = (x1 as i64 + x2 as i64) as f64 / 2.0;
            let mid_y = (y1 as i64 + y2 as i64) as f64 / 2.0;
            let c = gradient_color_at(
                *gx1, *gy1, *gx2, *gy2, *color1, *color2, *cyclic, mid_x, mid_y,
            );
            self.renderer.set_color(c);
        }
    }

    /// Fill a rectangle with a linear gradient, computing the colour per-pixel.
    ///
    /// Round-5: write directly into the pixel buffer row-by-row instead of
    /// looping through `set_color` + `draw_pixel` (each of which re-applied
    /// the affine transform and re-checked clip/bounds). We pre-clip the
    /// rectangle to the buffer + active clip once, then walk the visible
    /// rows touching `renderer.pixels_mut()` directly. The inner row loop
    /// computes one gradient color per pixel and writes it; gradient cost
    /// dominates per-pixel set_color overhead now.
    fn fill_rect_gradient(
        &mut self,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        gx1: f64,
        gy1: f64,
        gx2: f64,
        gy2: f64,
        color1: u32,
        color2: u32,
        cyclic: bool,
    ) {
        let buf_w = self.renderer.width() as i32;
        let buf_h = self.renderer.height() as i32;

        // Visible-rect intersection: rect ∩ buffer ∩ clip.
        let mut x0 = x.max(0);
        let mut y0 = y.max(0);
        let mut x1 = (x.saturating_add(w as i32)).min(buf_w);
        let mut y1 = (y.saturating_add(h as i32)).min(buf_h);
        if let Some(clip) = self.renderer.clip() {
            x0 = x0.max(clip.x);
            y0 = y0.max(clip.y);
            x1 = x1.min(clip.x.saturating_add(clip.width as i32));
            y1 = y1.min(clip.y.saturating_add(clip.height as i32));
        }
        if x0 >= x1 || y0 >= y1 {
            return;
        }

        // Round-7: detect the "vertical only" gradient (colour varies with y
        // only, not x). For such gradients every pixel in a scanline has the
        // same colour, so we can splat the row via `slice::fill` instead of
        // computing the gradient once per pixel. Common case: button-bar
        // top-to-bottom gradients in Swing L&Fs.
        let dx_g = gx2 - gx1;
        let dy_g = gy2 - gy1;
        let len_sq = dx_g * dx_g + dy_g * dy_g;
        let vertical_only = len_sq >= 1e-12 && (dx_g * dx_g) / len_sq < 1e-18;

        let buf_w_usize = self.renderer.width() as usize;
        let pixels = self.renderer.pixels_mut();
        let row_w = (x1 - x0) as usize;

        if vertical_only {
            for py in y0..y1 {
                let c = gradient_color_at(
                    gx1, gy1, gx2, gy2, color1, color2, cyclic, x0 as f64, py as f64,
                );
                let start = py as usize * buf_w_usize + x0 as usize;
                pixels[start..start + row_w].fill(c);
            }
            return;
        }

        // `gradient_color_at` recomputes `dx`, `dy`, `len_sq` and the
        // `(py-gy1)*dy` term on every call. All of those are row-invariant
        // (or fully invariant), so hoist them out of the loops. The inner
        // loop keeps the exact same per-pixel arithmetic the old code did
        // (`(px-gx1)*dx`, an add, and a divide by `len_sq`) so the result is
        // bit-identical to the previous `gradient_color_at(...)` path — only
        // the redundant recomputation is removed.
        //
        // NOTE: a per-pixel *colour* delta-add is intentionally NOT used —
        // `lerp_argb`'s per-channel `.round()` and the cyclic triangle-wave
        // make the colour non-affine in screen space, so a colour-add would
        // not be bit-exact. Only the redundant scalar work is hoisted.
        let degenerate = len_sq < 1e-12;
        for py in y0..y1 {
            let fy = py as f64;
            let row_base = (py as usize) * buf_w_usize;
            // Row-invariant y-component of the gradient dot product.
            let row_term = (fy - gy1) * dy_g;
            let row = &mut pixels[row_base + x0 as usize..row_base + x1 as usize];
            if degenerate {
                row.fill(color1);
                continue;
            }
            for (i, dst) in row.iter_mut().enumerate() {
                let fx = (x0 + i as i32) as f64;
                // Same expression as gradient_color_at, just with the
                // row-invariant term and len_sq hoisted.
                let mut t = ((fx - gx1) * dx_g + row_term) / len_sq;
                if cyclic {
                    t = t.rem_euclid(2.0);
                    if t > 1.0 {
                        t = 2.0 - t;
                    }
                } else {
                    t = t.clamp(0.0, 1.0);
                }
                *dst = lerp_argb(color1, color2, t);
            }
        }
    }
}

// ── Gradient computation ──────────────────────────────────────────────

/// Compute the ARGB colour at point (px, py) for a linear gradient defined
/// from (gx1, gy1) to (gx2, gy2) between color1 and color2.
fn gradient_color_at(
    gx1: f64,
    gy1: f64,
    gx2: f64,
    gy2: f64,
    color1: u32,
    color2: u32,
    cyclic: bool,
    px: f64,
    py: f64,
) -> u32 {
    let dx = gx2 - gx1;
    let dy = gy2 - gy1;
    let len_sq = dx * dx + dy * dy;
    if len_sq < 1e-12 {
        return color1;
    }

    let mut t = ((px - gx1) * dx + (py - gy1) * dy) / len_sq;

    if cyclic {
        t = t.rem_euclid(2.0);
        if t > 1.0 {
            t = 2.0 - t;
        }
    } else {
        t = t.clamp(0.0, 1.0);
    }

    lerp_argb(color1, color2, t)
}

/// Linearly interpolate between two ARGB colours.
fn lerp_argb(c1: u32, c2: u32, t: f64) -> u32 {
    let lerp = |shift: u32| -> u8 {
        let v1 = ((c1 >> shift) & 0xFF) as f64;
        let v2 = ((c2 >> shift) & 0xFF) as f64;
        (v1 + (v2 - v1) * t).round().clamp(0.0, 255.0) as u8
    };
    let a = lerp(24);
    let r = lerp(16);
    let g = lerp(8);
    let b = lerp(0);
    (a as u32) << 24 | (r as u32) << 16 | (g as u32) << 8 | b as u32
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::renderer::AffineTransform;
    use std::f64::consts::PI;

    #[test]
    fn test_create_and_dimensions() {
        let g = Graphics2DState::create(100, 50);
        assert_eq!(g.width(), 100);
        assert_eq!(g.height(), 50);
        assert_eq!(g.pixels().len(), 5000);
    }

    #[test]
    fn test_set_color_and_fill() {
        let mut g = Graphics2DState::create(10, 10);
        g.set_color(255, 0, 0, 255);
        g.fill_rect(0, 0, 10, 10);
        for p in g.pixels() {
            assert_eq!(*p, 0xFF_FF0000);
        }
    }

    #[test]
    fn test_draw_line_horizontal() {
        let mut g = Graphics2DState::create(20, 20);
        g.set_color(0, 255, 0, 255);
        g.draw_line(2, 5, 10, 5);
        for x in 2..=10 {
            assert_eq!(g.pixels()[(5 * 20 + x) as usize], 0xFF_00FF00);
        }
    }

    #[test]
    fn test_draw_rect_and_fill_rect() {
        let mut g = Graphics2DState::create(20, 20);
        g.set_color(0, 0, 255, 255);
        g.fill_rect(3, 3, 5, 5);
        assert_eq!(g.pixels()[(5 * 20 + 5) as usize], 0xFF_0000FF);

        g.set_color(255, 0, 0, 255);
        g.draw_rect(3, 3, 5, 5);
        assert_eq!(g.pixels()[(3 * 20 + 3) as usize], 0xFF_FF0000);
    }

    #[test]
    fn test_draw_oval() {
        let mut g = Graphics2DState::create(30, 30);
        g.set_color(255, 255, 0, 255);
        g.fill_oval(5, 5, 20, 20);
        assert_ne!(g.pixels()[(15 * 30 + 15) as usize], 0);
        assert_eq!(g.pixels()[(0 * 30 + 0) as usize], 0);
    }

    #[test]
    fn test_draw_polygon() {
        let mut g = Graphics2DState::create(20, 20);
        g.set_color(255, 0, 0, 255);
        let xs = [5, 15, 10];
        let ys = [2, 2, 18];
        g.fill_polygon(&xs, &ys);
        assert_ne!(g.pixels()[(8 * 20 + 10) as usize], 0);
    }

    #[test]
    fn test_draw_polyline() {
        let mut g = Graphics2DState::create(20, 20);
        g.set_color(0, 255, 255, 255);
        let xs = [0, 10, 19];
        let ys = [0, 10, 0];
        g.draw_polyline(&xs, &ys);
        assert_ne!(g.pixels()[(5 * 20 + 5) as usize], 0);
    }

    #[test]
    fn test_clear_rect() {
        let mut g = Graphics2DState::create(10, 10);
        g.set_color(255, 0, 0, 255);
        g.fill_rect(0, 0, 10, 10);
        g.clear_rect(2, 2, 4, 4);
        assert_eq!(g.pixels()[(3 * 10 + 3) as usize], 0xFF_FFFFFF);
        assert_eq!(g.pixels()[(0 * 10 + 0) as usize], 0xFF_FF0000);
    }

    #[test]
    fn test_clip_rect() {
        let mut g = Graphics2DState::create(20, 20);
        g.set_clip_rect(5, 5, 5, 5);
        assert_eq!(g.get_clip_bounds(), Some(Rect::new(5, 5, 5, 5)));

        g.set_color(255, 255, 255, 255);
        g.fill_rect(0, 0, 20, 20);

        for y in 0..20 {
            for x in 0..20 {
                let inside = x >= 5 && x < 10 && y >= 5 && y < 10;
                let val = g.pixels()[(y * 20 + x) as usize];
                if inside {
                    assert_eq!(val, 0xFF_FFFFFF);
                } else {
                    assert_eq!(val, 0);
                }
            }
        }
    }

    #[test]
    fn test_transform_translate() {
        let mut g = Graphics2DState::create(20, 20);
        g.translate(5.0, 5.0);
        g.set_color(255, 0, 0, 255);
        g.fill_rect(0, 0, 3, 3);
        assert_eq!(g.pixels()[(5 * 20 + 5) as usize], 0xFF_FF0000);
        assert_eq!(g.pixels()[(7 * 20 + 7) as usize], 0xFF_FF0000);
        assert_eq!(g.pixels()[(0 * 20 + 0) as usize], 0);
    }

    #[test]
    fn test_transform_scale() {
        let mut g = Graphics2DState::create(20, 20);
        g.scale(2.0, 2.0);
        g.set_color(0, 255, 0, 255);
        // Logical rect (1,1)-(3,3) => pixel coords at 2x: (2,2),(2,4),(4,2),(4,4)
        g.fill_rect(1, 1, 2, 2);
        assert_eq!(g.pixels()[(2 * 20 + 2) as usize], 0xFF_00FF00);
        assert_eq!(g.pixels()[(4 * 20 + 4) as usize], 0xFF_00FF00);
    }

    #[test]
    fn test_transform_rotate() {
        let mut g = Graphics2DState::create(20, 20);
        g.translate(10.0, 10.0);
        g.rotate(PI / 2.0);
        g.set_color(255, 0, 0, 255);
        // Logical (5, 0) -> rotated -> (0, 5) + translate(10,10) = (10, 15)
        g.fill_rect(5, 0, 1, 1);
        assert_eq!(g.pixels()[(15 * 20 + 10) as usize], 0xFF_FF0000);
    }

    #[test]
    fn test_save_restore() {
        let mut g = Graphics2DState::create(10, 10);
        g.set_color(255, 0, 0, 255);
        g.translate(3.0, 3.0);
        g.save();

        g.set_color(0, 255, 0, 255);
        g.translate(100.0, 100.0);

        g.restore();
        g.fill_rect(0, 0, 1, 1);
        assert_eq!(g.pixels()[(3 * 10 + 3) as usize], 0xFF_FF0000);
    }

    #[test]
    fn test_dispose_no_ops() {
        let mut g = Graphics2DState::create(10, 10);
        g.dispose();
        g.set_color(255, 0, 0, 255);
        g.fill_rect(0, 0, 10, 10);
        for p in g.pixels() {
            assert_eq!(*p, 0);
        }
    }

    #[test]
    fn test_linear_gradient_fill() {
        let mut g = Graphics2DState::create(10, 1);
        g.set_paint(Paint::LinearGradient {
            x1: 0.0,
            y1: 0.0,
            x2: 9.0,
            y2: 0.0,
            color1: 0xFF_000000,
            color2: 0xFF_FFFFFF,
            cyclic: false,
        });
        g.fill_rect(0, 0, 10, 1);
        let first = g.pixels()[0];
        let last = g.pixels()[9];
        let first_r = (first >> 16) & 0xFF;
        let last_r = (last >> 16) & 0xFF;
        assert!(
            first_r < 30,
            "first pixel red should be near 0, got {}",
            first_r
        );
        assert!(
            last_r > 225,
            "last pixel red should be near 255, got {}",
            last_r
        );
    }

    #[test]
    fn test_alpha_blending() {
        let mut g = Graphics2DState::create(1, 1);
        g.set_color(255, 0, 0, 255);
        g.fill_rect(0, 0, 1, 1);
        g.set_color(0, 0, 255, 128);
        g.fill_rect(0, 0, 1, 1);
        let p = g.pixels()[0];
        let r = (p >> 16) & 0xFF;
        let b = p & 0xFF;
        assert!(b > r, "blue {} should exceed red {}", b, r);
    }

    #[test]
    fn test_draw_string_produces_pixels() {
        let mut g = Graphics2DState::create(100, 20);
        g.set_color(255, 255, 255, 255);
        g.draw_string("Hello", 5, 15);
        let drawn = g.pixels().iter().filter(|&&p| p != 0).count();
        assert!(drawn > 0, "draw_string should produce some pixels");
    }

    #[test]
    fn test_draw_image() {
        let mut g = Graphics2DState::create(10, 10);
        let img = vec![0xFF_00FF00; 4];
        g.draw_image(&img, 2, 2, 3, 3);
        assert_eq!(g.pixels()[(3 * 10 + 3) as usize], 0xFF_00FF00);
        assert_eq!(g.pixels()[(4 * 10 + 4) as usize], 0xFF_00FF00);
    }

    #[test]
    fn test_draw_image_scaled() {
        let mut g = Graphics2DState::create(20, 20);
        let img = vec![0xFF_FF0000; 4];
        g.draw_image_scaled(&img, 2, 2, 5, 5, 6, 6);
        assert_eq!(g.pixels()[(5 * 20 + 5) as usize], 0xFF_FF0000);
        assert_eq!(g.pixels()[(10 * 20 + 10) as usize], 0xFF_FF0000);
    }

    #[test]
    fn test_copy_area() {
        let mut g = Graphics2DState::create(20, 20);
        g.set_color(0, 0, 255, 255);
        g.fill_rect(0, 0, 3, 3);
        g.copy_area(0, 0, 3, 3, 10, 10);
        assert_eq!(g.pixels()[(10 * 20 + 10) as usize], 0xFF_0000FF);
    }

    #[test]
    fn test_rendering_hint_antialias() {
        let mut g = Graphics2DState::create(20, 20);
        g.set_rendering_hint(RenderingHintKey::Antialiasing, RenderingHintValue::On);
        assert!(g.rendering_hints.antialias);
        g.set_rendering_hint(RenderingHintKey::Antialiasing, RenderingHintValue::Off);
        assert!(!g.rendering_hints.antialias);
    }

    #[test]
    fn test_set_stroke() {
        let mut g = Graphics2DState::create(20, 20);
        g.set_stroke(StrokeSpec {
            width: 3.0,
            cap: CapStyle::Round,
            join: JoinStyle::Bevel,
        });
        assert_eq!(g.stroke.width, 3.0);
        assert_eq!(g.stroke.cap, CapStyle::Round);
        assert_eq!(g.stroke.join, JoinStyle::Bevel);
    }

    #[test]
    fn test_draw_arc() {
        let mut g = Graphics2DState::create(40, 40);
        g.set_color(255, 0, 0, 255);
        g.draw_arc(5, 5, 30, 30, 0, 90);
        let drawn = g.pixels().iter().filter(|&&p| p != 0).count();
        assert!(drawn > 0, "draw_arc should produce pixels");
    }

    #[test]
    fn test_fill_arc() {
        let mut g = Graphics2DState::create(40, 40);
        g.set_color(0, 255, 0, 255);
        g.fill_arc(5, 5, 30, 30, 0, 90);
        let drawn = g.pixels().iter().filter(|&&p| p != 0).count();
        assert!(drawn > 5, "fill_arc should produce many pixels");
    }

    #[test]
    fn test_bilinear_interpolation_scaled_blit() {
        let mut g = Graphics2DState::create(10, 10);
        g.set_rendering_hint(
            RenderingHintKey::Interpolation,
            RenderingHintValue::BilinearInterpolation,
        );
        let src = vec![0xFF_FF0000, 0xFF_00FF00, 0xFF_0000FF, 0xFF_FFFFFF];
        g.draw_image_scaled(&src, 2, 2, 0, 0, 4, 4);

        assert_eq!(g.pixels()[0], 0xFF_FF0000);
        assert_eq!(g.pixels()[3], 0xFF_00FF00);
        let mid = g.pixels()[(1 * 10 + 1) as usize];
        let a = (mid >> 24) & 0xFF;
        assert_eq!(a, 255);
        let r = (mid >> 16) & 0xFF;
        assert!(
            r < 255 && r > 0,
            "midpoint red should be blended, got {}",
            r
        );
    }

    #[test]
    fn test_bicubic_interpolation_scaled_blit() {
        // Round-10 PERF Fix 1: bicubic interpolation should produce a
        // fully opaque, blended midpoint distinct from the source corner
        // colours (real 4×4 cubic-convolution, not the old "alias for
        // bilinear" fallback).
        let mut g = Graphics2DState::create(10, 10);
        g.set_rendering_hint(
            RenderingHintKey::Interpolation,
            RenderingHintValue::BicubicInterpolation,
        );
        let src = vec![0xFF_FF0000, 0xFF_00FF00, 0xFF_0000FF, 0xFF_FFFFFF];
        g.draw_image_scaled(&src, 2, 2, 0, 0, 4, 4);

        assert_eq!(g.pixels()[0], 0xFF_FF0000);
        let mid = g.pixels()[(1 * 10 + 1) as usize];
        let a = (mid >> 24) & 0xFF;
        // Cubic weights sum to ~1 so a fully-opaque input produces a
        // fully-opaque output after clamping.
        assert_eq!(a, 255);
        let r = (mid >> 16) & 0xFF;
        assert!(
            r < 255 && r > 0,
            "midpoint red should be blended, got {}",
            r
        );
    }

    #[test]
    fn test_negative_dimensions_no_panic() {
        let mut g = Graphics2DState::create(10, 10);
        g.set_color(255, 0, 0, 255);
        g.fill_rect(0, 0, -5, -5);
        g.draw_rect(0, 0, -5, -5);
        g.draw_oval(0, 0, -5, -5);
        g.fill_oval(0, 0, -5, -5);
        g.draw_arc(0, 0, -5, -5, 0, 90);
        g.fill_arc(0, 0, -5, -5, 0, 90);
        for p in g.pixels() {
            assert_eq!(*p, 0);
        }
    }

    #[test]
    fn test_set_font() {
        let mut g = Graphics2DState::create(10, 10);
        g.set_font("Monospaced", 1, 14);
        assert_eq!(g.font.family, "Monospaced");
        assert_eq!(g.font.style, 1);
        assert_eq!(g.font.size, 14);
    }

    #[test]
    fn test_affine_concatenate_order() {
        let mut g = Graphics2DState::create(20, 5);
        g.scale(2.0, 1.0);
        g.translate(5.0, 0.0);
        g.set_color(255, 0, 0, 255);
        g.fill_rect(0, 0, 1, 1);
        // Logical (0,0) -> translate -> (5,0) -> scale -> (10,0)
        assert_eq!(g.pixels()[(0 * 20 + 10) as usize], 0xFF_FF0000);
    }

    #[test]
    fn test_affine_invert_roundtrip() {
        let t = AffineTransform::translate(7.0, -3.0);
        let s = AffineTransform::scale(2.0, 0.5);
        let combined = t.concatenate(&s);
        let inv = combined.invert().unwrap();
        let id = combined.concatenate(&inv);
        assert!(
            id.is_identity(),
            "T * T^-1 should be identity, got {:?}",
            id
        );
    }
}
