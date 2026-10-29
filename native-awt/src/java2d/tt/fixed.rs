// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! FreeType's fixed-point helpers (`ftcalc.c`, `fttrigon.c`, `ttinterp.c`), on
//! the 64-bit `long` flavour of the library, so the TrueType interpreter and the
//! rasteriser round exactly as the JDK's bundled FreeType does.

pub const ONE_PIXEL_26_6: i64 = 64;

/// `FT_PIX_ROUND`.
#[inline]
pub fn pix_round(x: i64) -> i64 {
    (x + 32) & !63
}

/// `FT_PIX_FLOOR`.
#[inline]
pub fn pix_floor(x: i64) -> i64 {
    x & !63
}

/// `FT_PIX_CEIL`.
#[inline]
pub fn pix_ceil(x: i64) -> i64 {
    (x + 63) & !63
}

/// `FT_MulFix`: `a * b / 65536`, rounded half away from zero.
#[inline]
pub fn mul_fix(a: i64, b: i64) -> i64 {
    let ab = (a as i128) * (b as i128);
    ((ab + 0x8000 - if ab < 0 { 1 } else { 0 }) >> 16) as i64
}

/// `FT_MulDiv`: `a * b / c` rounded, sign symmetric.
#[inline]
pub fn mul_div(a: i64, b: i64, c: i64) -> i64 {
    let mut s = 1i64;
    let (mut a, mut b, mut c) = (a, b, c);
    if a < 0 {
        a = -a;
        s = -s;
    }
    if b < 0 {
        b = -b;
        s = -s;
    }
    if c < 0 {
        c = -c;
        s = -s;
    }
    let d: i64 = if c > 0 {
        (((a as i128) * (b as i128) + (c >> 1) as i128) / (c as i128)) as i64
    } else {
        0x7FFF_FFFF
    };
    if s < 0 {
        -d
    } else {
        d
    }
}

/// `FT_MulDiv_No_Round`.
#[inline]
pub fn mul_div_no_round(a: i64, b: i64, c: i64) -> i64 {
    let mut s = 1i64;
    let (mut a, mut b, mut c) = (a, b, c);
    if a < 0 {
        a = -a;
        s = -s;
    }
    if b < 0 {
        b = -b;
        s = -s;
    }
    if c < 0 {
        c = -c;
        s = -s;
    }
    let d: i64 = if c > 0 {
        (((a as i128) * (b as i128)) / (c as i128)) as i64
    } else {
        0x7FFF_FFFF
    };
    if s < 0 {
        -d
    } else {
        d
    }
}

/// `FT_DivFix`: `(a << 16) / b`, rounded.
#[inline]
pub fn div_fix(a: i64, b: i64) -> i64 {
    let mut s = 1i64;
    let (mut a, mut b) = (a, b);
    if a < 0 {
        a = -a;
        s = -s;
    }
    if b < 0 {
        b = -b;
        s = -s;
    }
    let q: i64 = if b > 0 {
        ((((a as i128) << 16) + (b >> 1) as i128) / (b as i128)) as i64
    } else {
        0x7FFF_FFFF
    };
    if s < 0 {
        -q
    } else {
        q
    }
}

/// `TT_MulFix14`: `a * b / 16384`, rounded half away from zero.
#[inline]
pub fn mul_fix14(a: i64, b: i64) -> i64 {
    let ab = (a as i128) * (b as i128);
    let neg = ab < 0;
    let m = ab.abs();
    let r = ((m + 0x2000) >> 14) as i64;
    if neg {
        -r
    } else {
        r
    }
}

/// `TT_DotFix14`.
#[inline]
pub fn dot_fix14(ax: i64, ay: i64, bx: i64, by: i64) -> i64 {
    let t = (ax as i128) * (bx as i128) + (ay as i128) * (by as i128);
    let t2 = t >> 127;
    ((t + 0x2000 + t2) >> 14) as i64
}

fn msb(x: u32) -> i32 {
    31 - x.leading_zeros() as i32
}

