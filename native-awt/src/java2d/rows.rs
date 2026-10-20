// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Row kernels of the Java2D native loops: each works on `n` pixels of
//! already-fetched row buffers, so the VM-facing driver only does region
//! iteration and array traffic.

use super::pixel::{
    alpha_blit_pixel, alpha_fill_pixel, load_argb, load_raw, store_argb, store_pixel_value, Chan3,
    Fmt,
};

/// `SetPixel` fill: `n` pixels of `pixel`.
pub fn fill_row(f: Fmt, dst: &mut [u8], n: usize, pixel: u32) {
    match f.bpp() {
        1 => dst[..n].fill(pixel as u8),
        _ => {
            for x in 0..n {
                store_pixel_value(f, dst, x, pixel);
            }
        }
    }
}

/// `AlphaMaskFill` over `n` pixels. `mask` is the coverage row (or `None`).
pub fn mask_fill_row(
    f: Fmt,
    rule: i32,
    src_a: u32,
    fg_pre: Chan3,
    dst: &mut [u8],
    mask: Option<&[u8]>,
    n: usize,
) {
    match mask {
        None => {
            for x in 0..n {
                alpha_fill_pixel(f, rule, src_a, fg_pre, 0xff, dst, x);
            }
        }
        Some(m) => {
            for x in 0..n {
                let path_a = m[x] as u32;
                if path_a == 0 {
                    continue;
                }
                alpha_fill_pixel(f, rule, src_a, fg_pre, path_a, dst, x);
            }
        }
    }
}

/// The `Convert` loops (`SrcNoEa`): copy `n` pixels from `sf` to `df`.
pub fn convert_row(sf: Fmt, df: Fmt, src: &[u8], dst: &mut [u8], n: usize) {
    if sf == df {
        let len = n * sf.bpp();
        dst[..len].copy_from_slice(&src[..len]);
        return;
    }
    for x in 0..n {
        let p = load_argb(sf, src, x);
        store_argb(df, dst, x, p.a, p.r, p.g, p.b);
    }
}

/// `AlphaMaskBlit` over `n` pixels.
pub fn alpha_blit_row(
    sf: Fmt,
    df: Fmt,
    rule: i32,
    extra_a: u32,
    src: &[u8],
    dst: &mut [u8],
    mask: Option<&[u8]>,
    n: usize,
) {
    match mask {
        None => {
            for x in 0..n {
                let s = load_raw(sf, src, x);
                alpha_blit_pixel(sf, df, rule, extra_a, s, 0xff, dst, x);
            }
        }
        Some(m) => {
            for x in 0..n {
                let path_a = m[x] as u32;
                if path_a == 0 {
                    continue;
                }
                let s = load_raw(sf, src, x);
                alpha_blit_pixel(sf, df, rule, extra_a, s, path_a, dst, x);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::pixel::{prepare_fill_color, store_raw, Raw};
    use super::*;

    #[test]
    fn fill_and_convert() {
        let mut a = vec![0u8; 3 * 4];
        fill_row(Fmt::ThreeByteBgr, &mut a, 4, 0x00112233);
        assert_eq!(&a[..3], &[0x33, 0x22, 0x11]);
        let mut b = vec![0u8; 4 * 4];
        convert_row(Fmt::ThreeByteBgr, Fmt::IntArgb, &a, &mut b, 4);
        assert_eq!(&b[..4], &[0x33, 0x22, 0x11, 0xff]);
    }

    #[test]
    fn mask_fill_zero_mask_is_noop() {
        let mut d = vec![0u8; 4];
        store_raw(
            Fmt::IntArgb,
            &mut d,
            0,
            Raw {
                a: 255,
                r: 1,
                g: 2,
                b: 3,
            },
        );
        let (a, fg) = prepare_fill_color(Fmt::IntArgb, 0xffffffff);
        mask_fill_row(Fmt::IntArgb, 3, a, fg, &mut d, Some(&[0]), 1);
        assert_eq!(d, vec![3, 2, 1, 255]);
    }
}
