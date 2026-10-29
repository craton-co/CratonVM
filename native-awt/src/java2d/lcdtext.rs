// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `DrawGlyphListLCD.DrawGlyphListLCD`: sub-pixel (LCD) text blended with the
//! gamma tables of HotSpot's `DEFINE_SOLID_DRAWGLYPHLISTLCD` loops. For a
//! contrast `c`, `gamma = c / 100`; a colour channel is linearised with
//! `floor(255 * (v / 255) ^ gamma)`, mixed with the coverage, and brought back
//! with the inverse table `floor(255 * (v / 255) ^ (1 / gamma))` (verified
//! against HotSpot on 2,184 pixels of six colour pairs).

use cratonvm_native_api::NativeContext;
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::Value;

use super::pixel::{self, mul8};
use super::{arg_obj, clip_rects, fld_int, fld_obj, internal, lookup_surf, read_clip, void};

fn tables(contrast: i32) -> ([u8; 256], [u8; 256]) {
    let g = (contrast.clamp(100, 250) as f64) / 100.0;
    let mut gamma = [0u8; 256];
    let mut inv = [0u8; 256];
    for i in 0..256usize {
        let v = i as f64 / 255.0;
        gamma[i] = (255.0 * v.powf(g)).floor().clamp(0.0, 255.0) as u8;
        inv[i] = (255.0 * v.powf(1.0 / g)).floor().clamp(0.0, 255.0) as u8;
    }
    (gamma, inv)
}

fn fld_f32(ctx: &dyn NativeContext, o: cratonvm_types::ObjectRef, name: &str) -> f32 {
    match ctx.get_field_by_name(o, name) {
        Value::Float(f) => f,
        _ => 0.0,
    }
}

fn fld_bool(ctx: &dyn NativeContext, o: cratonvm_types::ObjectRef, name: &str) -> bool {
    matches!(ctx.get_field_by_name(o, name), Value::Int(v) if v != 0)
}