/// `FT_Vector_NormLen`: normalises `(x, y)` to a 16.16 unit vector in place.
pub fn norm_len(x_: i64, y_: i64) -> (i64, i64) {
    let (x, y, _) = norm_len_l(x_, y_);
    (x, y)
}

/// `FT_Vector_NormLen` with its return value: the vector's length (the input's units).
pub fn norm_len_l(x_: i64, y_: i64) -> (i64, i64, u32) {
    let (mut sx, mut sy) = (1i64, 1i64);
    let mut x = x_ as i32;
    let mut y = y_ as i32;
    if x < 0 {
        x = x.wrapping_neg();
        sx = -1;
    }
    if y < 0 {
        y = y.wrapping_neg();
        sy = -1;
    }
    let (mut x, mut y) = (x as u32, y as u32);
    if x == 0 {
        let vy = if y > 0 { sy * 0x10000 } else { y_ };
        return (x_, vy, y as u32);
    } else if y == 0 {
        let vx = if x > 0 { sx * 0x10000 } else { x_ };
        return (vx, y_, x as u32);
    }
    let mut l: u32 = if x > y { x + (y >> 1) } else { y + (x >> 1) };
    let mut shift: i32 = 31 - msb(l);
    shift -= 15 + if l >= (0xAAAA_AAAAu32 >> shift) { 1 } else { 0 };
    if shift > 0 {
        x <<= shift;
        y <<= shift;
        l = if x > y { x + (y >> 1) } else { y + (x >> 1) };
    } else {
        x >>= -shift;
        y >>= -shift;
        l >>= -shift;
    }
    let mut b: i32 = 0x10000 - l as i32;
    let xi = x as i32;
    let yi = y as i32;
    let (mut u, mut v);
    loop {
        u = (xi.wrapping_add(((xi as i64 * b as i64) >> 16) as i32)) as u32;
        v = (yi.wrapping_add(((yi as i64 * b as i64) >> 16) as i32)) as u32;
        let sq = u.wrapping_mul(u).wrapping_add(v.wrapping_mul(v));
        let mut z: i32 = (sq as i32).wrapping_neg() / 0x200;
        z = z.wrapping_mul((0x10000 + b) >> 8) / 0x10000;
        b += z;
        if z <= 0 {
            break;
        }
    }
    let vx = if sx < 0 { -(u as i64) } else { u as i64 };
    let vy = if sy < 0 { -(v as i64) } else { v as i64 };
    // The length: the dot product of the (scaled) input with the unit vector, undone
    // by the scaling shift.
    let xi = x as i32;
    let yi = y as i32;
    let t = xi
        .wrapping_mul(u as i32)
        .wrapping_add(yi.wrapping_mul(v as i32));
    let q = if t >= 0 {
        t >> 16
    } else {
        t.wrapping_add(0xffff) >> 16
    };
    let la = q.wrapping_add(0x10000) as u32;
    let len = if shift > 0 {
        la.wrapping_add(1u32 << (shift - 1)) >> shift
    } else {
        la << (-shift)
    };
    (vx, vy, len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounding_is_sign_symmetric() {
        assert_eq!(mul_fix(3, 0x8000), 2);
        assert_eq!(mul_fix(-3, 0x8000), -2);
        assert_eq!(mul_div(7, 3, 2), 11);
        assert_eq!(mul_div(-7, 3, 2), -11);
        assert_eq!(mul_fix14(0x4000, 0x4000), 0x4000);
        assert_eq!(div_fix(1, 2), 0x8000);
    }

    #[test]
    fn norm_len_axis_and_diagonal() {
        assert_eq!(norm_len(5, 0), (0x10000, 0));
        assert_eq!(norm_len(0, -7), (0, -0x10000));
        let (x, y) = norm_len(100, 100);
        assert!((x - 46341).abs() <= 2 && (y - 46341).abs() <= 2, "{x} {y}");
    }
}
