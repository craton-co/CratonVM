// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Pixel formats and the Porter-Duff arithmetic of the Java2D native loops.
//!
//! The JDK's `libawt` loops (`AlphaMacros.h`, `AlphaMath.c`, the per-format
//! `*.h` load/store macros) are reproduced here for the formats
//! `BufferedImage` creates, so the genuine `sun.java2d` Java classes can run
//! on CratonVM and produce the same bytes HotSpot does. Everything in this file
//! is pure (no VM access) and unit-tested against values measured on HotSpot 25.

use std::sync::OnceLock;

/// A pixel layout a Rust loop reads and writes. These are the `SurfaceType`s
/// `BufImgSurfaceData.createData` picks for the standard `BufferedImage` types.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Fmt {
    IntArgb,
    IntArgbPre,
    IntRgb,
    IntBgr,
    ThreeByteBgr,
    FourByteAbgr,
    FourByteAbgrPre,
    ByteGray,
    Ushort565Rgb,
    Ushort555Rgb,
}

/// Every supported format, in the order `Fmt as usize` indexes.
pub const ALL_FMTS: [Fmt; 10] = [
    Fmt::IntArgb,
    Fmt::IntArgbPre,
    Fmt::IntRgb,
    Fmt::IntBgr,
    Fmt::ThreeByteBgr,
    Fmt::FourByteAbgr,
    Fmt::FourByteAbgrPre,
    Fmt::ByteGray,
    Fmt::Ushort565Rgb,
    Fmt::Ushort555Rgb,
];

impl Fmt {
    /// The `SurfaceType.desc` string the JDK gives this layout.
    pub fn from_desc(desc: &str) -> Option<Fmt> {
        Some(match desc {
            "Integer ARGB" => Fmt::IntArgb,
            "Integer ARGB Premultiplied" => Fmt::IntArgbPre,
            "Integer RGB" => Fmt::IntRgb,
            "Integer BGR" => Fmt::IntBgr,
            "3 Byte BGR" => Fmt::ThreeByteBgr,
            "4 Byte ABGR" => Fmt::FourByteAbgr,
            "4 Byte ABGR Premultiplied" => Fmt::FourByteAbgrPre,
            "8-bit Gray" => Fmt::ByteGray,
            "Short 565 RGB" => Fmt::Ushort565Rgb,
            "Short 555 RGB" => Fmt::Ushort555Rgb,
            _ => return None,
        })
    }

    /// The name of the `SurfaceType` static field.
    pub fn surface_type_field(self) -> &'static str {
        match self {
            Fmt::IntArgb => "IntArgb",
            Fmt::IntArgbPre => "IntArgbPre",
            Fmt::IntRgb => "IntRgb",
            Fmt::IntBgr => "IntBgr",
            Fmt::ThreeByteBgr => "ThreeByteBgr",
            Fmt::FourByteAbgr => "FourByteAbgr",
            Fmt::FourByteAbgrPre => "FourByteAbgrPre",
            Fmt::ByteGray => "ByteGray",
            Fmt::Ushort565Rgb => "Ushort565Rgb",
            Fmt::Ushort555Rgb => "Ushort555Rgb",
        }
    }

    /// Bytes per pixel.
    pub fn bpp(self) -> usize {
        match self {
            Fmt::IntArgb | Fmt::IntArgbPre | Fmt::IntRgb | Fmt::IntBgr => 4,
            Fmt::FourByteAbgr | Fmt::FourByteAbgrPre => 4,
            Fmt::ThreeByteBgr => 3,
            Fmt::ByteGray => 1,
            Fmt::Ushort565Rgb | Fmt::Ushort555Rgb => 2,
        }
    }

    /// Whether the layout stores an alpha channel.
    pub fn has_alpha(self) -> bool {
        matches!(
            self,
            Fmt::IntArgb | Fmt::IntArgbPre | Fmt::FourByteAbgr | Fmt::FourByteAbgrPre
        )
    }

    /// Whether stored colour channels are premultiplied by alpha.
    pub fn premult(self) -> bool {
        matches!(self, Fmt::IntArgbPre | Fmt::FourByteAbgrPre)
    }

    /// Single channel (gray) layout.
    pub fn is_gray(self) -> bool {
        self == Fmt::ByteGray
    }
}