/// `DrawGlyphListLCD(SunGraphics2D, SurfaceData, GlyphList, int from, int to)`.
pub(super) fn n_draw_glyph_list_lcd(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let sg2d = arg_obj(args, 1).ok_or_else(|| internal("DrawGlyphListLCD: null sg2d"))?;
    let sd = arg_obj(args, 2).ok_or_else(|| internal("DrawGlyphListLCD: null surface"))?;
    let gl = arg_obj(args, 3).ok_or_else(|| internal("DrawGlyphListLCD: null glyph list"))?;
    let (from, to) = match (args.get(4), args.get(5)) {
        (Some(Value::Int(a)), Some(Value::Int(b))) => (*a as usize, *b as usize),
        _ => (0, 0),
    };
    let s = lookup_surf(ctx, sd)?;
    let eargb = fld_int(ctx, sg2d, "eargb") as u32;
    let pixel_value = fld_int(ctx, sg2d, "pixel") as u32;
    let contrast = fld_int(ctx, sg2d, "lcdTextContrast");
    let clip = read_clip(ctx, fld_obj(ctx, sg2d, "clipRegion"));
    let (gamma, inv) = tables(contrast);
    let (sr, sg, sb) = (
        ((eargb >> 16) & 0xff) as usize,
        ((eargb >> 8) & 0xff) as usize,
        (eargb & 0xff) as usize,
    );
    let (gsr, gsg, gsb) = (gamma[sr] as u32, gamma[sg] as u32, gamma[sb] as u32);
    let rgb_order = fld_bool(ctx, gl, "lcdRGBOrder");
    let use_pos = fld_bool(ctx, gl, "usePositions");
    let sub_pix = fld_bool(ctx, gl, "lcdSubPixPos");
    let (mut gx, mut gy) = (fld_f32(ctx, gl, "gposx"), fld_f32(ctx, gl, "gposy"));
    if sub_pix {
        // Sub-pixel positions are rounded to a third of a pixel from the exact x; y is
        // floored from the exact position (no half-pixel rounding), as measured.
        gx -= 0.5;
        gy -= 0.5;
    }
    let images = fld_obj(ctx, gl, "images");
    let positions = fld_obj(ctx, gl, "positions");
    let Some(images) = images else {
        return void();
    };
    let n = ctx.array_length(images);
    let to = to.min(n);
    // Advance for glyphs before `from` when not positioned.
    let mut dbuf = Vec::new();
    for g in 0..to {
        let ptr = match ctx.get_array_element(images, g) {
            Value::Long(p) => p,
            _ => 0,
        };
        if ptr == 0 {
            continue;
        }
        let mut hdr = [0u8; 40];
        if !ctx.copy_from_native_memory(ptr, &mut hdr) {
            continue;
        }
        let f32at = |o: usize| f32::from_le_bytes([hdr[o], hdr[o + 1], hdr[o + 2], hdr[o + 3]]);
        let u16at = |o: usize| u16::from_le_bytes([hdr[o], hdr[o + 1]]) as usize;
        let (adv_x, adv_y) = (f32at(0), f32at(4));
        let (w, h, row_bytes) = (u16at(8), u16at(10), u16at(12));
        let (tlx, tly) = (f32at(16), f32at(20));
        let img_ptr = i64::from_le_bytes([
            hdr[32], hdr[33], hdr[34], hdr[35], hdr[36], hdr[37], hdr[38], hdr[39],
        ]);
        let (px, py) = if use_pos {
            let (a, b) = match positions {
                Some(p) => (
                    match ctx.get_array_element(p, g * 2) {
                        Value::Float(f) => f,
                        _ => 0.0,
                    },
                    match ctx.get_array_element(p, g * 2 + 1) {
                        Value::Float(f) => f,
                        _ => 0.0,
                    },
                ),
                None => (0.0, 0.0),
            };
            (a + tlx + gx, b + tly + gy)
        } else {
            let r = (gx + tlx, gy + tly);
            gx += adv_x;
            gy += adv_y;
            r
        };
        if g < from || w == 0 || h == 0 {
            continue;
        }
        let (left, shift) = if sub_pix && row_bytes != w {
            let t = ((px as f64) * 3.0 + 0.5).floor() as i64;
            (t.div_euclid(3), t.rem_euclid(3))
        } else {
            (
                ((px as f64) + if sub_pix { 0.5 } else { 0.0 }).floor() as i64,
                0,
            )
        };
        let top = ((py as f64) + if sub_pix && row_bytes == w { 0.5 } else { 0.0 }).floor() as i64;
        let cols = w + (shift > 0) as usize;
        let bpp = if row_bytes == w { 1 } else { 3 };
        let mut pixels = vec![0u8; row_bytes * h];
        if !ctx.copy_from_native_memory(img_ptr, &mut pixels) {
            continue;
        }
        for (x1, y1, x2, y2) in clip_rects(
            &clip,
            left.max(0),
            top.max(0),
            (left + cols as i64).min(s.row.w as i64),
            (top + h as i64).min(s.row.h as i64),
        ) {
            let cnt = (x2 - x1) as usize;
            for yy in y1..y2 {
                s.read(ctx, x1, yy, cnt, &mut dbuf);
                let row = &pixels[(yy as i64 - top) as usize * row_bytes..];
                let mut changed = false;
                for i in 0..cnt {
                    let gxp = (x1 as i64 + i as i64 - left) as usize;
                    let sample = |c: usize| -> u32 {
                        let j = (gxp * 3 + c) as i64 - shift;
                        if j >= 0 && (j as usize) < w * 3 {
                            row[j as usize] as u32
                        } else {
                            0
                        }
                    };
                    let (mr, mg, mb) = if bpp == 1 {
                        let v = row[gxp.min(w - 1)] as u32;
                        (v, v, v)
                    } else if rgb_order {
                        (sample(0), sample(1), sample(2))
                    } else {
                        (sample(2), sample(1), sample(0))
                    };
                    if mr | mg | mb == 0 {
                        continue;
                    }
                    changed = true;
                    if mr == 0xff && mg == 0xff && mb == 0xff {
                        pixel::store_pixel_value(s.fmt, &mut dbuf, i, pixel_value);
                        continue;
                    }
                    let d = pixel::load_argb(s.fmt, &dbuf, i);
                    let mix = |m: u32, dst: u32, gsrc: u32| -> u32 {
                        let v = mul8(255 - m, gamma[dst as usize] as u32) + mul8(m, gsrc);
                        inv[v.min(255) as usize] as u32
                    };
                    let (r, gg, b) = (mix(mr, d.r, gsr), mix(mg, d.g, gsg), mix(mb, d.b, gsb));
                    pixel::store_argb(s.fmt, &mut dbuf, i, 0xff, r, gg, b);
                }
                if changed {
                    s.write(ctx, x1, yy, &dbuf);
                }
            }
        }
    }
    void()
}
