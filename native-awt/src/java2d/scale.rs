// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `ScaledBlit`, `BlitBg` and `TransformHelper`: the image-drawing loops that
//! resample (nearest neighbour scaling, background compositing, affine
//! transforms with nearest / bilinear / bicubic interpolation).

use cratonvm_native_api::NativeContext;
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::Value;

use super::pixel::{self, Fmt, Raw};
use super::rows;
use super::{
    alpha_of, arg_int, arg_obj, clip_rects, fld_obj, internal, lookup_surf, read_clip, void,
};

fn arg_f64(args: &[Value], i: usize) -> f64 {
    match args.get(i) {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    }
}

fn fld_f64(ctx: &dyn NativeContext, o: cratonvm_types::ObjectRef, name: &str) -> f64 {
    match ctx.get_field_by_name(o, name) {
        Value::Double(v) => v,
        _ => 0.0,
    }
}

/// `ScaledBlit.Scale(src, dst, comp, clip, sx1, sy1, sx2, sy2, dx1, dy1, dx2, dy2)`.
pub(super) fn n_scaled_blit(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = arg_obj(args, 0).ok_or_else(|| internal("ScaledBlit: null receiver"))?;
    let src_sd = arg_obj(args, 1).ok_or_else(|| internal("ScaledBlit: null source"))?;
    let dst_sd = arg_obj(args, 2).ok_or_else(|| internal("ScaledBlit: null destination"))?;
    let comp = arg_obj(args, 3);
    let clip_obj = arg_obj(args, 4);
    let (sx1, sy1, sx2, sy2) = (
        arg_int(args, 5),
        arg_int(args, 6),
        arg_int(args, 7),
        arg_int(args, 8),
    );
    let (dx1, dy1, dx2, dy2) = (
        arg_f64(args, 9),
        arg_f64(args, 10),
        arg_f64(args, 11),
        arg_f64(args, 12),
    );
    if dx2 <= dx1 || dy2 <= dy1 || sx2 <= sx1 || sy2 <= sy1 {
        return void();
    }
    let convert = match ctx.get_field_by_name(this, "pNativePrim") {
        Value::Long(v) => ((v - 1) >> 16) & 1 == 1,
        _ => false,
    };
    let src = lookup_surf(ctx, src_sd)?;
    let dst = lookup_surf(ctx, dst_sd)?;
    let (rule, extra_a) = alpha_of(ctx, comp);
    let clip = read_clip(ctx, clip_obj);
    let scalex = (sx2 - sx1) as f64 / (dx2 - dx1);
    let scaley = (sy2 - sy1) as f64 / (dy2 - dy1);
    let ix1 = (dx1 - 0.5).ceil() as i64;
    let ix2 = (dx2 - 0.5).ceil() as i64;
    let iy1 = (dy1 - 0.5).ceil() as i64;
    let iy2 = (dy2 - 0.5).ceil() as i64;
    // Fixed-point stepping as `ScaledBlit.c`: the increment is truncated, the
    // location accumulates (measured against HotSpot on ten scale ratios).
    const SHIFT: u32 = 31;
    let one = (1i64 << SHIFT) as f64;
    let sxinc = (scalex * one) as i64;
    let syinc = (scaley * one) as i64;
    let sxloc = (((ix1 as f64 + 0.5 - dx1) * scalex) * one) as i64 + ((sx1 as i64) << SHIFT);
    let syloc = (((iy1 as f64 + 0.5 - dy1) * scaley) * one) as i64 + ((sy1 as i64) << SHIFT);
    let mut sbuf = Vec::new();
    let mut row = Vec::new();
    let mut dbuf = Vec::new();
    for (x1, y1, x2, y2) in clip_rects(
        &clip,
        ix1.max(0),
        iy1.max(0),
        ix2.min(dst.row.w as i64),
        iy2.min(dst.row.h as i64),
    ) {
        let n = (x2 - x1) as usize;
        for yy in y1..y2 {
            let sy = ((syloc + (yy as i64 - iy1) * syinc) >> SHIFT) + 0;
            let sy = sy
                .clamp(sy1 as i64, sy2 as i64 - 1)
                .clamp(0, src.row.h as i64 - 1) as i32;
            // Gather the source row (nearest neighbour) into `row`.
            let bpp = src.fmt.bpp();
            src.read(ctx, 0, sy, src.row.w as usize, &mut sbuf);
            row.clear();
            row.resize(n * bpp, 0);
            for i in 0..n {
                let xx = x1 + i as i32;
                let sx = (sxloc + (xx as i64 - ix1) * sxinc) >> SHIFT;
                let sx = sx
                    .clamp(sx1 as i64, sx2 as i64 - 1)
                    .clamp(0, src.row.w as i64 - 1) as usize;
                row[i * bpp..(i + 1) * bpp].copy_from_slice(&sbuf[sx * bpp..(sx + 1) * bpp]);
            }
            if convert {
                dbuf.clear();
                dbuf.resize(n * dst.fmt.bpp(), 0);
                rows::convert_row(src.fmt, dst.fmt, &row, &mut dbuf, n);
            } else {
                dst.read(ctx, x1, yy, n, &mut dbuf);
                if dst.fmt.is_gray() && src.fmt.has_alpha() && !src.fmt.premult() {
                    // `ScaledBlit.General` scales into an IntArgbPre temporary and
                    // blits that, so the colours are premultiplied before the gray reduction.
                    let mut pre = vec![0u8; n * Fmt::IntArgbPre.bpp()];
                    rows::convert_row(src.fmt, Fmt::IntArgbPre, &row, &mut pre, n);
                    rows::alpha_blit_row(
                        Fmt::IntArgbPre,
                        dst.fmt,
                        rule,
                        extra_a,
                        &pre,
                        &mut dbuf,
                        None,
                        n,
                    );
                } else {
                    rows::alpha_blit_row(src.fmt, dst.fmt, rule, extra_a, &row, &mut dbuf, None, n);
                }
            }
            dst.write(ctx, x1, yy, &dbuf);
        }
    }
    void()
}

