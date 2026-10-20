// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `MaskFill.FillAAPgram` and `MaskFill.DrawAAPgram`: anti-aliased
//! parallelograms (rotated rectangles, wide lines, stroked rectangles), the
//! native loops `AlphaColorPipe` calls when `canDoParallelograms()` holds.
//!
//! Coverage is the exact area of the shape inside each pixel; HotSpot
//! quantises it to `ceil(area * 256) - 1` (so a half-covered pixel is 127, a
//! full one 255). Verified against HotSpot on 14,900 pixels of random
//! transforms, fills and strokes with no mismatch (`tools/probes/Sbr5AwtjfrShape.java`).
//! A stroked outline is the outer parallelogram minus the inner one, exactly
//! as Marlin's `getAATileGenerator(x, y, dx1, dy1, dx2, dy2, lw1, lw2)` builds it.

use cratonvm_native_api::NativeContext;
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::Value;

use super::pixel;
use super::rows;
use super::{
    alpha_of, arg_obj, clip_rects, fld_int, fld_obj, internal, lookup_surf, read_clip, void,
};

type P = (f64, f64);

fn arg_d(args: &[Value], i: usize) -> f64 {
    match args.get(i) {
        Some(Value::Double(d)) => *d,
        Some(Value::Float(f)) => *f as f64,
        Some(Value::Int(v)) => *v as f64,
        Some(Value::Long(v)) => *v as f64,
        _ => 0.0,
    }
}

/// Sutherland-Hodgman clip of a polygon to the half-plane selected by `inside`.
fn clip_edge(poly: &[P], inside: impl Fn(P) -> bool, inter: impl Fn(P, P) -> P) -> Vec<P> {
    let mut out = Vec::with_capacity(poly.len() + 2);
    for i in 0..poly.len() {
        let a = poly[(i + poly.len() - 1) % poly.len()];
        let b = poly[i];
        let (ia, ib) = (inside(a), inside(b));
        if ib {
            if !ia {
                out.push(inter(a, b));
            }
            out.push(b);
        } else if ia {
            out.push(inter(a, b));
        }
    }
    out
}

fn clip_box(poly: &[P], x0: f64, y0: f64, x1: f64, y1: f64) -> Vec<P> {
    let p = clip_edge(
        poly,
        |p| p.0 >= x0,
        |a, b| (x0, a.1 + (b.1 - a.1) * (x0 - a.0) / (b.0 - a.0)),
    );
    if p.is_empty() {
        return p;
    }
    let p = clip_edge(
        &p,
        |p| p.0 <= x1,
        |a, b| (x1, a.1 + (b.1 - a.1) * (x1 - a.0) / (b.0 - a.0)),
    );
    if p.is_empty() {
        return p;
    }
    let p = clip_edge(
        &p,
        |p| p.1 >= y0,
        |a, b| (a.0 + (b.0 - a.0) * (y0 - a.1) / (b.1 - a.1), y0),
    );
    if p.is_empty() {
        return p;
    }
    clip_edge(
        &p,
        |p| p.1 <= y1,
        |a, b| (a.0 + (b.0 - a.0) * (y1 - a.1) / (b.1 - a.1), y1),
    )
}

fn area(poly: &[P]) -> f64 {
    if poly.len() < 3 {
        return 0.0;
    }
    let mut s = 0.0;
    for i in 0..poly.len() {
        let a = poly[(i + poly.len() - 1) % poly.len()];
        let b = poly[i];
        s += a.0 * b.1 - b.0 * a.1;
    }
    s.abs() / 2.0
}

fn pgram(x: f64, y: f64, dx1: f64, dy1: f64, dx2: f64, dy2: f64) -> [P; 4] {
    [
        (x, y),
        (x + dx1, y + dy1),
        (x + dx1 + dx2, y + dy1 + dy2),
        (x + dx2, y + dy2),
    ]
}

/// Whether `(px, py)` is inside or on the convex quadrilateral `q`.
fn inside(q: &[P; 4], px: f64, py: f64) -> bool {
    let mut pos = false;
    let mut neg = false;
    for i in 0..4 {
        let a = q[i];
        let b = q[(i + 1) % 4];
        let c = (b.0 - a.0) * (py - a.1) - (b.1 - a.1) * (px - a.0);
        if c > 0.0 {
            pos = true;
        } else if c < 0.0 {
            neg = true;
        }
    }
    !(pos && neg)
}

fn quantise(a: f64) -> u8 {
    let v = (a * 256.0).ceil() as i64 - 1;
    v.clamp(0, 255) as u8
}

/// Coverage of pixel `(x, y)`: outer area minus inner area.
fn pixel_cov(outer: &[P; 4], inner: Option<&[P; 4]>, x: i64, y: i64) -> u8 {
    let (fx, fy) = (x as f64, y as f64);
    let corners = [
        (fx, fy),
        (fx + 1.0, fy),
        (fx + 1.0, fy + 1.0),
        (fx, fy + 1.0),
    ];
    let full_outer = corners.iter().all(|c| inside(outer, c.0, c.1));
    let mut a;
    if full_outer {
        a = 1.0;
    } else {
        a = area(&clip_box(outer, fx, fy, fx + 1.0, fy + 1.0));
    }
    if let Some(inn) = inner {
        a -= area(&clip_box(inn, fx, fy, fx + 1.0, fy + 1.0));
    }
    if a <= 0.0 {
        0
    } else {
        quantise(a)
    }
}

