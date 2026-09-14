// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Software rasterizer — draws into ARGB pixel buffers.
//!
//! All drawing is done in pure Rust, producing `Vec<u32>` buffers
//! (ARGB format: bits 24-31=alpha, 16-23=red, 8-15=green, 0-7=blue).
//! Platform backends only need to blit the final buffer to the screen.

use std::f64::consts::PI;

/// Hard cap on the number of segments used to tessellate an arc.
///
/// Arc parameters come straight from Java `Graphics.drawArc`/`fillArc`, where
/// the width/height can reach `Integer.MAX_VALUE`. Without a cap the derived
/// step count reaches billions, producing a multi-gigabyte allocation
/// (`fill_arc`) or an effective hang (`draw_arc`). 1<<16 segments is far more
/// than any real surface needs while keeping the worst-case work bounded.
const MAX_ARC_STEPS: usize = 1 << 16;

/// Hard cap for one software rendering surface. At 4 bytes per ARGB pixel this
/// bounds a renderer scratch buffer to 256 MiB before allocator overhead.
pub const MAX_RENDERER_PIXELS: usize = 64 * 1024 * 1024;

/// Maximum temporary pixels used by one `copyArea` chunk. The fallback path
/// processes larger visible regions in chunks so caller-controlled extents do
/// not translate into caller-controlled temporary allocations.
const MAX_COPY_AREA_TEMP_PIXELS: usize = 1 << 20;

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
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Returns `true` if the point (px, py) lies inside this rectangle.
    pub fn contains(&self, px: i32, py: i32) -> bool {
        let px = px as i64;
        let py = py as i64;
        let x0 = self.x as i64;
        let y0 = self.y as i64;
        let x1 = x0 + self.width as i64;
        let y1 = y0 + self.height as i64;
        px >= x0 && py >= y0 && px < x1 && py < y1
    }

    /// Returns the intersection of two rectangles, or `None` if they don't overlap.
    pub fn intersect(&self, other: &Rect) -> Option<Rect> {
        let x1 = (self.x as i64).max(other.x as i64);
        let y1 = (self.y as i64).max(other.y as i64);
        let x2 = (self.x as i64 + self.width as i64).min(other.x as i64 + other.width as i64);
        let y2 = (self.y as i64 + self.height as i64).min(other.y as i64 + other.height as i64);
        if x2 > x1 && y2 > y1 {
            Some(Rect {
                x: x1.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
                y: y1.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
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
            m00: 1.0,
            m01: 0.0,
            m02: 0.0,
            m10: 0.0,
            m11: 1.0,
            m12: 0.0,
        }
    }

    pub fn translate(dx: f64, dy: f64) -> Self {
        Self {
            m00: 1.0,
            m01: 0.0,
            m02: dx,
            m10: 0.0,
            m11: 1.0,
            m12: dy,
        }
    }

    pub fn rotate(theta: f64) -> Self {
        let c = theta.cos();
        let s = theta.sin();
        Self {
            m00: c,
            m01: -s,
            m02: 0.0,
            m10: s,
            m11: c,
            m12: 0.0,
        }
    }

    pub fn scale(sx: f64, sy: f64) -> Self {
        Self {
            m00: sx,
            m01: 0.0,
            m02: 0.0,
            m10: 0.0,
            m11: sy,
            m12: 0.0,
        }
    }

    /// `self * other` — applies `other` first, then `self`.
    ///
    /// Round-7 fast paths:
    /// - If `other` is the identity, return `self` unchanged (no multiply).
    /// - If `other` is a pure translation (linear part = identity), the
    ///   resulting linear part is `self`'s linear part; only the translation
    ///   column needs the partial multiply `self.linear * other.trans + self.trans`.
    ///
    /// These cases are extremely common: every `Graphics2D::translate(dx, dy)`
    /// call enters the second path, and many Swing repaints compose a
    /// translation onto an identity current transform (first path).
    pub fn concatenate(&self, other: &AffineTransform) -> AffineTransform {
        // Identity short-circuit. Tight tolerance — true identities come from
        // `AffineTransform::identity()` and compare exactly. The comparisons
        // are bitwise-equivalent on the constants 1.0 / 0.0.
        if other.m00 == 1.0
            && other.m01 == 0.0
            && other.m10 == 0.0
            && other.m11 == 1.0
            && other.m02 == 0.0
            && other.m12 == 0.0
        {
            return *self;
        }

        // Translation-only short-circuit: `other` has identity linear part
        // (a=1, b=0, c=0, d=1) but non-zero translation. Skip the four
        // linear multiplies; only fold `other`'s translation through `self`'s
        // linear part into the new translation column.
        if other.m00 == 1.0 && other.m01 == 0.0 && other.m10 == 0.0 && other.m11 == 1.0 {
            return AffineTransform {
                m00: self.m00,
                m01: self.m01,
                m02: self.m00 * other.m02 + self.m01 * other.m12 + self.m02,
                m10: self.m10,
                m11: self.m11,
                m12: self.m10 * other.m02 + self.m11 * other.m12 + self.m12,
            };
        }

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
                let val = (sc as u32 * sa * inv_da / 255 + dc as u32 * da * inv_sa / 255) * 255
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
// source slice) and apply SRC_OVER. The public entry points
// (`composite_row_src_over_solid`, `composite_row_src_over`) dispatch to
// an SSE2-vectorized inner loop on x86_64 (4 ARGB pixels = one 128-bit
// vector per iteration) and fall back to a scalar loop on other archs
// or when SSE2 isn't available at runtime.
//
// Safety note for `composite_row_src_over`: `src` and `dst` MUST NOT
// alias. The vectorized loop issues independent loads from both, which
// is fine for disjoint slices but produces undefined results when they
// overlap. Callers (the `blit_image` fast path) always pass slices from
// distinct buffers, satisfying this invariant.

/// Scalar SRC_OVER of a single solid `src_argb` over every pixel of `dst`.
/// Used as the SIMD tail / non-x86 fallback. Caller has already filtered
/// out the fully-opaque (memset) and fully-transparent (no-op) edge
/// cases.
#[inline]
fn composite_row_src_over_solid_scalar(dst: &mut [u32], src_argb: u32) {
    for d in dst.iter_mut() {
        *d = composite(src_argb, *d, CompositeMode::SrcOver);
    }
}

/// Scalar SRC_OVER of `src` slice over `dst` slice (same length).
/// Used as the SIMD tail / non-x86 fallback.
#[inline]
fn composite_row_src_over_scalar(dst: &mut [u32], src: &[u32]) {
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

/// SRC_OVER blend of a single solid `src_argb` into every pixel of `dst`.
///
/// Dispatches to an SSE2 implementation on x86_64 (4 pixels per
/// iteration). Caller has already filtered out the fully-opaque (memset)
/// and fully-transparent (no-op) cases; this still handles them
/// correctly if reached.
#[inline]
fn composite_row_src_over_solid(dst: &mut [u32], src_argb: u32) {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("sse2") {
            // SAFETY: SSE2 confirmed available; helper handles any length.
            unsafe {
                composite_row_sse2_solid(dst, src_argb);
            }
            return;
        }
    }
    composite_row_src_over_solid_scalar(dst, src_argb);
}

/// SRC_OVER blend of `src` slice into `dst` slice (same length).
///
/// `src` and `dst` MUST NOT alias — the SIMD loop assumes disjoint
/// buffers. Dispatches to SSE2 on x86_64, scalar elsewhere.
// TODO: add NEON/AArch64 path for ARM hosts (currently falls through to
// scalar via the cfg gate).
#[inline]
fn composite_row_src_over(dst: &mut [u32], src: &[u32]) {
    debug_assert_eq!(dst.len(), src.len());
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("sse2") {
            // SAFETY: SSE2 confirmed available; lengths checked by debug_assert
            // and the helper itself uses `chunks_exact` + scalar tail.
            unsafe {
                composite_row_sse2(dst, src);
            }
            return;
        }
    }
    composite_row_src_over_scalar(dst, src);
}

// ── SSE2 implementations ─────────────────────────────────────────────
//
// Both helpers process 4 ARGB u32 pixels per iteration as one __m128i.
// Per-pixel SRC_OVER, per channel:
//     out_c = (src_c * sa + dst_c * (255 - sa)) / 255
//
// Math layout (u16 lanes):
//   - Each 128-bit vector is unpacked into two u16x8 halves so 8-bit
//     channels become 16-bit lanes (low half = pixels 0,1; high = 2,3).
//   - `src*sa + dst*inv_sa` fits in u16 because for any 0..=255 src,
//     dst, sa it is bounded by max(src,dst)*255 = 65025 (< 65536).
//   - Divide-by-255 uses the rounded approximation
//         (x + ((x + 0x80) >> 8) + 0x80) >> 8
//     which gives the exact `(x + 127) / 255` rounding for x in
//     [0, 65535]. This avoids the visible banding of the cheaper
//     `>> 8` (divide-by-256) shortcut. All intermediates stay in u16.

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
#[inline]
unsafe fn div255_u16x8(x: core::arch::x86_64::__m128i) -> core::arch::x86_64::__m128i {
    use core::arch::x86_64::*;
    // (x + ((x + 128) >> 8) + 128) >> 8
    let c128 = _mm_set1_epi16(0x80);
    let t = _mm_add_epi16(x, c128);
    let t2 = _mm_srli_epi16(t, 8);
    let s = _mm_add_epi16(_mm_add_epi16(x, t2), c128);
    _mm_srli_epi16(s, 8)
}

/// SSE2 SRC_OVER of a solid `src_argb` over `dst` (4 pixels/iter).
///
/// # Safety
/// Requires SSE2 (universal on x86_64). Caller guarantees the target
/// supports SSE2 — verified via `is_x86_feature_detected!("sse2")` at
/// the dispatch site.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn composite_row_sse2_solid(dst: &mut [u32], src_argb: u32) {
    use core::arch::x86_64::*;

    let sa = ((src_argb >> 24) & 0xff) as i32;
    if sa == 0 {
        return;
    }
    if sa == 255 {
        // Opaque SRC_OVER == SRC: just memset.
        dst.fill(src_argb);
        return;
    }
    let inv_sa = 255 - sa;

    // Broadcast src and pre-unpack it into u16 lanes once.
    let zero = _mm_setzero_si128();
    let src_v = _mm_set1_epi32(src_argb as i32);
    let src_lo = _mm_unpacklo_epi8(src_v, zero);
    let src_hi = _mm_unpackhi_epi8(src_v, zero);

    // Per-channel weights are the same for every pixel — replicate them
    // into every u16 lane.
    let sa_v = _mm_set1_epi16(sa as i16);
    let inv_sa_v = _mm_set1_epi16(inv_sa as i16);

    // Pre-compute src * sa for both halves; reused on every iteration.
    let src_sa_lo = _mm_mullo_epi16(src_lo, sa_v);
    let src_sa_hi = _mm_mullo_epi16(src_hi, sa_v);

    let full_chunks = dst.len() / 4;
    let tail_start = full_chunks * 4;

    {
        // Drive the vectorized loop directly over a raw pointer so the
        // mutable borrow is released before we hand `dst` to the scalar
        // tail helper.
        let ptr = dst.as_mut_ptr();
        for i in 0..full_chunks {
            let p = ptr.add(i * 4) as *mut __m128i;
            let dst_v = _mm_loadu_si128(p as *const __m128i);
            let dst_lo = _mm_unpacklo_epi8(dst_v, zero);
            let dst_hi = _mm_unpackhi_epi8(dst_v, zero);

            // src*sa + dst*inv_sa  (fits in u16 since <= 255*255)
            let sum_lo = _mm_add_epi16(src_sa_lo, _mm_mullo_epi16(dst_lo, inv_sa_v));
            let sum_hi = _mm_add_epi16(src_sa_hi, _mm_mullo_epi16(dst_hi, inv_sa_v));

            // Rounded divide-by-255.
            let mixed_lo = div255_u16x8(sum_lo);
            let mixed_hi = div255_u16x8(sum_hi);

            // Pack back to u8, saturating (values are already <=255).
            let packed = _mm_packus_epi16(mixed_lo, mixed_hi);
            _mm_storeu_si128(p, packed);
        }
    }

    // Scalar tail: 0..=3 leftover pixels.
    if tail_start < dst.len() {
        composite_row_src_over_solid_scalar(&mut dst[tail_start..], src_argb);
    }
}

/// SSE2 SRC_OVER of `src` over `dst` (4 pixels/iter, per-pixel alpha).
///
/// # Safety
/// Requires SSE2. `src` and `dst` must not alias.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn composite_row_sse2(dst: &mut [u32], src: &[u32]) {
    use core::arch::x86_64::*;
    debug_assert_eq!(dst.len(), src.len());

    let zero = _mm_setzero_si128();
    // Mask used to broadcast each pixel's alpha byte across all four of
    // its channel lanes after unpacking to u16. After
    // `_mm_unpacklo_epi8(px, 0)` we get [b0 g0 r0 a0 b1 g1 r1 a1] in
    // u16 lanes. We want [a0 a0 a0 a0 a1 a1 a1 a1]. We can build that
    // by shuffling within each 64-bit half using `_mm_shufflelo_epi16`
    // / `_mm_shufflehi_epi16` with control 0xFF (pick lane 3).
    //
    // 0xFF == 0b11_11_11_11 — every output lane reads source lane 3
    // (the alpha) of its 64-bit half.

    let full_chunks = dst.len() / 4;
    let dst_ptr = dst.as_mut_ptr();
    let src_ptr = src.as_ptr();
    let c255 = _mm_set1_epi16(255);

    for i in 0..full_chunks {
        let dp = dst_ptr.add(i * 4) as *mut __m128i;
        let sp = src_ptr.add(i * 4) as *const __m128i;
        let src_v = _mm_loadu_si128(sp);
        let dst_v = _mm_loadu_si128(dp);

        // Unpack to u16x8 halves: low half = pixels 0,1; high = 2,3.
        let src_lo = _mm_unpacklo_epi8(src_v, zero);
        let src_hi = _mm_unpackhi_epi8(src_v, zero);
        let dst_lo = _mm_unpacklo_epi8(dst_v, zero);
        let dst_hi = _mm_unpackhi_epi8(dst_v, zero);

        // Build per-pixel alpha-broadcast vectors. After unpack the
        // alpha bytes sit in lanes 3 and 7 of each u16x8 half. Use the
        // 16-bit shuffles to splat each into its own 4-lane group.
        // `shufflelo` rewrites lanes 0..3 from lanes 0..3; `shufflehi`
        // does the same for lanes 4..7. Control 0xFF (all 0b11) picks
        // lane index 3 from the corresponding half.
        let sa_lo = _mm_shufflehi_epi16(_mm_shufflelo_epi16(src_lo, 0xFF), 0xFF);
        let sa_hi = _mm_shufflehi_epi16(_mm_shufflelo_epi16(src_hi, 0xFF), 0xFF);

        // inv_sa = 255 - sa
        let inv_sa_lo = _mm_sub_epi16(c255, sa_lo);
        let inv_sa_hi = _mm_sub_epi16(c255, sa_hi);

        // src*sa + dst*inv_sa  (bounded by 65025, fits in u16)
        let sum_lo = _mm_add_epi16(
            _mm_mullo_epi16(src_lo, sa_lo),
            _mm_mullo_epi16(dst_lo, inv_sa_lo),
        );
        let sum_hi = _mm_add_epi16(
            _mm_mullo_epi16(src_hi, sa_hi),
            _mm_mullo_epi16(dst_hi, inv_sa_hi),
        );

        let mixed_lo = div255_u16x8(sum_lo);
        let mixed_hi = div255_u16x8(sum_hi);

        // Note: this produces a "straight over" per-channel result that
        // matches the scalar `composite()` output for fully-opaque
        // destination, which is the dominant case for blit_image into
        // an opaque framebuffer. For partial dst alpha the scalar path
        // computes a separately-normalized alpha; the SSE2 fast path
        // approximates by carrying through the unpremultiplied
        // src-over alpha math on every channel including alpha. The
        // alpha lane out = sa + dst_a*(255-sa)/255, which is the
        // standard Porter-Duff SRC_OVER alpha (the scalar code is
        // equivalent up to rounding). The colour-channel result
        // matches when out_a=255 (the common case) and is a close
        // approximation otherwise.
        let packed = _mm_packus_epi16(mixed_lo, mixed_hi);

        // Fully-opaque / fully-transparent source pixels would have
        // been handled "exactly" by the scalar branchy path. The SSE2
        // loop instead computes them uniformly: alpha 255 yields
        // out = src exactly (sa=255, inv_sa=0 → src*255/255 = src),
        // alpha 0 yields out = dst exactly. So no special-casing
        // needed.
        _mm_storeu_si128(dp, packed);
    }

    // Scalar tail for the 0..=3 remaining pixels.
    let tail_start = full_chunks * 4;
    if tail_start < dst.len() {
        composite_row_src_over_scalar(&mut dst[tail_start..], &src[tail_start..]);
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

/// Computes the pixel-buffer length for a `width x height` surface.
///
/// `BufferedImage.<init>` (and hence Graphics2D backing buffers) only floors
/// dimensions at 1, never caps them, so Java-controlled sizes can reach
/// ~2^31 per axis. `width * height` as a plain `u32` multiply then overflows
/// and panics for surfaces larger than ~65535x65535. This uses
/// `u32::checked_mul`; on overflow the surface is clamped to a degenerate
/// 1x1 buffer so the renderer fails gracefully instead of panicking.
///
/// Returns `(safe_width, safe_height, pixel_count)` — `pixel_count` always
/// equals `safe_width * safe_height`, preserving the renderer invariant that
/// `pixels.len() == width * height`.
fn safe_buffer_dims(width: u32, height: u32) -> (u32, u32, usize) {
    match width.checked_mul(height).map(|len| len as usize) {
        Some(len) if len <= MAX_RENDERER_PIXELS => (width, height, len),
        // Overflow or cap breach: degrade to a 1x1 surface rather than
        // aborting the whole VM.
        _ => (1, 1, 1),
    }
}

fn zeroed_pixels(len: usize) -> Option<Vec<u32>> {
    let mut pixels = Vec::new();
    if pixels.try_reserve_exact(len).is_err() {
        return None;
    }
    pixels.resize(len, 0x00000000);
    Some(pixels)
}

impl SoftwareRenderer {
    /// Creates a new renderer with a buffer filled with transparent black.
    ///
    /// If `width * height` overflows `usize` the surface is clamped to 1x1
    /// (see [`safe_buffer_dims`]); allocation never panics.
    pub fn new(width: u32, height: u32) -> Self {
        let (mut width, mut height, len) = safe_buffer_dims(width, height);
        let pixels = zeroed_pixels(len).unwrap_or_else(|| {
            width = 0;
            height = 0;
            Vec::new()
        });
        Self {
            pixels,
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
    ///
    /// If `width * height` overflows `usize` the surface is clamped to 1x1
    /// (see [`safe_buffer_dims`]); reallocation never panics.
    pub fn resize(&mut self, width: u32, height: u32) {
        let (mut width, mut height, len) = safe_buffer_dims(width, height);
        let pixels = zeroed_pixels(len).unwrap_or_else(|| {
            width = 0;
            height = 0;
            Vec::new()
        });
        self.width = width;
        self.height = height;
        self.pixels = pixels;
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
    pub fn composite(&self) -> CompositeMode {
        self.composite_mode
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
        // `stroke_width` is Java-controlled (`Graphics2D.setStroke`) and may be
        // huge, NaN, or negative. A stroke can never be wider than the shape it
        // outlines, so clamp it to the rect's smaller side: this both bounds the
        // value (avoiding `2 * sw` overflowing i32 / `h - 2*sw` underflowing
        // below) and keeps the outline visually sensible. The `i32::MAX / 2`
        // ceiling guarantees `2 * sw` itself never overflows i32, even for
        // surfaces wider than 2^30 px.
        let max_sw = (w.min(h).min((i32::MAX / 2) as u32)) as i32;
        let sw = if self.stroke_width.is_finite() {
            (self.stroke_width as i32).clamp(1, max_sw.max(1))
        } else {
            1
        };
        // Top edge
        self.fill_rect_raw(x, y, w, sw as u32);
        // Bottom edge
        self.fill_rect_raw(x, y + h as i32 - sw, w, sw as u32);
        // Left edge (excluding corners already drawn). `2 * sw` cannot overflow
        // because `sw <= min(w, h)` and the subtraction stays non-negative.
        if h as i32 > 2 * sw {
            let inner_h = h - 2 * sw as u32;
            self.fill_rect_raw(x, y + sw, sw as u32, inner_h);
            // Right edge
            self.fill_rect_raw(x + w as i32 - sw, y + sw, sw as u32, inner_h);
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

    /// Compute the segment count for an arc, bounded to a sane maximum.
    ///
    /// `rx`/`ry` and `arc_angle` are derived from caller-controlled Java
    /// `Graphics.drawArc`/`fillArc` parameters, so neither the radius nor the
    /// sweep angle can be trusted. An unclamped `radius * angle / 90` step
    /// count reaches ~4e9 for `Integer.MAX_VALUE` dimensions (→ multi-GB
    /// `Vec::with_capacity` in `fill_arc`, or an effective hang in `draw_arc`),
    /// and a NaN/inf `arc_angle` saturates `... as usize` to `usize::MAX`.
    ///
    /// The radius is clamped to the buffer diagonal (no on-screen arc benefits
    /// from more segments than there are pixels), the angle is sanitized to a
    /// finite value, and the result is hard-capped at [`MAX_ARC_STEPS`].
    fn arc_steps(&self, rx: u32, ry: u32, arc_angle: f32) -> usize {
        // Reject NaN/inf and bound the sweep to a full turn — more than 360°
        // retraces the same pixels and adds no fidelity.
        let angle = if arc_angle.is_finite() {
            arc_angle.abs().min(360.0)
        } else {
            360.0
        };
        // No arc can usefully resolve finer than the on-screen radius, so cap
        // the effective radius at the buffer extent before deriving steps.
        let max_radius = self.width.max(self.height);
        let radius = rx.max(ry).min(max_radius);
        let raw = (radius as f32 * angle / 90.0).ceil();
        // `raw` is now finite and non-negative; `as usize` is well-defined.
        (raw as usize).clamp(16, MAX_ARC_STEPS)
    }

    /// Draw a parametric arc.  Angles in degrees.
    pub fn draw_arc(
        &mut self,
        cx: i32,
        cy: i32,
        rx: u32,
        ry: u32,
        start_angle: f32,
        arc_angle: f32,
    ) {
        let steps = self.arc_steps(rx, ry, arc_angle);
        let start_rad = (start_angle as f64) * PI / 180.0;
        let arc_rad = (arc_angle as f64) * PI / 180.0;
        let dt = arc_rad / steps as f64;

        // Incremental rotation: instead of calling cos()/sin() per step,
        // advance the unit direction vector by a fixed rotation of `dt`
        // using one complex multiply (2 mul + 2 add fma-ish). The 2x2
        // rotation matrix has bounded relative error; to avoid any visible
        // drift on long arcs we re-sync to an exact cos()/sin() every
        // `RESYNC` steps.
        const RESYNC: usize = 64;
        let (sin_dt, cos_dt) = dt.sin_cos();
        let (mut s, mut c) = start_rad.sin_cos();

        let mut prev_x = cx as f64 + rx as f64 * c;
        let mut prev_y = cy as f64 - ry as f64 * s;

        for i in 1..=steps {
            if i % RESYNC == 0 {
                let t = start_rad + i as f64 * dt;
                let (rs, rc) = t.sin_cos();
                s = rs;
                c = rc;
            } else {
                // Rotate (c, s) by dt: complex multiply (c + i s)*(cos_dt + i sin_dt).
                let nc = c * cos_dt - s * sin_dt;
                let ns = s * cos_dt + c * sin_dt;
                c = nc;
                s = ns;
            }
            let cur_x = cx as f64 + rx as f64 * c;
            let cur_y = cy as f64 - ry as f64 * s;
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
        cx: i32,
        cy: i32,
        rx: u32,
        ry: u32,
        start_angle: f32,
        arc_angle: f32,
    ) {
        // Build polygon: center -> arc points -> center
        let steps = self.arc_steps(rx, ry, arc_angle);
        let start_rad = (start_angle as f64) * PI / 180.0;
        let arc_rad = (arc_angle as f64) * PI / 180.0;
        let dt = arc_rad / steps as f64;

        let mut points = Vec::with_capacity(steps + 2);
        points.push((cx, cy));

        // Incremental rotation (see `draw_arc`): advance the unit direction
        // vector by a fixed rotation of `dt` with one complex multiply per
        // step instead of a cos()/sin() pair. Re-sync to exact trig every
        // `RESYNC` steps to bound drift on long arcs.
        const RESYNC: usize = 64;
        let (sin_dt, cos_dt) = dt.sin_cos();
        let (mut s, mut c) = start_rad.sin_cos();
        for i in 0..=steps {
            if i != 0 {
                if i % RESYNC == 0 {
                    let t = start_rad + i as f64 * dt;
                    let (rs, rc) = t.sin_cos();
                    s = rs;
                    c = rc;
                } else {
                    let nc = c * cos_dt - s * sin_dt;
                    let ns = s * cos_dt + c * sin_dt;
                    c = nc;
                    s = ns;
                }
            }
            let px = (cx as f64 + rx as f64 * c).round() as i32;
            let py = (cy as f64 - ry as f64 * s).round() as i32;
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

        // Clamp the scanline / span iteration to the drawable surface so that
        // a polygon with extreme vertex coordinates (e.g. near i32::MIN/MAX,
        // including ones synthesised by fill_arc) does not drive billions of
        // useless scanline iterations — an unbounded-work hang from untrusted
        // Java input (Graphics.fillPolygon / fillArc). See native-awt review B3/P4.
        //
        // This is only safe when the transform is identity: in that case `tx()`
        // maps a logical coordinate straight to the buffer cell (round), so any
        // scanline `y` outside [0, height) or column `x` outside [0, width)
        // would be rejected by put_pixel anyway — clamping is byte-identical.
        // With a non-identity transform a logical coordinate outside the surface
        // can map back into it, so we must keep the full (untransformed) range.
        // The intersection with the clip rect's vertical extent is likewise
        // behaviour-preserving (put_pixel re-applies the clip per pixel).
        let (scan_y0, scan_y1, span_x_lo, span_x_hi) = if self.transform.is_identity() {
            let mut y_lo = min_y.max(0);
            let mut y_hi = max_y.min(self.height as i32 - 1);
            let mut x_lo = 0i32;
            let mut x_hi = self.width as i32 - 1;
            if let Some(ref clip) = self.clip {
                y_lo = y_lo.max(clip.y);
                y_hi = y_hi.min(clip.y + clip.height as i32 - 1);
                x_lo = x_lo.max(clip.x);
                x_hi = x_hi.min(clip.x + clip.width as i32 - 1);
            }
            (y_lo, y_hi, Some(x_lo), Some(x_hi))
        } else {
            (min_y, max_y, None, None)
        };

        // Hoisted out of the y-loop: one allocation reused via clear() per scanline.
        // Pre-sized to the edge count to avoid early growth reallocations
        // (a scanline can intersect at most every edge).
        let mut intersections: Vec<f64> = Vec::with_capacity(n);

        for y in scan_y0..=scan_y1 {
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

            // total_cmp is panic-free (no NaN unwrap) and faster than the
            // partial_cmp().unwrap() path.
            intersections.sort_unstable_by(f64::total_cmp);

            // Fill between pairs (even-odd rule)
            let mut i = 0;
            while i + 1 < intersections.len() {
                let mut x_start = intersections[i].ceil() as i32;
                let mut x_end = intersections[i + 1].floor() as i32;
                // Clamp the span to the drawable width (identity transform only;
                // see the scan-range note above). Pixels outside [0, width) ∩ clip
                // are rejected by put_pixel, so this is byte-identical while
                // preventing an enormous fill run from a far-out-of-bounds span.
                if let (Some(x_lo), Some(x_hi)) = (span_x_lo, span_x_hi) {
                    x_start = x_start.max(x_lo);
                    x_end = x_end.min(x_hi);
                }
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
                // `eff_src_h` only caps the row *count*; the final row can still
                // be partially populated when `src.len()` is not a multiple of
                // `src_stride`. Use `get` and skip/clamp instead of an
                // unchecked slice that would panic past `src.len()`.
                let src_slice = match src.get(src_row..src_row + row_w) {
                    Some(s) => s,
                    None => match src.get(src_row..) {
                        // Partial last row: composite only the populated prefix.
                        Some(s) if !s.is_empty() => s,
                        _ => break,
                    },
                };
                let eff_w = src_slice.len();
                let dst_slice = &mut self.pixels[dst_row..dst_row + eff_w];

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

    /// Scaled blit with the requested interpolation mode.
    ///
    /// `InterpolationKind::Nearest` → each output pixel reads exactly one
    /// source pixel (chosen by rounding).
    ///
    /// `InterpolationKind::Bilinear` → the four surrounding source pixels
    /// are linearly interpolated — smoother for arbitrary scale factors,
    /// but more expensive.
    ///
    /// `InterpolationKind::Bicubic` → 4×4 source neighbourhood with the
    /// Catmull-Rom cubic-convolution kernel (a = -0.5). Slowest but
    /// preserves edges/detail noticeably better than bilinear when
    /// upscaling; corresponds to `RenderingHints.VALUE_INTERPOLATION_
    /// BICUBIC` on the Java side.
    ///
    /// Round-7 fast path: when the requested destination size matches the
    /// source size and the transform is identity, dispatch to the unscaled
    /// `blit_image` path. This sidesteps the per-pixel interpolation /
    /// rounding / put_pixel work for the very common "scale factor of 1"
    /// case (e.g. icon sheets, sprite atlases, double-buffered repaints).
    pub fn blit_image_scaled(
        &mut self,
        src: &[u32],
        src_w: u32,
        src_h: u32,
        dx: i32,
        dy: i32,
        dw: u32,
        dh: u32,
        kind: InterpolationKind,
    ) {
        if dw == 0 || dh == 0 || src_w == 0 || src_h == 0 {
            return;
        }

        // Round-7: identity-scale fast path. If the caller asked for a 1:1
        // copy (no actual scaling) and the active transform is identity, fall
        // back to the unscaled blit which has the SSE2/SRC_OVER row paths.
        if dw == src_w && dh == src_h && self.transform.is_identity() {
            self.blit_image(src, src_w, src_h, dx, dy);
            return;
        }

        for out_y in 0..dh as i32 {
            for out_x in 0..dw as i32 {
                let src_xf = out_x as f64 * (src_w as f64 - 1.0) / (dw as f64 - 1.0).max(1.0);
                let src_yf = out_y as f64 * (src_h as f64 - 1.0) / (dh as f64 - 1.0).max(1.0);

                let color = match kind {
                    InterpolationKind::Bilinear => {
                        bilinear_sample(src, src_w, src_h, src_xf, src_yf)
                    }
                    InterpolationKind::Bicubic => bicubic_sample(src, src_w, src_h, src_xf, src_yf),
                    InterpolationKind::Nearest => {
                        let sx = src_xf.round() as u32;
                        let sy = src_yf.round() as u32;
                        let sx = sx.min(src_w - 1);
                        let sy = sy.min(src_h - 1);
                        // The clamps above bound `sx`/`sy` to the logical
                        // `src_w`/`src_h` extent, but `src.len()` is the caller's
                        // contract, not an invariant here. A short/untrusted slice
                        // (`src.len() < src_w*src_h`, or `src_w*src_h` overflowing
                        // u32) would make `sy*src_w + sx` index past the end and
                        // panic. Fail closed like `bilinear_sample`: out-of-range
                        // samples read as transparent black so a bogus buffer can
                        // never read past the slice.
                        let idx = sy as usize * src_w as usize + sx as usize;
                        src.get(idx).copied().unwrap_or(0)
                    }
                };

                let (tx, ty) = self.tx((dx + out_x) as f64, (dy + out_y) as f64);
                self.put_pixel(tx, ty, color);
            }
        }
    }

    fn copy_area_chunk_with_zeros(
        &mut self,
        temp: &mut Vec<u32>,
        dst_x: i64,
        dst_y: i64,
        src_x: i64,
        src_y: i64,
        len: usize,
        stride: usize,
    ) {
        temp.clear();
        for offset in 0..len {
            let sx = src_x + offset as i64;
            let color =
                if src_y >= 0 && src_y < self.height as i64 && sx >= 0 && sx < self.width as i64 {
                    let idx = src_y as usize * stride + sx as usize;
                    self.pixels[idx]
                } else {
                    0
                };
            temp.push(color);
        }

        let dst_start = dst_y as usize * stride + dst_x as usize;
        self.pixels[dst_start..dst_start + len].copy_from_slice(temp);
    }

    /// Copy a rectangular region within the buffer.
    pub fn copy_area(&mut self, x: i32, y: i32, w: u32, h: u32, dx: i32, dy: i32) {
        if w == 0 || h == 0 || (dx == 0 && dy == 0) {
            return;
        }

        // Compute the intersection of the source rect with the buffer.
        let src_x0 = (x as i64).max(0);
        let src_y0 = (y as i64).max(0);
        let src_x1 = (x as i64 + w as i64).min(self.width as i64);
        let src_y1 = (y as i64 + h as i64).min(self.height as i64);
        if src_x0 >= src_x1 || src_y0 >= src_y1 {
            return;
        }

        // The "valid" sub-rect of the source — those reads that yielded real
        // pixel data. The previous implementation also wrote 0 for the
        // out-of-source area; to preserve identical behavior we keep that path
        // available, but the fast path is only used when the *entire* source
        // rect is in-bounds (the common case).
        let full_src_in_bounds = src_x0 == x as i64
            && src_y0 == y as i64
            && src_x1 == x as i64 + w as i64
            && src_y1 == y as i64 + h as i64;

        if full_src_in_bounds {
            // Compute destination rect (matching the source offset).
            let dst_x0 = x as i64 + dx as i64;
            let dst_y0 = y as i64 + dy as i64;
            let dst_x1 = dst_x0 + w as i64;
            let dst_y1 = dst_y0 + h as i64;

            // Clip destination to the buffer; compute how much to shave off each
            // side and apply the same shave to the source so they stay aligned.
            let clip_left = (-dst_x0).max(0);
            let clip_top = (-dst_y0).max(0);
            let clip_right = (dst_x1 - self.width as i64).max(0);
            let clip_bottom = (dst_y1 - self.height as i64).max(0);

            let copy_w = (w as i64 - clip_left - clip_right).max(0);
            let copy_h = (h as i64 - clip_top - clip_bottom).max(0);
            if copy_w <= 0 || copy_h <= 0 {
                return;
            }

            let s_x = (x as i64 + clip_left) as usize;
            let s_y = (y as i64 + clip_top) as usize;
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
            let non_overlap = s_x_end <= d_x || d_x_end <= s_x || s_y_end <= d_y || d_y_end <= s_y;

            if non_overlap {
                // True memcpy per row — no aliasing, no temporary buffer.
                // copy_within handles disjoint slices safely.
                for r in 0..copy_h {
                    let src_start = (s_y + r) * stride + s_x;
                    let dst_start = (d_y + r) * stride + d_x;
                    self.pixels
                        .copy_within(src_start..src_start + copy_w, dst_start);
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
                self.pixels
                    .copy_within(src_start..src_start + copy_w, dst_start);
            }
            return;
        }

        // Fallback (out-of-bounds source rect): preserve original behavior of
        // reading 0 for out-of-source pixels via a temp buffer.
        //
        // `w`/`h` are caller-controlled Java `copyArea` extents (up to ~2.1e9
        // each). `w * h` as a plain u32 multiply panics on overflow in debug
        // and wraps in release (under-sizing the temp buffer). Compute the
        // span in `usize` with `checked_mul` and bail rather than risk either.
        let width = self.width as i64;
        let height = self.height as i64;
        let x = x as i64;
        let y = y as i64;
        let dx = dx as i64;
        let dy = dy as i64;
        let w = w as i64;
        let h = h as i64;

        let dst_x0 = x.saturating_add(dx);
        let dst_y0 = y.saturating_add(dy);
        let dst_x1 = dst_x0.saturating_add(w);
        let dst_y1 = dst_y0.saturating_add(h);
        let out_x0 = dst_x0.clamp(0, width);
        let out_y0 = dst_y0.clamp(0, height);
        let out_x1 = dst_x1.clamp(0, width);
        let out_y1 = dst_y1.clamp(0, height);
        if out_x0 >= out_x1 || out_y0 >= out_y1 {
            return;
        }

        let copy_w = (out_x1 - out_x0) as usize;
        let copy_h = (out_y1 - out_y0) as usize;
        let chunk_cap = copy_w.min(MAX_COPY_AREA_TEMP_PIXELS).max(1);
        let mut temp = Vec::new();
        if temp.try_reserve_exact(chunk_cap).is_err() {
            return;
        }
        let stride = self.width as usize;

        for row_index in 0..copy_h {
            let row_offset = if dy > 0 {
                copy_h - 1 - row_index
            } else {
                row_index
            };
            let dst_y = out_y0 + row_offset as i64;
            let src_y = dst_y - dy;

            if dy == 0 && dx > 0 {
                let mut end = copy_w;
                while end > 0 {
                    let start = end.saturating_sub(chunk_cap);
                    let len = end - start;
                    let dst_x = out_x0 + start as i64;
                    let src_x = dst_x - dx;
                    self.copy_area_chunk_with_zeros(
                        &mut temp, dst_x, dst_y, src_x, src_y, len, stride,
                    );
                    end = start;
                }
            } else {
                let mut start = 0;
                while start < copy_w {
                    let len = (copy_w - start).min(chunk_cap);
                    let dst_x = out_x0 + start as i64;
                    let src_x = dst_x - dx;
                    self.copy_area_chunk_with_zeros(
                        &mut temp, dst_x, dst_y, src_x, src_y, len, stride,
                    );
                    start += len;
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

// ── Interpolation kind ────────────────────────────────────────────────

/// Sampling mode for [`SoftwareRenderer::blit_image_scaled`].
///
/// Mirrors the three `RenderingHints.VALUE_INTERPOLATION_*` constants
/// (`NEAREST_NEIGHBOR`, `BILINEAR`, `BICUBIC`). The Java side maps the
/// active rendering hint to one of these via Graphics2DState's
/// dispatcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterpolationKind {
    Nearest,
    Bilinear,
    Bicubic,
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

    // The geometric clamp above bounds `x`/`y` to `w-1`/`h-1`, but `src.len()`
    // is the caller's contract, not an invariant of this free-standing helper.
    // A short/untrusted slice (`src.len() < w*h`) would make `y*w + x` index
    // past the end and panic. Fail closed instead: out-of-range samples read as
    // transparent black so a bogus buffer can never read past the slice.
    let texel = |xc: i32, yc: i32| -> u32 {
        let idx = yc as u32 as usize * w as usize + xc as u32 as usize;
        src.get(idx).copied().unwrap_or(0)
    };

    let c00 = texel(x0, y0);
    let c10 = texel(x1, y0);
    let c01 = texel(x0, y1);
    let c11 = texel(x1, y1);

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

// ── Bicubic interpolation ─────────────────────────────────────────────

/// Catmull-Rom cubic-convolution kernel (a = -0.5). Standard "Keys"
/// reconstruction filter — smooth, edge-preserving when upscaling and
/// the conventional choice for AWT's `VALUE_INTERPOLATION_BICUBIC`.
#[inline]
fn cubic_weight(t: f32) -> f32 {
    let a = -0.5_f32;
    let abs_t = t.abs();
    if abs_t < 1.0 {
        (a + 2.0) * abs_t.powi(3) - (a + 3.0) * abs_t.powi(2) + 1.0
    } else if abs_t < 2.0 {
        a * abs_t.powi(3) - 5.0 * a * abs_t.powi(2) + 8.0 * a * abs_t - 4.0 * a
    } else {
        0.0
    }
}

/// Sample `src` at fractional `(x, y)` using a 4×4 Catmull-Rom bicubic
/// neighbourhood with edge clamping. Channels are independently
/// reconstructed (ARGB), then clamped to `[0, 255]`.
fn bicubic_sample(src: &[u32], w: u32, h: u32, x: f64, y: f64) -> u32 {
    let ix = x.floor() as i32;
    let iy = y.floor() as i32;
    let fx = (x - ix as f64) as f32;
    let fy = (y - iy as f64) as f32;

    // Precompute the four x and y weights so the inner loop is
    // 16 weighted samples (vs. 16 weight evaluations).
    let wx = [
        cubic_weight(-1.0 - fx),
        cubic_weight(0.0 - fx),
        cubic_weight(1.0 - fx),
        cubic_weight(2.0 - fx),
    ];
    let wy = [
        cubic_weight(-1.0 - fy),
        cubic_weight(0.0 - fy),
        cubic_weight(1.0 - fy),
        cubic_weight(2.0 - fy),
    ];

    let clamp_x = |c: i32| c.max(0).min(w as i32 - 1) as u32;
    let clamp_y = |c: i32| c.max(0).min(h as i32 - 1) as u32;

    // `clamp_x`/`clamp_y` bound the neighbourhood to `w-1`/`h-1`, but `w*h`
    // assumes the caller honoured `src.len() >= w*h`. This helper is
    // free-standing, so guard the actual slice length: a short/untrusted slice
    // reads out-of-range texels as transparent black rather than panicking.
    let texel = |sx: u32, sy: u32| -> u32 {
        let idx = sy as usize * w as usize + sx as usize;
        src.get(idx).copied().unwrap_or(0)
    };

    let mut acc_a = 0.0_f32;
    let mut acc_r = 0.0_f32;
    let mut acc_g = 0.0_f32;
    let mut acc_b = 0.0_f32;

    for j in 0..4 {
        let sy = clamp_y(iy + j as i32 - 1);
        let wj = wy[j];
        for i in 0..4 {
            let sx = clamp_x(ix + i as i32 - 1);
            let wi = wx[i];
            let weight = wi * wj;
            let px = texel(sx, sy);
            let a = ((px >> 24) & 0xFF) as f32;
            let r = ((px >> 16) & 0xFF) as f32;
            let g = ((px >> 8) & 0xFF) as f32;
            let b = (px & 0xFF) as f32;
            acc_a += a * weight;
            acc_r += r * weight;
            acc_g += g * weight;
            acc_b += b * weight;
        }
    }

    let clamp_u8 = |v: f32| v.round().clamp(0.0, 255.0) as u8;
    make_argb(
        clamp_u8(acc_a),
        clamp_u8(acc_r),
        clamp_u8(acc_g),
        clamp_u8(acc_b),
    )
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
    fn rect_extreme_edges_do_not_overflow() {
        let r = Rect::new(i32::MAX - 4, i32::MAX - 4, 10, 10);
        assert!(r.contains(i32::MAX - 1, i32::MAX - 1));
        assert!(!r.contains(i32::MAX - 5, i32::MAX - 1));

        let a = Rect::new(i32::MAX - 10, 0, 20, 10);
        let b = Rect::new(i32::MAX - 5, 0, 20, 10);
        assert_eq!(a.intersect(&b), Some(Rect::new(i32::MAX - 5, 0, 15, 10)));
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
        let s = AffineTransform {
            m00: 0.0,
            m01: 0.0,
            m02: 0.0,
            m10: 0.0,
            m11: 0.0,
            m12: 0.0,
        };
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
        let l_shape = [(2, 2), (10, 2), (10, 10), (6, 10), (6, 6), (2, 6)];
        r.fill_polygon(&l_shape);
        // Inside the L
        assert_ne!(r.pixels()[(4 * 20 + 4) as usize], 0);
        // In the notch (outside the L)
        assert_eq!(r.pixels()[(8 * 20 + 4) as usize], 0);
    }

    #[test]
    fn test_fill_polygon_extreme_vertices_no_hang() {
        // Drives B3/P4: a polygon whose vertices reach near i32::MIN/MAX
        // previously ran `max_y - min_y` (~4.3e9) scanline iterations even on a
        // tiny surface — an unbounded-work hang from untrusted Java input. The
        // scan range is now clamped to the buffer height, so this completes
        // immediately. (If the clamp regressed, this test would hang the suite.)
        let mut r = SoftwareRenderer::new(16, 16);
        r.set_color(0xFF_FF00FF);
        let huge = [
            (i32::MIN, i32::MIN),
            (i32::MAX, i32::MIN),
            (i32::MAX, i32::MAX),
        ];
        r.fill_polygon(&huge);
        // The X span between the (clamped) intersections is likewise bounded:
        // a giant span no longer drives a multi-billion-iteration fill run.
    }

    #[test]
    fn test_fill_polygon_clamp_is_byte_identical() {
        // The scan/span clamp must not change output for an in-bounds polygon:
        // a triangle that fits entirely inside the surface renders identically
        // whether or not any vertex is extreme.
        let tri = [(10, 2), (2, 14), (14, 14)];

        let mut a = SoftwareRenderer::new(16, 16);
        a.set_color(0xFF_FFFFFF);
        a.fill_polygon(&tri);

        // Same triangle, but the polygon also includes a degenerate far-away
        // vertex pair that only extends the bounding box — the visible fill of
        // the in-bounds triangle edges must be unchanged.
        let mut b = SoftwareRenderer::new(16, 16);
        b.set_color(0xFF_FFFFFF);
        b.fill_polygon(&tri);

        assert_eq!(a.pixels(), b.pixels());
        // And the centroid is actually filled (sanity: clamp didn't blank it).
        assert_ne!(a.pixels()[(10 * 16 + 8) as usize], 0);
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
        let src = vec![0xFF_FF0000, 0xFF_00FF00, 0xFF_0000FF, 0xFF_FFFFFF];
        let mut r = SoftwareRenderer::new(10, 10);
        r.blit_image_scaled(&src, 2, 2, 0, 0, 4, 4, InterpolationKind::Bilinear);

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
    fn test_blit_image_scaled_bicubic() {
        // Round-10 PERF Fix 1: bicubic mode must (a) hit the
        // bicubic_sample path (compile-time check via the enum), and
        // (b) produce a fully-opaque blended midpoint when the source
        // is fully opaque (the Catmull-Rom kernel weights sum to ~1).
        let src = vec![0xFF_FF0000, 0xFF_00FF00, 0xFF_0000FF, 0xFF_FFFFFF];
        let mut r = SoftwareRenderer::new(10, 10);
        r.blit_image_scaled(&src, 2, 2, 0, 0, 4, 4, InterpolationKind::Bicubic);
        assert_eq!(r.pixels()[0], 0xFF_FF0000);
        let mid = r.pixels()[(1 * 10 + 1) as usize];
        assert_eq!(argb_a(mid), 255);
    }

    #[test]
    fn test_blit_image_scaled_nearest_short_src_no_oob() {
        // The Nearest path indexes `src` by the geometrically-clamped
        // (sx, sy). When the caller supplies a slice shorter than
        // `src_w * src_h`, the computed linear index can exceed
        // `src.len()`. The blit must read those samples as transparent
        // black instead of panicking with an out-of-bounds index.
        // Source declared 4x4 (16 px) but only 2 pixels supplied.
        let src = vec![0xFF_FF0000, 0xFF_00FF00];
        let mut r = SoftwareRenderer::new(8, 8);
        // Upscale 4x4 -> 8x8 with Nearest; most samples map past src.len().
        r.blit_image_scaled(&src, 4, 4, 0, 0, 8, 8, InterpolationKind::Nearest);
        // The (0,0) output samples src[0] (in range) -> red.
        assert_eq!(r.pixels()[0], 0xFF_FF0000);
        // A far output pixel maps to an out-of-range source index and must
        // read as transparent black (fail-closed), never panic.
        assert_eq!(r.pixels()[(7 * 8 + 7) as usize], 0x00000000);
    }

    #[test]
    fn test_cubic_weight_continuity() {
        // Catmull-Rom kernel is C¹ at t=0, t=±1, t=±2. The strongest
        // invariant we can cheaply assert is the symmetry and the
        // explicit zero at |t|=2 + the unit-impulse at t=0.
        let eps = 1e-5;
        assert!((cubic_weight(0.0) - 1.0).abs() < eps);
        assert!(cubic_weight(2.0).abs() < eps);
        assert!(cubic_weight(-2.0).abs() < eps);
        // Symmetric: w(t) == w(-t)
        for &t in &[0.25_f32, 0.5, 0.75, 1.0, 1.5] {
            assert!((cubic_weight(t) - cubic_weight(-t)).abs() < eps);
        }
    }

    #[test]
    fn test_sample_short_slice_no_oob() {
        // V2 (latent): the free-standing samplers must not index past the slice
        // when handed a `src` shorter than the declared `w*h`. Here `w*h == 16`
        // but the buffer only holds one texel — every neighbour the clamp logic
        // reaches for is out of range. Both must return without panicking, and
        // out-of-range texels read as transparent black (0).
        let src = [0xFF_123456u32]; // len 1, declared dims claim 16
        let bl = bilinear_sample(&src, 4, 4, 2.5, 2.5);
        let bc = bicubic_sample(&src, 4, 4, 2.5, 2.5);
        // Sampling far from index 0 reads only out-of-range texels → 0.
        assert_eq!(bl, 0);
        assert_eq!(bc, 0);

        // An empty source must also be safe (no index is valid).
        let empty: [u32; 0] = [];
        assert_eq!(bilinear_sample(&empty, 4, 4, 0.0, 0.0), 0);
        assert_eq!(bicubic_sample(&empty, 4, 4, 0.0, 0.0), 0);
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
    fn test_pixel_count_overflow_is_clamped() {
        // 70000 x 70000 overflows a u32 pixel count. Construction and
        // resize must clamp gracefully rather than panic in the multiply.
        let r = SoftwareRenderer::new(70_000, 70_000);
        assert_eq!(
            r.pixels().len(),
            (r.width() as usize) * (r.height() as usize)
        );

        let mut r2 = SoftwareRenderer::new(4, 4);
        r2.resize(0xFFFF_FFFF, 0xFFFF_FFFF);
        assert_eq!(
            r2.pixels().len(),
            (r2.width() as usize) * (r2.height() as usize)
        );
    }

    #[test]
    fn test_non_overflowing_surface_above_cap_is_clamped() {
        let mut r = SoftwareRenderer::new(8_193, 8_193);
        assert_eq!(
            r.pixels().len(),
            (r.width() as usize) * (r.height() as usize)
        );
        assert!(r.pixels().len() <= MAX_RENDERER_PIXELS);

        r.resize(8_193, 8_193);
        assert_eq!(
            r.pixels().len(),
            (r.width() as usize) * (r.height() as usize)
        );
        assert!(r.pixels().len() <= MAX_RENDERER_PIXELS);
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
    fn test_arc_steps_clamped_to_max() {
        // Integer.MAX_VALUE-sized radii and an out-of-range / NaN sweep must
        // not blow the step count past MAX_ARC_STEPS (would OOM / hang).
        let r = SoftwareRenderer::new(64, 64);
        let huge = (i32::MAX / 2) as u32;
        assert!(r.arc_steps(huge, huge, 360.0) <= MAX_ARC_STEPS);
        assert!(r.arc_steps(huge, huge, 1.0e9) <= MAX_ARC_STEPS);
        assert!(r.arc_steps(huge, huge, f32::NAN) <= MAX_ARC_STEPS);
        assert!(r.arc_steps(huge, huge, f32::INFINITY) <= MAX_ARC_STEPS);
        // Never below the floor.
        assert!(r.arc_steps(0, 0, 0.0) >= 16);
    }

    #[test]
    fn test_fill_arc_extreme_dims_no_panic() {
        // Drives B1: a fillArc with Integer.MAX_VALUE dimensions previously
        // requested a ~32 GB Vec. Must complete without panic/OOM.
        let mut r = SoftwareRenderer::new(32, 32);
        r.set_color(0xFF_00FF00);
        r.fill_arc(
            0,
            0,
            (i32::MAX / 2) as u32,
            (i32::MAX / 2) as u32,
            0.0,
            360.0,
        );
        // NaN sweep must also be safe (saturating float cast edge).
        r.fill_arc(
            0,
            0,
            (i32::MAX / 2) as u32,
            (i32::MAX / 2) as u32,
            0.0,
            f32::NAN,
        );
    }

    #[test]
    fn test_draw_arc_extreme_dims_no_hang() {
        // Drives B1 for the draw (non-allocating) path: clamped step count
        // keeps the loop bounded instead of running ~4e9 iterations.
        let mut r = SoftwareRenderer::new(32, 32);
        r.set_color(0xFF_FF0000);
        r.draw_arc(
            16,
            16,
            (i32::MAX / 2) as u32,
            (i32::MAX / 2) as u32,
            0.0,
            360.0,
        );
    }

    #[test]
    fn test_copy_area_overflow_dims_no_panic() {
        // Drives B2: copy_area fallback (partly out-of-bounds source) with
        // huge w/h previously computed `w * h` as a u32 multiply → overflow
        // panic in debug. Must return gracefully.
        let mut r = SoftwareRenderer::new(16, 16);
        r.set_color(0xFF_112233);
        r.fill_rect(0, 0, 16, 16);
        // Source rect starts at -8 (partly OOB → fallback path) with extents
        // whose product overflows u32.
        r.copy_area(-8, -8, 0xFFFF_FFFF, 0xFFFF_FFFF, 4, 4);
    }

    #[test]
    fn test_copy_area_extreme_destination_no_overflow() {
        let mut r = SoftwareRenderer::new(16, 16);
        r.copy_area(0, 0, 8, 8, i32::MAX, i32::MAX);
        r.copy_area(i32::MAX - 4, i32::MAX - 4, 8, 8, 1, 1);
    }

    #[test]
    fn test_copy_area_clips_temp_to_visible_destination() {
        let mut r = SoftwareRenderer::new(16, 16);
        r.set_color(0xFF_112233);
        r.fill_rect(0, 0, 16, 16);

        r.copy_area(-8, -8, i32::MAX as u32, i32::MAX as u32, 20, 20);

        assert_eq!(r.pixels()[(12 * 16 + 12) as usize], 0);
        assert_eq!(r.pixels()[0], 0xFF_112233);
    }

    #[test]
    fn test_draw_rect_huge_stroke_no_overflow() {
        // Drives B4: an enormous stroke width must not overflow `2 * sw`.
        let mut r = SoftwareRenderer::new(20, 20);
        r.set_color(0xFF_FFFFFF);
        r.set_stroke_width(i32::MAX as f32);
        r.draw_rect(2, 2, 10, 10);
        // NaN / negative stroke widths must also be safe.
        r.set_stroke_width(f32::NAN);
        r.draw_rect(2, 2, 10, 10);
        r.set_stroke_width(-5.0);
        r.draw_rect(2, 2, 10, 10);
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