/// `BlitBg.BlitBg(src, dst, comp, clip, bgColor, sx, sy, dx, dy, w, h)`: the
/// source composited over the background colour, stored (copy) to the destination.
pub(super) fn n_blit_bg(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let src_sd = arg_obj(args, 1).ok_or_else(|| internal("BlitBg: null source"))?;
    let dst_sd = arg_obj(args, 2).ok_or_else(|| internal("BlitBg: null destination"))?;
    let clip_obj = arg_obj(args, 4);
    let bg = arg_int(args, 5) as u32;
    let (sx, sy, dx, dy, w, h) = (
        arg_int(args, 6),
        arg_int(args, 7),
        arg_int(args, 8),
        arg_int(args, 9),
        arg_int(args, 10),
        arg_int(args, 11),
    );
    if w <= 0 || h <= 0 {
        return void();
    }
    let src = lookup_surf(ctx, src_sd)?;
    let dst = lookup_surf(ctx, dst_sd)?;
    let clip = read_clip(ctx, clip_obj);
    let (ox, oy) = (sx as i64 - dx as i64, sy as i64 - dy as i64);
    let bx1 = (dx as i64).max(0).max(-ox);
    let by1 = (dy as i64).max(0).max(-oy);
    let bx2 = (dx as i64 + w as i64)
        .min(dst.row.w as i64)
        .min(src.row.w as i64 - ox);
    let by2 = (dy as i64 + h as i64)
        .min(dst.row.h as i64)
        .min(src.row.h as i64 - oy);
    let (bga, bgr, bgg, bgb) = (bg >> 24, (bg >> 16) & 0xff, (bg >> 8) & 0xff, bg & 0xff);
    let mut sbuf = Vec::new();
    let mut dbuf = Vec::new();
    for (x1, y1, x2, y2) in clip_rects(&clip, bx1, by1, bx2, by2) {
        let n = (x2 - x1) as usize;
        for yy in y1..y2 {
            src.read(
                ctx,
                (x1 as i64 + ox) as i32,
                (yy as i64 + oy) as i32,
                n,
                &mut sbuf,
            );
            dbuf.clear();
            dbuf.resize(n * dst.fmt.bpp(), 0);
            for i in 0..n {
                if dst.fmt.is_gray() {
                    // `Blit.General` for BlitBg: fill the background, then composite the
                    // source over it in the destination's own (gray) arithmetic.
                    pixel::store_argb(dst.fmt, &mut dbuf, i, bga, bgr, bgg, bgb);
                    let raw = pixel::load_raw(src.fmt, &sbuf, i);
                    pixel::alpha_blit_pixel(src.fmt, dst.fmt, 3, 0xff, raw, 0xff, &mut dbuf, i);
                    continue;
                }
                let p = pixel::load_argb(src.fmt, &sbuf, i);
                let (a, r, g, b) = if p.a == 0xff {
                    (p.a, p.r, p.g, p.b)
                } else if p.a == 0 {
                    (bga, bgr, bgg, bgb)
                } else {
                    // Source over the (opaque or translucent) background.
                    let inv = 0xff - p.a;
                    let bg_w = pixel::mul8(inv, bga);
                    let ra = p.a + bg_w;
                    let mix = |s: u32, d: u32| -> u32 {
                        let v = pixel::mul8(p.a, s) + pixel::mul8(bg_w, d);
                        if ra != 0 && ra < 0xff {
                            pixel::div8(v, ra)
                        } else {
                            v
                        }
                    };
                    (ra, mix(p.r, bgr), mix(p.g, bgg), mix(p.b, bgb))
                };
                pixel::store_argb(dst.fmt, &mut dbuf, i, a, r.min(255), g.min(255), b.min(255));
            }
            dst.write(ctx, x1, yy, &dbuf);
        }
    }
    void()
}