struct Tables {
    mul: Vec<u8>,
    div: Vec<u8>,
}

fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| {
        // AlphaMath.c `initAlphaTables`.
        let mut mul = vec![0u8; 256 * 256];
        let mut div = vec![0u8; 256 * 256];
        for i in 1u32..256 {
            let inc = (i << 16) + (i << 8) + i;
            let mut val = inc.wrapping_add(1 << 23);
            for j in 1u32..256 {
                mul[(i * 256 + j) as usize] = (val >> 24) as u8;
                val = val.wrapping_add(inc);
            }
        }
        for i in 1u32..256 {
            // Measured against HotSpot (every (a, v) pair): the increment is the
            // quotient ROUNDED to nearest, not truncated.
            let inc = ((0xffu32 << 24) + i / 2) / i;
            let mut val = 1u32 << 23;
            let mut j = 0u32;
            while j < i {
                div[(i * 256 + j) as usize] = (val >> 24) as u8;
                val = val.wrapping_add(inc);
                j += 1;
            }
            while j < 256 {
                div[(i * 256 + j) as usize] = 255;
                j += 1;
            }
        }
        Tables { mul, div }
    })
}

/// `MUL8(a, b)`: `a * b / 255` the way `mul8table` rounds it.
#[inline]
pub fn mul8(a: u32, b: u32) -> u32 {
    tables().mul[((a & 0xff) * 256 + (b & 0xff)) as usize] as u32
}

/// `DIV8(v, a)`: `v * 255 / a` the way `div8table[a][v]` rounds it.
#[inline]
pub fn div8(v: u32, a: u32) -> u32 {
    tables().div[((a & 0xff) * 256 + (v & 0xff)) as usize] as u32
}

/// `ComposeByteGrayFrom3ByteRgb`.
#[inline]
pub fn gray_of(r: u32, g: u32, b: u32) -> u32 {
    (77 * r + 150 * g + 29 * b + 128) / 256
}

/// One pixel as raw channels: alpha and three colour channels exactly as the
/// layout stores them (premultiplied for premultiplied layouts, the gray value
/// replicated in all three for gray).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Raw {
    pub a: u32,
    pub r: u32,
    pub g: u32,
    pub b: u32,
}

#[inline]
fn ext5(v: u32) -> u32 {
    (v << 3) | (v >> 2)
}