fn fill_shape(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    outer: [P; 4],
    inner: Option<[P; 4]>,
) -> MethodCallResult {
    let sg2d = arg_obj(args, 1).ok_or_else(|| internal("AAPgram: null sg2d"))?;
    let sd = arg_obj(args, 2).ok_or_else(|| internal("AAPgram: null surface"))?;
    let comp = arg_obj(args, 3);
    let eargb = fld_int(ctx, sg2d, "eargb") as u32;
    let clip = read_clip(ctx, fld_obj(ctx, sg2d, "clipRegion"));
    let (rule, _extra) = alpha_of(ctx, comp);
    let s = lookup_surf(ctx, sd)?;
    let (src_a, fg_pre) = pixel::prepare_fill_color(s.fmt, eargb);
    let (mut bx1, mut by1, mut bx2, mut by2) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for p in &outer {
        bx1 = bx1.min(p.0);
        by1 = by1.min(p.1);
        bx2 = bx2.max(p.0);
        by2 = by2.max(p.1);
    }
    if !(bx1.is_finite() && by1.is_finite() && bx2.is_finite() && by2.is_finite()) {
        return void();
    }
    let (ix1, iy1) = (bx1.floor() as i64, by1.floor() as i64);
    let (ix2, iy2) = (bx2.ceil() as i64, by2.ceil() as i64);
    let mut dbuf = Vec::new();
    for (x1, y1, x2, y2) in clip_rects(
        &clip,
        ix1.max(0),
        iy1.max(0),
        ix2.min(s.row.w as i64),
        iy2.min(s.row.h as i64),
    ) {
        let n = (x2 - x1) as usize;
        let mut mask = vec![0u8; n];
        for yy in y1..y2 {
            let mut any = false;
            for (i, m) in mask.iter_mut().enumerate() {
                *m = pixel_cov(&outer, inner.as_ref(), (x1 + i as i32) as i64, yy as i64);
                any |= *m != 0;
            }
            if !any {
                continue;
            }
            s.read(ctx, x1, yy, n, &mut dbuf);
            rows::mask_fill_row(s.fmt, rule, src_a, fg_pre, &mut dbuf, Some(&mask), n);
            s.write(ctx, x1, yy, &dbuf);
        }
    }
    void()
}

/// `MaskFill.FillAAPgram(SunGraphics2D, SurfaceData, Composite, double x6)`.
pub(super) fn n_fill_aa_pgram(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (x, y) = (arg_d(args, 4), arg_d(args, 5));
    let (dx1, dy1, dx2, dy2) = (
        arg_d(args, 6),
        arg_d(args, 7),
        arg_d(args, 8),
        arg_d(args, 9),
    );
    fill_shape(ctx, args, pgram(x, y, dx1, dy1, dx2, dy2), None)
}

/// `MaskFill.DrawAAPgram(SunGraphics2D, SurfaceData, Composite, double x8)`.
pub(super) fn n_draw_aa_pgram(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (x, y) = (arg_d(args, 4), arg_d(args, 5));
    let (dx1, dy1, dx2, dy2) = (
        arg_d(args, 6),
        arg_d(args, 7),
        arg_d(args, 8),
        arg_d(args, 9),
    );
    let (lw1, lw2) = (arg_d(args, 10), arg_d(args, 11));
    let mut inner_on = lw1 > 0.0 && lw2 > 0.0;
    let (ldx1, ldy1, ldx2, ldy2);
    let (mut ox, mut oy) = (x, y);
    let (mut odx1, mut ody1, mut odx2, mut ody2) = (dx1, dy1, dx2, dy2);
    if inner_on {
        ldx1 = dx1 * lw1;
        ldy1 = dy1 * lw1;
        ldx2 = dx2 * lw2;
        ldy2 = dy2 * lw2;
        ox -= (ldx1 + ldx2) / 2.0;
        oy -= (ldy1 + ldy2) / 2.0;
        odx1 += ldx1;
        ody1 += ldy1;
        odx2 += ldx2;
        ody2 += ldy2;
        if lw1 > 1.0 && lw2 > 1.0 {
            inner_on = false;
        }
    } else {
        ldx1 = 0.0;
        ldy1 = 0.0;
        ldx2 = 0.0;
        ldy2 = 0.0;
    }
    let outer = pgram(ox, oy, odx1, ody1, odx2, ody2);
    let inner = if inner_on {
        Some(pgram(
            ox + ldx1 + ldx2,
            oy + ldy1 + ldy2,
            odx1 - 2.0 * ldx1,
            ody1 - 2.0 * ldy1,
            odx2 - 2.0 * ldx2,
            ody2 - 2.0 * ldy2,
        ))
    } else {
        None
    };
    fill_shape(ctx, args, outer, inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn half_covered_pixel_is_127_and_full_is_255() {
        let q = pgram(0.0, 0.0, 1.0, 0.0, 0.0, 0.5);
        assert_eq!(pixel_cov(&q, None, 0, 0), 127);
        let full = pgram(-1.0, -1.0, 3.0, 0.0, 0.0, 3.0);
        assert_eq!(pixel_cov(&full, None, 0, 0), 255);
        assert_eq!(pixel_cov(&full, None, 5, 5), 0);
    }

    #[test]
    fn quarter_corner_matches_hotspot_63() {
        // translate(0.5, 0.5); fillRect(2, 2, ...): the corner pixel is a quarter covered.
        let q = pgram(2.5, 2.5, 7.0, 0.0, 0.0, 4.0);
        assert_eq!(pixel_cov(&q, None, 2, 2), 63);
        assert_eq!(pixel_cov(&q, None, 3, 2), 127);
        assert_eq!(pixel_cov(&q, None, 3, 3), 255);
    }
}