/// One source pixel as premultiplied ARGB channels.
fn load_pre(f: Fmt, src: &[u8], x: usize) -> [i32; 4] {
    let p: Raw = pixel::load_raw(f, src, x);
    if f.premult() || p.a == 0xff {
        [p.a as i32, p.r as i32, p.g as i32, p.b as i32]
    } else {
        [
            p.a as i32,
            pixel::mul8(p.a, p.r) as i32,
            pixel::mul8(p.a, p.g) as i32,
            pixel::mul8(p.a, p.b) as i32,
        ]
    }
}

/// HotSpot's bicubic coefficient table row for fraction `xfa/256`:
/// Catmull-Rom (a = -0.5) x 256, the first three truncated and the
/// last one the remainder so the row sums to 256 (fitted to HotSpot output).
fn cubic_table(xfa: usize) -> [i32; 4] {
    let a = -0.5f64;
    let w = |x: f64| -> f64 {
        let x = x.abs();
        if x <= 1.0 {
            (a + 2.0) * x * x * x - (a + 3.0) * x * x + 1.0
        } else if x < 2.0 {
            a * x * x * x - 5.0 * a * x * x + 8.0 * a * x - 4.0 * a
        } else {
            0.0
        }
    };
    let t = xfa as f64 / 256.0;
    let c0 = (w(1.0 + t) * 256.0) as i32;
    let c1 = (w(t) * 256.0) as i32;
    let c2 = (w(1.0 - t) * 256.0) as i32;
    [c0, c1, c2, 256 - c0 - c1 - c2]
}