/// Read the pixel at index `x` of a row.
#[inline]
pub fn load_raw(f: Fmt, row: &[u8], x: usize) -> Raw {
    match f {
        Fmt::IntArgb | Fmt::IntArgbPre => {
            let o = x * 4;
            let p = u32::from_le_bytes([row[o], row[o + 1], row[o + 2], row[o + 3]]);
            Raw {
                a: p >> 24,
                r: (p >> 16) & 0xff,
                g: (p >> 8) & 0xff,
                b: p & 0xff,
            }
        }
        Fmt::IntRgb => {
            let o = x * 4;
            let p = u32::from_le_bytes([row[o], row[o + 1], row[o + 2], row[o + 3]]);
            Raw {
                a: 0xff,
                r: (p >> 16) & 0xff,
                g: (p >> 8) & 0xff,
                b: p & 0xff,
            }
        }
        Fmt::IntBgr => {
            let o = x * 4;
            let p = u32::from_le_bytes([row[o], row[o + 1], row[o + 2], row[o + 3]]);
            Raw {
                a: 0xff,
                r: p & 0xff,
                g: (p >> 8) & 0xff,
                b: (p >> 16) & 0xff,
            }
        }
        Fmt::ThreeByteBgr => {
            let o = x * 3;
            Raw {
                a: 0xff,
                b: row[o] as u32,
                g: row[o + 1] as u32,
                r: row[o + 2] as u32,
            }
        }
        Fmt::FourByteAbgr | Fmt::FourByteAbgrPre => {
            let o = x * 4;
            Raw {
                a: row[o] as u32,
                b: row[o + 1] as u32,
                g: row[o + 2] as u32,
                r: row[o + 3] as u32,
            }
        }
        Fmt::ByteGray => {
            let g = row[x] as u32;
            Raw {
                a: 0xff,
                r: g,
                g,
                b: g,
            }
        }
        Fmt::Ushort565Rgb => {
            let o = x * 2;
            let p = u16::from_le_bytes([row[o], row[o + 1]]) as u32;
            let r5 = (p >> 11) & 0x1f;
            let g6 = (p >> 5) & 0x3f;
            let b5 = p & 0x1f;
            Raw {
                a: 0xff,
                r: ext5(r5),
                g: (g6 << 2) | (g6 >> 4),
                b: ext5(b5),
            }
        }
        Fmt::Ushort555Rgb => {
            let o = x * 2;
            let p = u16::from_le_bytes([row[o], row[o + 1]]) as u32;
            Raw {
                a: 0xff,
                r: ext5((p >> 10) & 0x1f),
                g: ext5((p >> 5) & 0x1f),
                b: ext5(p & 0x1f),
            }
        }
    }
}

/// `Load<fmt>To4ByteArgb`: the pixel as non-premultiplied 8-bit ARGB.
#[inline]
pub fn load_argb(f: Fmt, row: &[u8], x: usize) -> Raw {
    let mut p = load_raw(f, row, x);
    if f.premult() && p.a != 0xff {
        p.r = div8(p.r, p.a);
        p.g = div8(p.g, p.a);
        p.b = div8(p.b, p.a);
    }
    p
}