/// `TransformHelper.Transform(...)`.
pub(super) fn n_transform_helper(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let src_sd = arg_obj(args, 2).ok_or_else(|| internal("Transform: null source"))?;
    let dst_sd = arg_obj(args, 3).ok_or_else(|| internal("Transform: null destination"))?;
    let comp = arg_obj(args, 4);
    let clip_obj = arg_obj(args, 5);
    let itx = arg_obj(args, 6).ok_or_else(|| internal("Transform: null transform"))?;
    let interp = arg_int(args, 7);
    let (sx1, sy1, sx2, sy2) = (
        arg_int(args, 8),
        arg_int(args, 9),
        arg_int(args, 10),
        arg_int(args, 11),
    );
    let (dx1, dy1, dx2, dy2) = (
        arg_int(args, 12),
        arg_int(args, 13),
        arg_int(args, 14),
        arg_int(args, 15),
    );
    let edges_arr = arg_obj(args, 16);
    let (dxoff, dyoff) = (arg_int(args, 17), arg_int(args, 18));
    let src = lookup_surf(ctx, src_sd)?;
    let dst = lookup_surf(ctx, dst_sd)?;
    let (rule, extra_a) = alpha_of(ctx, comp);
    let clip = read_clip(ctx, clip_obj);
    let m = [
        fld_f64(ctx, itx, "m00"),
        fld_f64(ctx, itx, "m01"),
        fld_f64(ctx, itx, "m02"),
        fld_f64(ctx, itx, "m10"),
        fld_f64(ctx, itx, "m11"),
        fld_f64(ctx, itx, "m12"),
    ];
    let _ = fld_obj(ctx, itx, "state");
    if dx2 <= dx1 || dy2 <= dy1 {
        return void();
    }
    let h = (dy2 - dy1) as usize;
    let mut edges = vec![0i32; h * 2 + 2];
    let mut top: Option<i32> = None;
    let mut bottom = 0i32;
    // Source pixels, fully read once (transform sources are small tiles).
    let mut srows: Vec<Vec<u8>> = Vec::with_capacity(src.row.h as usize);
    for y in 0..src.row.h {
        let mut b = Vec::new();
        src.read(ctx, 0, y, src.row.w as usize, &mut b);
        srows.push(b);
    }
    let sw = sx2.min(src.row.w);
    let sh = sy2.min(src.row.h);
    let px_at = |x: i32, y: i32| -> [i32; 4] {
        let x = x.clamp(sx1.max(0), sw - 1) as usize;
        let y = y.clamp(sy1.max(0), sh - 1) as usize;
        load_pre(src.fmt, &srows[y], x)
    };
    let mut line = vec![0u8; ((dx2 - dx1) as usize) * 4];
    let mut dbuf = Vec::new();
    // Device-space clip: the destination rectangle shifted by the offsets.
    let rects = clip_rects(
        &clip,
        (dx1 + dxoff) as i64,
        (dy1 + dyoff) as i64,
        (dx2 + dxoff) as i64,
        (dy2 + dyoff) as i64,
    );
    for (cx1, cy1, cx2, cy2) in rects {
        for yy in cy1..cy2.min(dst.row.h) {
            let mut min_x = i32::MAX;
            let mut max_x = i32::MIN;
            let mut first = true;
            let mut span_start = 0i32;
            let flush = |ctx: &mut dyn NativeContext,
                         start: i32,
                         end: i32,
                         line: &[u8],
                         dbuf: &mut Vec<u8>| {
                let n = (end - start) as usize;
                if n == 0 {
                    return;
                }
                let dxd = start.max(0);
                let skip = (dxd - start) as usize;
                let n2 = n - skip.min(n);
                if n2 == 0 || dxd + n2 as i32 > dst.row.w {
                    return;
                }
                dst.read(ctx, dxd, yy, n2, dbuf);
                rows::alpha_blit_row(
                    Fmt::IntArgbPre,
                    dst.fmt,
                    rule,
                    extra_a,
                    &line[skip * 4..],
                    dbuf,
                    None,
                    n2,
                );
                dst.write(ctx, dxd, yy, dbuf);
            };
            for xx in cx1..cx2 {
                let (fx, fy) = (xx as f64 + 0.5, yy as f64 + 0.5);
                let sx = m[0] * fx + m[1] * fy + m[2];
                let sy = m[3] * fx + m[4] * fy + m[5];
                let inside =
                    sx >= sx1 as f64 && sx < sx2 as f64 && sy >= sy1 as f64 && sy < sy2 as f64;
                if !inside {
                    if !first {
                        flush(
                            ctx,
                            span_start,
                            xx,
                            &line[..((xx - span_start) as usize) * 4],
                            &mut dbuf,
                        );
                        first = true;
                    }
                    continue;
                }
                if first {
                    span_start = xx;
                    first = false;
                }
                let px: [i32; 4] = match interp {
                    2 | 3 => {
                        // HotSpot walks 32.32 fixed-point positions with a
                        // truncated per-pixel increment.
                        let one = (1i64 << 32) as f64;
                        let fx0 = m[0] * (cx1 as f64 + 0.5) + m[1] * (yy as f64 + 0.5) + m[2] - 0.5;
                        let fy0 = m[3] * (cx1 as f64 + 0.5) + m[4] * (yy as f64 + 0.5) + m[5] - 0.5;
                        let k = (xx - cx1) as i64;
                        let px_fix = (fx0 * one) as i64 + k * ((m[0] * one) as i64);
                        let py_fix = (fy0 * one) as i64 + k * ((m[3] * one) as i64);
                        let (x0, y0) = ((px_fix >> 32) as i32, (py_fix >> 32) as i32);
                        let xf = (((px_fix & 0xffff_ffff) >> 24) & 0xff) as usize;
                        let yf = (((py_fix & 0xffff_ffff) >> 24) & 0xff) as usize;
                        if interp == 2 {
                            let (p00, p10, p01, p11) = (
                                px_at(x0, y0),
                                px_at(x0 + 1, y0),
                                px_at(x0, y0 + 1),
                                px_at(x0 + 1, y0 + 1),
                            );
                            let (xf, yf) = (xf as i32, yf as i32);
                            let mut out = [0i32; 4];
                            for c in 0..4 {
                                let top_v = (p00[c] << 8) + (p10[c] - p00[c]) * xf;
                                let bot_v = (p01[c] << 8) + (p11[c] - p01[c]) * xf;
                                out[c] = (((top_v << 8) + (bot_v - top_v) * yf + 0x8000) >> 16)
                                    .clamp(0, 255);
                            }
                            out
                        } else {
                            let wx = cubic_table(xf);
                            let wy = cubic_table(yf);
                            let mut acc = [0i32; 4];
                            for (j, wyj) in wy.iter().enumerate() {
                                let mut row = [0i32; 4];
                                for (i, wxi) in wx.iter().enumerate() {
                                    let p = px_at(x0 - 1 + i as i32, y0 - 1 + j as i32);
                                    for c in 0..4 {
                                        row[c] += p[c] * wxi;
                                    }
                                }
                                for c in 0..4 {
                                    acc[c] += row[c] * wyj;
                                }
                            }
                            let mut out = [0i32; 4];
                            for c in 0..4 {
                                out[c] = ((acc[c] + 0x8000) >> 16).clamp(0, 255);
                            }
                            // Keep premultiplied channels within alpha.
                            for c in 1..4 {
                                out[c] = out[c].min(out[0]);
                            }
                            out
                        }
                    }
                    _ => px_at(sx.floor() as i32, sy.floor() as i32),
                };
                let off = ((xx - span_start) as usize) * 4;
                // IntArgbPre is little-endian: b, g, r, a.
                line[off] = px[3].clamp(0, 255) as u8;
                line[off + 1] = px[2].clamp(0, 255) as u8;
                line[off + 2] = px[1].clamp(0, 255) as u8;
                line[off + 3] = px[0].clamp(0, 255) as u8;
                min_x = min_x.min(xx);
                max_x = max_x.max(xx + 1);
            }
            if !first {
                let end = cx2;
                flush(
                    ctx,
                    span_start,
                    end,
                    &line[..((end - span_start) as usize) * 4],
                    &mut dbuf,
                );
            }
            // Edges are relative to the destination rectangle's origin.
            let ry = yy - dyoff - dy1;
            if min_x != i32::MAX {
                if top.is_none() {
                    top = Some(ry);
                }
                bottom = ry + 1;
                let idx = 2 + 2 * ry as usize;
                if idx + 1 < edges.len() {
                    edges[idx] = min_x - dxoff - dx1;
                    edges[idx + 1] = max_x - dxoff - dx1;
                }
            }
        }
    }
    if let Some(arr) = edges_arr {
        if let Some(t) = top {
            edges[0] = t;
            edges[1] = bottom;
        }
        ctx.write_int_array_from(arr, 0, &edges);
    }
    void()
}