/// Write raw channels (already in the layout's own representation).
#[inline]
pub fn store_raw(f: Fmt, row: &mut [u8], x: usize, p: Raw) {
    match f {
        Fmt::IntArgb | Fmt::IntArgbPre => {
            let v = (p.a << 24) | (p.r << 16) | (p.g << 8) | p.b;
            row[x * 4..x * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        Fmt::IntRgb => {
            let v = (p.r << 16) | (p.g << 8) | p.b;
            row[x * 4..x * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        Fmt::IntBgr => {
            let v = (p.b << 16) | (p.g << 8) | p.r;
            row[x * 4..x * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        Fmt::ThreeByteBgr => {
            let o = x * 3;
            row[o] = p.b as u8;
            row[o + 1] = p.g as u8;
            row[o + 2] = p.r as u8;
        }
        Fmt::FourByteAbgr | Fmt::FourByteAbgrPre => {
            let o = x * 4;
            row[o] = p.a as u8;
            row[o + 1] = p.b as u8;
            row[o + 2] = p.g as u8;
            row[o + 3] = p.r as u8;
        }
        Fmt::ByteGray => {
            row[x] = p.g as u8;
        }
        Fmt::Ushort565Rgb => {
            let v = (((p.r >> 3) << 11) | ((p.g >> 2) << 5) | (p.b >> 3)) as u16;
            row[x * 2..x * 2 + 2].copy_from_slice(&v.to_le_bytes());
        }
        Fmt::Ushort555Rgb => {
            let v = (((p.r >> 3) << 10) | ((p.g >> 3) << 5) | (p.b >> 3)) as u16;
            row[x * 2..x * 2 + 2].copy_from_slice(&v.to_le_bytes());
        }
    }
}

/// `Store<fmt>From4ByteArgb`: store a non-premultiplied ARGB pixel.
#[inline]
pub fn store_argb(f: Fmt, row: &mut [u8], x: usize, a: u32, r: u32, g: u32, b: u32) {
    let mut p = Raw { a, r, g, b };
    if f.premult() && a != 0xff {
        p.r = mul8(a, r);
        p.g = mul8(a, g);
        p.b = mul8(a, b);
    }
    if f.is_gray() {
        p.g = gray_of(r, g, b);
    }
    store_raw(f, row, x, p);
}

/// Store the low `bpp` bytes of `pixel` little-endian (the loops' `SetPixel`).
#[inline]
pub fn store_pixel_value(f: Fmt, row: &mut [u8], x: usize, pixel: u32) {
    let n = f.bpp();
    let b = pixel.to_le_bytes();
    row[x * n..x * n + n].copy_from_slice(&b[..n]);
}

/// The source/destination factor operands of the twelve `AlphaComposite` rules:
/// `(srcF, dstF)` for the given destination alpha and source alpha.
#[inline]
pub fn rule_factors(rule: i32, dst_a: u32, src_a: u32) -> (u32, u32) {
    match rule {
        1 => (0, 0),                        // CLEAR
        2 => (0xff, 0),                     // SRC
        3 => (0xff, 0xff - src_a),          // SRC_OVER
        4 => (0xff - dst_a, 0xff),          // DST_OVER
        5 => (dst_a, 0),                    // SRC_IN
        6 => (0, src_a),                    // DST_IN
        7 => (0xff - dst_a, 0),             // SRC_OUT
        8 => (0, 0xff - src_a),             // DST_OUT
        9 => (0, 0xff),                     // DST
        10 => (dst_a, 0xff - src_a),        // SRC_ATOP
        11 => (0xff - dst_a, src_a),        // DST_ATOP
        12 => (0xff - dst_a, 0xff - src_a), // XOR
        _ => (0xff, 0xff - src_a),
    }
}

/// Whether the rule reads the destination alpha when computing `srcF`/`dstF`
/// (only used for the opaque-destination shortcut).
#[inline]
pub fn rule_uses_dst_alpha(rule: i32) -> bool {
    matches!(rule, 4 | 5 | 7 | 10 | 11 | 12)
}

/// The colour channels of a blend result, one slot per channel actually used.
#[derive(Clone, Copy)]
struct Chan {
    r: u32,
    g: u32,
    b: u32,
}

/// One `AlphaMaskBlit` pixel: composite `src` (loaded raw, in `sf`) over the
/// destination pixel at `x` of `dst_row` (layout `df`) with `rule`, the 8-bit
/// extra alpha `extra_a` and the coverage `path_a` (0xff when there is no mask).
#[inline]
pub fn alpha_blit_pixel(
    sf: Fmt,
    df: Fmt,
    rule: i32,
    extra_a: u32,
    src: Raw,
    path_a: u32,
    dst_row: &mut [u8],
    x: usize,
) {
    // HotSpot's FourByteAbgrPre sources load through the un-premultiplying
    // `LoadFourByteAbgrPreTo4ByteArgb`, so they run the non-premultiplied
    // formula (only IntArgbPre has the hand-written premultiplied loop).
    let (sf, src) = if sf == Fmt::FourByteAbgrPre && df != Fmt::IntArgbPre {
        let u = |c: u32| {
            if src.a == 0 || src.a == 0xff {
                c
            } else {
                div8(c, src.a)
            }
        };
        (
            Fmt::FourByteAbgr,
            Raw {
                a: src.a,
                r: u(src.r),
                g: u(src.g),
                b: u(src.b),
            },
        )
    } else {
        (sf, src)
    };
    let src_a = mul8(extra_a, src.a);
    let d = load_raw(df, dst_row, x);
    let dst_a = d.a;
    let (mut src_f, mut dst_f) = rule_factors(rule, dst_a, src_a);
    if path_a != 0xff {
        src_f = mul8(path_a, src_f);
        dst_f = 0xff - path_a + mul8(path_a, dst_f);
    }
    let gray = df.is_gray();
    let mut res_a;
    let mut res = Chan { r: 0, g: 0, b: 0 };
    if src_f != 0 {
        res_a = mul8(src_f, src_a);
        let color_f = if sf.premult() {
            mul8(src_f, extra_a)
        } else {
            res_a
        };
        if color_f != 0 {
            let mut c = Chan {
                r: src.r,
                g: src.g,
                b: src.b,
            };
            if gray {
                let y = gray_of(src.r, src.g, src.b);
                c = Chan { r: y, g: y, b: y };
            }
            if color_f != 0xff {
                c.r = mul8(color_f, c.r);
                c.g = mul8(color_f, c.g);
                c.b = mul8(color_f, c.b);
            }
            res = c;
        } else if dst_f == 0xff {
            return;
        }
    } else {
        if dst_f == 0xff {
            return;
        }
        res_a = 0;
    }
    let mut dst_f_col = dst_f;
    if dst_f != 0 {
        let da = mul8(dst_f, dst_a);
        res_a += da;
        if !df.premult() {
            dst_f_col = da;
        }
        if dst_f_col != 0 {
            let mut t = Chan {
                r: d.r,
                g: d.g,
                b: d.b,
            };
            if dst_f_col != 0xff {
                t.r = mul8(dst_f_col, t.r);
                t.g = mul8(dst_f_col, t.g);
                t.b = mul8(dst_f_col, t.b);
            }
            res.r += t.r;
            res.g += t.g;
            res.b += t.b;
        }
    }
    if !df.premult() && res_a != 0 && res_a < 0xff {
        res.r = div8(res.r, res_a);
        res.g = div8(res.g, res_a);
        res.b = div8(res.b, res_a);
    }
    if gray {
        res.r = res.g;
        res.b = res.g;
    }
    store_raw(
        df,
        dst_row,
        x,
        Raw {
            a: res_a.min(0xff),
            r: res.r.min(0xff),
            g: res.g.min(0xff),
            b: res.b.min(0xff),
        },
    );
}

/// One `AlphaMaskFill` pixel: the solid colour `fg` (non-premultiplied
/// `eargb`) composited into the destination pixel. `fg_pre` is `fg` already
/// premultiplied (and gray-converted for gray destinations) by
/// [`prepare_fill_color`].
#[inline]
pub fn alpha_fill_pixel(
    df: Fmt,
    rule: i32,
    src_a: u32,
    fg_pre: Chan3,
    path_a: u32,
    dst_row: &mut [u8],
    x: usize,
) {
    let d = load_raw(df, dst_row, x);
    let dst_a = d.a;
    let (mut src_f, mut dst_f) = rule_factors(rule, dst_a, src_a);
    if path_a != 0xff {
        src_f = mul8(path_a, src_f);
        dst_f = 0xff - path_a + mul8(path_a, dst_f);
    }
    let gray = df.is_gray();
    let mut res_a;
    let mut res = Chan { r: 0, g: 0, b: 0 };
    if src_f != 0 {
        if src_f == 0xff {
            res_a = src_a;
            res = Chan {
                r: fg_pre.r,
                g: fg_pre.g,
                b: fg_pre.b,
            };
        } else {
            res_a = mul8(src_f, src_a);
            res = Chan {
                r: mul8(src_f, fg_pre.r),
                g: mul8(src_f, fg_pre.g),
                b: mul8(src_f, fg_pre.b),
            };
        }
    } else {
        if dst_f == 0xff {
            return;
        }
        res_a = 0;
    }
    if dst_f != 0 {
        let da = mul8(dst_f, dst_a);
        res_a += da;
        let df_col = if df.premult() { dst_f } else { da };
        if df_col != 0 {
            let mut t = Chan {
                r: d.r,
                g: d.g,
                b: d.b,
            };
            if df_col != 0xff {
                t.r = mul8(df_col, t.r);
                t.g = mul8(df_col, t.g);
                t.b = mul8(df_col, t.b);
            }
            res.r += t.r;
            res.g += t.g;
            res.b += t.b;
        }
    }
    if !df.premult() && res_a != 0 && res_a < 0xff {
        res.r = div8(res.r, res_a);
        res.g = div8(res.g, res_a);
        res.b = div8(res.b, res_a);
    }
    if gray {
        res.r = res.g;
        res.b = res.g;
    }
    store_raw(
        df,
        dst_row,
        x,
        Raw {
            a: res_a.min(0xff),
            r: res.r.min(0xff),
            g: res.g.min(0xff),
            b: res.b.min(0xff),
        },
    );
}

/// Three colour channels (public mirror of the private blend accumulator).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chan3 {
    pub r: u32,
    pub g: u32,
    pub b: u32,
}

/// Premultiply a fill colour the way `AlphaMaskFill` does before its loop, and
/// convert it to gray for gray destinations.
pub fn prepare_fill_color(df: Fmt, eargb: u32) -> (u32, Chan3) {
    let a = eargb >> 24;
    let mut r = (eargb >> 16) & 0xff;
    let mut g = (eargb >> 8) & 0xff;
    let mut b = eargb & 0xff;
    if df.is_gray() {
        let y = gray_of(r, g, b);
        r = y;
        g = y;
        b = y;
    }
    if a != 0xff {
        r = mul8(a, r);
        g = mul8(a, g);
        b = mul8(a, b);
    }
    (a, Chan3 { r, g, b })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mul8_div8_identities() {
        for v in 0..256 {
            assert_eq!(mul8(255, v), v);
            assert_eq!(mul8(v, 255), v);
            assert_eq!(mul8(0, v), 0);
        }
        // 128/255 of 255 is 128.
        assert_eq!(mul8(128, 255), 128);
        // div8 undoes mul8 at full coverage.
        for v in 0..256 {
            assert_eq!(div8(v, 255), v);
        }
        assert_eq!(div8(10, 0), 0);
    }

    #[test]
    fn div8_ties_match_hotspot() {
        // Measured on HotSpot 25: exact halves round either way depending on
        // how the table's increment rounds.
        assert_eq!(div8(7, 14), 128);
        assert_eq!(div8(11, 22), 127);
        assert_eq!(div8(9, 18), 128);
        assert_eq!(div8(3, 18), 43);
        assert_eq!(div8(13, 26), 127);
    }

    #[test]
    fn srcover_argb_over_argb() {
        // 50% red over opaque blue.
        let mut row = vec![0u8; 4];
        store_raw(
            Fmt::IntArgb,
            &mut row,
            0,
            Raw {
                a: 255,
                r: 0,
                g: 0,
                b: 255,
            },
        );
        let (sa, fg) = prepare_fill_color(Fmt::IntArgb, 0x80ff0000);
        alpha_fill_pixel(Fmt::IntArgb, 3, sa, fg, 0xff, &mut row, 0);
        let p = load_raw(Fmt::IntArgb, &row, 0);
        assert_eq!(p.a, 255);
        assert_eq!(p.r, mul8(0x80, 0xff));
        assert_eq!(p.b, mul8(0xff - 0x80, 0xff));
    }

    #[test]
    fn roundtrip_formats() {
        for f in ALL_FMTS {
            let mut row = vec![0u8; 16];
            store_argb(f, &mut row, 1, 0xff, 0x12, 0x34, 0x56);
            let p = load_argb(f, &row, 1);
            assert_eq!(p.a, 0xff);
            if f == Fmt::ByteGray {
                assert_eq!(p.g, gray_of(0x12, 0x34, 0x56));
            } else if matches!(f, Fmt::Ushort565Rgb | Fmt::Ushort555Rgb) {
                assert!((p.r as i32 - 0x12).abs() < 8);
            } else {
                assert_eq!((p.r, p.g, p.b), (0x12, 0x34, 0x56), "{f:?}");
            }
        }
    }
}
