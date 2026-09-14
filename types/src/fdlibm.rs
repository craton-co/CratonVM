// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! fdlibm ports for the `java.lang.StrictMath` functions whose spec is a
//! bit-for-bit contract, not an accuracy bound.
//!
//! `StrictMath` is required to produce *identical* results on every platform
//! and every VM, and the specification names the algorithm: the FDLIBM 5.3
//! routines, which the reference JDK now carries as Java in
//! `java.lang.FdLibm`. Platform libm is only required to be *close* — glibc,
//! msvcrt, macOS libm and the HotSpot x86 intrinsics all differ from fdlibm in
//! the last ULP on a large fraction of inputs. Delegating `StrictMath` to
//! `f64::ln` and friends therefore violates the contract, and the deviation is
//! observable: see W7-44-numberformat-enum-and-double-tostring.md, where
//! `new Random(42).nextGaussian()` printed `1.141905315473055` against
//! HotSpot's `1.1419053154730547` — one ULP apart, entirely because
//! `java.util.Random`'s polar method multiplies by
//! `StrictMath.sqrt(-2 * StrictMath.log(s) / s)` and our `log` was libm's.
//!
//! Everything else in that expression (`*`, `/`, `sqrt`) is exactly-rounded
//! IEEE 754 and agrees already; `log` was the whole gap. Measured on
//! Temurin 25.0.3+9 / Windows x86-64, `Math.log` and `StrictMath.log` return
//! different bit patterns for **73015 of 1000000** uniform draws in `(0, 1)`
//! — 7.3%, not a corner case.
//!
//! `log` was measured first because `nextGaussian` made it visible. It is not
//! special. Replaying an oracle dumped out of HotSpot (100k samples per
//! function, `probes/StrictMathOracleDumpProbe.java`) against the very libm
//! Rust's `f64` methods reach — MSVC's CRT on Windows x86-64 — the whole
//! surface deviates, and by more than `log` does:
//!
//! ```text
//!   IEEEremainder 49.83%   cbrt 30.98%   cosh 28.55%   sinh 28.08%
//!   pow  9.73%   exp  9.62%   log1p 7.58%   log(0,1) 7.37%   expm1 7.12%
//!   tan  3.95%   asin 2.55%   cos  2.43%   sin   2.37%   tanh     2.32%
//!   acos 0.89%   atan2 0.36%  hypot 0.32%  log10 0.26%   log(wide) 0.12%
//!   atan 0.01%   sqrt 0.00%
//! ```
//!
//! So every function in this module is ported from `java.lang.FdLibm` as
//! shipped in JDK 25 — the normative text, since JDK 21 moved these routines
//! out of native code and into Java. `sqrt` is deliberately absent: IEEE 754
//! requires it to be exactly rounded, so `FdLibm.Sqrt.compute` and the hardware
//! instruction are the same function and the measured deviation is a structural
//! zero, not an empirical one.
//!
//! # Rules this module follows
//!
//! **Bracketing is preserved exactly.** Floating-point addition is not
//! associative; re-associating any polynomial or reconstruction expression
//! changes the last bit and defeats the entire point. Where the Java reads
//! `k*ln2_hi - ((hfsq - (s*(hfsq+r) + k*ln2_lo)) - f)`, so does the Rust.
//!
//! **Constants are written by bit pattern.** The source writes them as hex
//! float literals (`0x1.62e42feep-1`) and Rust has no such literal. A decimal
//! transcription is one more place for a last-ULP mistake to enter, so every
//! constant here was produced by running `Double.doubleToRawLongBits` over the
//! literals lifted straight out of `FdLibm.java`. `f64::from_bits` is not
//! `const` at this crate's MSRV, hence `let` rather than `const`.
//!
//! **Java integer semantics are reproduced, not approximated.** Java's `int`
//! arithmetic wraps and its shifts mask the count to 5 bits; Rust's panic. Every
//! site that can reach those cases uses `wrapping_*`. Java hex literals above
//! `0x7fffffff` are *negative* `int`s and the comparisons against them are
//! signed — `hx <= 0xbfd2_bec3` in `Log1p` is a signed comparison against
//! `-1076626237`, and porting it as unsigned silently takes the wrong branch.
//!
//! **Cross-function calls go to this module, not to libm.** `Log10` calls
//! `log`, `Atan2` calls `atan`, and `Sinh`/`Cosh`/`Tanh` call `expm1`/`exp`,
//! exactly as the Java calls `StrictMath.log`/`atan`/`expm1`/`exp`. Routing any
//! of those to `f64::ln` would reintroduce the whole defect one level down.
//!
//! # Scope
//!
//! These are `StrictMath` bodies only. `java.lang.Math` carries a *1-ULP*
//! accuracy bound plus semi-monotonicity, which platform libm meets, and is
//! correctly served by `f64::ln` and friends — `Math` is deliberately left
//! alone.

// `x - x` and `(x - x) / (x - x)` are fdlibm's idioms for "produce a NaN that
// carries this argument's payload" and "produce the default NaN"; `hx - hp` on
// two words derived from the same value is how `IEEEremainder` asks whether the
// operands are equal. Every one of these is deliberate in the source being
// ported, so `eq_op` — which is right about ordinary code — is wrong about all
// 22 sites here. Allowed once, at the module, rather than sprinkled: a
// per-site allow would have to be re-justified on every future port and would
// eventually be pasted onto a site where the lint IS right.
#![allow(clippy::eq_op)]

// ---------------------------------------------------------------------------
// Bit-manipulation helpers — `FdLibm`'s `__HI` / `__LO` / `__HI_LO`.
// ---------------------------------------------------------------------------

/// `__HI(x)`: the high 32 bits as a **signed** int. fdlibm tests `hx < 0` to
/// read the sign bit, so the signedness is load-bearing, not incidental.
#[inline]
fn hi(x: f64) -> i32 {
    (x.to_bits() >> 32) as u32 as i32
}

/// `__LO(x)`: the low 32 bits. Declared `/*unsigned*/ int` in the Java, i.e.
/// held in a signed `int` but compared with `Integer.compareUnsigned`; the
/// call sites below cast to `u32` where the Java does.
#[inline]
fn lo(x: f64) -> i32 {
    x.to_bits() as u32 as i32
}

/// `__HI(x, high)`: replace the high word, keep the low word of the current `x`.
#[inline]
fn with_hi(x: f64, high: i32) -> f64 {
    f64::from_bits((x.to_bits() & 0x0000_0000_ffff_ffff) | ((high as u32 as u64) << 32))
}

/// `__LO(x, low)`: replace the low word, keep the high word of the current `x`.
#[inline]
fn with_lo(x: f64, low: i32) -> f64 {
    f64::from_bits((x.to_bits() & 0xffff_ffff_0000_0000) | (low as u32 as u64))
}

/// `__HI_LO(high, low)`: assemble a double from two words.
#[inline]
fn hi_lo(high: i32, low: i32) -> f64 {
    f64::from_bits(((high as u32 as u64) << 32) | (low as u32 as u64))
}

/// `Math.powerOfTwoD(n)` — exactly `2^n` for `-1022 <= n <= 1023`. Used by
/// `Hypot` and `scalb`; a package-private JDK helper, not `Math.pow`.
#[inline]
fn power_of_two_d(n: i32) -> f64 {
    f64::from_bits((((n as i64 + 1023) as u64) << 52) & 0x7ff0_0000_0000_0000)
}

/// `Math.scalb(d, scaleFactor)` — `d * 2^scaleFactor`, computed so that no
/// intermediate over- or underflows when the final result is representable.
///
/// Ported rather than approximated by `d * power_of_two_d(n)`: `Pow` and
/// `KernelRemPio2` both call it with factors that would overflow a single
/// multiply, and the JDK's staged-by-512 form is what fixes the rounding of the
/// subnormal results.
fn scalb(mut d: f64, mut scale_factor: i32) -> f64 {
    // MAX_EXPONENT + -MIN_EXPONENT + SIGNIFICAND_WIDTH + 1 = 1023+1022+53+1.
    const MAX_SCALE: i32 = 2099;
    let scale_increment;
    let exp_delta;
    if scale_factor < 0 {
        scale_factor = scale_factor.max(-MAX_SCALE);
        scale_increment = -512;
        exp_delta = f64::from_bits(0x1FF0_0000_0000_0000); // 2^-512
    } else {
        scale_factor = scale_factor.min(MAX_SCALE);
        scale_increment = 512;
        exp_delta = f64::from_bits(0x5FF0_0000_0000_0000); // 2^512
    }
    // `(scaleFactor >> 9-1) >>> 32-9` — the sign-fill trick that makes the
    // following mask behave like a truncating remainder for negative inputs.
    let t = ((scale_factor >> 8) as u32 >> 23) as i32;
    let exp_adjust = ((scale_factor + t) & (512 - 1)) - t;
    d *= power_of_two_d(exp_adjust);
    scale_factor -= exp_adjust;
    while scale_factor != 0 {
        d *= exp_delta;
        scale_factor -= scale_increment;
    }
    d
}

const EXP_BITS_I: i32 = 0x7ff0_0000;
const EXP_SIGNIF_BITS_I: i32 = 0x7fff_ffff;
const SIGN_BIT_I: i32 = 0x8000_0000u32 as i32;

/// `StrictMath.log(double)` — the natural logarithm, fdlibm `__ieee754_log`.
///
/// A line-for-line port of `java.lang.FdLibm.Log.compute` as shipped in
/// JDK 25 (itself a transliteration of FDLIBM 5.3 `e_log.c`), including its
/// integer high-word manipulation and its exact evaluation order. The order
/// matters: floating-point addition is not associative, so re-bracketing any
/// of the polynomial or reconstruction expressions changes the last bit and
/// defeats the point of the port.
///
/// Method (fdlibm's own commentary, condensed):
///   1. Argument reduction: `x = 2^k * (1+f)` with `sqrt(2)/2 < 1+f < sqrt(2)`,
///      so `log(x) = k*ln2 + log(1+f)`.
///   2. `s = f/(2+f)`, `z = s*s`, and `log(1+f) = f - s*(f - R)` where the
///      polynomial `R` approximates `2s^2*(...)` on `|z| <= 0.1716`.
///   3. Reconstruction as `k*ln2_hi - ((hfsq - (s*(hfsq+R) + k*ln2_lo)) - f)`,
///      with `ln2` split hi/lo so the `k*ln2` term carries no rounding error.
///
/// Special cases match the spec exactly: `log(+-0) = -inf`, `log(x<0) = NaN`,
/// `log(+inf) = +inf`, `log(NaN) = NaN`. The *bit pattern* of the produced NaN
/// is whatever the hardware's default quiet NaN is (x86-64 SSE yields the
/// negative "indefinite" QNaN, which is what HotSpot reports there too); the
/// spec only requires "a NaN", so tests assert NaN-ness rather than bits.
#[allow(clippy::excessive_precision)]
pub fn log(x: f64) -> f64 {
    // fdlibm's constants, by bit pattern rather than decimal literal: the
    // source writes them as C99/Java hex floats (`0x1.62e42feep-1`) and Rust
    // has no hex float literal, so a decimal transcription would be one more
    // place for a last-ULP mistake to enter. `f64::from_bits` is not `const`
    // at this crate's MSRV (1.80), hence `let` rather than `const`.
    let ln2_hi = f64::from_bits(0x3FE6_2E42_FEE0_0000); // 0x1.62e42feep-1
    let ln2_lo = f64::from_bits(0x3DEA_39EF_3579_3C76); // 0x1.a39ef35793c76p-33
    let lg1 = f64::from_bits(0x3FE5_5555_5555_5593); // 0x1.5555555555593p-1
    let lg2 = f64::from_bits(0x3FD9_9999_9997_FA04); // 0x1.999999997fa04p-2
    let lg3 = f64::from_bits(0x3FD2_4924_9422_9359); // 0x1.2492494229359p-2
    let lg4 = f64::from_bits(0x3FCC_71C5_1D8E_78AF); // 0x1.c71c51d8e78afp-3
    let lg5 = f64::from_bits(0x3FC7_4664_96CB_03DE); // 0x1.7466496cb03dep-3
    let lg6 = f64::from_bits(0x3FC3_9A09_D078_C69F); // 0x1.39a09d078c69fp-3
    let lg7 = f64::from_bits(0x3FC2_F112_DF3E_5244); // 0x1.2f112df3e5244p-3
    let two54 = f64::from_bits(0x4350_0000_0000_0000); // 0x1.0p54

    const EXP_BITS: i32 = 0x7ff0_0000;
    const EXP_SIGNIF_BITS: i32 = 0x7fff_ffff;

    // `__HI` / `__LO`: the high word is SIGNED (fdlibm tests `hx < 0` for the
    // sign bit), the low word is unsigned.
    let mut x = x;
    let mut hx = (x.to_bits() >> 32) as u32 as i32;
    let lx = (x.to_bits() & 0xffff_ffff) as u32;

    let mut k: i32 = 0;
    if hx < 0x0010_0000 {
        // x < 2^-1022
        if ((hx & EXP_SIGNIF_BITS) as u32 | lx) == 0 {
            return -two54 / 0.0; // log(+-0) = -inf
        }
        if hx < 0 {
            return (x - x) / 0.0; // log(-#) = NaN
        }
        k -= 54;
        x *= two54; // subnormal: scale up
        hx = (x.to_bits() >> 32) as u32 as i32;
    }
    if hx >= EXP_BITS {
        return x + x; // +inf -> +inf, NaN -> NaN
    }
    k += (hx >> 20) - 1023;
    hx &= 0x000f_ffff;
    let mut i: i32 = (hx + 0x0009_5f64) & 0x0010_0000;
    // `__HI(x, high)`: replace the high word, keep the low word of the CURRENT x.
    let new_hi = (hx | (i ^ 0x3ff0_0000)) as u32 as u64;
    x = f64::from_bits((new_hi << 32) | (x.to_bits() & 0xffff_ffff));
    k += i >> 20;
    let f = x - 1.0;

    if (0x000f_ffff & (2 + hx)) < 3 {
        // |f| < 2^-20
        if f == 0.0 {
            if k == 0 {
                return 0.0;
            }
            let dk = k as f64;
            return dk * ln2_hi + dk * ln2_lo;
        }
        let r = f * f * (0.5 - 0.33333333333333333 * f);
        if k == 0 {
            return f - r;
        }
        let dk = k as f64;
        return dk * ln2_hi - ((r - dk * ln2_lo) - f);
    }

    let s = f / (2.0 + f);
    let dk = k as f64;
    let z = s * s;
    i = hx - 0x0006_147a;
    let w = z * z;
    let j: i32 = 0x0006_b851 - hx;
    let t1 = w * (lg2 + w * (lg4 + w * lg6));
    let t2 = z * (lg1 + w * (lg3 + w * (lg5 + w * lg7)));
    i |= j;
    let r = t2 + t1;
    if i > 0 {
        let hfsq = 0.5 * f * f;
        if k == 0 {
            f - (hfsq - s * (hfsq + r))
        } else {
            dk * ln2_hi - ((hfsq - (s * (hfsq + r) + dk * ln2_lo)) - f)
        }
    } else if k == 0 {
        f - s * (f - r)
    } else {
        dk * ln2_hi - ((s * (f - r) - dk * ln2_lo) - f)
    }
}

// ---------------------------------------------------------------------------
// Trigonometry: sin / cos / tan and the shared argument reduction.
// ---------------------------------------------------------------------------

/// `FdLibm.Sin.__kernel_sin` — sine on `[-pi/4, pi/4]`.
///
/// `x` is the reduced argument, `y` its tail from the reduction, and `iy` says
/// whether that tail is present (0 = `y` is exactly zero, so the cheaper form
/// is used). The two branches are NOT algebraically interchangeable in floating
/// point; picking the wrong one costs the last bit.
#[allow(clippy::excessive_precision)]
fn kernel_sin(x: f64, y: f64, iy: i32) -> f64 {
    let s1 = f64::from_bits(0xBFC5_5555_5555_5549); // -0x1.5555555555549p-3
    let s2 = f64::from_bits(0x3F81_1111_1110_F8A6); //  0x1.111111110f8a6p-7
    let s3 = f64::from_bits(0xBF2A_01A0_19C1_61D5); // -0x1.a01a019c161d5p-13
    let s4 = f64::from_bits(0x3EC7_1DE3_57B1_FE7D); //  0x1.71de357b1fe7dp-19
    let s5 = f64::from_bits(0xBE5A_E5E6_8A2B_9CEB); // -0x1.ae5e68a2b9cebp-26
    let s6 = f64::from_bits(0x3DE5_D93A_5ACF_D57C); //  0x1.5d93a5acfd57cp-33

    let ix = hi(x) & EXP_SIGNIF_BITS_I;
    if ix < 0x3e40_0000 {
        // |x| < 2^-27
        if (x as i32) == 0 {
            return x; // generate inexact
        }
    }
    let z = x * x;
    let v = z * x;
    let r = s2 + z * (s3 + z * (s4 + z * (s5 + z * s6)));
    if iy == 0 {
        x + v * (s1 + z * r)
    } else {
        x - ((z * (0.5 * y - v * r) - y) - v * s1)
    }
}

/// `FdLibm.Cos.__kernel_cos` — cosine on `[-pi/4, pi/4]`.
#[allow(clippy::excessive_precision)]
fn kernel_cos(x: f64, y: f64) -> f64 {
    let c1 = f64::from_bits(0x3FA5_5555_5555_554C); //  0x1.555555555554cp-5
    let c2 = f64::from_bits(0xBF56_C16C_16C1_5177); // -0x1.6c16c16c15177p-10
    let c3 = f64::from_bits(0x3EFA_01A0_19CB_1590); //  0x1.a01a019cb159p-16
    let c4 = f64::from_bits(0xBE92_7E4F_809C_52AD); // -0x1.27e4f809c52adp-22
    let c5 = f64::from_bits(0x3E21_EE9E_BDB4_B1C4); //  0x1.1ee9ebdb4b1c4p-29
    let c6 = f64::from_bits(0xBDA8_FAE9_BE88_38D4); // -0x1.8fae9be8838d4p-37

    let ix = hi(x) & EXP_SIGNIF_BITS_I;
    if ix < 0x3e40_0000 {
        // |x| < 2^-27
        if (x as i32) == 0 {
            return 1.0; // generate inexact
        }
    }
    let z = x * x;
    let r = z * (c1 + z * (c2 + z * (c3 + z * (c4 + z * (c5 + z * c6)))));
    if ix < 0x3FD3_3333 {
        // |x| < 0.3
        1.0 - (0.5 * z - (z * r - x * y))
    } else {
        // The `qx` split keeps `1 - 0.5*z` from cancelling catastrophically:
        // `a = 1 - qx` is exact and carries the leading bits.
        let qx = if ix > 0x3fe9_0000 {
            0.28125 // x > 0.78125
        } else {
            hi_lo(ix - 0x0020_0000, 0)
        };
        let hz = 0.5 * z - qx;
        let a = 1.0 - qx;
        a - (hz - (z * r - x * y))
    }
}

/// `FdLibm.Tan.__kernel_tan` — tangent on `[-pi/4, pi/4]`.
///
/// `iy` is `+1` for `tan` and `-1` for `-1/tan`, which is how the caller folds
/// the odd quadrants in without a second polynomial.
#[allow(clippy::excessive_precision)]
fn kernel_tan(mut x: f64, mut y: f64, iy: i32) -> f64 {
    let pio4 = f64::from_bits(0x3FE9_21FB_5444_2D18); // 0x1.921fb54442d18p-1
    let pio4lo = f64::from_bits(0x3C81_A626_3314_5C07); // 0x1.1a62633145c07p-55
    let t = [
        f64::from_bits(0x3FD5_5555_5555_5563), //  0x1.5555555555563p-2
        f64::from_bits(0x3FC1_1111_1110_FE7A), //  0x1.111111110fe7ap-3
        f64::from_bits(0x3FAB_A1BA_1BB3_41FE), //  0x1.ba1ba1bb341fep-5
        f64::from_bits(0x3F96_64F4_8406_D637), //  0x1.664f48406d637p-6
        f64::from_bits(0x3F82_26E3_E96E_8493), //  0x1.226e3e96e8493p-7
        f64::from_bits(0x3F6D_6D22_C956_0328), //  0x1.d6d22c9560328p-9
        f64::from_bits(0x3F57_DBC8_FEE0_8315), //  0x1.7dbc8fee08315p-10
        f64::from_bits(0x3F43_44D8_F2F2_6501), //  0x1.344d8f2f26501p-11
        f64::from_bits(0x3F30_26F7_1A8D_1068), //  0x1.026f71a8d1068p-12
        f64::from_bits(0x3F14_7E88_A037_92A6), //  0x1.47e88a03792a6p-14
        f64::from_bits(0x3F12_B80F_32F0_A7E9), //  0x1.2b80f32f0a7e9p-14
        f64::from_bits(0xBEF3_75CB_DB60_5373), // -0x1.375cbdb605373p-16
        f64::from_bits(0x3EFB_2A70_74BF_7AD4), //  0x1.b2a7074bf7ad4p-16
    ];

    let hx = hi(x);
    let ix = hx & EXP_SIGNIF_BITS_I;
    if ix < 0x3e30_0000 {
        // |x| < 2^-28
        if (x as i32) == 0 {
            if ((ix | lo(x)) | (iy + 1)) == 0 {
                return 1.0 / x.abs();
            } else if iy == 1 {
                return x;
            } else {
                // compute -1/(x+y) carefully
                let mut z = x + y;
                let w = z;
                z = with_lo(z, 0);
                let v = y - (z - x);
                let a = -1.0 / w;
                let t0 = with_lo(a, 0);
                let s = 1.0 + t0 * z;
                return t0 + a * (s + t0 * v);
            }
        }
    }
    if ix >= 0x3FE5_9428 {
        // |x| >= 0.6744 — fold through pi/4 so the polynomial stays accurate.
        if hx < 0 {
            x = -x;
            y = -y;
        }
        let z = pio4 - x;
        let w = pio4lo - y;
        x = z + w;
        y = 0.0;
    }
    let z = x * x;
    let w0 = z * z;
    let mut r = t[1] + w0 * (t[3] + w0 * (t[5] + w0 * (t[7] + w0 * (t[9] + w0 * t[11]))));
    let v = z * (t[2] + w0 * (t[4] + w0 * (t[6] + w0 * (t[8] + w0 * (t[10] + w0 * t[12])))));
    let s = z * x;
    r = y + z * (s * (r + v) + y);
    r += t[0] * s;
    let w = x + r;
    if ix >= 0x3FE5_9428 {
        let v = iy as f64;
        return ((1 - ((hx >> 30) & 2)) as f64) * (v - 2.0 * (x - (w * w / (w + v) - r)));
    }
    if iy == 1 {
        w
    } else {
        // Not `-1.0/(x + r)`: that would be accurate to 2 ULP, and 2 ULP is a
        // different function from fdlibm's.
        let z = with_lo(w, 0);
        let v = r - (z - x); // z + v = r + x
        let a = -1.0 / w;
        let t0 = with_lo(a, 0);
        let s = 1.0 + t0 * z;
        t0 + a * (s + t0 * v)
    }
}

/// `FdLibm.KernelRemPio2.__kernel_rem_pio2` — the multi-precision reduction
/// used when `|x|` is large enough that `x - n*pi/2` cannot be formed in
/// double precision without losing every significant bit.
///
/// This is the routine that makes `sin(1e300)` mean anything at all: it works
/// in 24-bit chunks against a 396-bit table of `2/pi`.
#[allow(clippy::needless_range_loop)]
fn kernel_rem_pio2(
    x: &[f64],
    y: &mut [f64],
    e0: i32,
    nx: usize,
    prec: usize,
    ipio2: &[i32],
) -> i32 {
    let pio2 = [
        f64::from_bits(0x3FF9_21FB_4000_0000), // 0x1.921fb4p0
        f64::from_bits(0x3E74_442D_0000_0000), // 0x1.4442dp-24
        f64::from_bits(0x3CF8_4698_8000_0000), // 0x1.846988p-48
        f64::from_bits(0x3B78_CC51_6000_0000), // 0x1.8cc516p-72
        f64::from_bits(0x39F0_1B83_8000_0000), // 0x1.01b838p-96
        f64::from_bits(0x387A_2520_4000_0000), // 0x1.a25204p-120
        f64::from_bits(0x36E3_8222_8000_0000), // 0x1.382228p-145
        f64::from_bits(0x3569_F31D_0000_0000), // 0x1.9f31dp-169
    ];
    let two24 = f64::from_bits(0x4170_0000_0000_0000); // 0x1.0p24
    let twon24 = f64::from_bits(0x3E70_0000_0000_0000); // 0x1.0p-24
    const INIT_JK: [usize; 4] = [2, 3, 4, 6];

    let mut iq = [0i32; 20];
    let mut f = [0.0f64; 20];
    let mut fq = [0.0f64; 20];
    let mut q = [0.0f64; 20];

    let jk = INIT_JK[prec];
    let jp = jk;
    let jx = nx - 1;
    let mut jv = (e0 - 3) / 24;
    if jv < 0 {
        jv = 0;
    }
    let mut q0 = e0 - 24 * (jv + 1);

    // set up f[0..] = ipio2[jv - jx ..], zero-filled below index 0
    let f_base = jv - jx as i32;
    let m = jx + jk;
    for (i, item) in f.iter_mut().take(m + 1).enumerate() {
        let j = f_base + i as i32;
        *item = if j < 0 { 0.0 } else { ipio2[j as usize] as f64 };
    }

    for i in 0..=jk {
        let mut fw = 0.0;
        for j in 0..=jx {
            fw += x[j] * f[jx + i - j];
        }
        q[i] = fw;
    }

    let mut jz = jk;
    let mut n;
    let mut z;
    let mut ih;
    loop {
        // distill q[] into iq[] reversingly
        let mut i = 0usize;
        let mut j = jz;
        z = q[jz];
        while j > 0 {
            let fw = ((twon24 * z) as i32) as f64;
            iq[i] = (z - two24 * fw) as i32;
            z = q[j - 1] + fw;
            i += 1;
            j -= 1;
        }

        // compute n
        z = scalb(z, q0);
        z -= 8.0 * (z * 0.125).floor(); // trim off integer >= 8
        n = z as i32;
        z -= n as f64;
        ih = 0;
        if q0 > 0 {
            // need iq[jz-1] to determine n
            let i = iq[jz - 1] >> (24 - q0);
            n += i;
            iq[jz - 1] -= i << (24 - q0);
            ih = iq[jz - 1] >> (23 - q0);
        } else if q0 == 0 {
            ih = iq[jz - 1] >> 23;
        } else if z >= 0.5 {
            ih = 2;
        }

        if ih > 0 {
            // q > 0.5: replace q by 1-q and remember to negate
            n += 1;
            let mut carry = 0;
            for i in 0..jz {
                let j = iq[i];
                if carry == 0 {
                    if j != 0 {
                        carry = 1;
                        iq[i] = 0x100_0000 - j;
                    }
                } else {
                    iq[i] = 0xff_ffff - j;
                }
            }
            if q0 > 0 {
                // rare case: chance is 1 in 12
                match q0 {
                    1 => iq[jz - 1] &= 0x7f_ffff,
                    2 => iq[jz - 1] &= 0x3f_ffff,
                    _ => {}
                }
            }
            if ih == 2 {
                z = 1.0 - z;
                if carry != 0 {
                    z -= scalb(1.0, q0);
                }
            }
        }

        if z == 0.0 {
            let mut j = 0;
            for i in (jk..=jz - 1).rev() {
                j |= iq[i];
            }
            if j == 0 {
                // need recomputation: add more terms of q
                let mut k = 1;
                while iq[jk - k] == 0 {
                    k += 1;
                }
                for i in (jz + 1)..=(jz + k) {
                    f[jx + i] = ipio2[(jv as usize) + i] as f64;
                    let mut fw = 0.0;
                    for j in 0..=jx {
                        fw += x[j] * f[jx + i - j];
                    }
                    q[i] = fw;
                }
                jz += k;
                continue;
            }
        }
        break;
    }

    // chop off zero terms
    if z == 0.0 {
        jz -= 1;
        q0 -= 24;
        while iq[jz] == 0 {
            jz -= 1;
            q0 -= 24;
        }
    } else {
        // break z into 24-bit if necessary
        z = scalb(z, -q0);
        if z >= two24 {
            let fw = ((twon24 * z) as i32) as f64;
            iq[jz] = (z - two24 * fw) as i32;
            jz += 1;
            q0 += 24;
            iq[jz] = fw as i32;
        } else {
            iq[jz] = z as i32;
        }
    }

    // convert integer "bit" chunk to floating-point value
    let mut fw = scalb(1.0, q0);
    for i in (0..=jz).rev() {
        q[i] = fw * (iq[i] as f64);
        fw *= twon24;
    }

    // compute PIo2[0,...,jp]*q[jz,...,0]
    for i in (0..=jz).rev() {
        let mut fw = 0.0;
        let mut k = 0usize;
        while k <= jp && k <= jz - i {
            fw += pio2[k] * q[i + k];
            k += 1;
        }
        fq[jz - i] = fw;
    }

    match prec {
        0 => {
            let mut fw = 0.0;
            for i in (0..=jz).rev() {
                fw += fq[i];
            }
            y[0] = if ih == 0 { fw } else { -fw };
        }
        1 | 2 => {
            let mut fw = 0.0;
            for i in (0..=jz).rev() {
                fw += fq[i];
            }
            y[0] = if ih == 0 { fw } else { -fw };
            let mut fw2 = fq[0] - fw;
            for i in 1..=jz {
                fw2 += fq[i];
            }
            y[1] = if ih == 0 { fw2 } else { -fw2 };
        }
        3 => {
            // "painful" in the original, and unreached from here: the two
            // callers below both pass prec = 2. Ported anyway rather than
            // stubbed, because a stub is an exclusion and exclusions rot.
            for i in (1..=jz).rev() {
                let fw = fq[i - 1] + fq[i];
                fq[i] += fq[i - 1] - fw;
                fq[i - 1] = fw;
            }
            for i in (2..=jz).rev() {
                let fw = fq[i - 1] + fq[i];
                fq[i] += fq[i - 1] - fw;
                fq[i - 1] = fw;
            }
            let mut fw = 0.0;
            for i in (2..=jz).rev() {
                fw += fq[i];
            }
            if ih == 0 {
                y[0] = fq[0];
                y[1] = fq[1];
                y[2] = fw;
            } else {
                y[0] = -fq[0];
                y[1] = -fq[1];
                y[2] = -fw;
            }
        }
        _ => {}
    }
    n & 7
}

/// `FdLibm.RemPio2.__ieee754_rem_pio2` — reduce `x` to `y[0] + y[1]` in
/// `[-pi/4, pi/4]`, returning `n` such that `x = n*(pi/2) + y`.
#[allow(clippy::excessive_precision)]
fn rem_pio2(x: f64, y: &mut [f64]) -> i32 {
    // 396 hex digits of 2/pi, 24 bits at a time.
    const TWO_OVER_PI: [i32; 66] = [
        0xA2F983, 0x6E4E44, 0x1529FC, 0x2757D1, 0xF534DD, 0xC0DB62, 0x95993C, 0x439041, 0xFE5163,
        0xABDEBB, 0xC561B7, 0x246E3A, 0x424DD2, 0xE00649, 0x2EEA09, 0xD1921C, 0xFE1DEB, 0x1CB129,
        0xA73EE8, 0x8235F5, 0x2EBB44, 0x84E99C, 0x7026B4, 0x5F7E41, 0x3991D6, 0x398353, 0x39F49C,
        0x845F8B, 0xBDF928, 0x3B1FF8, 0x97FFDE, 0x05980F, 0xEF2F11, 0x8B5A0A, 0x6D1F6D, 0x367ECF,
        0x27CB09, 0xB74F46, 0x3F669E, 0x5FEA2D, 0x7527BA, 0xC7EBE5, 0xF17B3D, 0x0739F7, 0x8A5292,
        0xEA6BFB, 0x5FB11F, 0x8D5D08, 0x560330, 0x46FC7B, 0x6BABF0, 0xCFBC20, 0x9AF436, 0x1DA9E3,
        0x91615E, 0xE61B08, 0x659985, 0x5F14A0, 0x68408D, 0xFFD880, 0x4D7327, 0x310606, 0x1556CA,
        0x73A8C9, 0x60E27B, 0xC08C6B,
    ];
    // High words of n*(pi/2) for n = 1..32, used to detect the cancellation
    // cases where one round of reduction is not enough.
    const NPIO2_HW: [i32; 32] = [
        0x3FF921FB, 0x400921FB, 0x4012D97C, 0x401921FB, 0x401F6A7A, 0x4022D97C, 0x4025FDBB,
        0x402921FB, 0x402C463A, 0x402F6A7A, 0x4031475C, 0x4032D97C, 0x40346B9C, 0x4035FDBB,
        0x40378FDB, 0x403921FB, 0x403AB41B, 0x403C463A, 0x403DD85A, 0x403F6A7A, 0x40407E4C,
        0x4041475C, 0x4042106C, 0x4042D97C, 0x4043A28C, 0x40446B9C, 0x404534AC, 0x4045FDBB,
        0x4046C6CB, 0x40478FDB, 0x404858EB, 0x404921FB,
    ];

    let invpio2 = f64::from_bits(0x3FE4_5F30_6DC9_C883); // 0x1.45f306dc9c883p-1
    let pio2_1 = f64::from_bits(0x3FF9_21FB_5440_0000); // 0x1.921fb544p0
    let pio2_1t = f64::from_bits(0x3DD0_B461_1A62_6331); // 0x1.0b4611a626331p-34
    let pio2_2 = f64::from_bits(0x3DD0_B461_1A60_0000); // 0x1.0b4611a6p-34
    let pio2_2t = f64::from_bits(0x3BA3_198A_2E03_7073); // 0x1.3198a2e037073p-69
    let pio2_3 = f64::from_bits(0x3BA3_198A_2E00_0000); // 0x1.3198a2ep-69
    let pio2_3t = f64::from_bits(0x397B_839A_2520_49C1); // 0x1.b839a252049c1p-104
    let two24 = f64::from_bits(0x4170_0000_0000_0000); // 0x1.0p24

    let hx = hi(x);
    let ix = hx & EXP_SIGNIF_BITS_I;
    if ix <= 0x3fe9_21fb {
        // |x| ~<= pi/4, no reduction needed
        y[0] = x;
        y[1] = 0.0;
        return 0;
    }
    if ix < 0x4002_d97c {
        // |x| < 3pi/4, special case with n = +-1
        if hx > 0 {
            let mut z = x - pio2_1;
            if ix != 0x3ff9_21fb {
                // 33+53 bit pi is good enough
                y[0] = z - pio2_1t;
                y[1] = (z - y[0]) - pio2_1t;
            } else {
                // near pi/2, use 33+33+53 bit pi
                z -= pio2_2;
                y[0] = z - pio2_2t;
                y[1] = (z - y[0]) - pio2_2t;
            }
            return 1;
        } else {
            let mut z = x + pio2_1;
            if ix != 0x3ff9_21fb {
                y[0] = z + pio2_1t;
                y[1] = (z - y[0]) + pio2_1t;
            } else {
                z += pio2_2;
                y[0] = z + pio2_2t;
                y[1] = (z - y[0]) + pio2_2t;
            }
            return -1;
        }
    }
    if ix <= 0x4139_21fb {
        // |x| ~<= 2^19*(pi/2), medium size: iterate until the cancellation is gone
        let t = x.abs();
        let n = (t * invpio2 + 0.5) as i32;
        let fn_ = n as f64;
        let mut r = t - fn_ * pio2_1;
        let mut w = fn_ * pio2_1t; // 1st round good to 85 bits
        if n < 32 && ix != NPIO2_HW[(n - 1) as usize] {
            y[0] = r - w; // quick check no cancellation
        } else {
            let j = ix >> 20;
            y[0] = r - w;
            let i = j - ((hi(y[0]) >> 20) & 0x7ff);
            if i > 16 {
                // 2nd iteration needed, good to 118
                let t2 = r;
                w = fn_ * pio2_2;
                r = t2 - w;
                w = fn_ * pio2_2t - ((t2 - r) - w);
                y[0] = r - w;
                let i = j - ((hi(y[0]) >> 20) & 0x7ff);
                if i > 49 {
                    // 3rd iteration needed, 151 bits accuracy — covers all cases
                    let t3 = r;
                    w = fn_ * pio2_3;
                    r = t3 - w;
                    w = fn_ * pio2_3t - ((t3 - r) - w);
                    y[0] = r - w;
                }
            }
        }
        y[1] = (r - y[0]) - w;
        if hx < 0 {
            y[0] = -y[0];
            y[1] = -y[1];
            return -n;
        }
        return n;
    }
    if ix >= EXP_BITS_I {
        // x is inf or NaN
        y[0] = x - x;
        y[1] = y[0];
        return 0;
    }
    // Set z = scalbn(|x|, ilogb(x) - 23) and split into three 24-bit chunks.
    let mut z = with_lo(0.0, lo(x));
    let e0 = (ix >> 20) - 1046; // e0 = ilogb(z) - 23
    z = with_hi(z, ix - (e0 << 20));
    let mut tx = [0.0f64; 3];
    for item in tx.iter_mut().take(2) {
        *item = (z as i32) as f64;
        z = (z - *item) * two24;
    }
    tx[2] = z;
    let mut nx = 3usize;
    while tx[nx - 1] == 0.0 {
        nx -= 1; // skip zero term
    }
    let n = kernel_rem_pio2(&tx, y, e0, nx, 2, &TWO_OVER_PI);
    if hx < 0 {
        y[0] = -y[0];
        y[1] = -y[1];
        return -n;
    }
    n
}

/// `StrictMath.sin(double)` — fdlibm `__ieee754_sin`.
///
/// Reduce to `[-pi/4, pi/4]` and pick the kernel by quadrant. `sin(+-inf)` and
/// `sin(NaN)` are NaN, produced as `x - x` so the NaN payload propagates.
pub fn sin(x: f64) -> f64 {
    let ix = hi(x) & EXP_SIGNIF_BITS_I;
    if ix <= 0x3fe9_21fb {
        kernel_sin(x, 0.0, 0)
    } else if ix >= EXP_BITS_I {
        x - x
    } else {
        let mut y = [0.0f64; 2];
        let n = rem_pio2(x, &mut y);
        match n & 3 {
            0 => kernel_sin(y[0], y[1], 1),
            1 => kernel_cos(y[0], y[1]),
            2 => -kernel_sin(y[0], y[1], 1),
            _ => -kernel_cos(y[0], y[1]),
        }
    }
}

/// `StrictMath.cos(double)` — fdlibm `__ieee754_cos`. Same reduction as `sin`,
/// quadrant table rotated by one.
pub fn cos(x: f64) -> f64 {
    let ix = hi(x) & EXP_SIGNIF_BITS_I;
    if ix <= 0x3fe9_21fb {
        kernel_cos(x, 0.0)
    } else if ix >= EXP_BITS_I {
        x - x
    } else {
        let mut y = [0.0f64; 2];
        let n = rem_pio2(x, &mut y);
        match n & 3 {
            0 => kernel_cos(y[0], y[1]),
            1 => -kernel_sin(y[0], y[1], 1),
            2 => -kernel_cos(y[0], y[1]),
            _ => kernel_sin(y[0], y[1], 1),
        }
    }
}

/// `StrictMath.tan(double)` — fdlibm `__ieee754_tan`. One kernel serves both
/// parities: `iy = 1 - 2*(n & 1)` asks it for `tan` or for `-1/tan`.
pub fn tan(x: f64) -> f64 {
    let ix = hi(x) & EXP_SIGNIF_BITS_I;
    if ix <= 0x3fe9_21fb {
        kernel_tan(x, 0.0, 1)
    } else if ix >= EXP_BITS_I {
        x - x
    } else {
        let mut y = [0.0f64; 2];
        let n = rem_pio2(x, &mut y);
        kernel_tan(y[0], y[1], 1 - ((n & 1) << 1))
    }
}

// ---------------------------------------------------------------------------
// Inverse trigonometry: asin / acos / atan / atan2.
// ---------------------------------------------------------------------------

/// The shared `pS`/`qS` rational approximation to `asin`'s tail, used verbatim
/// by both `Asin` and `Acos` (the JDK duplicates the constants; one copy here,
/// because two copies of one primitive is how they drift apart).
#[inline]
#[allow(clippy::excessive_precision)]
fn asin_rational(t: f64) -> (f64, f64) {
    let ps0 = f64::from_bits(0x3FC5_5555_5555_5555); //  0x1.5555555555555p-3
    let ps1 = f64::from_bits(0xBFD4_D612_03EB_6F7D); // -0x1.4d61203eb6f7dp-2
    let ps2 = f64::from_bits(0x3FC9_C155_0E88_4455); //  0x1.9c1550e884455p-3
    let ps3 = f64::from_bits(0xBFA4_8228_B568_8F3B); // -0x1.48228b5688f3bp-5
    let ps4 = f64::from_bits(0x3F49_EFE0_7501_B288); //  0x1.9efe07501b288p-11
    let ps5 = f64::from_bits(0x3F02_3DE1_0DFD_F709); //  0x1.23de10dfdf709p-15
    let qs1 = f64::from_bits(0xC003_3A27_1C8A_2D4B); // -0x1.33a271c8a2d4bp1
    let qs2 = f64::from_bits(0x4000_2AE5_9C59_8AC8); //  0x1.02ae59c598ac8p1
    let qs3 = f64::from_bits(0xBFE6_066C_1B8D_0159); // -0x1.6066c1b8d0159p-1
    let qs4 = f64::from_bits(0x3FB3_B8C5_B12E_9282); //  0x1.3b8c5b12e9282p-4
    let p = t * (ps0 + t * (ps1 + t * (ps2 + t * (ps3 + t * (ps4 + t * ps5)))));
    let q = 1.0 + t * (qs1 + t * (qs2 + t * (qs3 + t * qs4)));
    (p, q)
}

/// `StrictMath.asin(double)` — fdlibm `__ieee754_asin`.
#[allow(clippy::excessive_precision)]
pub fn asin(x: f64) -> f64 {
    let pio2_hi = f64::from_bits(0x3FF9_21FB_5444_2D18); // 0x1.921fb54442d18p0
    let pio2_lo = f64::from_bits(0x3C91_A626_3314_5C07); // 0x1.1a62633145c07p-54
    let pio4_hi = f64::from_bits(0x3FE9_21FB_5444_2D18); // 0x1.921fb54442d18p-1
    let huge = 1.0e300f64;

    let hx = hi(x);
    let ix = hx & EXP_SIGNIF_BITS_I;
    if ix >= 0x3ff0_0000 {
        // |x| >= 1
        if ((ix - 0x3ff0_0000) | lo(x)) == 0 {
            return x * pio2_hi + x * pio2_lo; // asin(+-1) = +-pi/2 with inexact
        }
        return (x - x) / (x - x); // asin(|x| > 1) is NaN
    } else if ix < 0x3fe0_0000 {
        // |x| < 0.5
        let mut t = 0.0;
        if ix < 0x3e40_0000 {
            // |x| < 2^-27
            if huge + x > 1.0 {
                return x; // return x with inexact if x != 0
            }
        } else {
            t = x * x;
        }
        let (p, q) = asin_rational(t);
        let w = p / q;
        return x + x * w;
    }
    // |x| >= 0.5: asin(x) = pi/2 - 2*asin(sqrt((1-x)/2))
    let mut w = 1.0 - x.abs();
    let t = w * 0.5;
    let (p, q) = asin_rational(t);
    let s = t.sqrt();
    let tt;
    if ix >= 0x3FEF_3333 {
        // |x| > 0.975 — the pio4 split below would not help here
        w = p / q;
        tt = pio2_hi - (2.0 * (s + s * w) - pio2_lo);
    } else {
        // Split `sqrt(t)` into a head with a zero low word and an exact
        // correction `c`, so the subtraction from pi/4 does not cancel.
        w = with_lo(s, 0);
        let c = (t - w * w) / (s + w);
        let r = p / q;
        let p2 = 2.0 * s * r - (pio2_lo - 2.0 * c);
        let q2 = pio4_hi - 2.0 * w;
        tt = pio4_hi - (p2 - q2);
    }
    if hx > 0 {
        tt
    } else {
        -tt
    }
}

/// `StrictMath.acos(double)` — fdlibm `__ieee754_acos`.
#[allow(clippy::excessive_precision)]
pub fn acos(x: f64) -> f64 {
    let pio2_hi = f64::from_bits(0x3FF9_21FB_5444_2D18); // 0x1.921fb54442d18p0
    let pio2_lo = f64::from_bits(0x3C91_A626_3314_5C07); // 0x1.1a62633145c07p-54

    let hx = hi(x);
    let ix = hx & EXP_SIGNIF_BITS_I;
    if ix >= 0x3ff0_0000 {
        // |x| >= 1
        if ((ix - 0x3ff0_0000) | lo(x)) == 0 {
            return if hx > 0 {
                0.0 // acos(1) = 0
            } else {
                std::f64::consts::PI + 2.0 * pio2_lo // acos(-1) = pi
            };
        }
        return (x - x) / (x - x); // acos(|x| > 1) is NaN
    }
    if ix < 0x3fe0_0000 {
        // |x| < 0.5
        if ix <= 0x3c60_0000 {
            return pio2_hi + pio2_lo; // |x| < 2^-57
        }
        let z = x * x;
        let (p, q) = asin_rational(z);
        let r = p / q;
        pio2_hi - (x - (pio2_lo - x * r))
    } else if hx < 0 {
        // x < -0.5
        let z = (1.0 + x) * 0.5;
        let (p, q) = asin_rational(z);
        let s = z.sqrt();
        let r = p / q;
        let w = r * s - pio2_lo;
        std::f64::consts::PI - 2.0 * (s + w)
    } else {
        // x > 0.5
        let z = (1.0 - x) * 0.5;
        let s = z.sqrt();
        let df = with_lo(s, 0);
        let c = (z - df * df) / (s + df);
        let (p, q) = asin_rational(z);
        let r = p / q;
        let w = r * s + c;
        2.0 * (df + w)
    }
}

/// `StrictMath.atan(double)` — fdlibm `__atan`.
///
/// Reduce `|x|` into one of four intervals around 0.5/1.0/1.5/inf, evaluate an
/// odd polynomial there, and add back the tabulated `atan` of the interval
/// centre as a hi/lo pair.
#[allow(clippy::excessive_precision)]
pub fn atan(mut x: f64) -> f64 {
    let atanhi = [
        f64::from_bits(0x3FDD_AC67_0561_BB4F), // atan(0.5) hi
        f64::from_bits(0x3FE9_21FB_5444_2D18), // atan(1.0) hi
        f64::from_bits(0x3FEF_730B_D281_F69B), // atan(1.5) hi
        f64::from_bits(0x3FF9_21FB_5444_2D18), // atan(inf) hi
    ];
    let atanlo = [
        f64::from_bits(0x3C7A_2B7F_222F_65E2), // atan(0.5) lo
        f64::from_bits(0x3C81_A626_3314_5C07), // atan(1.0) lo
        f64::from_bits(0x3C70_0788_7AF0_CBBD), // atan(1.5) lo
        f64::from_bits(0x3C91_A626_3314_5C07), // atan(inf) lo
    ];
    let at = [
        f64::from_bits(0x3FD5_5555_5555_550D), //  0x1.555555555550dp-2
        f64::from_bits(0xBFC9_9999_9998_EBC4), // -0x1.999999998ebc4p-3
        f64::from_bits(0x3FC2_4924_9200_83FF), //  0x1.24924920083ffp-3
        f64::from_bits(0xBFBC_71C6_FE23_1671), // -0x1.c71c6fe231671p-4
        f64::from_bits(0x3FB7_45CD_C54C_206E), //  0x1.745cdc54c206ep-4
        f64::from_bits(0xBFB3_B0F2_AF74_9A6D), // -0x1.3b0f2af749a6dp-4
        f64::from_bits(0x3FB1_0D66_A0D0_3D51), //  0x1.10d66a0d03d51p-4
        f64::from_bits(0xBFAD_DE2D_52DE_FD9A), // -0x1.dde2d52defd9ap-5
        f64::from_bits(0x3FA9_7B4B_2476_0DEB), //  0x1.97b4b24760debp-5
        f64::from_bits(0xBFA2_B444_2C6A_6C2F), // -0x1.2b4442c6a6c2fp-5
        f64::from_bits(0x3F90_AD3A_E322_DA11), //  0x1.0ad3ae322da11p-6
    ];
    let huge = 1.0e300f64;

    let hx = hi(x);
    let ix = hx & EXP_SIGNIF_BITS_I;
    let id: i32;
    if ix >= 0x4410_0000 {
        // |x| >= 2^66 — atan is pi/2 to within a rounding
        if ix > EXP_BITS_I || (ix == EXP_BITS_I && lo(x) != 0) {
            return x + x; // NaN
        }
        return if hx > 0 {
            atanhi[3] + atanlo[3]
        } else {
            -atanhi[3] - atanlo[3]
        };
    }
    if ix < 0x3fdc_0000 {
        // |x| < 0.4375
        if ix < 0x3e20_0000 {
            // |x| < 2^-29
            if huge + x > 1.0 {
                return x; // raise inexact
            }
        }
        id = -1;
    } else {
        x = x.abs();
        if ix < 0x3ff3_0000 {
            if ix < 0x3fe6_0000 {
                // 7/16 <= |x| < 11/16
                id = 0;
                x = (2.0 * x - 1.0) / (2.0 + x);
            } else {
                // 11/16 <= |x| < 19/16
                id = 1;
                x = (x - 1.0) / (x + 1.0);
            }
        } else if ix < 0x4003_8000 {
            // 19/16 <= |x| < 2.4375
            id = 2;
            x = (x - 1.5) / (1.0 + 1.5 * x);
        } else {
            // 2.4375 <= |x| < 2^66
            id = 3;
            x = -1.0 / x;
        }
    }
    let z = x * x;
    let w = z * z;
    // Split odd/even so the two Estrin chains are independent.
    let s1 = z * (at[0] + w * (at[2] + w * (at[4] + w * (at[6] + w * (at[8] + w * at[10])))));
    let s2 = w * (at[1] + w * (at[3] + w * (at[5] + w * (at[7] + w * at[9]))));
    if id < 0 {
        x - x * (s1 + s2)
    } else {
        let z = atanhi[id as usize] - ((x * (s1 + s2) - atanlo[id as usize]) - x);
        if hx < 0 {
            -z
        } else {
            z
        }
    }
}

/// `StrictMath.atan2(double, double)` — fdlibm `__ieee754_atan2`.
///
/// The whole special-case ladder is normative: `atan2(+-0, -x)` is `+-pi`,
/// `atan2(+-inf, +-inf)` are the four odd multiples of `pi/4`, and the `tiny`
/// addend exists to raise inexact, which changes nothing about the value but is
/// part of the algorithm's shape.
#[allow(clippy::excessive_precision)]
pub fn atan2(y: f64, x: f64) -> f64 {
    let tiny = 1.0e-300f64;
    let pi_o_4 = f64::from_bits(0x3FE9_21FB_5444_2D18); // 0x1.921fb54442d18p-1
    let pi_o_2 = f64::from_bits(0x3FF9_21FB_5444_2D18); // 0x1.921fb54442d18p0
    let pi_lo = f64::from_bits(0x3CA1_A626_3314_5C07); // 0x1.1a62633145c07p-53

    let hx = hi(x);
    let ix = hx & EXP_SIGNIF_BITS_I;
    let lx = lo(x);
    let hy = hi(y);
    let iy = hy & EXP_SIGNIF_BITS_I;
    let ly = lo(y);

    if x.is_nan() || y.is_nan() {
        return x + y;
    }
    // `hx == 0x3ff00000 && lx == 0`, i.e. x is exactly 1.0, written the way
    // fdlibm writes it: OR the two word differences and test for zero.
    //
    // `wrapping_sub`, not `-`: `hi()` is signed (fdlibm reads the sign bit as
    // `hx < 0`), so for any NEGATIVE x this subtracts 0x3ff00000 from a large
    // negative `i32` and underflows. The C original is `hx-0x3ff00000` on an
    // `int32_t` and wraps; we need the same bits, and only whether they are
    // ZERO is ever read, which wrapping preserves exactly. Plain `-` made this
    // a debug-only panic for every negative x — `atan2(y, -1.0)` was enough.
    if (hx.wrapping_sub(0x3ff0_0000) | lx) == 0 {
        return atan(y); // x = 1.0
    }
    let m = ((hy >> 31) & 1) | ((hx >> 30) & 2); // 2*sign(x) + sign(y)

    if (iy | ly) == 0 {
        return match m {
            0 | 1 => y,                        // atan(+-0, +anything) = +-0
            2 => std::f64::consts::PI + tiny,  // atan(+0, -anything) = pi
            _ => -std::f64::consts::PI - tiny, // atan(-0, -anything) = -pi
        };
    }
    if (ix | lx) == 0 {
        return if hy < 0 {
            -pi_o_2 - tiny
        } else {
            pi_o_2 + tiny
        };
    }
    if ix == EXP_BITS_I {
        if iy == EXP_BITS_I {
            return match m {
                0 => pi_o_4 + tiny,        // atan(+inf, +inf)
                1 => -pi_o_4 - tiny,       // atan(-inf, +inf)
                2 => 3.0 * pi_o_4 + tiny,  // atan(+inf, -inf)
                _ => -3.0 * pi_o_4 - tiny, // atan(-inf, -inf)
            };
        } else {
            return match m {
                0 => 0.0,                          // atan(+..., +inf)
                1 => -0.0,                         // atan(-..., +inf)
                2 => std::f64::consts::PI + tiny,  // atan(+..., -inf)
                _ => -std::f64::consts::PI - tiny, // atan(-..., -inf)
            };
        }
    }
    if iy == EXP_BITS_I {
        return if hy < 0 {
            -pi_o_2 - tiny
        } else {
            pi_o_2 + tiny
        };
    }

    let k = (iy - ix) >> 20;
    let z = if k > 60 {
        pi_o_2 + 0.5 * pi_lo // |y/x| > 2^60
    } else if hx < 0 && k < -60 {
        0.0 // |y|/x < -2^60
    } else {
        atan((y / x).abs()) // safe to do y/x
    };
    match m {
        0 => z,                                  // atan(+, +)
        1 => -z,                                 // atan(-, +)
        2 => std::f64::consts::PI - (z - pi_lo), // atan(+, -)
        _ => (z - pi_lo) - std::f64::consts::PI, // atan(-, -)
    }
}

// ---------------------------------------------------------------------------
// Roots, exponentials, logarithms.
// ---------------------------------------------------------------------------

/// `StrictMath.cbrt(double)` — fdlibm `__cbrt`, in the JDK's revised form.
///
/// The `B1`/`B2` magic biases turn a divide-by-three of the *exponent field*
/// into a 5-bit-accurate first approximation; one rational refinement takes it
/// to 23 bits and a final Newton step to 53. The `__LO(t, 0)` then
/// `__HI(t, __HI(t)+1)` pair is not a rounding fudge: it forces `t` to have a
/// zero low word so `t*t` below is exact, and nudges it up so the last Newton
/// step converges from the correct side.
#[allow(clippy::excessive_precision)]
pub fn cbrt(x: f64) -> f64 {
    const B1: i32 = 715094163; // (682 - 0.03306235651) * 2^20
    const B2: i32 = 696219795; // (664 - 0.03306235651) * 2^20
    let c = f64::from_bits(0x3FE1_5F15_F15F_15F1); //   19/35
    let d = f64::from_bits(0xBFE6_91DE_2532_C834); // -864/1225
    let e = f64::from_bits(0x3FF6_A0EA_0EA0_EA0F); //   99/70
    let f = f64::from_bits(0x3FF9_B6DB_6DB6_DB6E); //   45/28
    let g = f64::from_bits(0x3FD6_DB6D_B6DB_6DB7); //    5/14

    if x == 0.0 || !x.is_finite() {
        return x; // handles signed zeros, +-inf and NaN
    }
    let sign = if x < 0.0 { -1.0 } else { 1.0 };
    let x = x.abs();

    let mut t;
    if x < f64::from_bits(0x0010_0000_0000_0000) {
        // subnormal: scale by 2^54 first
        t = f64::from_bits(0x4350_0000_0000_0000); // 2^54
        t *= x;
        t = with_hi(0.0, hi(t) / 3 + B2);
    } else {
        t = with_hi(0.0, hi(x) / 3 + B1);
    }

    let mut r = t * t / x;
    let mut s = c + r * t;
    t *= g + f / (s + e + d / s);

    t = with_lo(t, 0);
    t = with_hi(t, hi(t).wrapping_add(1));

    s = t * t; // t*t is exact
    r = x / s;
    let w = t + t;
    r = (r - t) / (w + r); // r - s is exact
    t += t * r;
    sign * t
}

/// `StrictMath.hypot(double, double)` — fdlibm `__ieee754_hypot`.
///
/// `sqrt(x*x + y*y)` computed without ever forming `x*x`, which would overflow
/// for perfectly representable answers. The two branches split the larger term
/// into an exact head/tail pair so the sum under the root is exact to more than
/// 53 bits.
#[allow(clippy::excessive_precision)]
pub fn hypot(x: f64, y: f64) -> f64 {
    let two_minus_600 = f64::from_bits(0x1A70_0000_0000_0000); // 2^-600
    let two_plus_600 = f64::from_bits(0x6570_0000_0000_0000); // 2^600

    let mut a = x.abs();
    let mut b = y.abs();
    if !a.is_finite() || !b.is_finite() {
        if a == f64::INFINITY || b == f64::INFINITY {
            return f64::INFINITY; // hypot(inf, NaN) is inf, not NaN
        }
        return a + b; // propagate NaN significand bits
    }
    if b > a {
        std::mem::swap(&mut a, &mut b);
    }
    let mut ha = hi(a);
    let mut hb = hi(b);
    if (ha - hb) > 0x3c0_0000 {
        return a + b; // a/b > 2^60: b is below a's last bit
    }
    let mut k = 0i32;
    if a > f64::from_bits(0x5F30_0000_FFFF_FFFF) {
        // a > ~2^500 — scale down to keep a*a finite
        ha -= 0x2580_0000;
        hb -= 0x2580_0000;
        a *= two_minus_600;
        b *= two_minus_600;
        k += 600;
    }
    if b < f64::from_bits(0x20B0_0000_0000_0000) {
        // b < 2^-500
        if b < f64::MIN_POSITIVE {
            // subnormal b, or zero
            if b == 0.0 {
                return a;
            }
            let t1 = f64::from_bits(0x7FD0_0000_0000_0000); // 2^1022
            b *= t1;
            a *= t1;
            k -= 1022;
        } else {
            ha += 0x2580_0000;
            hb += 0x2580_0000;
            a *= two_plus_600;
            b *= two_plus_600;
            k -= 600;
        }
    }

    let w;
    let t1;
    let t2;
    if a - b > b {
        t1 = with_hi(0.0, ha);
        t2 = a - t1;
        w = (t1 * t1 - (b * (-b) - t2 * (a + t1))).sqrt();
    } else {
        let a2 = a + a;
        let y1 = with_hi(0.0, hb);
        let y2 = b - y1;
        t1 = with_hi(0.0, ha + 0x0010_0000);
        t2 = a2 - t1;
        let w0 = a - b;
        w = (t1 * y1 - (w0 * (-w0) - (t1 * y2 + t2 * b))).sqrt();
    }
    if k != 0 {
        power_of_two_d(k) * w
    } else {
        w
    }
}

/// `StrictMath.pow(double, double)` — fdlibm `__ieee754_pow`.
///
/// The longest routine in fdlibm, and the one whose special cases are most
/// often got wrong. `x^y` is computed as `2^(y * log2(x))` with `log2(x)` and
/// the product both carried as unevaluated hi/lo pairs, because a single
/// rounding of `y*log2(x)` would cost several ULP in the result.
///
/// The `y_is_int` classification (0 = not an integer, 1 = odd, 2 = even) is
/// what decides the sign for negative bases, and it is computed from `|y|`
/// against `2^53` rather than by `fract()`, since above `2^53` every double is
/// an even integer.
// `-1.0 * z` is not `-z`: they differ in the sign of a produced NaN, and this
// site is reached with `z` already special-cased. Kept as the source writes it.
#[allow(
    clippy::excessive_precision,
    clippy::neg_multiply,
    clippy::assign_op_pattern
)]
pub fn pow(x: f64, y: f64) -> f64 {
    let mut z;
    let mut r;
    let mut s;
    let mut t;
    let mut u;
    let mut v;
    let mut w;
    let mut i: i32;
    let mut j: i32;
    let mut k: i32;
    let mut n: i32;

    if y == 0.0 {
        return 1.0; // x^0 = 1 for every x, NaN included
    }
    if x.is_nan() || y.is_nan() {
        return x + y;
    }
    let y_abs = y.abs();
    let mut x_abs = x.abs();

    if y == 2.0 {
        return x * x;
    } else if y == 0.5 {
        if x >= -f64::MAX {
            // handle x == -inf below; `+ 0.0` normalises -0.0
            return (x + 0.0).sqrt();
        }
    } else if y_abs == 1.0 {
        return if y == 1.0 { x } else { 1.0 / x };
    } else if y_abs == f64::INFINITY {
        if x_abs == 1.0 {
            return y - y; // 1^+-inf is NaN
        } else if x_abs > 1.0 {
            return if y >= 0.0 { y } else { 0.0 };
        } else {
            return if y < 0.0 { -y } else { 0.0 };
        }
    }

    let hx = hi(x);
    let mut ix = hx & EXP_SIGNIF_BITS_I;

    // Is y an integer, and if so is it odd?
    let mut y_is_int = 0i32;
    if hx < 0 {
        if y_abs >= f64::from_bits(0x4340_0000_0000_0000) {
            y_is_int = 2; // |y| >= 2^53, so ulp(y) = 2 and y is even
        } else if y_abs >= 1.0 {
            let y_abs_as_long = y_abs as i64;
            if (y_abs_as_long as f64) == y_abs {
                y_is_int = 2 - ((y_abs_as_long & 1) as i32);
            }
        }
    }

    if x_abs == 0.0 || x_abs == f64::INFINITY || x_abs == 1.0 {
        z = x_abs; // x is +-0, +-inf, +-1
        if y < 0.0 {
            z = 1.0 / z;
        }
        if hx < 0 {
            if ((ix - 0x3ff0_0000) | y_is_int) == 0 {
                z = (z - z) / (z - z); // (-1)^non-int is NaN
            } else if y_is_int == 1 {
                z = -1.0 * z; // (x<0)^odd = -(|x|^odd)
            }
        }
        return z;
    }

    n = (hx >> 31) + 1;
    if (n | y_is_int) == 0 {
        return (x - x) / (x - x); // (x<0)^(non-int) is NaN
    }
    s = 1.0; // sign of the result
    if (n | (y_is_int - 1)) == 0 {
        s = -1.0; // (-ve)^(odd int)
    }

    let t1;
    let t2;
    if y_abs > f64::from_bits(0x41E0_0000_FFFF_FFFF) {
        // |y| > ~2^31: only x extremely close to 1 can give a finite result,
        // so log(x) is computed by its leading term alone.
        let inv_ln2 = f64::from_bits(0x3FF7_1547_652B_82FE);
        let inv_ln2_h = f64::from_bits(0x3FF7_1547_6000_0000); // 24 bits of 1/ln2
        let inv_ln2_l = f64::from_bits(0x3E54_AE0B_F85D_DF44); // 1/ln2 tail
        if x_abs < f64::from_bits(0x3FEF_FFFF_0000_0000) {
            return if y < 0.0 { s * f64::INFINITY } else { s * 0.0 };
        }
        if x_abs > f64::from_bits(0x3FF0_0000_FFFF_FFFF) {
            return if y > 0.0 { s * f64::INFINITY } else { s * 0.0 };
        }
        t = x_abs - 1.0; // t has 20 trailing zeros
        w = (t * t) * (0.5 - t * (0.3333333333333333333333 - t * 0.25));
        u = inv_ln2_h * t; // inv_ln2_h has 21 significant bits
        v = t * inv_ln2_l - w * inv_ln2;
        let mut t1_ = u + v;
        t1_ = with_lo(t1_, 0);
        t1 = t1_;
        t2 = v - (t1 - u);
    } else {
        let cp = f64::from_bits(0x3FEE_C709_DC3A_03FD); // 2/(3ln2)
        let cp_h = f64::from_bits(0x3FEE_C709_E000_0000); // (float) cp
        let cp_l = f64::from_bits(0xBE3E_2FE0_145B_01F5); // tail of cp_h
        let bp = [1.0f64, 1.5f64];
        let dp_h = [0.0f64, f64::from_bits(0x3FE2_B803_4000_0000)];
        let dp_l = [0.0f64, f64::from_bits(0x3E4C_FDEB_43CF_D006)];
        let l1 = f64::from_bits(0x3FE3_3333_3333_3303);
        let l2 = f64::from_bits(0x3FDB_6DB6_DB6F_ABFF);
        let l3 = f64::from_bits(0x3FD5_5555_518F_264D);
        let l4 = f64::from_bits(0x3FD1_7460_A91D_4101);
        let l5 = f64::from_bits(0x3FCD_864A_93C9_DB65);
        let l6 = f64::from_bits(0x3FCA_7E28_4A45_4EEF);

        n = 0;
        if ix < 0x0010_0000 {
            x_abs *= f64::from_bits(0x4340_0000_0000_0000); // subnormal: * 2^53
            n -= 53;
            ix = hi(x_abs);
        }
        n += (ix >> 20) - 0x3ff;
        j = ix & 0x000f_ffff;
        ix = j | 0x3ff0_0000; // normalise ix to [1, 2)
        if j <= 0x3988E {
            k = 0; // |x| < sqrt(3/2)
        } else if j < 0xBB67A {
            k = 1; // |x| < sqrt(3)
        } else {
            k = 0;
            n += 1;
            ix -= 0x0010_0000;
        }
        x_abs = with_hi(x_abs, ix);

        u = x_abs - bp[k as usize];
        v = 1.0 / (x_abs + bp[k as usize]);
        let ss = u * v;
        let mut s_h = ss;
        s_h = with_lo(s_h, 0);
        let mut t_h = with_hi(0.0, ((ix >> 1) | 0x2000_0000) + 0x0008_0000 + (k << 18));
        let mut t_l = x_abs - (t_h - bp[k as usize]);
        let s_l = v * ((u - s_h * t_h) - s_h * t_l);
        let mut s2 = ss * ss;
        r = s2 * s2 * (l1 + s2 * (l2 + s2 * (l3 + s2 * (l4 + s2 * (l5 + s2 * l6)))));
        r += s_l * (s_h + ss);
        s2 = s_h * s_h;
        t_h = 3.0 + s2 + r;
        t_h = with_lo(t_h, 0);
        t_l = r - ((t_h - 3.0) - s2);
        u = s_h * t_h;
        v = s_l * t_h + t_l * ss;
        let mut p_h_ = u + v;
        p_h_ = with_lo(p_h_, 0);
        let p_l = v - (p_h_ - u);
        let z_h = cp_h * p_h_;
        let z_l = cp_l * p_h_ + p_l * cp + dp_l[k as usize];
        t = n as f64;
        let mut t1_ = ((z_h + z_l) + dp_h[k as usize]) + t;
        t1_ = with_lo(t1_, 0);
        t1 = t1_;
        t2 = z_l - (((t1 - t) - dp_h[k as usize]) - z_h);
    }

    // p_h + p_l = y * log2(x), split so the product is carried to > 53 bits.
    // (The `p_l` computed inside the branch above was scratch for `z_l`; this
    // assignment is the one the rest of the routine reads.)
    let y1 = with_lo(y, 0);
    let p_l = (y - y1) * t1 + y * t2;
    let p_h = y1 * t1;
    z = p_l + p_h;
    j = hi(z);
    i = lo(z);
    if j >= 0x4090_0000 {
        // z >= 1024
        if ((j - 0x4090_0000) | i) != 0 {
            return s * f64::INFINITY; // overflow
        }
        let ovt = 8.0085662595372944372e-17; // -(1024 - log2(ovfl + .5ulp))
        if p_l + ovt > z - p_h {
            return s * f64::INFINITY;
        }
    } else if (j & EXP_SIGNIF_BITS_I) >= 0x4090_cc00 {
        // z <= -1075
        if (j.wrapping_sub(0xc090_cc00u32 as i32) | i) != 0 {
            return s * 0.0; // underflow
        }
        if p_l <= z - p_h {
            return s * 0.0;
        }
    }

    // Now compute 2^(p_h + p_l).
    let p1 = f64::from_bits(0x3FC5_5555_5555_553E);
    let p2 = f64::from_bits(0xBF66_C16C_16BE_BD93);
    let p3 = f64::from_bits(0x3F11_566A_AF25_DE2C);
    let p4 = f64::from_bits(0xBEBB_BD41_C5D2_6BF1);
    let p5 = f64::from_bits(0x3E66_3769_72BE_A4D0);
    let lg2 = f64::from_bits(0x3FE6_2E42_FEFA_39EF);
    let lg2_h = f64::from_bits(0x3FE6_2E43_0000_0000);
    let lg2_l = f64::from_bits(0xBE20_5C61_0CA8_6C39);

    i = j & EXP_SIGNIF_BITS_I;
    k = (i >> 20) - 0x3ff;
    n = 0;
    let mut p_h = p_h;
    if i > 0x3fe0_0000 {
        // |z| > 0.5: set n = round(z), leaving |p_h| <= 0.5
        n = j + (0x0010_0000 >> (k + 1));
        k = ((n & EXP_SIGNIF_BITS_I) >> 20) - 0x3ff;
        t = with_hi(0.0, n & !(0x000f_ffff >> k));
        n = ((n & 0x000f_ffff) | 0x0010_0000) >> (20 - k);
        if j < 0 {
            n = -n;
        }
        p_h -= t;
    }
    t = p_l + p_h;
    t = with_lo(t, 0);
    u = t * lg2_h;
    v = (p_l - (t - p_h)) * lg2 + t * lg2_l;
    z = u + v;
    w = v - (z - u);
    t = z * z;
    let t1 = z - t * (p1 + t * (p2 + t * (p3 + t * (p4 + t * p5))));
    r = (z * t1) / (t1 - 2.0) - (w + z * w);
    z = 1.0 - (r - z);
    j = hi(z);
    j = j.wrapping_add(n << 20);
    if (j >> 20) <= 0 {
        z = scalb(z, n); // subnormal output
    } else {
        z = with_hi(z, hi(z).wrapping_add(n << 20));
    }
    s * z
}

/// `StrictMath.exp(double)` — fdlibm `__ieee754_exp`.
///
/// `exp(x) = 2^k * exp(r)` with `|r| <= 0.5*ln2`, and `exp(r)` from a degree-5
/// rational in `r*r`. The `k` is applied by adding to the exponent field, not
/// by a multiply, which is what keeps the subnormal tail exact.
#[allow(clippy::excessive_precision)]
pub fn exp(mut x: f64) -> f64 {
    let half = [0.5f64, -0.5f64];
    let huge = 1.0e300f64;
    let twom1000 = f64::from_bits(0x0170_0000_0000_0000); // 2^-1000
    let o_threshold = f64::from_bits(0x4086_2E42_FEFA_39EF); // 709.782712893384
    let u_threshold = f64::from_bits(0xC087_4910_D52D_3051); // -745.1332191019411
    let ln2hi = [
        f64::from_bits(0x3FE6_2E42_FEE0_0000),
        f64::from_bits(0xBFE6_2E42_FEE0_0000),
    ];
    let ln2lo = [
        f64::from_bits(0x3DEA_39EF_3579_3C76),
        f64::from_bits(0xBDEA_39EF_3579_3C76),
    ];
    let invln2 = f64::from_bits(0x3FF7_1547_652B_82FE);
    let p1 = f64::from_bits(0x3FC5_5555_5555_553E);
    let p2 = f64::from_bits(0xBF66_C16C_16BE_BD93);
    let p3 = f64::from_bits(0x3F11_566A_AF25_DE2C);
    let p4 = f64::from_bits(0xBEBB_BD41_C5D2_6BF1);
    let p5 = f64::from_bits(0x3E66_3769_72BE_A4D0);

    let mut hx = hi(x);
    let xsb = ((hx >> 31) & 1) as usize; // sign bit of x
    hx &= EXP_SIGNIF_BITS_I; // high word of |x|

    if hx >= 0x4086_2E42 {
        // |x| >= 709.78...
        if hx >= 0x7ff0_0000 {
            if ((hx & 0xf_ffff) | lo(x)) != 0 {
                return x + x; // NaN
            }
            return if xsb == 0 { x } else { 0.0 }; // exp(+-inf) = {inf, 0}
        }
        if x > o_threshold {
            return huge * huge; // overflow
        }
        if x < u_threshold {
            return twom1000 * twom1000; // underflow
        }
    }

    // The Java declares `hi`, `lo` and `k` with initializers and reassigns them
    // in the branches; a `let` binding of the tuple is the same computation with
    // no dead store, which the workspace's `-D warnings` would otherwise reject.
    let (hi_, lo_, k) = if hx > 0x3fd6_2e42 {
        // |x| > 0.5*ln2
        if hx < 0x3FF0_A2B2 {
            // and |x| < 1.5*ln2
            let hi_ = x - ln2hi[xsb];
            let lo_ = ln2lo[xsb];
            x = hi_ - lo_;
            (hi_, lo_, 1 - xsb as i32 - xsb as i32)
        } else {
            let k = (invln2 * x + half[xsb]) as i32;
            let t = k as f64;
            let hi_ = x - t * ln2hi[0]; // t*ln2hi is exact here
            let lo_ = t * ln2lo[0];
            x = hi_ - lo_;
            (hi_, lo_, k)
        }
    } else if hx < 0x3e30_0000 {
        // |x| < 2^-28
        if huge + x > 1.0 {
            return 1.0 + x; // trigger inexact
        }
        (0.0, 0.0, 0)
    } else {
        (0.0, 0.0, 0)
    };

    let t = x * x;
    let c = x - t * (p1 + t * (p2 + t * (p3 + t * (p4 + t * p5))));
    if k == 0 {
        return 1.0 - ((x * c) / (c - 2.0) - x);
    }
    let mut y = 1.0 - ((lo_ - (x * c) / (2.0 - c)) - hi_);
    if k >= -1021 {
        y = with_hi(y, hi(y).wrapping_add(k << 20));
        y
    } else {
        // Scale in two steps so the intermediate does not underflow to zero.
        y = with_hi(y, hi(y).wrapping_add((k + 1000) << 20));
        y * twom1000
    }
}

/// `StrictMath.log10(double)` — fdlibm `__ieee754_log10`.
///
/// `log10(x) = k*log10(2) + log10(1+f)` with `log10(1+f)` from `log(1+f)/ln10`.
/// It calls `log` in this module, not `f64::ln` — routing it to libm would
/// reintroduce the 7.3% deviation one level down.
#[allow(clippy::excessive_precision)]
pub fn log10(mut x: f64) -> f64 {
    let ivln10 = f64::from_bits(0x3FDB_CB7B_1526_E50E); // 0x1.bcb7b1526e50ep-2
    let log10_2hi = f64::from_bits(0x3FD3_4413_509F_6000); // 0x1.34413509f6p-2
    let log10_2lo = f64::from_bits(0x3D59_FEF3_11F1_2B36); // 0x1.9fef311f12b36p-42
    let two54 = f64::from_bits(0x4350_0000_0000_0000);

    let mut hx = hi(x);
    let lx = lo(x);
    let mut k = 0i32;
    if hx < 0x0010_0000 {
        // x < 2^-1022
        if ((hx & EXP_SIGNIF_BITS_I) | lx) == 0 {
            return -two54 / 0.0; // log10(+-0) = -inf
        }
        if hx < 0 {
            return (x - x) / 0.0; // log10(-#) = NaN
        }
        k -= 54;
        x *= two54; // subnormal: scale up
        hx = hi(x);
    }
    if hx >= EXP_BITS_I {
        return x + x; // +inf -> +inf, NaN -> NaN
    }
    k += (hx >> 20) - 1023;
    let i = ((k & SIGN_BIT_I) as u32 >> 31) as i32;
    hx = (hx & 0x000f_ffff) | ((0x3ff - i) << 20);
    let y = (k + i) as f64;
    x = with_hi(x, hx);
    let z = y * log10_2lo + ivln10 * log(x);
    z + y * log10_2hi
}

/// `StrictMath.log1p(double)` — fdlibm `__log1p`.
///
/// Exists because `log(1+x)` loses every significant bit for small `x`: the
/// `1+x` rounds away the information the answer is made of. The `c` correction
/// term recovers what `u = 1+x` dropped.
#[allow(clippy::excessive_precision)]
pub fn log1p(x: f64) -> f64 {
    let ln2_hi = f64::from_bits(0x3FE6_2E42_FEE0_0000);
    let ln2_lo = f64::from_bits(0x3DEA_39EF_3579_3C76);
    let lp1 = f64::from_bits(0x3FE5_5555_5555_5593);
    let lp2 = f64::from_bits(0x3FD9_9999_9997_FA04);
    let lp3 = f64::from_bits(0x3FD2_4924_9422_9359);
    let lp4 = f64::from_bits(0x3FCC_71C5_1D8E_78AF);
    let lp5 = f64::from_bits(0x3FC7_4664_96CB_03DE);
    let lp6 = f64::from_bits(0x3FC3_9A09_D078_C69F);
    let lp7 = f64::from_bits(0x3FC2_F112_DF3E_5244);
    let two54 = f64::from_bits(0x4350_0000_0000_0000);

    let mut f = 0.0f64;
    let mut c = 0.0f64;
    let mut hu = 0i32;
    let hx = hi(x);
    let ax = hx & EXP_SIGNIF_BITS_I;
    let mut k = 1i32;

    if hx < 0x3FDA_827A {
        // x < 0.41422
        if ax >= 0x3ff0_0000 {
            // x <= -1.0
            if x == -1.0 {
                return f64::NEG_INFINITY;
            }
            return f64::NAN; // log1p(x < -1) = NaN
        }
        if ax < 0x3e20_0000 {
            // |x| < 2^-29
            if two54 + x > 0.0 && ax < 0x3c90_0000 {
                // |x| < 2^-54 — raise inexact and return x
                return x;
            }
            return x - x * x * 0.5;
        }
        // NOTE: `0xbfd2_bec3` is a NEGATIVE Java int (-1076626237) and this is
        // a SIGNED comparison. Read as unsigned it would take the wrong branch
        // for every x in (-0.2929, 0).
        if hx > 0 || hx <= 0xbfd2_bec3u32 as i32 {
            // -0.2929 < x < 0.41422: no reduction, f = x
            k = 0;
            f = x;
            hu = 1;
        }
    }
    if hx >= EXP_BITS_I {
        return x + x;
    }
    let mut u;
    if k != 0 {
        if hx < 0x4340_0000 {
            u = 1.0 + x;
            hu = hi(u);
            k = (hu >> 20) - 1023;
            // The correction term: exactly what `1 + x` rounded away.
            c = if k > 0 { 1.0 - (u - x) } else { x - (u - 1.0) };
            c /= u;
        } else {
            u = x;
            hu = hi(u);
            k = (hu >> 20) - 1023;
            c = 0.0;
        }
        hu &= 0x000f_ffff;
        if hu < 0x6_a09e {
            u = with_hi(u, hu | 0x3ff0_0000); // normalise u
        } else {
            k += 1;
            u = with_hi(u, hu | 0x3fe0_0000); // normalise u/2
            hu = (0x0010_0000 - hu) >> 2;
        }
        f = u - 1.0;
    }

    let hfsq = 0.5 * f * f;
    if hu == 0 {
        // |f| < 2^-20
        if f == 0.0 {
            if k == 0 {
                return 0.0;
            }
            c += (k as f64) * ln2_lo;
            return (k as f64) * ln2_hi + c;
        }
        let r = hfsq * (1.0 - 0.66666666666666666 * f);
        if k == 0 {
            return f - r;
        }
        return (k as f64) * ln2_hi - ((r - ((k as f64) * ln2_lo + c)) - f);
    }
    let s = f / (2.0 + f);
    let z = s * s;
    let r = z * (lp1 + z * (lp2 + z * (lp3 + z * (lp4 + z * (lp5 + z * (lp6 + z * lp7))))));
    if k == 0 {
        f - (hfsq - s * (hfsq + r))
    } else {
        (k as f64) * ln2_hi - ((hfsq - (s * (hfsq + r) + ((k as f64) * ln2_lo + c))) - f)
    }
}

/// `StrictMath.expm1(double)` — fdlibm `__expm1`.
///
/// The dual of `log1p`: `exp(x) - 1` computed without ever forming `exp(x)`,
/// which for small `x` would round to exactly 1 and give an answer of 0.
#[allow(clippy::excessive_precision)]
pub fn expm1(mut x: f64) -> f64 {
    let huge = 1.0e300f64;
    let tiny = 1.0e-300f64;
    let o_threshold = f64::from_bits(0x4086_2E42_FEFA_39EF); // 709.782712893384
    let ln2_hi = f64::from_bits(0x3FE6_2E42_FEE0_0000);
    let ln2_lo = f64::from_bits(0x3DEA_39EF_3579_3C76);
    let invln2 = f64::from_bits(0x3FF7_1547_652B_82FE);
    let q1 = f64::from_bits(0xBFA1_1111_1111_10F4);
    let q2 = f64::from_bits(0x3F5A_01A0_19FE_5585);
    let q3 = f64::from_bits(0xBF14_CE19_9EAA_DBB7);
    let q4 = f64::from_bits(0x3ED0_CFCA_86E6_5239);
    let q5 = f64::from_bits(0xBE8A_FDB7_6E09_C32D);

    let mut hx = hi(x);
    let xsb = hx & SIGN_BIT_I; // sign bit of x
    hx &= EXP_SIGNIF_BITS_I; // high word of |x|

    if hx >= 0x4043_687A {
        // |x| >= 56*ln2
        if hx >= 0x4086_2E42 {
            // |x| >= 709.78...
            if hx >= 0x7ff0_0000 {
                if ((hx & 0xf_ffff) | lo(x)) != 0 {
                    return x + x; // NaN
                }
                return if xsb == 0 { x } else { -1.0 }; // expm1(+-inf) = {inf, -1}
            }
            if x > o_threshold {
                return huge * huge; // overflow
            }
        }
        if xsb != 0 {
            // x < -56*ln2, so exp(x) is below ulp(1): return -1 with inexact
            if x + tiny < 0.0 {
                return tiny - 1.0;
            }
        }
    }

    // As in `exp`: the Java's declared-then-reassigned `hi`/`lo`/`c`/`k` become
    // one `let` binding, which is the same computation without the dead store.
    let (c, k) = if hx > 0x3fd6_2e42 {
        // |x| > 0.5*ln2
        let (hi_, lo_, k) = if hx < 0x3FF0_A2B2 {
            // and |x| < 1.5*ln2
            if xsb == 0 {
                (x - ln2_hi, ln2_lo, 1)
            } else {
                (x + ln2_hi, -ln2_lo, -1)
            }
        } else {
            let k = (invln2 * x + if xsb == 0 { 0.5 } else { -0.5 }) as i32;
            let t = k as f64;
            (x - t * ln2_hi, t * ln2_lo, k) // t*ln2_hi is exact here
        };
        x = hi_ - lo_;
        // `c` is what the `hi - lo` subtraction rounded away; the k != 0 tail
        // below needs it to stay exact.
        ((hi_ - x) - lo_, k)
    } else if hx < 0x3c90_0000 {
        // |x| < 2^-54: expm1(x) = x
        let t = huge + x; // raise inexact when x != 0
        return x - (t - (huge + x));
    } else {
        (0.0, 0)
    };

    let hfx = 0.5 * x;
    let hxs = x * hfx;
    let r1 = 1.0 + hxs * (q1 + hxs * (q2 + hxs * (q3 + hxs * (q4 + hxs * q5))));
    let t = 3.0 - r1 * hfx;
    let mut e = hxs * ((r1 - t) / (6.0 - x * t));
    if k == 0 {
        return x - (x * e - hxs); // c is 0
    }
    e = x * (e - c) - c;
    e -= hxs;
    if k == -1 {
        return 0.5 * (x - e) - 0.5;
    }
    if k == 1 {
        return if x < -0.25 {
            -2.0 * (e - (x + 0.5))
        } else {
            1.0 + 2.0 * (x - e)
        };
    }
    let mut y;
    if k <= -2 || k > 56 {
        // suffices to return exp(x) - 1
        y = 1.0 - (e - x);
        y = with_hi(y, hi(y).wrapping_add(k << 20));
        return y - 1.0;
    }
    let mut t = 1.0f64;
    if k < 20 {
        t = with_hi(t, 0x3ff0_0000 - (0x20_0000 >> k)); // t = 1 - 2^-k
        y = t - (e - x);
        y = with_hi(y, hi(y).wrapping_add(k << 20));
    } else {
        t = with_hi(t, (0x3ff - k) << 20); // t = 2^-k
        y = x - (e + t);
        y += 1.0;
        y = with_hi(y, hi(y).wrapping_add(k << 20));
    }
    y
}

// ---------------------------------------------------------------------------
// Hyperbolics. All three are defined in terms of `expm1`/`exp` above, exactly
// as the Java defines them in terms of `StrictMath.expm1`/`StrictMath.exp`.
// ---------------------------------------------------------------------------

/// `StrictMath.sinh(double)` — fdlibm `__ieee754_sinh`.
pub fn sinh(x: f64) -> f64 {
    let shuge = 1.0e307f64;
    let jx = hi(x);
    let ix = jx & EXP_SIGNIF_BITS_I;
    if ix >= EXP_BITS_I {
        return x + x; // sinh(+-inf) = +-inf, sinh(NaN) = NaN
    }
    let h = if jx < 0 { -0.5 } else { 0.5 };
    if ix < 0x4036_0000 {
        // |x| < 22
        if ix < 0x3e30_0000 {
            // |x| < 2^-28
            if shuge + x > 1.0 {
                return x; // sinh(tiny) = tiny with inexact
            }
        }
        let t = expm1(x.abs());
        if ix < 0x3ff0_0000 {
            return h * (2.0 * t - t * t / (t + 1.0));
        }
        return h * (t + t / (t + 1.0));
    }
    if ix < 0x4086_2E42 {
        // |x| < log(DBL_MAX)
        return h * exp(x.abs());
    }
    // |x| in [log(DBL_MAX), overflow threshold]: split the exp so it fits
    let lx = lo(x);
    // NOTE: the JDK writes `Long.compareUnsigned` here and `Integer.
    // compareUnsigned` in the otherwise identical `Cosh` guard. Both operands
    // sign-extend to long, so the two are NOT the same predicate. Reproduced as
    // written rather than unified — this port's job is to match the reference,
    // not to improve it.
    let lhs = ((lx as i64) as u64) <= (((0x8fb9_f87du32 as i32) as i64) as u64);
    if ix < 0x4086_33CE || (ix == 0x4086_33ce && lhs) {
        let w = exp(0.5 * x.abs());
        let t = h * w;
        return t * w;
    }
    x * shuge // overflow
}

/// `StrictMath.cosh(double)` — fdlibm `__ieee754_cosh`.
pub fn cosh(x: f64) -> f64 {
    let huge = 1.0e300f64;
    let ix = hi(x) & EXP_SIGNIF_BITS_I;
    if ix >= EXP_BITS_I {
        return x * x; // cosh(+-inf) = +inf, cosh(NaN) = NaN
    }
    if ix < 0x3fd6_2e43 {
        // |x| < 0.5*ln2
        let t = expm1(x.abs());
        let w = 1.0 + t;
        if ix < 0x3c80_0000 {
            return w; // cosh(tiny) = 1
        }
        return 1.0 + (t * t) / (w + w);
    }
    if ix < 0x4036_0000 {
        // |x| < 22
        let t = exp(x.abs());
        return 0.5 * t + 0.5 / t;
    }
    if ix < 0x4086_2E42 {
        // |x| < log(DBL_MAX)
        return 0.5 * exp(x.abs());
    }
    let lx = lo(x);
    // See the note in `sinh`: this guard uses the 32-bit unsigned compare.
    if ix < 0x4086_33CE || (ix == 0x4086_33ce && (lx as u32) <= 0x8fb9_f87d) {
        let w = exp(0.5 * x.abs());
        let t = 0.5 * w;
        return t * w;
    }
    huge * huge // overflow
}

/// `StrictMath.tanh(double)` — fdlibm `__ieee754_tanh`.
pub fn tanh(x: f64) -> f64 {
    let tiny = 1.0e-300f64;
    let jx = hi(x);
    let ix = jx & EXP_SIGNIF_BITS_I;
    if ix >= EXP_BITS_I {
        // tanh(+-inf) = +-1, tanh(NaN) = NaN. Written as `1/x +- 1` so the
        // signed zero of `1/-inf` carries the sign.
        return if jx >= 0 {
            1.0 / x + 1.0
        } else {
            1.0 / x - 1.0
        };
    }
    let z;
    if ix < 0x4036_0000 {
        // |x| < 22
        if ix < 0x3c80_0000 {
            return x * (1.0 + x); // |x| < 2^-55: tanh(small) = small
        }
        if ix >= 0x3ff0_0000 {
            // |x| >= 1
            let t = expm1(2.0 * x.abs());
            z = 1.0 - 2.0 / (t + 2.0);
        } else {
            let t = expm1(-2.0 * x.abs());
            z = -t / (t + 2.0);
        }
    } else {
        z = 1.0 - tiny; // |x| > 22: +-1 with inexact raised
    }
    if jx >= 0 {
        z
    } else {
        -z
    }
}

// ---------------------------------------------------------------------------
// IEEE remainder.
// ---------------------------------------------------------------------------

/// `signedZero(sign)` — `+0.0 * (double) sign`, i.e. `-0.0` when `sign` is the
/// sign bit pattern and `+0.0` when it is zero.
#[inline]
fn signed_zero(sign: i32) -> f64 {
    0.0 * (sign as f64)
}

/// `FdLibm.IEEEremainder.ilogb(hz, lz)` — the unbiased exponent, subnormals
/// included.
fn ilogb(hz: i32, lz: i32) -> i32 {
    let mut iz;
    if hz < 0x0010_0000 {
        // subnormal z
        if hz == 0 {
            iz = -1043;
            let mut i = lz;
            while i > 0 {
                iz -= 1;
                i = i.wrapping_shl(1);
            }
        } else {
            iz = -1022;
            let mut i = hz.wrapping_shl(11);
            while i > 0 {
                iz -= 1;
                i = i.wrapping_shl(1);
            }
        }
    } else {
        iz = (hz >> 20) - 1023;
    }
    iz
}

/// `FdLibm.IEEEremainder.__ieee754_fmod` — the C `fmod`, by shift-and-subtract
/// on the significands. Bit-exact and exact-valued: `fmod` has no rounding.
fn fmod(x: f64, y: f64) -> f64 {
    let mut hx = hi(x);
    let mut lx = lo(x);
    let mut hy = hi(y);
    let mut ly = lo(y);
    let sx = hx & SIGN_BIT_I; // sign of x
    hx ^= sx; // |x|
    hy &= EXP_SIGNIF_BITS_I; // |y|

    // y = 0, or x not finite, or y is NaN
    if (hy | ly) == 0
        || hx >= EXP_BITS_I
        || (hy | ((ly | ly.wrapping_neg()) as u32 >> 31) as i32) > EXP_BITS_I
    {
        return (x * y) / (x * y);
    }
    if hx <= hy {
        if hx < hy || (lx as u32) < (ly as u32) {
            return x; // |x| < |y|
        }
        if lx == ly {
            return signed_zero(sx); // |x| == |y|
        }
    }

    let ix = ilogb(hx, lx);
    let mut iy = ilogb(hy, ly);

    // Set up {hx, lx} and {hy, ly} as fixed-point significands.
    if ix >= -1022 {
        hx = 0x0010_0000 | (0x000f_ffff & hx);
    } else {
        let n = -1022 - ix;
        if n <= 31 {
            hx = hx.wrapping_shl(n as u32) | ((lx as u32) >> (32 - n)) as i32;
            lx = lx.wrapping_shl(n as u32);
        } else {
            hx = lx.wrapping_shl((n - 32) as u32);
            lx = 0;
        }
    }
    if iy >= -1022 {
        hy = 0x0010_0000 | (0x000f_ffff & hy);
    } else {
        let n = -1022 - iy;
        if n <= 31 {
            hy = hy.wrapping_shl(n as u32) | ((ly as u32) >> (32 - n)) as i32;
            ly = ly.wrapping_shl(n as u32);
        } else {
            hy = ly.wrapping_shl((n - 32) as u32);
            ly = 0;
        }
    }

    // Fixed-point fmod: one conditional subtract per bit of exponent gap.
    let mut n = ix - iy;
    while n != 0 {
        n -= 1;
        let mut hz = hx.wrapping_sub(hy);
        let lz = lx.wrapping_sub(ly);
        if (lx as u32) < (ly as u32) {
            hz -= 1;
        }
        if hz < 0 {
            hx = hx.wrapping_add(hx).wrapping_add(((lx as u32) >> 31) as i32);
            lx = lx.wrapping_add(lx);
        } else {
            if (hz | lz) == 0 {
                return signed_zero(sx);
            }
            hx = hz.wrapping_add(hz).wrapping_add(((lz as u32) >> 31) as i32);
            lx = lz.wrapping_add(lz);
        }
    }
    let mut hz = hx.wrapping_sub(hy);
    let lz = lx.wrapping_sub(ly);
    if (lx as u32) < (ly as u32) {
        hz -= 1;
    }
    if hz >= 0 {
        hx = hz;
        lx = lz;
    }
    if (hx | lx) == 0 {
        return signed_zero(sx);
    }
    while hx < 0x0010_0000 {
        // normalise x
        hx = hx.wrapping_add(hx).wrapping_add(((lx as u32) >> 31) as i32);
        lx = lx.wrapping_add(lx);
        iy -= 1;
    }
    if iy >= -1022 {
        hx = (hx - 0x0010_0000) | ((iy + 1023) << 20);
        hi_lo(hx | sx, lx)
    } else {
        // subnormal output
        let n = -1022 - iy;
        if n <= 20 {
            lx = (((lx as u32) >> n) as i32) | hx.wrapping_shl((32 - n) as u32);
            hx >>= n;
        } else if n <= 31 {
            lx = hx.wrapping_shl((32 - n) as u32) | (((lx as u32) >> n) as i32);
            hx = sx;
        } else {
            lx = hx.wrapping_shr((n - 32) as u32);
            hx = sx;
        }
        let x = hi_lo(hx | sx, lx);
        x * 1.0 // create the necessary signal
    }
}

/// `StrictMath.IEEEremainder(double, double)` — fdlibm `__ieee754_remainder`.
///
/// `x - n*p` where `n` is the integer NEAREST `x/p`, **ties to even**. Not
/// `x - round(x/p)*p`: `round` is ties-away, `x/p` can overflow to infinity for
/// perfectly ordinary operands, and the two-step reduction below is what keeps
/// the result exact.
pub fn ieee_remainder(mut x: f64, mut p: f64) -> f64 {
    let hx = hi(x);
    let lx = lo(x);
    let hp = hi(p);
    let lp = lo(p);
    let sx = hx & SIGN_BIT_I;
    let hp = hp & EXP_SIGNIF_BITS_I;
    let hx = hx & EXP_SIGNIF_BITS_I;

    if (hp | lp) == 0 {
        return (x * p) / (x * p); // p = 0
    }
    if hx >= EXP_BITS_I || (hp >= EXP_BITS_I && ((hp - EXP_BITS_I) | lp) != 0) {
        // x not finite, or p is NaN
        return (x * p) / (x * p);
    }
    if hp <= 0x7fdf_ffff {
        x = fmod(x, p + p); // now x < 2p
    }
    // `|x| == |p|` — again the fdlibm spelling, OR of the word differences.
    //
    // `hx`/`hp` are both masked to `EXP_SIGNIF_BITS_I` above so their
    // difference cannot overflow, but `lx`/`lp` are whole low words: `lo()`
    // reinterprets them as `i32` (the C declares them `uint32_t`), so `lx - lp`
    // overflows whenever the two straddle the `i32` range — a mantissa with its
    // top bit set against one without. `wrapping_sub` is the C's unsigned
    // subtraction bit for bit, and as above only zero-ness is read.
    if (hx.wrapping_sub(hp) | lx.wrapping_sub(lp)) == 0 {
        return 0.0 * x;
    }
    x = x.abs();
    p = p.abs();
    if hp < 0x0020_0000 {
        if x + x > p {
            x -= p;
            if x + x >= p {
                x -= p;
            }
        }
    } else {
        let p_half = 0.5 * p;
        if x > p_half {
            x -= p;
            if x >= p_half {
                x -= p;
            }
        }
    }
    with_hi(x, hi(x) ^ sx)
}

#[cfg(test)]
mod tests {
    use super::log;

    /// `(input bits, StrictMath.log(input) bits)` captured from Temurin
    /// jdk-25.0.3+9 with `probes/StrictMathLogVectorProbe.java`. Regenerate
    /// with that probe rather than by hand — the point of the table is that it
    /// is the reference implementation's output, not ours.
    ///
    /// Coverage: the value `Random(42).nextGaussian()` actually reduces to, the
    /// exact powers of two, both sides of 1.0, `sqrt(2)/2` and `sqrt(2)` (the
    /// argument-reduction boundary), `MIN_VALUE` / `MIN_NORMAL` / `MAX_VALUE`
    /// (the subnormal rescaling path), +-0, negative, +-inf, NaN, and 60
    /// pseudorandom draws spanning the whole exponent range.
    const VECTORS: &[(u64, u64)] = &[
        (0x3FD5D9E5352FEE22, 0xBFF131AE8F1BF126),
        (0x3FF0000000000000, 0x0000000000000000),
        (0x4000000000000000, 0x3FE62E42FEFA39EF),
        (0x3FE0000000000000, 0xBFE62E42FEFA39EF),
        (0x4005BF0A8B145769, 0x3FF0000000000000),
        (0x4024000000000000, 0x40026BB1BBB55516),
        (0x3FB999999999999A, 0xC0026BB1BBB55515),
        (0x01A56E1FC2F8F359, 0xC085963447F87FB5),
        (0x7E37E43C8800759C, 0x4085963447F87FB5),
        (0x0000000000000001, 0xC0874385446D71C3),
        (0x0010000000000000, 0xC086232BDD7ABCD2),
        (0x7FEFFFFFFFFFFFFF, 0x40862E42FEFA39EF),
        (0x3FEFFFFFFFFFFFFF, 0xBCA0000000000000),
        (0x3FF0000000000001, 0x3CAFFFFFFFFFFFFF),
        (0x3FFFFFFFFFFFFFFF, 0x3FE62E42FEFA39EE),
        (0x3FF6A09E667F3BCD, 0x3FD62E42FEFA39F0),
        (0x3FE6A09E667F3BCD, 0xBFD62E42FEFA39EE),
        (0x4008000000000000, 0x3FF193EA7AAD030A),
        (0x4059000000000000, 0x40126BB1BBB55516),
        (0x3DDB7CDFD9D7BDBB, 0xC037069E2AA2AA5B),
        (0x4202A05F20000000, 0x4037069E2AA2AA5B),
        (0x0000000000000000, 0xFFF0000000000000),
        (0x8000000000000000, 0xFFF0000000000000),
        (0xBFF0000000000000, 0xFFF8000000000000),
        (0x7FF8000000000000, 0x7FF8000000000000),
        (0x7FF0000000000000, 0x7FF0000000000000),
        (0xFFF0000000000000, 0xFFF8000000000000),
        (0x000FFFFFFFFFFFFF, 0xC086232BDD7ABCD2),
        (0x3FD11AC481A55B66, 0xBFF51CD3CECDB156),
        (0x3FD70FEFB3EE83EC, 0xBFF054BAC8E12D42),
        (0x3FA41018C98403B0, 0xC009EA668791B61A),
        (0x3FA2FEE963C42D10, 0xC00A5A560A4C24DF),
        (0x3FE77E53F0E540DE, 0xBFD3C6E067C676A8),
        (0x3FE207D8D7674F18, 0xBFE25B71D4B7174B),
        (0x3FE9F033AC9C5072, 0xBFCAE1CB6DFFB491),
        (0x3FCB6D6F2F515F18, 0xBFF8A5D382499D01),
        (0x3FEB0659F4EC09AD, 0xBFC5A127B825148E),
        (0x3FCF7D7BAEA5343C, 0xBFF6700BAB34CBDE),
        (0x3FEF9EEFE748CCB0, 0xBF88691E777A494B),
        (0x3FEC15495A59FA19, 0xBFC0B66418C50A10),
        (0x3FDDA152E525AB0E, 0xBFE8A493ABC57B47),
        (0x3FE7706122046534, 0xBFD3ECEB29191F0A),
        (0x3FDF4D17D5F0269A, 0xBFE6E326AEE11024),
        (0x3FE4C8A0BD153340, 0xBFDB9ECA2394F9A1),
        (0x3FEFC007993EEF7C, 0xBF800E2B51BF9E78),
        (0x3FBDB8F8A8C6B710, 0xC00139E678C77CD4),
        (0x3FEF7A159D268F25, 0xBF90E0B5FAD885F1),
        (0x3FD19ADE9BBC7C34, 0xBFF4A6B79BBFBEF3),
        (0x3FDC212477C8232E, 0xBFEA4E5C6FB0B54E),
        (0x3FDFF647A04875D2, 0xBFE637FCD8EC706F),
        (0x3FE312145594EA27, 0xBFE090123F7ABED1),
        (0x3FE1AAEA239E0DA3, 0xBFE3020F5872EF37),
        (0x3FEC140A3C7EACB1, 0xBFC0BC12B8ECFE09),
        (0x3FEBF37B12774182, 0xBFC150D66AA4F0D8),
        (0x3FE7D8ECAE7E7E6D, 0xBFD2D1EAC5881AA6),
        (0x3FCDBF916235D8B0, 0xBFF7591EC7699771),
        (0x3FED96E04717D6C8, 0xBFB40CD999C4754B),
        (0x3FECB67832E1BAC0, 0xBFBBBFB37861796E),
        (0x3FB651303DEAAE70, 0xC00384C193F084BC),
        (0x3FD47BEDD6E99218, 0xBFF23A4DA6DCC35E),
        (0x3FE7844354712FD2, 0xBFD3B6B762C4544B),
        (0x3FE5954F5EA3B98B, 0xBFD9345360CDBD90),
        (0x3FEC7ED20E8F07F2, 0xBFBDB1BFCC854138),
        (0x3FC5AE6B3E5ECB4C, 0xBFFC68C58CCBC0F6),
        (0x3FBF3B895F886880, 0xC000D46921A3FF4F),
        (0x3FEFCBC06C7CA10B, 0xBF7A3534DCB3469A),
        (0x3FD5D2AC918F8DD2, 0xBFF136F90E7CC974),
        (0x3FE9697522A32C03, 0xBFCD81918E1D57C7),
        (0x5F8447E424275C6C, 0x4075E46C305E18E1),
        (0x65A0A1B4175D9488, 0x407A201A7F2669BA),
        (0x3446BE21135D4742, 0xC060288A2DF40E49),
        (0x56E8A93EDBA987BA, 0x406FDA2A3949F84D),
        (0x0148FFBC6194A3F0, 0xC085B63E35EA17BD),
        (0x5FAD5B4357304CAD, 0x407600854E9EFA51),
        (0x7FA44373B7490F61, 0x4086146CF8B3F7E8),
        (0x3DCB22414019FF7F, 0xC037BB61B39E008F),
        (0x34E6BE746046BC1A, 0xC05E95763594007C),
        (0x6FE2FD9FE02BFFD7, 0x40809E85A1658D9F),
        (0x5368B6D82CF5EC6B, 0x406B001D31F992F6),
        (0x31E841D1D304183A, 0xC063715805318452),
        (0x7841BA6D97A75BE1, 0x4083850682FB3182),
        (0x54A7B6B56A0E469E, 0x406CBA67E2734326),
        (0x4D43FDBFA5B94BFE, 0x40627B9E21BA22F0),
        (0x31E7B83ACAB5FA3C, 0xC063720F91F4EB8B),
        (0x7940275275B4282B, 0x4083DD01100ED1DA),
        (0x26873D4C620CDAAA, 0xC0719BCD5FB045A3),
        (0x13040B26E9F8F4ED, 0xC07F225C2CA4E69F),
        (0x12EBE8E9FA2E6C28, 0xC07F333E5EFB46F9),
    ];

    /// The contract is bit-for-bit, so this asserts bits — not a tolerance.
    /// A tolerance here would pass on platform libm and prove nothing, which
    /// is exactly how the one-ULP `nextGaussian` divergence survived a green
    /// suite for months.
    #[test]
    fn log_matches_hotspot_strictmath_bit_for_bit() {
        for &(xb, want) in VECTORS {
            let x = f64::from_bits(xb);
            let got = log(x);
            if f64::from_bits(want).is_nan() {
                // The spec fixes NaN-ness, not the NaN payload/sign.
                assert!(
                    got.is_nan(),
                    "log({x:?}) [{xb:#018x}] should be NaN, got {got:?}"
                );
                continue;
            }
            assert_eq!(
                got.to_bits(),
                want,
                "log({x:?}) [{xb:#018x}]: got {:#018x} ({got:?}), want {want:#018x} ({:?})",
                got.to_bits(),
                f64::from_bits(want)
            );
        }
    }

    /// The specific value the differential probe reports. Kept separate from
    /// the table so a regression names the observable, not "vector #1".
    #[test]
    fn log_reproduces_the_next_gaussian_multiplier() {
        // `s` after `new Random(42)`'s first accepted polar pair.
        let s = f64::from_bits(0x3FD5D9E5352FEE22);
        assert_eq!(log(s).to_bits(), 0xBFF1_31AE_8F1B_F126);
        // Deliberately NOT asserted here: that this differs from `s.ln()`.
        // It does on Windows/x86-64 and on the HotSpot `Math.log` intrinsic
        // (both give ...F127), which is how the divergence was found — but
        // whether a given host libm rounds this particular argument the
        // fdlibm way is a property of that host, not of the contract, so an
        // inequality assertion here would be a test of the machine.
    }

    /// Exactness at the points the spec calls out by name.
    #[test]
    fn log_special_cases() {
        assert_eq!(log(1.0), 0.0);
        assert!(log(1.0).is_sign_positive());
        assert_eq!(log(0.0), f64::NEG_INFINITY);
        assert_eq!(log(-0.0), f64::NEG_INFINITY);
        assert_eq!(log(f64::INFINITY), f64::INFINITY);
        assert!(log(-1.0).is_nan());
        assert!(log(f64::NEG_INFINITY).is_nan());
        assert!(log(f64::NAN).is_nan());
    }

    /// A coarse whole-range guard: across the full exponent range this must
    /// agree with the host libm to a few ULP. It cannot check the last bit —
    /// the golden table above owns that — but it catches a botched port (wrong
    /// branch taken, mistyped constant, sign lost in the high-word surgery),
    /// which the 90-entry table alone might miss on some untested exponent.
    #[test]
    fn log_tracks_libm_across_the_whole_exponent_range() {
        let mut bits: u64 = 0x9E3779B97F4A7C15;
        let mut checked = 0u32;
        for _ in 0..20000 {
            // xorshift64* — this crate has no rand dependency.
            bits ^= bits >> 12;
            bits ^= bits << 25;
            bits ^= bits >> 27;
            let x = f64::from_bits(bits.wrapping_mul(0x2545F4914F6CDD1D) & 0x7FEF_FFFF_FFFF_FFFF);
            if x == 0.0 || !x.is_finite() {
                continue;
            }
            let a = log(x);
            let b = x.ln();
            // Relative, not bitwise: near x == 1 the result underflows toward
            // zero and a bit-distance comparison stops meaning anything.
            let denom = b.abs().max(f64::MIN_POSITIVE);
            assert!(
                (a - b).abs() <= 1e-15 * denom,
                "log({x:e}) = {a:e} but libm says {b:e}"
            );
            checked += 1;
        }
        assert!(checked > 10000, "generator produced too few finite draws");
    }

    // GENERATED by probes/StrictMathVectorProbe.java against Temurin
    // jdk-25.0.3+9. Regenerate with that probe rather than editing: the
    // point of these tables is that they are the reference implementation's
    // output, not this port's.

    /// `StrictMath.sin`, 89 vectors from Temurin 25.0.3+9.
    const SIN_VECTORS: &[(u64, u64)] = &[
        (0x0000000000000000, 0x0000000000000000),
        (0x8000000000000000, 0x8000000000000000),
        (0x3FF0000000000000, 0x3FEAED548F090CEE),
        (0xBFF0000000000000, 0xBFEAED548F090CEE),
        (0x4000000000000000, 0x3FED18F6EAD1B446),
        (0xC000000000000000, 0xBFED18F6EAD1B446),
        (0x3FE0000000000000, 0x3FDEAEE8744B05F0),
        (0xBFE0000000000000, 0xBFDEAEE8744B05F0),
        (0x0000000000000001, 0x0000000000000001),
        (0x8000000000000001, 0x8000000000000001),
        (0x0010000000000000, 0x0010000000000000),
        (0x8010000000000000, 0x8010000000000000),
        (0x000FFFFFFFFFFFFF, 0x000FFFFFFFFFFFFF),
        (0x7FEFFFFFFFFFFFFF, 0x3F7452FC98B34E97),
        (0xFFEFFFFFFFFFFFFF, 0xBF7452FC98B34E97),
        (0x7FF0000000000000, 0xFFF8000000000000),
        (0xFFF0000000000000, 0xFFF8000000000000),
        (0x7FF8000000000000, 0x7FF8000000000000),
        (0x3FEFFFFFFFFFFFFF, 0x3FEAED548F090CED),
        (0x3FF0000000000001, 0x3FEAED548F090CEF),
        (0x3FE921FB54442D18, 0x3FE6A09E667F3BCC),
        (0x3FE921FB54442D19, 0x3FE6A09E667F3BCD),
        (0x3FE921FB54442D17, 0x3FE6A09E667F3BCC),
        (0x3FF921FB54442D18, 0x3FF0000000000000),
        (0x3FF921FB54442D19, 0x3FF0000000000000),
        (0x3FF921FB54442D17, 0x3FF0000000000000),
        (0x4002D97C7F3321D2, 0x3FE6A09E667F3BCD),
        (0x4002D97C7F3321D3, 0x3FE6A09E667F3BCA),
        (0x4002D97C7F3321D1, 0x3FE6A09E667F3BD0),
        (0x400921FB54442D18, 0x3CA1A62633145C07),
        (0x400921FB54442D19, 0xBCB72CECE675D1FD),
        (0x400921FB54442D17, 0x3CC469898CC51702),
        (0x401921FB54442D18, 0xBCB1A62633145C07),
        (0x401921FB54442D19, 0x3CC72CECE675D1FD),
        (0x401921FB54442D17, 0xBCD469898CC51702),
        (0x3E40000000000000, 0x3E40000000000000),
        (0x3E40000000000001, 0x3E40000000000001),
        (0x3E3FFFFFFFFFFFFF, 0x3E3FFFFFFFFFFFFF),
        (0x3E30000000000000, 0x3E30000000000000),
        (0x3E30000000000001, 0x3E30000000000001),
        (0x3E2FFFFFFFFFFFFF, 0x3E2FFFFFFFFFFFFF),
        (0x3FD3333333333333, 0x3FD2E9CD95BABA33),
        (0x3FD3333333333334, 0x3FD2E9CD95BABA34),
        (0x3FD3333333333332, 0x3FD2E9CD95BABA32),
        (0x3FE594AF4F0D844D, 0x3FE3FB5210A73CF2),
        (0x3FE594AF4F0D844E, 0x3FE3FB5210A73CF3),
        (0x3FE594AF4F0D844C, 0x3FE3FB5210A73CF2),
        (0x3FE9000000000000, 0x3FE6888A4E134B2F),
        (0x3FE9000000000001, 0x3FE6888A4E134B2F),
        (0x3FE8FFFFFFFFFFFF, 0x3FE6888A4E134B2E),
        (0x412921FB54442D18, 0xBDC1A62633145C07),
        (0x412921FB54442D19, 0x3DD72CECE675D1FD),
        (0x412921FB54442D17, 0xBDE469898CC51702),
        (0x4130000000000000, 0x3FD526CCB2FC8656),
        (0x4130000000000001, 0x3FD526CCB338EDB1),
        (0x412FFFFFFFFFFFFF, 0x3FD526CCB2DE52A8),
        (0x412E848000000000, 0xBFD6664B2568D867),
        (0x412E848000000001, 0xBFD6664B254ADE88),
        (0x412E847FFFFFFFFF, 0xBFD6664B2586D247),
        (0x4480F0CF064DD592, 0xBFEB453AB76BF397),
        (0x4480F0CF064DD593, 0xBFD5BC86C4FF6025),
        (0x4480F0CF064DD591, 0xBFEFC2138F97C901),
        (0x4410000000000000, 0xBFB82198CC8E1773),
        (0x4410000000000001, 0xBFDEAD987B2573B4),
        (0x440FFFFFFFFFFFFF, 0x3FED93FDDD006588),
        (0x4001DED1D1AE5486, 0x3FE93881F5FF184E),
        (0x3FEDC2305FBA59DA, 0x3FE9A6A6131BDB02),
        (0x3FF8F8E53DE68638, 0x3FEFFF967EE8DC23),
        (0x400FDA6D384CF752, 0xBFE7D47329A051FA),
        (0xC010E5A78DB3DDE5, 0x3FEC43514EC99DE1),
        (0x3FF4F5AE47B836CE, 0x3FEEEAF23EB4E9CC),
        (0x4018CCDFCF71457E, 0xBFB5409C66B49C8D),
        (0xC00E20923727C93D, 0x3FE2B48A39BE5F55),
        (0x3F87BE43BBF8BB3E, 0x3F87BE20E0AFF42E),
        (0x401696BD7B580825, 0xBFE301C239C62011),
        (0xC00C2BEDC38A3CBE, 0x3FD7BAFBF445BC88),
        (0xBFE23733E7EC79EC, 0xBFE13F69CF77D7C6),
        (0x400C19EFFBFA59BC, 0xBFD73514A22D72B3),
        (0x3FE049525C6D1FB5, 0x3FDF2F488DAA78C0),
        (0x4016BEF1B7F40763, 0xBFE1FB53328CD009),
        (0x40026357595FB32F, 0x3FE7E4F6581E863E),
        (0x40176D7D1227411C, 0xBFDA764F08B735FE),
        (0x3FFEA36D1E67DF10, 0x3FEE1FC5E9778F36),
        (0xC00E09DCDBF4F489, 0x3FE26A8C37FAD214),
        (0x40123A60FBEBF014, 0xBFEF9D4FB3C986EA),
        (0x3FEAB43BC30EA101, 0x3FE7B5F51C5EA49B),
        (0xC01168723F264CCE, 0x3FEDF1BD224F0480),
        (0xC00769EF83E913EF, 0xBFCB4AB00E3B70A2),
        (0x3FE9EE361BA0C5F1, 0x3FE72F374539EB87),
    ];

    /// `StrictMath.cos`, 89 vectors from Temurin 25.0.3+9.
    const COS_VECTORS: &[(u64, u64)] = &[
        (0x0000000000000000, 0x3FF0000000000000),
        (0x8000000000000000, 0x3FF0000000000000),
        (0x3FF0000000000000, 0x3FE14A280FB5068C),
        (0xBFF0000000000000, 0x3FE14A280FB5068C),
        (0x4000000000000000, 0xBFDAA22657537205),
        (0xC000000000000000, 0xBFDAA22657537205),
        (0x3FE0000000000000, 0x3FEC1528065B7D50),
        (0xBFE0000000000000, 0x3FEC1528065B7D50),
        (0x0000000000000001, 0x3FF0000000000000),
        (0x8000000000000001, 0x3FF0000000000000),
        (0x0010000000000000, 0x3FF0000000000000),
        (0x8010000000000000, 0x3FF0000000000000),
        (0x000FFFFFFFFFFFFF, 0x3FF0000000000000),
        (0x7FEFFFFFFFFFFFFF, 0xBFEFFFE62ECFAB75),
        (0xFFEFFFFFFFFFFFFF, 0xBFEFFFE62ECFAB75),
        (0x7FF0000000000000, 0xFFF8000000000000),
        (0xFFF0000000000000, 0xFFF8000000000000),
        (0x7FF8000000000000, 0x7FF8000000000000),
        (0x3FEFFFFFFFFFFFFF, 0x3FE14A280FB5068C),
        (0x3FF0000000000001, 0x3FE14A280FB5068A),
        (0x3FE921FB54442D18, 0x3FE6A09E667F3BCD),
        (0x3FE921FB54442D19, 0x3FE6A09E667F3BCC),
        (0x3FE921FB54442D17, 0x3FE6A09E667F3BCE),
        (0x3FF921FB54442D18, 0x3C91A62633145C07),
        (0x3FF921FB54442D19, 0xBCA72CECE675D1FD),
        (0x3FF921FB54442D17, 0x3CB469898CC51702),
        (0x4002D97C7F3321D2, 0xBFE6A09E667F3BCC),
        (0x4002D97C7F3321D3, 0xBFE6A09E667F3BCF),
        (0x4002D97C7F3321D1, 0xBFE6A09E667F3BC9),
        (0x400921FB54442D18, 0xBFF0000000000000),
        (0x400921FB54442D19, 0xBFF0000000000000),
        (0x400921FB54442D17, 0xBFF0000000000000),
        (0x401921FB54442D18, 0x3FF0000000000000),
        (0x401921FB54442D19, 0x3FF0000000000000),
        (0x401921FB54442D17, 0x3FF0000000000000),
        (0x3E40000000000000, 0x3FF0000000000000),
        (0x3E40000000000001, 0x3FF0000000000000),
        (0x3E3FFFFFFFFFFFFF, 0x3FF0000000000000),
        (0x3E30000000000000, 0x3FF0000000000000),
        (0x3E30000000000001, 0x3FF0000000000000),
        (0x3E2FFFFFFFFFFFFF, 0x3FF0000000000000),
        (0x3FD3333333333333, 0x3FEE921DD42F09BA),
        (0x3FD3333333333334, 0x3FEE921DD42F09BA),
        (0x3FD3333333333332, 0x3FEE921DD42F09BB),
        (0x3FE594AF4F0D844D, 0x3FE8FE9F26EAE490),
        (0x3FE594AF4F0D844E, 0x3FE8FE9F26EAE48F),
        (0x3FE594AF4F0D844C, 0x3FE8FE9F26EAE491),
        (0x3FE9000000000000, 0x3FE6B898FA9EFB5D),
        (0x3FE9000000000001, 0x3FE6B898FA9EFB5C),
        (0x3FE8FFFFFFFFFFFF, 0x3FE6B898FA9EFB5E),
        (0x412921FB54442D18, 0x3FF0000000000000),
        (0x412921FB54442D19, 0x3FF0000000000000),
        (0x412921FB54442D17, 0x3FF0000000000000),
        (0x4130000000000000, 0x3FEE33ADA92FE2AE),
        (0x4130000000000001, 0x3FEE33ADA9254F48),
        (0x412FFFFFFFFFFFFF, 0x3FEE33ADA9352C61),
        (0x412E848000000000, 0x3FEDF9DF9906D32C),
        (0x412E848000000001, 0x3FEDF9DF990C6CBF),
        (0x412E847FFFFFFFFF, 0x3FEDF9DF9901399A),
        (0x4480F0CF064DD592, 0x3FE0BE2CEF01C8F4),
        (0x4480F0CF064DD593, 0x3FEE190E2070604A),
        (0x4480F0CF064DD591, 0xBFBF6AC59E0F597D),
        (0x4410000000000000, 0x3FEFDB86251005F5),
        (0x4410000000000001, 0xBFEC1583C9508BCF),
        (0x440FFFFFFFFFFFFF, 0x3FD86C9E1478460E),
        (0x4013274C9C9925D2, 0x3FB36F3CC2C3CB4C),
        (0x400ABE1D7287242C, 0xBFEF5AAFA61D0EF2),
        (0x400E49492B1C74A4, 0xBFE99634918AC5A4),
        (0x4008FD17CA210767, 0xBFEFFEABD014BBE7),
        (0xBFF9F1B45009FEDD, 0xBFA9F44628843AA0),
        (0x4002CEAFAC4D7468, 0xBFE681FDB6E32D25),
        (0x400AFD4E1C38525B, 0xBFEF245A03F58D0B),
        (0x3FEC98AA6E6A7E52, 0x3FE40CF6B04A0669),
        (0x400550C0F7AC9CAA, 0xBFEC6CF7F78700BA),
        (0x4002D7F0E97C5D4C, 0xBFE69C3F18756349),
        (0x4012069FB897D9E2, 0xBFCA2C02EA236D50),
        (0x4005AE0ADE312EC9, 0xBFED10BC79A316A4),
        (0x4010B42C5535D345, 0xBFE05AC078CF829E),
        (0xBFE353378415C897, 0x3FEA57050A353EA3),
        (0x400EC845F326FCF5, 0xBFE858C34569C072),
        (0x400964804F88C783, 0xBFEFFBADE33C58F9),
        (0x3FF3C81E385C8521, 0x3FD501E0F001FE06),
        (0x3FAA1026E873251C, 0x3FEFF563728097C7),
        (0xC0116858F880A8D2, 0xBFD6932111499ADF),
        (0xBFED939DEE5840BA, 0x3FE347018146DD1D),
        (0x401158E8714D4B85, 0xBFD7799E56C8C0F8),
        (0x3FD41973DC7F714D, 0x3FEE6F529AB912EE),
        (0xC00B8BFF5CE5FEB8, 0xBFEE8DD48878ED6B),
        (0x3FECF3CB07CF8814, 0x3FE3C5A0800FECE9),
    ];

    /// `StrictMath.tan`, 89 vectors from Temurin 25.0.3+9.
    const TAN_VECTORS: &[(u64, u64)] = &[
        (0x0000000000000000, 0x0000000000000000),
        (0x8000000000000000, 0x8000000000000000),
        (0x3FF0000000000000, 0x3FF8EB245CBEE3A6),
        (0xBFF0000000000000, 0xBFF8EB245CBEE3A6),
        (0x4000000000000000, 0xC0017AF62E0950F8),
        (0xC000000000000000, 0x40017AF62E0950F8),
        (0x3FE0000000000000, 0x3FE17B4F5BF3474A),
        (0xBFE0000000000000, 0xBFE17B4F5BF3474A),
        (0x0000000000000001, 0x0000000000000001),
        (0x8000000000000001, 0x8000000000000001),
        (0x0010000000000000, 0x0010000000000000),
        (0x8010000000000000, 0x8010000000000000),
        (0x000FFFFFFFFFFFFF, 0x000FFFFFFFFFFFFF),
        (0x7FEFFFFFFFFFFFFF, 0xBF74530CFE729484),
        (0xFFEFFFFFFFFFFFFF, 0x3F74530CFE729484),
        (0x7FF0000000000000, 0xFFF8000000000000),
        (0xFFF0000000000000, 0xFFF8000000000000),
        (0x7FF8000000000000, 0x7FF8000000000000),
        (0x3FEFFFFFFFFFFFFF, 0x3FF8EB245CBEE3A4),
        (0x3FF0000000000001, 0x3FF8EB245CBEE3A9),
        (0x3FE921FB54442D18, 0x3FEFFFFFFFFFFFFF),
        (0x3FE921FB54442D19, 0x3FF0000000000001),
        (0x3FE921FB54442D17, 0x3FEFFFFFFFFFFFFD),
        (0x3FF921FB54442D18, 0x434D02967C31CDB5),
        (0x3FF921FB54442D19, 0xC33617A15494767A),
        (0x3FF921FB54442D17, 0x4329153D9443ED0B),
        (0x4002D97C7F3321D2, 0xBFF0000000000001),
        (0x4002D97C7F3321D3, 0xBFEFFFFFFFFFFFFA),
        (0x4002D97C7F3321D1, 0xBFF0000000000005),
        (0x400921FB54442D18, 0xBCA1A62633145C07),
        (0x400921FB54442D19, 0x3CB72CECE675D1FD),
        (0x400921FB54442D17, 0xBCC469898CC51702),
        (0x401921FB54442D18, 0xBCB1A62633145C07),
        (0x401921FB54442D19, 0x3CC72CECE675D1FD),
        (0x401921FB54442D17, 0xBCD469898CC51702),
        (0x3E40000000000000, 0x3E40000000000000),
        (0x3E40000000000001, 0x3E40000000000001),
        (0x3E3FFFFFFFFFFFFF, 0x3E3FFFFFFFFFFFFF),
        (0x3E30000000000000, 0x3E30000000000000),
        (0x3E30000000000001, 0x3E30000000000001),
        (0x3E2FFFFFFFFFFFFF, 0x3E2FFFFFFFFFFFFF),
        (0x3FD3333333333333, 0x3FD3CC2A44E29998),
        (0x3FD3333333333334, 0x3FD3CC2A44E29999),
        (0x3FD3333333333332, 0x3FD3CC2A44E29996),
        (0x3FE594AF4F0D844D, 0x3FE995054EA00C37),
        (0x3FE594AF4F0D844E, 0x3FE995054EA00C38),
        (0x3FE594AF4F0D844C, 0x3FE995054EA00C35),
        (0x3FE9000000000000, 0x3FEFBC511DF5917F),
        (0x3FE9000000000001, 0x3FEFBC511DF59181),
        (0x3FE8FFFFFFFFFFFF, 0x3FEFBC511DF5917D),
        (0x412921FB54442D18, 0xBDC1A62633145C07),
        (0x412921FB54442D19, 0x3DD72CECE675D1FD),
        (0x412921FB54442D17, 0xBDE469898CC51702),
        (0x4130000000000000, 0x3FD6692E5779206F),
        (0x4130000000000001, 0x3FD6692E57C0F96C),
        (0x412FFFFFFFFFFFFF, 0x3FD6692E575533F1),
        (0x412E848000000000, 0xBFD7E9768AB734C0),
        (0x412E848000000001, 0xBFD7E9768A92BD30),
        (0x412E847FFFFFFFFF, 0xBFD7E9768ADBAC50),
        (0x4480F0CF064DD592, 0xBFFA0F79C1B6B258),
        (0x4480F0CF064DD593, 0xBFD71C31A4BF91A6),
        (0x4480F0CF064DD591, 0x40202C7650CAF023),
        (0x4410000000000000, 0xBFB83D39FB22BDD2),
        (0x4410000000000001, 0x3FE17A56D45AD3C9),
        (0x440FFFFFFFFFFFFF, 0x4003604D965646FE),
        (0xC012B33047413CFB, 0xC03AB9B47C03A3B7),
        (0xC0082C9734FF2FF2, 0x3FBED24FB3DE6AFF),
        (0xBFD7586E4C824E1E, 0xBFD8706A1D836E73),
        (0x3FEB666A3C188526, 0x3FF271D29A361DC9),
        (0x401500B702E1251A, 0xBFFACB6FE187C8BA),
        (0xBFC5F923DB10433C, 0xBFC6310E210DACC3),
        (0x40125851E01090C8, 0x401F8AEAE5786DC3),
        (0x4011BF939B410E9D, 0x400C5231B8E177E2),
        (0x4016DFE1C9A0E103, 0xBFE4440CEC2C05A9),
        (0x40066FB586994946, 0xBFD66D3FD8549238),
        (0x400C5EB523EDAF29, 0x3FDB690CF34A008D),
        (0xC016E37F5F2C4B07, 0x3FE41B9D80F9B052),
        (0xC0126DEAA130F7A7, 0xC022F7FDE6507D6F),
        (0xBFE9BBD7C1FB6CED, 0xBFF09CD330464A53),
        (0x3FF0DDFC6B8FCDC8, 0x3FFC2A8C5CB6CB4C),
        (0x3FFB5BE2851E17C3, 0xC01C90204332372E),
        (0x3FF2AEDF566C0B2A, 0x4002C2604FC1474F),
        (0x3FD4A789E4237A9A, 0x3FD567178A01F456),
        (0xC00EAA24D3215CFE, 0xBFEA7DA2BE18F5B5),
        (0xC0158B705441C9BA, 0x3FF40A148845F211),
        (0x4000FC576C3761D3, 0xBFF9F4A57F76EEF8),
        (0x400BE1C7213D8830, 0x3FD6E7014423F9BB),
        (0xC013CFEE9374D5BC, 0x40104C5440EAA6D8),
        (0xC009DCEE821934FE, 0xBFB76F11D464F668),
    ];

    /// `StrictMath.asin`, 65 vectors from Temurin 25.0.3+9.
    const ASIN_VECTORS: &[(u64, u64)] = &[
        (0x0000000000000000, 0x0000000000000000),
        (0x8000000000000000, 0x8000000000000000),
        (0x3FF0000000000000, 0x3FF921FB54442D18),
        (0xBFF0000000000000, 0xBFF921FB54442D18),
        (0x4000000000000000, 0xFFF8000000000000),
        (0xC000000000000000, 0xFFF8000000000000),
        (0x3FE0000000000000, 0x3FE0C152382D7366),
        (0xBFE0000000000000, 0xBFE0C152382D7366),
        (0x0000000000000001, 0x0000000000000001),
        (0x8000000000000001, 0x8000000000000001),
        (0x0010000000000000, 0x0010000000000000),
        (0x8010000000000000, 0x8010000000000000),
        (0x000FFFFFFFFFFFFF, 0x000FFFFFFFFFFFFF),
        (0x7FEFFFFFFFFFFFFF, 0xFFF8000000000000),
        (0xFFEFFFFFFFFFFFFF, 0xFFF8000000000000),
        (0x7FF0000000000000, 0xFFF8000000000000),
        (0xFFF0000000000000, 0xFFF8000000000000),
        (0x7FF8000000000000, 0x7FF8000000000000),
        (0x3FEFFFFFFFFFFFFF, 0x3FF921FB50442D18),
        (0x3FF0000000000001, 0xFFF8000000000000),
        (0x3FE0000000000000, 0x3FE0C152382D7366),
        (0x3FE0000000000001, 0x3FE0C152382D7367),
        (0x3FDFFFFFFFFFFFFF, 0x3FE0C152382D7365),
        (0x3FEF333333333333, 0x3FF58C2B5CE0C3E5),
        (0x3FEF333333333334, 0x3FF58C2B5CE0C3E7),
        (0x3FEF333333333332, 0x3FF58C2B5CE0C3E3),
        (0x3E40000000000000, 0x3E40000000000000),
        (0x3E40000000000001, 0x3E40000000000001),
        (0x3E3FFFFFFFFFFFFF, 0x3E3FFFFFFFFFFFFF),
        (0x3C60000000000000, 0x3C60000000000000),
        (0x3C60000000000001, 0x3C60000000000001),
        (0x3C5FFFFFFFFFFFFF, 0x3C5FFFFFFFFFFFFF),
        (0x3FF0000000000000, 0x3FF921FB54442D18),
        (0x3FF0000000000001, 0xFFF8000000000000),
        (0x3FEFFFFFFFFFFFFF, 0x3FF921FB50442D18),
        (0xBFF0000000000000, 0xBFF921FB54442D18),
        (0xBFEFFFFFFFFFFFFF, 0xBFF921FB50442D18),
        (0xBFF0000000000001, 0xFFF8000000000000),
        (0x3FF8000000000000, 0xFFF8000000000000),
        (0x3FF8000000000001, 0xFFF8000000000000),
        (0x3FF7FFFFFFFFFFFF, 0xFFF8000000000000),
        (0x3FEDA08E34D3C17C, 0x3FF2EEB3184C70F5),
        (0x3FE195D722F42706, 0x3FE29E3A1C67DBEA),
        (0x3FBF6AA9E5805E60, 0x3FBF7EFD4FAA67FD),
        (0xBFE9B31FDEC64B52, 0xBFEDD7177CA99619),
        (0xBFEAB47C7D254222, 0xBFEF97D22FB907FA),
        (0x3F873E7E8CABA580, 0x3F873E9F41847EFF),
        (0xBFE46C532C6990EA, 0xBFE62667BFDE70A1),
        (0xBFE97E572456FEFA, 0xBFED7F22E2F43C62),
        (0x3FB0A86C7AFB28F0, 0x3FB0AB705736BBEA),
        (0x3F9C723540686DC0, 0x3F9C73251B50B43F),
        (0x3FE90656BC0538D4, 0x3FECBBA62A435934),
        (0x3FDE765E222505F8, 0x3FDFBFA43C2287BB),
        (0x3FE4B3796794B78C, 0x3FE683435CFAFBA0),
        (0x3FEF61C08B24BA36, 0x3FF5FB94ECC82403),
        (0x3FE6287B3DAEE30E, 0x3FE879CBE612D6CC),
        (0xBFE76836E874BE32, 0xBFEA4159FE046E74),
        (0xBFE812CC1B52299A, 0xBFEB3FCCAFCD1913),
        (0x3FD191844A0A1620, 0x3FD1CC01C44BE82D),
        (0x3FEDA69F1A9679E4, 0x3FF2F6BEBA6376CD),
        (0xBFE837083B3EC804, 0xBFEB7702F1313934),
        (0x3FD5E0802B64A8E0, 0x3FD653BC2A3C3023),
        (0xBF97659B808AD200, 0xBF976620F2C09B40),
        (0x3FE34AE44F6B6C96, 0x3FE4B53758F1FB0A),
        (0x3FE028287CCE5F96, 0x3FE0EFC4A101A9CD),
    ];

    /// `StrictMath.acos`, 65 vectors from Temurin 25.0.3+9.
    const ACOS_VECTORS: &[(u64, u64)] = &[
        (0x0000000000000000, 0x3FF921FB54442D18),
        (0x8000000000000000, 0x3FF921FB54442D18),
        (0x3FF0000000000000, 0x0000000000000000),
        (0xBFF0000000000000, 0x400921FB54442D18),
        (0x4000000000000000, 0xFFF8000000000000),
        (0xC000000000000000, 0xFFF8000000000000),
        (0x3FE0000000000000, 0x3FF0C152382D7366),
        (0xBFE0000000000000, 0x4000C152382D7366),
        (0x0000000000000001, 0x3FF921FB54442D18),
        (0x8000000000000001, 0x3FF921FB54442D18),
        (0x0010000000000000, 0x3FF921FB54442D18),
        (0x8010000000000000, 0x3FF921FB54442D18),
        (0x000FFFFFFFFFFFFF, 0x3FF921FB54442D18),
        (0x7FEFFFFFFFFFFFFF, 0xFFF8000000000000),
        (0xFFEFFFFFFFFFFFFF, 0xFFF8000000000000),
        (0x7FF0000000000000, 0xFFF8000000000000),
        (0xFFF0000000000000, 0xFFF8000000000000),
        (0x7FF8000000000000, 0x7FF8000000000000),
        (0x3FEFFFFFFFFFFFFF, 0x3E50000000000000),
        (0x3FF0000000000001, 0xFFF8000000000000),
        (0x3FE0000000000000, 0x3FF0C152382D7366),
        (0x3FE0000000000001, 0x3FF0C152382D7365),
        (0x3FDFFFFFFFFFFFFF, 0x3FF0C152382D7366),
        (0x3FEF333333333333, 0x3FCCAE7FBB1B499A),
        (0x3FEF333333333334, 0x3FCCAE7FBB1B4988),
        (0x3FEF333333333332, 0x3FCCAE7FBB1B49AC),
        (0x3E40000000000000, 0x3FF921FB52442D18),
        (0x3E40000000000001, 0x3FF921FB52442D18),
        (0x3E3FFFFFFFFFFFFF, 0x3FF921FB52442D18),
        (0x3C60000000000000, 0x3FF921FB54442D18),
        (0x3C60000000000001, 0x3FF921FB54442D18),
        (0x3C5FFFFFFFFFFFFF, 0x3FF921FB54442D18),
        (0x3FF0000000000000, 0x0000000000000000),
        (0x3FF0000000000001, 0xFFF8000000000000),
        (0x3FEFFFFFFFFFFFFF, 0x3E50000000000000),
        (0xBFF0000000000000, 0x400921FB54442D18),
        (0xBFEFFFFFFFFFFFFF, 0x400921FB52442D18),
        (0xBFF0000000000001, 0xFFF8000000000000),
        (0x3FF8000000000000, 0xFFF8000000000000),
        (0x3FF8000000000001, 0xFFF8000000000000),
        (0x3FF7FFFFFFFFFFFF, 0xFFF8000000000000),
        (0x3FDCCBC37B121ED8, 0x3FF1AA66ABBA856B),
        (0xBFE8079324C3F32C, 0x40035CAFF1FED795),
        (0x3FE21CBE345B7796, 0x3FEF033209B0DFB4),
        (0xBFD173EB83140104, 0x3FFD8D4A6E6B3F40),
        (0xBFEBEB03C7DE4D4C, 0x40050C37CDAFB1DB),
        (0x3FE84754B4F58A92, 0x3FE6B3F9F8BC3FBA),
        (0xBFE539653C5B3882, 0x40025E1F1CA92BDA),
        (0x3FE132F7ED7359B2, 0x3FF00DC37118CAAB),
        (0x3FE8B5A5028902B0, 0x3FE60888C26E61B0),
        (0x3FDAB00061334148, 0x3FF2402782F4886B),
        (0x3FE18BFECE567528, 0x3FEFB183CBE1C2FC),
        (0xBFEC710A72BB256A, 0x400552F90AA10C2A),
        (0x3FA686A631811680, 0x3FF86DB73D5B8803),
        (0xBFD27C676D745D2C, 0x3FFDD22F1A5981CE),
        (0xBFE6156994AE854E, 0x4002A8D7B0739E74),
        (0x3FE13FC1D522E938, 0x3FF0062D2000CB02),
        (0x3FB09E6EC693E070, 0x3FF817E480CC9951),
        (0xBFD4A2EDF65B9F1C, 0x3FFE62BE1046394C),
        (0x3FE6D0BA30C0417E, 0x3FE8DDA908E76329),
        (0x3FEDFB7C3095789E, 0x3FD6D91D53D0BCD6),
        (0x3FCEE9CBADAF4600, 0x3FF53AE11D44450E),
        (0xBFEBFA2690F63FBC, 0x400513FCA256B0A9),
        (0xBFCB70620C845AE0, 0x3FFC96E5C1762EEE),
        (0x3FE0265DFF5F7A04, 0x3FF0AB2293F671BA),
    ];

    /// `StrictMath.atan`, 65 vectors from Temurin 25.0.3+9, plus nine pinned by
    /// hand — see the note at the head of the table.
    const ATAN_VECTORS: &[(u64, u64)] = &[
        // `atan` is the one function in this family whose deviation rate is
        // near zero: 9 disagreements in 100k samples, 0.009%. At that rate a
        // 65-vector table has better than a 99% chance of containing NO vector
        // that platform libm gets wrong — that is, of passing whether or not
        // `atan` is really ported. A test that cannot fail is the same shape of
        // hole as the `1e-12` tolerance that let the `log` divergence live for
        // months, and the low rate is exactly what makes it easy to miss.
        //
        // These nine are the precise inputs, found by replaying the 100k-sample
        // oracle against MSVC's CRT, where libm and fdlibm disagree. Their
        // expected values are HotSpot's, like every other row here. Do NOT drop
        // them when regenerating the table: without them this test is
        // decorative. (`probes/StrictMathOracleDumpProbe.java` re-derives them.)
        (0x3FC787F309A073B3, 0x3FC7456E8101130A),
        (0xBFACE2846DD20B29, 0xBFACDAAFF4DF50B7),
        (0xBFAE6D6442F69379, 0xBFAE643DB3DC2D88),
        (0xBFCF73FD4317B3F7, 0xBFCED78DDD648E46),
        (0xBF8F394144F1E923, 0xBF8F38A2BF55AE80),
        (0x4009D1204F40129A, 0x3FF4532B7461D664),
        (0x3FAE8FC970EBE372, 0x3FAE8683C1EB60B6),
        (0xBFC8E75AC44A4E60, 0xBFC898B0A082D683),
        (0x3FA98F6580A5DDA3, 0x3FA989F7FF9B0928),
        (0x0000000000000000, 0x0000000000000000),
        (0x8000000000000000, 0x8000000000000000),
        (0x3FF0000000000000, 0x3FE921FB54442D18),
        (0xBFF0000000000000, 0xBFE921FB54442D18),
        (0x4000000000000000, 0x3FF1B6E192EBBE44),
        (0xC000000000000000, 0xBFF1B6E192EBBE44),
        (0x3FE0000000000000, 0x3FDDAC670561BB4F),
        (0xBFE0000000000000, 0xBFDDAC670561BB4F),
        (0x0000000000000001, 0x0000000000000001),
        (0x8000000000000001, 0x8000000000000001),
        (0x0010000000000000, 0x0010000000000000),
        (0x8010000000000000, 0x8010000000000000),
        (0x000FFFFFFFFFFFFF, 0x000FFFFFFFFFFFFF),
        (0x7FEFFFFFFFFFFFFF, 0x3FF921FB54442D18),
        (0xFFEFFFFFFFFFFFFF, 0xBFF921FB54442D18),
        (0x7FF0000000000000, 0x3FF921FB54442D18),
        (0xFFF0000000000000, 0xBFF921FB54442D18),
        (0x7FF8000000000000, 0x7FF8000000000000),
        (0x3FEFFFFFFFFFFFFF, 0x3FE921FB54442D18),
        (0x3FF0000000000001, 0x3FE921FB54442D19),
        (0x3FDC000000000000, 0x3FDA64EEC3CC23FD),
        (0x3FDC000000000001, 0x3FDA64EEC3CC23FE),
        (0x3FDBFFFFFFFFFFFF, 0x3FDA64EEC3CC23FC),
        (0x3FE6000000000000, 0x3FE345F01CCE37BB),
        (0x3FE6000000000001, 0x3FE345F01CCE37BC),
        (0x3FE5FFFFFFFFFFFF, 0x3FE345F01CCE37BA),
        (0x3FF3000000000000, 0x3FEBDE70ED439FE7),
        (0x3FF3000000000001, 0x3FEBDE70ED439FE8),
        (0x3FF2FFFFFFFFFFFF, 0x3FEBDE70ED439FE6),
        (0x4003800000000000, 0x3FF2E75728833A54),
        (0x4003800000000001, 0x3FF2E75728833A54),
        (0x40037FFFFFFFFFFF, 0x3FF2E75728833A54),
        (0x3FF8000000000000, 0x3FEF730BD281F69B),
        (0x3FF8000000000001, 0x3FEF730BD281F69C),
        (0x3FF7FFFFFFFFFFFF, 0x3FEF730BD281F69B),
        (0x4410000000000000, 0x3FF921FB54442D18),
        (0x4410000000000001, 0x3FF921FB54442D18),
        (0x440FFFFFFFFFFFFF, 0x3FF921FB54442D18),
        (0x3E20000000000000, 0x3E20000000000000),
        (0x3E20000000000001, 0x3E20000000000001),
        (0x3E1FFFFFFFFFFFFF, 0x3E1FFFFFFFFFFFFF),
        (0x05041D180E239A1A, 0x05041D180E239A1A),
        (0x4C86B3B7812681E9, 0x3FF921FB54442D18),
        (0x33A50310391AF7A3, 0x33A50310391AF7A3),
        (0xEC439F1292910FD1, 0xBFF921FB54442D18),
        (0xFF0A377313F394B4, 0xBFF921FB54442D18),
        (0x0F074206676759E4, 0x0F074206676759E4),
        (0x2FCBBD2B7EB92814, 0x2FCBBD2B7EB92814),
        (0x420079B20C5F82F9, 0x3FF921FB543C6830),
        (0x9666919F21AF8E25, 0x9666919F21AF8E25),
        (0xEA24FF60B95F79BC, 0xBFF921FB54442D18),
        (0x19662A7234E71D3F, 0x19662A7234E71D3F),
        (0x1E87BD66338F659F, 0x1E87BD66338F659F),
        (0x76A8783A8F5F4E15, 0x3FF921FB54442D18),
        (0xF107CC02B8934F52, 0xBFF921FB54442D18),
        (0x746CAE7730D55FF9, 0x3FF921FB54442D18),
        (0x16805DFE51D51079, 0x16805DFE51D51079),
        (0x5167CF159B2D220E, 0x3FF921FB54442D18),
        (0x5AC367AD46A7F89F, 0x3FF921FB54442D18),
        (0xE608C470D3C081D2, 0xBFF921FB54442D18),
        (0x16A0349481A6E59D, 0x16A0349481A6E59D),
        (0x0F083E9C0CCF3C7E, 0x0F083E9C0CCF3C7E),
        (0xF5A53F00DD745BDE, 0xBFF921FB54442D18),
        (0x9FA9A140F340E432, 0x9FA9A140F340E432),
        (0xA54BC20F762E8753, 0xA54BC20F762E8753),
    ];

    /// `StrictMath.exp`, 71 vectors from Temurin 25.0.3+9.
    const EXP_VECTORS: &[(u64, u64)] = &[
        (0x0000000000000000, 0x3FF0000000000000),
        (0x8000000000000000, 0x3FF0000000000000),
        (0x3FF0000000000000, 0x4005BF0A8B14576A),
        (0xBFF0000000000000, 0x3FD78B56362CEF38),
        (0x4000000000000000, 0x401D8E64B8D4DDAE),
        (0xC000000000000000, 0x3FC152AAA3BF81CC),
        (0x3FE0000000000000, 0x3FFA61298E1E069C),
        (0xBFE0000000000000, 0x3FE368B2FC6F960A),
        (0x0000000000000001, 0x3FF0000000000000),
        (0x8000000000000001, 0x3FF0000000000000),
        (0x0010000000000000, 0x3FF0000000000000),
        (0x8010000000000000, 0x3FF0000000000000),
        (0x000FFFFFFFFFFFFF, 0x3FF0000000000000),
        (0x7FEFFFFFFFFFFFFF, 0x7FF0000000000000),
        (0xFFEFFFFFFFFFFFFF, 0x0000000000000000),
        (0x7FF0000000000000, 0x7FF0000000000000),
        (0xFFF0000000000000, 0x0000000000000000),
        (0x7FF8000000000000, 0x7FF8000000000000),
        (0x3FEFFFFFFFFFFFFF, 0x4005BF0A8B145769),
        (0x3FF0000000000001, 0x4005BF0A8B14576B),
        (0x3FD62E42FEFA39EF, 0x3FF6A09E667F3BCC),
        (0x3FD62E42FEFA39F0, 0x3FF6A09E667F3BCD),
        (0x3FD62E42FEFA39EE, 0x3FF6A09E667F3BCC),
        (0x3FF0A2B23F3BAB73, 0x4006A09E667F3BCC),
        (0x3FF0A2B23F3BAB74, 0x4006A09E667F3BCE),
        (0x3FF0A2B23F3BAB72, 0x4006A09E667F3BCA),
        (0x3E30000000000000, 0x3FF0000001000000),
        (0x3E30000000000001, 0x3FF0000001000000),
        (0x3E2FFFFFFFFFFFFF, 0x3FF0000001000000),
        (0x40862E42FEFA39EF, 0x7FEFFFFFFFFFFF2A),
        (0x40862E42FEFA39F0, 0x7FF0000000000000),
        (0x40862E42FEFA39EE, 0x7FEFFFFFFFFFFB2A),
        (0xC0874910D52D3051, 0x0000000000000001),
        (0xC0874910D52D3050, 0x0000000000000001),
        (0xC0874910D52D3052, 0x0000000000000000),
        (0xC086200000000000, 0x0017C8AB2288C9AB),
        (0xC0861FFFFFFFFFFF, 0x0017C8AB2288CCA4),
        (0xC086200000000001, 0x0017C8AB2288C6B2),
        (0x4086300000000000, 0x7FF0000000000000),
        (0x4086300000000001, 0x7FF0000000000000),
        (0x40862FFFFFFFFFFF, 0x7FF0000000000000),
        (0x3FF0000000000000, 0x4005BF0A8B14576A),
        (0x3FF0000000000001, 0x4005BF0A8B14576B),
        (0x3FEFFFFFFFFFFFFF, 0x4005BF0A8B145769),
        (0xBFF0000000000000, 0x3FD78B56362CEF38),
        (0xBFEFFFFFFFFFFFFF, 0x3FD78B56362CEF38),
        (0xBFF0000000000001, 0x3FD78B56362CEF36),
        (0x407E080847A7C0A4, 0x6B429BC146F63B7B),
        (0xC085B44FE96B9F01, 0x014FD2B47DD33251),
        (0x407B8A68456AF65C, 0x67AA6EA9BBFD7B84),
        (0x407CE7D51B092129, 0x69A2C86C4FAC1482),
        (0x407B8087C758B0BA, 0x679C83CDD7D49C30),
        (0x407A35DE75480A8E, 0x65C034A0CF29772D),
        (0xC03F4B715EE2226E, 0x3D1CDDAA826C00A3),
        (0x404BB3ADD6D1D8F4, 0x44EE7FD2DFD4D617),
        (0x407C5830362580AC, 0x68D36A860B9EFBEB),
        (0xC0809F0EDA8081E9, 0x0FF93699477F2F22),
        (0xC066D540AE10675A, 0x2F762EB15798EFCF),
        (0xC04E177BDB9B180B, 0x3A820BC9106CDC44),
        (0xC06AD122EE0B0503, 0x2C967B4F71482D1A),
        (0xC03752FCA9E84F21, 0x3DD465D380E2C067),
        (0x408416462DA11E78, 0x79E4468EE06158F0),
        (0xC071A8B50331FFBD, 0x2674BF7788F62338),
        (0x407B3DCAB0B1D2E6, 0x673C2AB3E597E7DF),
        (0xC07B1C4AA88129F4, 0x18D27084F07B0AB4),
        (0x407ACDA594F90349, 0x669A1064A00C19D2),
        (0xC031EE7437B9CC7A, 0x3E518360B92D324C),
        (0xC06CBF82DC00736E, 0x2B32682E40582FB6),
        (0x40754488EAEDF903, 0x5E9E622884C0C146),
        (0x4068E5464710E0F9, 0x51E42B6B281C7562),
        (0x40796086E19BD175, 0x648B81A255059743),
    ];

    /// `StrictMath.cbrt`, 65 vectors from Temurin 25.0.3+9.
    const CBRT_VECTORS: &[(u64, u64)] = &[
        (0x0000000000000000, 0x0000000000000000),
        (0x8000000000000000, 0x8000000000000000),
        (0x3FF0000000000000, 0x3FF0000000000000),
        (0xBFF0000000000000, 0xBFF0000000000000),
        (0x4000000000000000, 0x3FF428A2F98D728B),
        (0xC000000000000000, 0xBFF428A2F98D728B),
        (0x3FE0000000000000, 0x3FE965FEA53D6E3D),
        (0xBFE0000000000000, 0xBFE965FEA53D6E3D),
        (0x0000000000000001, 0x2990000000000000),
        (0x8000000000000001, 0xA990000000000000),
        (0x0010000000000000, 0x2AA428A2F98D728B),
        (0x8010000000000000, 0xAAA428A2F98D728B),
        (0x000FFFFFFFFFFFFF, 0x2AA428A2F98D728B),
        (0x7FEFFFFFFFFFFFFF, 0x554428A2F98D728B),
        (0xFFEFFFFFFFFFFFFF, 0xD54428A2F98D728B),
        (0x7FF0000000000000, 0x7FF0000000000000),
        (0xFFF0000000000000, 0xFFF0000000000000),
        (0x7FF8000000000000, 0x7FF8000000000000),
        (0x3FEFFFFFFFFFFFFF, 0x3FF0000000000000),
        (0x3FF0000000000001, 0x3FF0000000000000),
        (0x4020000000000000, 0x4000000000000000),
        (0x4020000000000001, 0x4000000000000000),
        (0x401FFFFFFFFFFFFF, 0x4000000000000000),
        (0xC020000000000000, 0xC000000000000000),
        (0xC01FFFFFFFFFFFFF, 0xC000000000000000),
        (0xC020000000000001, 0xC000000000000000),
        (0x403B000000000000, 0x4008000000000000),
        (0x403B000000000001, 0x4008000000000000),
        (0x403AFFFFFFFFFFFF, 0x4008000000000000),
        (0x0010000000000000, 0x2AA428A2F98D728B),
        (0x0010000000000001, 0x2AA428A2F98D728B),
        (0x000FFFFFFFFFFFFF, 0x2AA428A2F98D728B),
        (0x7FE0000000000000, 0x5540000000000000),
        (0x7FE0000000000001, 0x5540000000000000),
        (0x7FDFFFFFFFFFFFFF, 0x5540000000000000),
        (0x01A56E1FC2F8F359, 0x2B2BFF2EE48E0530),
        (0x01A56E1FC2F8F35A, 0x2B2BFF2EE48E0530),
        (0x01A56E1FC2F8F358, 0x2B2BFF2EE48E052F),
        (0x7E37E43C8800759C, 0x54B249AD2594C37D),
        (0x7E37E43C8800759D, 0x54B249AD2594C37D),
        (0x7E37E43C8800759B, 0x54B249AD2594C37D),
        (0xE26BE039B390FE8F, 0xCB6E8FC575F65452),
        (0x94E80D0939487774, 0xB19717A11C195A55),
        (0x5228521A0AF38C50, 0x4602658AD4BF4D6A),
        (0x4CC128793FB66264, 0x4434A2424847095E),
        (0x3887998AF71D2EFA, 0x3D76F26EB75A1D4F),
        (0x77418BD47647C447, 0x52607FE852B3DF5F),
        (0x1C25DCF7A6D0893B, 0x3401C13F6A77D2F8),
        (0xD0E3DA06BC386165, 0xC595A959819413D5),
        (0xE803BBC631A1CCA6, 0xCD4B3CCFCD832298),
        (0x7422A04FFD6680D3, 0x515534D01369EA8D),
        (0xB383041B584D7FBF, 0xBBCAE7434E5FB884),
        (0x87E055E8A9F58DA6, 0xAD401C703982FB51),
        (0xE6EACD7B797C2A7A, 0xCCEE2A0CCFEBACB5),
        (0x9909CD2D3046F192, 0xB2F7A3B5C104B313),
        (0x0EA73976F48AFDED, 0x2F821DB124A9373A),
        (0x60A412BAEA3545A2, 0x4AD5BDE5DBBDE07F),
        (0x0D0BD1E28E76A55A, 0x2EF83D91B0700D19),
        (0x83EA9277126DD011, 0xABEE13D8B7B36846),
        (0xCCA680553F33BDEA, 0xC42C74A869A114F1),
        (0x96895B80BEAF4E6F, 0xB222A78812BDC358),
        (0xFBA9FCCECBBE47A3, 0xD3D7B238CB857CDE),
        (0x374053775BF1D194, 0x3D0991DCE539AFD8),
        (0xAB0261E5C26A8D8A, 0xB8F51D059613341E),
        (0x9787669ED22C050C, 0xB276E1E19A221D38),
    ];

    /// `StrictMath.log10`, 62 vectors from Temurin 25.0.3+9.
    const LOG10_VECTORS: &[(u64, u64)] = &[
        (0x0000000000000000, 0xFFF0000000000000),
        (0x8000000000000000, 0xFFF0000000000000),
        (0x3FF0000000000000, 0x0000000000000000),
        (0xBFF0000000000000, 0xFFF8000000000000),
        (0x4000000000000000, 0x3FD34413509F79FF),
        (0xC000000000000000, 0xFFF8000000000000),
        (0x3FE0000000000000, 0xBFD34413509F79FF),
        (0xBFE0000000000000, 0xFFF8000000000000),
        (0x0000000000000001, 0xC07434E6420F4374),
        (0x8000000000000001, 0xFFF8000000000000),
        (0x0010000000000000, 0xC0733A7146F72A42),
        (0x8010000000000000, 0xFFF8000000000000),
        (0x000FFFFFFFFFFFFF, 0xC0733A7146F72A42),
        (0x7FEFFFFFFFFFFFFF, 0x40734413509F79FF),
        (0xFFEFFFFFFFFFFFFF, 0xFFF8000000000000),
        (0x7FF0000000000000, 0x7FF0000000000000),
        (0xFFF0000000000000, 0xFFF8000000000000),
        (0x7FF8000000000000, 0x7FF8000000000000),
        (0x3FEFFFFFFFFFFFFF, 0xBC8BCB7B1526E50E),
        (0x3FF0000000000001, 0x3C9BCB7B1526E50D),
        (0x3FF0000000000000, 0x0000000000000000),
        (0x3FF0000000000001, 0x3C9BCB7B1526E50D),
        (0x3FEFFFFFFFFFFFFF, 0xBC8BCB7B1526E50E),
        (0x4024000000000000, 0x3FF0000000000000),
        (0x4024000000000001, 0x3FF0000000000000),
        (0x4023FFFFFFFFFFFF, 0x3FEFFFFFFFFFFFFF),
        (0x4059000000000000, 0x4000000000000000),
        (0x4059000000000001, 0x4000000000000000),
        (0x4058FFFFFFFFFFFF, 0x4000000000000000),
        (0x01A56E1FC2F8F359, 0xC072C00000000000),
        (0x01A56E1FC2F8F35A, 0xC072C00000000000),
        (0x01A56E1FC2F8F358, 0xC072C00000000000),
        (0x7E37E43C8800759C, 0x4072C00000000000),
        (0x7E37E43C8800759D, 0x4072C00000000000),
        (0x7E37E43C8800759B, 0x4072C00000000000),
        (0x0010000000000000, 0xC0733A7146F72A42),
        (0x0010000000000001, 0xC0733A7146F72A42),
        (0x000FFFFFFFFFFFFF, 0xC0733A7146F72A42),
        (0x1FC9446255FDE2D0, 0xC0635A9FD7722F99),
        (0x6FCBD3E7A0AF9C94, 0x406CD0E7DF8E4422),
        (0x102F15CBC0202E05, 0xC06CBFFC046FF5DC),
        (0x4DC8128E753A7C0A, 0x4050AD1F362CC29E),
        (0x39A2632B82A9DDB2, 0xC03E57F81C881537),
        (0x4A637C186FDB8DEA, 0x40492DC550B4E23C),
        (0x038DABB1A00371A7, 0xC0722D3EA96F05A9),
        (0x352792EB1E2AEA63, 0xC049F476E0233908),
        (0x696ED5CB443A3323, 0x4068FBC51FB2BF02),
        (0x32A3AADBA08D965D, 0xC05001E79E0EB505),
        (0x460D1BD924BFB578, 0x403D75B6589EEFC2),
        (0x5E0A0706F847AEBD, 0x4062203735DEA457),
        (0x3EC49025450E3CEF, 0xC0167141A4D48AD5),
        (0x49C080DE006F83E7, 0x4047A33885C9760E),
        (0x03616C13B2F22FE8, 0xC0723A93CC2E861F),
        (0x2B8C0C8443F86984, 0xC0588C5A528259F7),
        (0x28649E987696F645, 0xC05C58339BBE6E6C),
        (0x4AAFAE5B09343E85, 0x4049E2EB697FF1C9),
        (0x486E4DE92B77EB09, 0x4044754DB7AD6775),
        (0x132CB5DB6396F166, 0xC06AF2B4F1CA00FB),
        (0x24A24075563C443B, 0xC0606FC605A5A5B3),
        (0x34AB51A5477784B7, 0xC04B2085E0006471),
        (0x194561148A8F1510, 0xC06746C635889E87),
        (0x028FD0D69D3E57D7, 0xC07279D2CB182256),
    ];

    /// `StrictMath.log1p`, 71 vectors from Temurin 25.0.3+9.
    const LOG1P_VECTORS: &[(u64, u64)] = &[
        (0x0000000000000000, 0x0000000000000000),
        (0x8000000000000000, 0x8000000000000000),
        (0x3FF0000000000000, 0x3FE62E42FEFA39EF),
        (0xBFF0000000000000, 0xFFF0000000000000),
        (0x4000000000000000, 0x3FF193EA7AAD030A),
        (0xC000000000000000, 0x7FF8000000000000),
        (0x3FE0000000000000, 0x3FD9F323ECBF984C),
        (0xBFE0000000000000, 0xBFE62E42FEFA39EF),
        (0x0000000000000001, 0x0000000000000001),
        (0x8000000000000001, 0x8000000000000001),
        (0x0010000000000000, 0x0010000000000000),
        (0x8010000000000000, 0x8010000000000000),
        (0x000FFFFFFFFFFFFF, 0x000FFFFFFFFFFFFF),
        (0x7FEFFFFFFFFFFFFF, 0x40862E42FEFA39EF),
        (0xFFEFFFFFFFFFFFFF, 0x7FF8000000000000),
        (0x7FF0000000000000, 0x7FF0000000000000),
        (0xFFF0000000000000, 0x7FF8000000000000),
        (0x7FF8000000000000, 0x7FF8000000000000),
        (0x3FEFFFFFFFFFFFFF, 0x3FE62E42FEFA39EF),
        (0x3FF0000000000001, 0x3FE62E42FEFA39F0),
        (0x3FDA82949A5657FB, 0x3FD62E5616BC4029),
        (0x3FDA82949A5657FC, 0x3FD62E5616BC402A),
        (0x3FDA82949A5657FA, 0x3FD62E5616BC4028),
        (0xBFD2BEDFA43FE5C9, 0xBFD62E6B3842A25C),
        (0xBFD2BEDFA43FE5C8, 0xBFD62E6B3842A25B),
        (0xBFD2BEDFA43FE5CA, 0xBFD62E6B3842A25E),
        (0x3E20000000000000, 0x3E1FFFFFFF800000),
        (0x3E20000000000001, 0x3E1FFFFFFF800002),
        (0x3E1FFFFFFFFFFFFF, 0x3E1FFFFFFF7FFFFF),
        (0x3C90000000000000, 0x3C90000000000000),
        (0x3C90000000000001, 0x3C90000000000001),
        (0x3C8FFFFFFFFFFFFF, 0x3C8FFFFFFFFFFFFF),
        (0xBFF0000000000000, 0xFFF0000000000000),
        (0xBFEFFFFFFFFFFFFF, 0xC0425E4F7B2737FA),
        (0xBFF0000000000001, 0x7FF8000000000000),
        (0x3FF0000000000000, 0x3FE62E42FEFA39EF),
        (0x3FF0000000000001, 0x3FE62E42FEFA39F0),
        (0x3FEFFFFFFFFFFFFF, 0x3FE62E42FEFA39EF),
        (0x7E37E43C8800759C, 0x4085963447F87FB5),
        (0x7E37E43C8800759D, 0x4085963447F87FB5),
        (0x7E37E43C8800759B, 0x4085963447F87FB5),
        (0x4330000000000000, 0x404205966F2B4F12),
        (0x4330000000000001, 0x404205966F2B4F12),
        (0x432FFFFFFFFFFFFF, 0x404205966F2B4F12),
        (0x4340000000000000, 0x40425E4F7B2737FA),
        (0x4340000000000001, 0x40425E4F7B2737FA),
        (0x433FFFFFFFFFFFFF, 0x40425E4F7B2737FA),
        (0xBFC56D2D73A34C70, 0xBFC772F0363E0101),
        (0xBF73BB4D6EBA4000, 0xBF73C7822D0BD5BD),
        (0x3FE0FE9183EA1E0C, 0x3FDB4318C84948E0),
        (0x3FBF44FA6862FEC0, 0x3FBD809380836789),
        (0xBFCF9F76ADDEE650, 0xBFD22926CB4B44E2),
        (0x3FE37BC0112D2B96, 0x3FDE6EFA2B8159B3),
        (0x3FEF41A014C5CF64, 0x3FE5CE845BA05F49),
        (0xBFE2D4FB5AAA5490, 0xBFEC6A07F5A76F18),
        (0xBFE36D80EA232FCA, 0xBFEDE5532E969DCF),
        (0xBFDB6F2F7D15A540, 0xBFE1E9A8F284526B),
        (0x3FB16E52D3F9B9A0, 0x3FB0DCF771E27B70),
        (0xBFCEAAA7E465FFC8, 0xBFD18762DB50C459),
        (0x3FA461FE756E4600, 0x3FA3FCCE674F7A8B),
        (0x3FE864B3F65D0C36, 0x3FE221B746D8422F),
        (0x3FBD3CE5C2D93DA0, 0x3FBBAF75C3ACA83E),
        (0xBFEB3FFD9B43CBB2, 0xBFFE857660713582),
        (0xBFEC7A2C5B8446BA, 0xC001A6E4B92B3556),
        (0x3FDDD6E55A813584, 0x3FD87E310BAC39EC),
        (0xBFE20F41D39B9A72, 0xBFEA971948877A57),
        (0x3FEFDE2BD554B2DA, 0x3FE61D546FADD938),
        (0x3FE47561C2EE0C1A, 0x3FDFA26596AB3913),
        (0xBFBDEDF6665FD9E0, 0xBFBFD4237C3E863E),
        (0xBFCC891B7CAB3498, 0xBFD0248A412E24C0),
        (0xBFE9DCCF36992F44, 0xBFFA6BD324DCAF20),
    ];

    /// `StrictMath.expm1`, 77 vectors from Temurin 25.0.3+9.
    const EXPM1_VECTORS: &[(u64, u64)] = &[
        (0x0000000000000000, 0x0000000000000000),
        (0x8000000000000000, 0x8000000000000000),
        (0x3FF0000000000000, 0x3FFB7E151628AED2),
        (0xBFF0000000000000, 0xBFE43A54E4E98864),
        (0x4000000000000000, 0x40198E64B8D4DDAE),
        (0xC000000000000000, 0xBFEBAB5557101F8D),
        (0x3FE0000000000000, 0x3FE4C2531C3C0D38),
        (0xBFE0000000000000, 0xBFD92E9A0720D3EC),
        (0x0000000000000001, 0x0000000000000001),
        (0x8000000000000001, 0x8000000000000001),
        (0x0010000000000000, 0x0010000000000000),
        (0x8010000000000000, 0x8010000000000000),
        (0x000FFFFFFFFFFFFF, 0x000FFFFFFFFFFFFF),
        (0x7FEFFFFFFFFFFFFF, 0x7FF0000000000000),
        (0xFFEFFFFFFFFFFFFF, 0xBFF0000000000000),
        (0x7FF0000000000000, 0x7FF0000000000000),
        (0xFFF0000000000000, 0xBFF0000000000000),
        (0x7FF8000000000000, 0x7FF8000000000000),
        (0x3FEFFFFFFFFFFFFF, 0x3FFB7E151628AED1),
        (0x3FF0000000000001, 0x3FFB7E151628AED6),
        (0x3FD62E42FEFA39EF, 0x3FDA827999FCEF32),
        (0x3FD62E42FEFA39F0, 0x3FDA827999FCEF34),
        (0x3FD62E42FEFA39EE, 0x3FDA827999FCEF30),
        (0x3FF0A2B23F3BAB73, 0x3FFD413CCCFE7798),
        (0x3FF0A2B23F3BAB74, 0x3FFD413CCCFE779B),
        (0x3FF0A2B23F3BAB72, 0x3FFD413CCCFE7795),
        (0x4043687A9F1AF2B1, 0x436FFFFFFFFFFFEC),
        (0x4043687A9F1AF2B2, 0x4370000000000016),
        (0x4043687A9F1AF2B0, 0x436FFFFFFFFFFFAC),
        (0x3C90000000000000, 0x3C90000000000000),
        (0x3C90000000000001, 0x3C90000000000001),
        (0x3C8FFFFFFFFFFFFF, 0x3C8FFFFFFFFFFFFF),
        (0xBFD0000000000000, 0xBFCC5041854DF7D4),
        (0xBFCFFFFFFFFFFFFF, 0xBFCC5041854DF7D4),
        (0xBFD0000000000001, 0xBFCC5041854DF7D6),
        (0x40862E42FEFA39EF, 0x7FEFFFFFFFFFFF2A),
        (0x40862E42FEFA39F0, 0x7FF0000000000000),
        (0x40862E42FEFA39EE, 0x7FEFFFFFFFFFFB2A),
        (0xC043687A9F1AF2B1, 0xBFF0000000000000),
        (0xC043687A9F1AF2B0, 0xBFF0000000000000),
        (0xC043687A9F1AF2B2, 0xBFF0000000000000),
        (0x3FF0000000000000, 0x3FFB7E151628AED2),
        (0x3FF0000000000001, 0x3FFB7E151628AED6),
        (0x3FEFFFFFFFFFFFFF, 0x3FFB7E151628AED1),
        (0xBFF0000000000000, 0xBFE43A54E4E98864),
        (0xBFEFFFFFFFFFFFFF, 0xBFE43A54E4E98864),
        (0xBFF0000000000001, 0xBFE43A54E4E98865),
        (0x4034000000000000, 0x41BCEB088A68E804),
        (0x4034000000000001, 0x41BCEB088A68E821),
        (0x4033FFFFFFFFFFFF, 0x41BCEB088A68E7E7),
        (0x404C000000000000, 0x44FBAED16A6E0DA7),
        (0x404C000000000001, 0x44FBAED16A6E0DDE),
        (0x404BFFFFFFFFFFFF, 0x44FBAED16A6E0D70),
        (0xC00B33D0D3F65A2A, 0xBFEEEEB0381FF317),
        (0x40164921651FA4AB, 0x40705CE5BD1D7F28),
        (0xC0312FDD18DF6004, 0xBFEFFFFFED9067B2),
        (0x402A80BCB18D2DC8, 0x41215C6EDD7E743E),
        (0x403357316BE84C4B, 0x41ADE91CA2F540EF),
        (0x400F42223D947ECC, 0x404861CAD1415BE7),
        (0x4013B0851598F577, 0x40610A8ABBE7ADB3),
        (0xC026CE747A77A118, 0xBFEFFFE898DDF1CE),
        (0x4031A2BDE939BCFF, 0x4185C0082F02B09F),
        (0x4020365E14FC0CB6, 0x40A9E3D340D53A5C),
        (0xC03202DB3A1C99EA, 0xBFEFFFFFF7EA0865),
        (0x3FDD8CA5BD9DA140, 0x3FE2C6EF9BE24F5E),
        (0x401A0A1ADDB691A0, 0x4084F5E70C86BA82),
        (0x401C70FE35A2899A, 0x40931E474B02318D),
        (0x402D96F81A0B4D02, 0x414450A35CDAD8E8),
        (0x4030CE9D8349AEE6, 0x4172FE94D9DF5000),
        (0xC031D4EA72F9D25C, 0xBFEFFFFFF6532625),
        (0xC01454AB474556B8, 0xBFEFCD2EE07D23B8),
        (0x400582B25FBBF1F8, 0x402B6DB2E4A5769D),
        (0xC0205582AC1D3640, 0xBFEFFDACB14252C8),
        (0xC01AA66CECFE3944, 0xBFEFF588043CA4C9),
        (0x4026E801B17353EB, 0x40F6FF376AA9E81E),
        (0xC027C6F91B8A5E56, 0xBFEFFFF198B3F84E),
        (0xC02B794297F1D83C, 0xBFEFFFFDBB2F658D),
    ];

    /// `StrictMath.sinh`, 71 vectors from Temurin 25.0.3+9.
    const SINH_VECTORS: &[(u64, u64)] = &[
        (0x0000000000000000, 0x0000000000000000),
        (0x8000000000000000, 0x8000000000000000),
        (0x3FF0000000000000, 0x3FF2CD9FC44EB982),
        (0xBFF0000000000000, 0xBFF2CD9FC44EB982),
        (0x4000000000000000, 0x400D03CF63B6E1A0),
        (0xC000000000000000, 0xC00D03CF63B6E1A0),
        (0x3FE0000000000000, 0x3FE0ACD00FE63B97),
        (0xBFE0000000000000, 0xBFE0ACD00FE63B97),
        (0x0000000000000001, 0x0000000000000001),
        (0x8000000000000001, 0x8000000000000001),
        (0x0010000000000000, 0x0010000000000000),
        (0x8010000000000000, 0x8010000000000000),
        (0x000FFFFFFFFFFFFF, 0x000FFFFFFFFFFFFF),
        (0x7FEFFFFFFFFFFFFF, 0x7FF0000000000000),
        (0xFFEFFFFFFFFFFFFF, 0xFFF0000000000000),
        (0x7FF0000000000000, 0x7FF0000000000000),
        (0xFFF0000000000000, 0xFFF0000000000000),
        (0x7FF8000000000000, 0x7FF8000000000000),
        (0x3FEFFFFFFFFFFFFF, 0x3FF2CD9FC44EB982),
        (0x3FF0000000000001, 0x3FF2CD9FC44EB984),
        (0x4036000000000000, 0x41DAB5ADB9C43600),
        (0x4036000000000001, 0x41DAB5ADB9C4361A),
        (0x4035FFFFFFFFFFFF, 0x41DAB5ADB9C435E5),
        (0xC036000000000000, 0xC1DAB5ADB9C43600),
        (0xC035FFFFFFFFFFFF, 0xC1DAB5ADB9C435E5),
        (0xC036000000000001, 0xC1DAB5ADB9C4361A),
        (0x3FD62E42FEFA39EF, 0x3FD6A09E667F3BCC),
        (0x3FD62E42FEFA39F0, 0x3FD6A09E667F3BCE),
        (0x3FD62E42FEFA39EE, 0x3FD6A09E667F3BCB),
        (0x3E30000000000000, 0x3E30000000000000),
        (0x3E30000000000001, 0x3E30000000000001),
        (0x3E2FFFFFFFFFFFFF, 0x3E2FFFFFFFFFFFFF),
        (0x3C80000000000000, 0x3C80000000000000),
        (0x3C80000000000001, 0x3C80000000000001),
        (0x3C7FFFFFFFFFFFFF, 0x3C7FFFFFFFFFFFFF),
        (0x40862E42FEFA39EF, 0x7FDFFFFFFFFFFF2A),
        (0x40862E42FEFA39F0, 0x7FE0000000000196),
        (0x40862E42FEFA39EE, 0x7FDFFFFFFFFFFB2A),
        (0x408633CE8FB9F87D, 0x7FEFFFFFFFFFFD3B),
        (0x408633CE8FB9F87E, 0x7FF0000000000000),
        (0x408633CE8FB9F87C, 0x7FEFFFFFFFFFF93B),
        (0x3FF0000000000000, 0x3FF2CD9FC44EB982),
        (0x3FF0000000000001, 0x3FF2CD9FC44EB984),
        (0x3FEFFFFFFFFFFFFF, 0x3FF2CD9FC44EB982),
        (0xBFF0000000000000, 0xBFF2CD9FC44EB982),
        (0xBFEFFFFFFFFFFFFF, 0xBFF2CD9FC44EB982),
        (0xBFF0000000000001, 0xBFF2CD9FC44EB984),
        (0xC009A1EEC006954A, 0xC028978A2033CB5E),
        (0x402351C40FC14C3C, 0x40BE9C8865E48FE2),
        (0x3FFE95D8760E5F30, 0x400A76BA006A0F96),
        (0x4028418873C9E9A4, 0x40F69492F3E6C01C),
        (0xC032A41E3B079F2B, 0xC18DB888A1E588B9),
        (0xC01F785D47167538, 0xC094664901E5EA7F),
        (0xC0339F969519B2AA, 0xC1A3D7D905163D6A),
        (0xC01534C8F38A5C2C, 0xC059148C80B7A955),
        (0x403376B1CC3D6D01, 0x41A0E9E1D0D78640),
        (0x400AB85AFFAB7B6A, 0x402C2F265B1FF401),
        (0x401FA10564BDE951, 0x409539CD1364AF7D),
        (0xC033F4B795F60A7D, 0xC1ABABD7080C616F),
        (0xC02ABB02BDD87BBB, 0xC1137432D095A64B),
        (0xC03198FDC613D0C7, 0xC174EFF1D665D064),
        (0xC026F152282D2A12, 0xC0E76B5C4E2684E4),
        (0xC0267BDA6C517867, 0xC0E29E3C4FE8B542),
        (0xC00C104365EA5CAC, 0xC030ACBCCDDB98DE),
        (0x40309995DF3D8E30, 0x415EE19480C1B286),
        (0xC02E21C400AFA268, 0xC13AA40FF11FAAD2),
        (0x4012DA724EB86827, 0x404BDA726F11B908),
        (0x402931479EF31413, 0x41020861AE1AB33D),
        (0xC02989D88CD4CB8E, 0xC10570275D5593A0),
        (0x401610C1320E8502, 0x405F172F05D2D916),
        (0x402890A4A0478194, 0x40FA5A7064B95429),
    ];

    /// `StrictMath.cosh`, 71 vectors from Temurin 25.0.3+9.
    const COSH_VECTORS: &[(u64, u64)] = &[
        (0x0000000000000000, 0x3FF0000000000000),
        (0x8000000000000000, 0x3FF0000000000000),
        (0x3FF0000000000000, 0x3FF8B07551D9F551),
        (0xBFF0000000000000, 0x3FF8B07551D9F551),
        (0x4000000000000000, 0x400E18FA0DF2D9BC),
        (0xC000000000000000, 0x400E18FA0DF2D9BC),
        (0x3FE0000000000000, 0x3FF20AC1862AE8D0),
        (0xBFE0000000000000, 0x3FF20AC1862AE8D0),
        (0x0000000000000001, 0x3FF0000000000000),
        (0x8000000000000001, 0x3FF0000000000000),
        (0x0010000000000000, 0x3FF0000000000000),
        (0x8010000000000000, 0x3FF0000000000000),
        (0x000FFFFFFFFFFFFF, 0x3FF0000000000000),
        (0x7FEFFFFFFFFFFFFF, 0x7FF0000000000000),
        (0xFFEFFFFFFFFFFFFF, 0x7FF0000000000000),
        (0x7FF0000000000000, 0x7FF0000000000000),
        (0xFFF0000000000000, 0x7FF0000000000000),
        (0x7FF8000000000000, 0x7FF8000000000000),
        (0x3FEFFFFFFFFFFFFF, 0x3FF8B07551D9F550),
        (0x3FF0000000000001, 0x3FF8B07551D9F552),
        (0x4036000000000000, 0x41DAB5ADB9C43600),
        (0x4036000000000001, 0x41DAB5ADB9C4361A),
        (0x4035FFFFFFFFFFFF, 0x41DAB5ADB9C435E5),
        (0xC036000000000000, 0x41DAB5ADB9C43600),
        (0xC035FFFFFFFFFFFF, 0x41DAB5ADB9C435E5),
        (0xC036000000000001, 0x41DAB5ADB9C4361A),
        (0x3FD62E42FEFA39EF, 0x3FF0F876CCDF6CD9),
        (0x3FD62E42FEFA39F0, 0x3FF0F876CCDF6CDA),
        (0x3FD62E42FEFA39EE, 0x3FF0F876CCDF6CD9),
        (0x3E30000000000000, 0x3FF0000000000000),
        (0x3E30000000000001, 0x3FF0000000000000),
        (0x3E2FFFFFFFFFFFFF, 0x3FF0000000000000),
        (0x3C80000000000000, 0x3FF0000000000000),
        (0x3C80000000000001, 0x3FF0000000000000),
        (0x3C7FFFFFFFFFFFFF, 0x3FF0000000000000),
        (0x40862E42FEFA39EF, 0x7FDFFFFFFFFFFF2A),
        (0x40862E42FEFA39F0, 0x7FE0000000000196),
        (0x40862E42FEFA39EE, 0x7FDFFFFFFFFFFB2A),
        (0x408633CE8FB9F87D, 0x7FEFFFFFFFFFFD3B),
        (0x408633CE8FB9F87E, 0x7FF0000000000000),
        (0x408633CE8FB9F87C, 0x7FEFFFFFFFFFF93B),
        (0x3FF0000000000000, 0x3FF8B07551D9F551),
        (0x3FF0000000000001, 0x3FF8B07551D9F552),
        (0x3FEFFFFFFFFFFFFF, 0x3FF8B07551D9F550),
        (0xBFF0000000000000, 0x3FF8B07551D9F551),
        (0xBFEFFFFFFFFFFFFF, 0x3FF8B07551D9F550),
        (0xBFF0000000000001, 0x3FF8B07551D9F552),
        (0x40324F58413F56DC, 0x418557B0F29D5DDC),
        (0x401C83D6D40EC8FB, 0x40837D443D723A24),
        (0x401F67E4DEA4AEA8, 0x409412F63350C178),
        (0xC00DC7CCB449B7FC, 0x4034B277864F0F9F),
        (0xC0200784E3068C68, 0x4097A21E23BCEC44),
        (0x402A339EBD7D0026, 0x410DDDFEF5E12E52),
        (0x3FF5A7A165FE2EF0, 0x4000841C904CA75E),
        (0xC01046699AE16545, 0x403D403545484898),
        (0xC029A8126DC3E08A, 0x4106BDE73F870248),
        (0xC00A1B886BBC789C, 0x402A2D7AF649127A),
        (0x4028A96E83A5A98C, 0x40FBA91B1E18D656),
        (0x40319188E7771433, 0x41745613BC89430B),
        (0x40302B788B0B301C, 0x415415F519362269),
        (0xC01A0B72CDD87E0C, 0x407504F83F99C5D1),
        (0x3FF6E5D381BD89BC, 0x4001B08539844260),
        (0xC033D24CB6614DCC, 0x41A830B5E1C84427),
        (0xC033DBDF4A3FB8D6, 0x41A91CA84CE67D12),
        (0xC0316169F297554D, 0x4170D9F38791BC98),
        (0xC00C46887C023F2A, 0x40312709EC71B000),
        (0x402BDA98EA65B061, 0x41210EBBB39325F3),
        (0xC033E261A6CE1E47, 0x41A9C236693C7842),
        (0xBFDCD561F629F960, 0x3FF1A6C3D06650FB),
        (0x4006F65DC4981A82, 0x4021B2EC92730EE7),
        (0xC0192760E8E3D94F, 0x4070D295DAB6B328),
    ];

    /// `StrictMath.tanh`, 71 vectors from Temurin 25.0.3+9.
    const TANH_VECTORS: &[(u64, u64)] = &[
        (0x0000000000000000, 0x0000000000000000),
        (0x8000000000000000, 0x8000000000000000),
        (0x3FF0000000000000, 0x3FE85EFAB514F394),
        (0xBFF0000000000000, 0xBFE85EFAB514F394),
        (0x4000000000000000, 0x3FEED9505E1BC3D4),
        (0xC000000000000000, 0xBFEED9505E1BC3D4),
        (0x3FE0000000000000, 0x3FDD9353D7568AF3),
        (0xBFE0000000000000, 0xBFDD9353D7568AF3),
        (0x0000000000000001, 0x0000000000000001),
        (0x8000000000000001, 0x8000000000000001),
        (0x0010000000000000, 0x0010000000000000),
        (0x8010000000000000, 0x8010000000000000),
        (0x000FFFFFFFFFFFFF, 0x000FFFFFFFFFFFFF),
        (0x7FEFFFFFFFFFFFFF, 0x3FF0000000000000),
        (0xFFEFFFFFFFFFFFFF, 0xBFF0000000000000),
        (0x7FF0000000000000, 0x3FF0000000000000),
        (0xFFF0000000000000, 0xBFF0000000000000),
        (0x7FF8000000000000, 0x7FF8000000000000),
        (0x3FEFFFFFFFFFFFFF, 0x3FE85EFAB514F394),
        (0x3FF0000000000001, 0x3FE85EFAB514F395),
        (0x4036000000000000, 0x3FF0000000000000),
        (0x4036000000000001, 0x3FF0000000000000),
        (0x4035FFFFFFFFFFFF, 0x3FF0000000000000),
        (0xC036000000000000, 0xBFF0000000000000),
        (0xC035FFFFFFFFFFFF, 0xBFF0000000000000),
        (0xC036000000000001, 0xBFF0000000000000),
        (0x3FD62E42FEFA39EF, 0x3FD5555555555555),
        (0x3FD62E42FEFA39F0, 0x3FD5555555555555),
        (0x3FD62E42FEFA39EE, 0x3FD5555555555555),
        (0x3E30000000000000, 0x3E30000000000000),
        (0x3E30000000000001, 0x3E30000000000001),
        (0x3E2FFFFFFFFFFFFF, 0x3E2FFFFFFFFFFFFF),
        (0x3C80000000000000, 0x3C80000000000000),
        (0x3C80000000000001, 0x3C80000000000001),
        (0x3C7FFFFFFFFFFFFF, 0x3C7FFFFFFFFFFFFF),
        (0x40862E42FEFA39EF, 0x3FF0000000000000),
        (0x40862E42FEFA39F0, 0x3FF0000000000000),
        (0x40862E42FEFA39EE, 0x3FF0000000000000),
        (0x408633CE8FB9F87D, 0x3FF0000000000000),
        (0x408633CE8FB9F87E, 0x3FF0000000000000),
        (0x408633CE8FB9F87C, 0x3FF0000000000000),
        (0x3FF0000000000000, 0x3FE85EFAB514F394),
        (0x3FF0000000000001, 0x3FE85EFAB514F395),
        (0x3FEFFFFFFFFFFFFF, 0x3FE85EFAB514F394),
        (0xBFF0000000000000, 0xBFE85EFAB514F394),
        (0xBFEFFFFFFFFFFFFF, 0xBFE85EFAB514F394),
        (0xBFF0000000000001, 0xBFE85EFAB514F395),
        (0xC03360C558AB4154, 0xBFF0000000000000),
        (0xC0294DDECE2ECE60, 0xBFEFFFFFFFFD2F09),
        (0xBFDC2862FAA6E4F0, 0xBFDA78ACDEC90432),
        (0x4020604F6E5207F2, 0x3FEFFFFFAD0D896B),
        (0xC032B3EE255F85F0, 0xBFEFFFFFFFFFFFFF),
        (0x402F147A2A2C844C, 0x3FEFFFFFFFFFFDC4),
        (0xBFD38EC1920E3440, 0xBFD2F883E44C469F),
        (0x4033434DE410D8A8, 0x3FF0000000000000),
        (0xC032483307CD5A3C, 0xBFEFFFFFFFFFFFFE),
        (0xC01F902A4D89F482, 0xBFEFFFFF69AB2603),
        (0xC0187900C91F606B, 0xBFEFFFEBA758FB9B),
        (0x4031DE47ABFF0893, 0x3FEFFFFFFFFFFFFB),
        (0xC0332C941C299C22, 0xBFF0000000000000),
        (0x40210741CDE90416, 0x3FEFFFFFD4CA49F3),
        (0xC0032A2B7DDD0F6E, 0xBFEF7916F2005220),
        (0x4021D10E5B18271F, 0x3FEFFFFFEC5B0B74),
        (0xC015313FDB73A5BC, 0xBFEFFF9719319743),
        (0xC02887250D1FC6FA, 0xBFEFFFFFFFF9E117),
        (0xC013610BDC51252D, 0xBFEFFEFC45CC4CB7),
        (0x3FF94F8A1E770B30, 0x3FED67A321FEF957),
        (0x401452DE848CE735, 0x3FEFFF5E0A6777D0),
        (0xC0205D4C19DA15F1, 0xBFEFFFFFAC1223D6),
        (0xC025B9D753E49BAD, 0xBFEFFFFFFF9B25F8),
        (0x3FEB8C9111CC68D0, 0x3FE64B93BFA9D7DA),
    ];

    /// `StrictMath.atan2`, 64 vectors from Temurin 25.0.3+9.
    const ATAN2_VECTORS: &[(u64, u64, u64)] = &[
        (0x0000000000000000, 0xBFF0000000000000, 0x400921FB54442D18),
        (0x8000000000000000, 0x0010000000000000, 0x8000000000000000),
        (0x3FF0000000000000, 0x7FF8000000000000, 0x7FF8000000000000),
        (0xBFF0000000000000, 0x0000000000000001, 0xBFF921FB54442D18),
        (0x4000000000000000, 0x7E37E43C8800759B, 0x01B56E1FC2F8F359),
        (0xC000000000000000, 0x3FE0000000000000, 0xBFF5368C951E9CFD),
        (0x3FE0000000000000, 0x7FEFFFFFFFFFFFFF, 0x0002000000000000),
        (0xBFE0000000000000, 0x3FF0000000000000, 0xBFDDAC670561BB4F),
        (0x0000000000000001, 0x01A56E1FC2F8F35A, 0x3B17E43C8800759A),
        (0x8000000000000001, 0x3FF0000000000000, 0x8000000000000001),
        (0x0010000000000000, 0x8000000000000001, 0x3FF921FB54442D1A),
        (0x8010000000000000, 0xFFF0000000000000, 0xC00921FB54442D18),
        (0x000FFFFFFFFFFFFF, 0x0000000000000000, 0x3FF921FB54442D18),
        (0x7FEFFFFFFFFFFFFF, 0x7E37E43C8800759D, 0x3FF921FB52C5E950),
        (0xFFEFFFFFFFFFFFFF, 0xC000000000000000, 0xBFF921FB54442D19),
        (0x7FF0000000000000, 0x000FFFFFFFFFFFFF, 0x3FF921FB54442D18),
        (0xFFF0000000000000, 0x3FF0000000000001, 0xBFF921FB54442D18),
        (0x7FF8000000000000, 0x01A56E1FC2F8F359, 0x7FF8000000000000),
        (0x3FEFFFFFFFFFFFFF, 0x8000000000000000, 0x3FF921FB54442D18),
        (0x3FF0000000000001, 0x0000000000000001, 0x3FF921FB54442D18),
        (0x3FF0000000000000, 0x7FF0000000000000, 0x0000000000000000),
        (0x3FF0000000000001, 0x3FEFFFFFFFFFFFFF, 0x3FE921FB54442D1A),
        (0x3FEFFFFFFFFFFFFF, 0x7E37E43C8800759C, 0x01A56E1FC2F8F358),
        (0x0000000000000000, 0x4000000000000000, 0x0000000000000000),
        (0x0000000000000001, 0x8010000000000000, 0x400921FB54442D18),
        (0x8000000000000001, 0x3FEFFFFFFFFFFFFF, 0x8000000000000001),
        (0x01A56E1FC2F8F359, 0x8000000000000001, 0x3FF921FB54442D19),
        (0x01A56E1FC2F8F35A, 0x0000000000000000, 0x3FF921FB54442D18),
        (0x01A56E1FC2F8F358, 0xBFE0000000000000, 0x400921FB54442D18),
        (0x7E37E43C8800759C, 0xFFEFFFFFFFFFFFFF, 0x400921FB53850B34),
        (0x7E37E43C8800759D, 0x3FF0000000000001, 0x3FF921FB54442D18),
        (0x7E37E43C8800759B, 0x01A56E1FC2F8F358, 0x3FF921FB54442D18),
        (0x736F71A93387B336, 0x3081BA44511291DD, 0x3FF921FB54442D18),
        (0x3C800857E72B9400, 0xCC43DFF50DC12E11, 0x400921FB54442D18),
        (0x202675D97EAD9155, 0xE0AE8CD9D0BBDCB6, 0x400921FB54442D18),
        (0x72E848A93A90019D, 0x02AC4EE87B460939, 0x3FF921FB54442D18),
        (0x7DE3372173C225DF, 0xAE859BB15636D01E, 0x3FF921FB54442D19),
        (0xDC00E242FB9045ED, 0x636A3991D00AF1FB, 0xB8849A16EF72B5A2),
        (0x77057506E5B4621C, 0x224F2E723EAC5182, 0x3FF921FB54442D18),
        (0x1A62A453038F1656, 0x0FC68FA03F118C46, 0x3FF921FB54442D18),
        (0xF0693223F6312B9B, 0xC68F71B0AD8C8043, 0xBFF921FB54442D19),
        (0xCDA8C289F58A2C5A, 0x1B4000707C6AC04C, 0xBFF921FB54442D18),
        (0x6DE01B8FC4DF6D29, 0x6F23464324DFA7E7, 0x3EAABE0D55D5E876),
        (0xF509E121DA772EB4, 0xE8E1AF893478FB4E, 0xBFF921FB54442D19),
        (0xCE86148C9ECC1712, 0xAAAEB1DF0B285A7F, 0xBFF921FB54442D19),
        (0x072D232EAA930FB1, 0x2960805480E456AA, 0x1DBC4094CD81F17F),
        (0xE38BB271FF66EA9C, 0x096F2CF368C23F59, 0xBFF921FB54442D18),
        (0x428FA728D65DDCCD, 0x6A4F5520D5AFD126, 0x183029E3AE058BCC),
        (0x6EA7B4DAC95A2CC0, 0x904F49CA049361E6, 0x3FF921FB54442D19),
        (0x67831DB5A19C3B8E, 0x7C0BDD56C977067F, 0x2B65F3FC846DFDC5),
        (0xF082C60EA22C9C2B, 0x912070DBD5BC0CEE, 0xBFF921FB54442D19),
        (0x58AE66BC34330FA5, 0x69E2CEFB5500112E, 0x2EB9DC9A658BFCB5),
        (0x0B6F0FB2033E4675, 0x28C2AE7439F37DF2, 0x229A9A5761773B0E),
        (0x3E4560A6C1490642, 0x4963A7628A3D827A, 0x34D1673B08CF572D),
        (0x1B676FACAC8E18A1, 0x68A1CD888EADFCE2, 0x0000000000000000),
        (0xA46555F7E20D02B8, 0xA0814F3B17D89CC3, 0xBFF921FB54442D19),
        (0x5BE85959DD0F46D5, 0x5DEA3BD642432401, 0x3DEDB3764B71FEBA),
        (0x6B27B50F100FA852, 0x758E3DB926A43C0D, 0x3589160CACF0D385),
        (0xDE86AEF3506AE75B, 0x84CF733A92516415, 0xBFF921FB54442D19),
        (0xC08CFE3A18897D75, 0xAFCFF71B982D18CC, 0xBFF921FB54442D19),
        (0x52871F3D261B5CD8, 0x2B20E45861CE10D3, 0x3FF921FB54442D18),
        (0xA6AE13FDF76E63F7, 0x06AF026C733C50EF, 0xBFF921FB54442D18),
        (0x74A2D36FC11C6BCC, 0xCF299E6793CF8189, 0x3FF921FB54442D19),
        (0xC60A59A50C1262BF, 0x54ADB26D52CD6330, 0xB14C64C60F0CC18F),
    ];

    /// `StrictMath.pow`, 87 vectors from Temurin 25.0.3+9.
    const POW_VECTORS: &[(u64, u64, u64)] = &[
        (0x0000000000000000, 0xBFF0000000000000, 0x7FF0000000000000),
        (0x8000000000000000, 0x0010000000000000, 0x0000000000000000),
        (0x3FF0000000000000, 0x7FF8000000000000, 0x7FF8000000000000),
        (0xBFF0000000000000, 0x3FE0000000000001, 0xFFF8000000000000),
        (0x4000000000000000, 0xBFF0000000000001, 0x3FDFFFFFFFFFFFFF),
        (0xC000000000000000, 0x4340000000000000, 0x7FF0000000000000),
        (0x3FE0000000000000, 0x0000000000000001, 0x3FF0000000000000),
        (0xBFE0000000000000, 0xC000000000000000, 0x4010000000000000),
        (0x0000000000000001, 0x000FFFFFFFFFFFFF, 0x3FF0000000000000),
        (0x8000000000000001, 0x3FF0000000000001, 0xFFF8000000000000),
        (0x0010000000000000, 0x3FF0000000000000, 0x0010000000000000),
        (0x8010000000000000, 0x4008000000000001, 0xFFF8000000000000),
        (0x000FFFFFFFFFFFFF, 0x433FFFFFFFFFFFFF, 0x0000000000000000),
        (0x7FEFFFFFFFFFFFFF, 0x0000000000000000, 0x3FF0000000000000),
        (0xFFEFFFFFFFFFFFFF, 0xBFE0000000000000, 0xFFF8000000000000),
        (0x7FF0000000000000, 0xFFEFFFFFFFFFFFFF, 0x0000000000000000),
        (0xFFF0000000000000, 0x4000000000000001, 0x7FF0000000000000),
        (0x7FF8000000000000, 0x3FEFFFFFFFFFFFFF, 0x7FF8000000000000),
        (0x3FEFFFFFFFFFFFFF, 0xC008000000000000, 0x3FF0000000000002),
        (0x3FF0000000000001, 0x41E0000000000001, 0x3FF0000080000200),
        (0x4000000000000000, 0x3FF0000000000000, 0x4000000000000000),
        (0x4000000000000001, 0x8000000000000001, 0x3FF0000000000000),
        (0x3FFFFFFFFFFFFFFF, 0xFFF0000000000000, 0x0000000000000000),
        (0xC000000000000000, 0x3FE0000000000000, 0xFFF8000000000000),
        (0xBFFFFFFFFFFFFFFF, 0xBFEFFFFFFFFFFFFF, 0xFFF8000000000000),
        (0xC000000000000001, 0xC008000000000001, 0xFFF8000000000000),
        (0x3FE0000000000000, 0x0000000000000000, 0x3FF0000000000000),
        (0x3FE0000000000001, 0x4000000000000000, 0x3FD0000000000002),
        (0x3FDFFFFFFFFFFFFF, 0x8010000000000000, 0x3FF0000000000000),
        (0x3FF0000000000000, 0x3FEFFFFFFFFFFFFF, 0x3FF0000000000000),
        (0x3FF0000000000001, 0x3FDFFFFFFFFFFFFF, 0x3FF0000000000000),
        (0x3FEFFFFFFFFFFFFF, 0x4008000000000000, 0x3FEFFFFFFFFFFFFD),
        (0xBFF0000000000000, 0x4340000000000001, 0x3FF0000000000000),
        (0xBFEFFFFFFFFFFFFF, 0x8000000000000001, 0xFFF8000000000000),
        (0xBFF0000000000001, 0x3FE0000000000000, 0xFFF8000000000000),
        (0x4024000000000000, 0x7FEFFFFFFFFFFFFF, 0x7FF0000000000000),
        (0x4024000000000001, 0x4000000000000000, 0x4059000000000003),
        (0x4023FFFFFFFFFFFF, 0x3FF0000000000001, 0x4024000000000002),
        (0x0000000000000000, 0x4007FFFFFFFFFFFF, 0x0000000000000000),
        (0x0000000000000001, 0x41E0000000000000, 0x0000000000000000),
        (0x8000000000000001, 0x8000000000000000, 0x3FF0000000000000),
        (0x0000000000000000, 0x0000000000000001, 0x0000000000000000),
        (0x8000000000000000, 0x7FF0000000000000, 0x0000000000000000),
        (0x3FF0000000000000, 0x3FFFFFFFFFFFFFFF, 0x3FF0000000000000),
        (0xBFF0000000000000, 0xBFF0000000000000, 0xBFF0000000000000),
        (0x4000000000000000, 0xC007FFFFFFFFFFFF, 0x3FC0000000000001),
        (0xC000000000000000, 0x41DFFFFFFFFFFFFF, 0xFFF8000000000000),
        (0x402F97D7AEFEC742, 0xBFDBF7155DF426F0, 0x3FD329B354B701C0),
        (0x4053B4071B3363A2, 0x403255B94356C4F8, 0x4726E21872276028),
        (0x403D27EC3146F971, 0x402E5A6D4DDED4AA, 0x448CBE2F9BA9A53B),
        (0x404EDC01949606E6, 0x4033BEA120573841, 0x47459D9F88493E44),
        (0x40527887CD948715, 0x40066293C1AC0828, 0x4104A8006A33F69D),
        (0x4044E9F2F2DE8922, 0x400B9C9BE7404F94, 0x4118198ED2915DD0),
        (0x40564B17A1C9C60D, 0xBFFF8CCB54B0103C, 0x3F22B3C4B6539BF5),
        (0x40368DE8DF88EB9E, 0xBFF95D03688EFDC8, 0x3F7D52B059E9FDEC),
        (0x402332F5BEA5B253, 0xC024EE923D930A5D, 0x3DCCD6CBCEE6610F),
        (0x403F113240F85589, 0x400AB8CE39D5DE8E, 0x40F790D66D2A0214),
        (0x402FA1BB6D071EA5, 0x40122AF3F6FB2422, 0x41110D8A67EC546F),
        (0x40494803AB359A23, 0xC027BABB288106D4, 0x3BBCC15CB1F60DD5),
        (0x40533BCED37BD22E, 0x3FEFBB9C9579B208, 0x40528C7D1D0569BA),
        (0x4047B76772F54C2B, 0x4029001AEC1619DD, 0x44483B51BDEDFB7F),
        (0x4018B4FC16A4CCF7, 0xC025DE022CCDC040, 0x3E236A3193157FC9),
        (0x401A4D82EB476CF1, 0xC030517CCB51E4D8, 0x3D294BA91F93D815),
        (0x40434D0C23363C57, 0x401E03AB9FE0D6BD, 0x426766403FF27FC9),
        (0x4046CE84E61973A3, 0x4026797EB66E85DD, 0x43CE8CAE1BCA005C),
        (0x405723343C63F9B3, 0x403309DAE18A485A, 0x47B4922F0EEB6662),
        (0x4049F7577A34FE34, 0x40217517BEC996F7, 0x430ABDDE9E1D95D0),
        (0x4057B9153212755B, 0xC02EF1BB5606AE4C, 0x3994C28ECB343F56),
        (0x4052CE5CE7F3610B, 0xC0336AE77ED2E19F, 0x385F4A33C0A0EAA6),
        (0x404121894213FE5B, 0x401AFBE7EB2AD98F, 0x4215090C81F34BCA),
        (0x4029369CED4D8D45, 0xC008960A7DFCA860, 0x3F3B2AD5779CEFCA),
        (0x404C140ED4D196BE, 0xC031757FFBC4D45E, 0x39973FC048A466E9),
        (0x4045B4F9B5D44944, 0x3FA184AC00D4BD80, 0x3FF2341092734A80),
        (0x403D1CBA49031D5E, 0xC02C0806285E12D0, 0x3BAC85CFFB45692B),
        (0x40199DE88FA4A93A, 0xC030EE6175EA47B6, 0x3D18F38933BB4282),
        (0x4057088E4E16AE54, 0xC02D5106A04B61CC, 0x39F4538040880076),
        (0x404FB195207E8422, 0x402E50DEDF896859, 0x459AAC898BD6BFD5),
        (0x40441086FDA0D809, 0xC01B70CD7453005C, 0x3DA5FDA8F96C765B),
        (0x404E16EAC81825F4, 0x40306107C8EE5F35, 0x45FC3CAD06B29451),
        (0x40304F062A1426A2, 0x3FF4BED910356760, 0x4042A9C41033C702),
        (0x405393896F013888, 0xC01FB6F01AEF5494, 0x3CD165022209B1DC),
        (0x404D2BC4B576E470, 0xC031C4F5411F52C8, 0x396B08A5FCE51690),
        (0x405141924DC2EEA6, 0x4031F428A2A02C06, 0x46C9A2D96B6F9C1D),
        (0x40562172D2926A08, 0x40007826ECA82578, 0x40C3E8ED558C33EB),
        (0x4052C058EDF9E70F, 0xC02C4D4392A4B23E, 0x3A6CF12848AA4193),
        (0x4047A2EAED164CFD, 0x402ADF441DA83DF5, 0x449ACC1303284D56),
        (0x4052990B4DB21AAD, 0x402CC632C030CE8A, 0x4585CA01A8C55418),
    ];

    /// `StrictMath.hypot`, 70 vectors from Temurin 25.0.3+9.
    const HYPOT_VECTORS: &[(u64, u64, u64)] = &[
        (0x0000000000000000, 0xBFF0000000000000, 0x3FF0000000000000),
        (0x8000000000000000, 0x0010000000000000, 0x0010000000000000),
        (0x3FF0000000000000, 0x7FF8000000000000, 0x7FF8000000000000),
        (0xBFF0000000000000, 0x4010000000000001, 0x40107E0F66AFED08),
        (0x4000000000000000, 0x20AFFFFFFFFFFFFF, 0x4000000000000000),
        (0xC000000000000000, 0x0000000000000000, 0x4000000000000000),
        (0x3FE0000000000000, 0xBFE0000000000000, 0x3FE6A09E667F3BCD),
        (0xBFE0000000000000, 0xFFEFFFFFFFFFFFFF, 0x7FEFFFFFFFFFFFFF),
        (0x0000000000000001, 0x4008000000000001, 0x4008000000000001),
        (0x8000000000000001, 0x5F2FFFFFFFFFFFFF, 0x5F2FFFFFFFFFFFFF),
        (0x0010000000000000, 0x0000000000000000, 0x0010000000000000),
        (0x8010000000000000, 0x4000000000000000, 0x4000000000000000),
        (0x000FFFFFFFFFFFFF, 0x8010000000000000, 0x0016A09E667F3BCC),
        (0x7FEFFFFFFFFFFFFF, 0x3FEFFFFFFFFFFFFF, 0x7FEFFFFFFFFFFFFF),
        (0xFFEFFFFFFFFFFFFF, 0x400FFFFFFFFFFFFF, 0x7FEFFFFFFFFFFFFF),
        (0x7FF0000000000000, 0x0010000000000000, 0x7FF0000000000000),
        (0xFFF0000000000000, 0x8000000000000000, 0x7FF0000000000000),
        (0x7FF8000000000000, 0x0000000000000001, 0x7FF8000000000000),
        (0x3FEFFFFFFFFFFFFF, 0x7FF0000000000000, 0x7FF0000000000000),
        (0x3FF0000000000001, 0x4007FFFFFFFFFFFF, 0x40094C583ADA5B52),
        (0x4008000000000000, 0x20B0000000000000, 0x4008000000000000),
        (0x4008000000000001, 0x0000000000000001, 0x4008000000000001),
        (0x4007FFFFFFFFFFFF, 0xC000000000000000, 0x400CD82B446159F2),
        (0x4010000000000000, 0x000FFFFFFFFFFFFF, 0x4010000000000000),
        (0x4010000000000001, 0x3FF0000000000001, 0x40107E0F66AFED08),
        (0x400FFFFFFFFFFFFF, 0x5F30000000000000, 0x5F30000000000000),
        (0x5F30000000000000, 0x0010000000000001, 0x5F30000000000000),
        (0x5F30000000000001, 0x3FF0000000000000, 0x5F30000000000001),
        (0x5F2FFFFFFFFFFFFF, 0x8000000000000001, 0x5F2FFFFFFFFFFFFF),
        (0x20B0000000000000, 0xFFF0000000000000, 0x7FF0000000000000),
        (0x20B0000000000001, 0x4010000000000000, 0x4010000000000000),
        (0x20AFFFFFFFFFFFFF, 0x20B0000000000001, 0x20B6A09E667F3BCD),
        (0x0010000000000000, 0x8000000000000001, 0x0010000000000000),
        (0x0010000000000001, 0x3FE0000000000000, 0x3FE0000000000000),
        (0x000FFFFFFFFFFFFF, 0x7FEFFFFFFFFFFFFF, 0x7FEFFFFFFFFFFFFF),
        (0x0000000000000000, 0x4008000000000000, 0x4008000000000000),
        (0x0000000000000001, 0x5F30000000000001, 0x5F30000000000001),
        (0x8000000000000001, 0x000FFFFFFFFFFFFF, 0x000FFFFFFFFFFFFF),
        (0x70A36808C0827290, 0x550FA74F43B50315, 0x70A36808C0827290),
        (0x4520FFC2FD62AEA4, 0x7F265FB1D52F606E, 0x7F265FB1D52F606E),
        (0x6604A3305948DD72, 0x1DAC944A5046E277, 0x6604A3305948DD72),
        (0x216EC0C6F43F1072, 0x3D83BC047888EF48, 0x3D83BC047888EF48),
        (0x524DDABD07FA950D, 0x4C40DFBC476DD031, 0x524DDABD07FA950D),
        (0x4FE2BBBDDA801D3B, 0x1465401668C3C4FD, 0x4FE2BBBDDA801D3B),
        (0x55A6C2D8FE4F4E6C, 0x238B048E25A1877B, 0x55A6C2D8FE4F4E6C),
        (0x24067ABB4A7DDAAC, 0x2CA547CBF3FF0339, 0x2CA547CBF3FF0339),
        (0x3AE1F01F71088801, 0x5CA4153C5DBB4592, 0x5CA4153C5DBB4592),
        (0x47877B84CD22CA17, 0x11E22BDDA8C2E9EB, 0x47877B84CD22CA17),
        (0x09A4F22144B3D21E, 0x18CB9A5A7B7A8639, 0x18CB9A5A7B7A8639),
        (0x73C6721661BA38F3, 0x1769885748C4DBC5, 0x73C6721661BA38F3),
        (0x25E93DE2F83DA77D, 0x4680C10C4634D033, 0x4680C10C4634D033),
        (0x2ECC5B1D46082159, 0x7405D315F97772C0, 0x7405D315F97772C0),
        (0x552FE85DE0D6D5B9, 0x13C2D98C780291CA, 0x552FE85DE0D6D5B9),
        (0x5D2F5E7C884C9A89, 0x3B28425079D8A495, 0x5D2F5E7C884C9A89),
        (0x02A230FEF8EE75D2, 0x3A6325A695FB0D4B, 0x3A6325A695FB0D4B),
        (0x306181CBBF14D963, 0x5C69030BE12FF744, 0x5C69030BE12FF744),
        (0x3F28DC253A4274AD, 0x0921F73FB6515588, 0x3F28DC253A4274AD),
        (0x51832AD51CFBC0EF, 0x4547BFA56EAEC591, 0x51832AD51CFBC0EF),
        (0x4BE45DA5B948AB92, 0x11811ECC09645A54, 0x4BE45DA5B948AB92),
        (0x2EC37215F8BC02D5, 0x0F66764C3E249382, 0x2EC37215F8BC02D5),
        (0x752FD34A7E7288AC, 0x4945A9D4CB14050A, 0x752FD34A7E7288AC),
        (0x79E84542CC35CFE5, 0x68E89F844A3972E3, 0x79E84542CC35CFE5),
        (0x2168CF4F680D478D, 0x3F62F177D9ED6507, 0x3F62F177D9ED6507),
        (0x3C6859C09F4A1891, 0x0560A4A400E077C2, 0x3C6859C09F4A1891),
        (0x4105F2E435C0B9A7, 0x3F68F8E591FFEC2C, 0x4105F2E435C0B9A8),
        (0x5EA0EF2BCDAD5D6E, 0x7685A658D2F43CF3, 0x7685A658D2F43CF3),
        (0x386658AE3CEE9ED4, 0x5D401BE5B16DBB5D, 0x5D401BE5B16DBB5D),
        (0x236161D119140FB7, 0x3D2B9320942F788C, 0x3D2B9320942F788C),
        (0x78C3CF452B748DCE, 0x6EEC0D51FD08FF04, 0x78C3CF452B748DCE),
        (0x66E68D34035A81CA, 0x44A78DD034F3A06A, 0x66E68D34035A81CA),
    ];

    /// `StrictMath.IEEEremainder`, 81 vectors from Temurin 25.0.3+9.
    const IEEEREMAINDER_VECTORS: &[(u64, u64, u64)] = &[
        (0x0000000000000000, 0xBFF0000000000000, 0x0000000000000000),
        (0x8000000000000000, 0x0010000000000000, 0x8000000000000000),
        (0x3FF0000000000000, 0x7FF8000000000000, 0x7FF8000000000000),
        (0xBFF0000000000000, 0x4000000000000001, 0xBFF0000000000000),
        (0x4000000000000000, 0x000FFFFFFFFFFFFF, 0x0000000800000000),
        (0xC000000000000000, 0xBFF0000000000000, 0x8000000000000000),
        (0x3FE0000000000000, 0x0010000000000000, 0x0000000000000000),
        (0xBFE0000000000000, 0x7FF8000000000000, 0x7FF8000000000000),
        (0x0000000000000001, 0x4000000000000001, 0x0000000000000001),
        (0x8000000000000001, 0x000FFFFFFFFFFFFF, 0x8000000000000001),
        (0x0010000000000000, 0xBFF0000000000000, 0x0010000000000000),
        (0x8010000000000000, 0x0010000000000000, 0x8000000000000000),
        (0x000FFFFFFFFFFFFF, 0x7FF8000000000000, 0x7FF8000000000000),
        (0x7FEFFFFFFFFFFFFF, 0x4000000000000001, 0x3EF8000000000000),
        (0xFFEFFFFFFFFFFFFF, 0x000FFFFFFFFFFFFF, 0x8000000000020000),
        (0x7FF0000000000000, 0xBFF0000000000000, 0xFFF8000000000000),
        (0xFFF0000000000000, 0x0010000000000000, 0xFFF8000000000000),
        (0x7FF8000000000000, 0x7FF8000000000000, 0x7FF8000000000000),
        (0x3FEFFFFFFFFFFFFF, 0x4000000000000001, 0x3FEFFFFFFFFFFFFF),
        (0x3FF0000000000001, 0x000FFFFFFFFFFFFF, 0x0000000800000000),
        (0x3FF8000000000000, 0xBFF0000000000000, 0xBFE0000000000000),
        (0x3FF8000000000001, 0x0010000000000000, 0x0000000000000000),
        (0x3FF7FFFFFFFFFFFF, 0x7FF8000000000000, 0x7FF8000000000000),
        (0x4004000000000000, 0x4000000000000001, 0x3FDFFFFFFFFFFFF8),
        (0x4004000000000001, 0x000FFFFFFFFFFFFF, 0x0000001200000000),
        (0x4003FFFFFFFFFFFF, 0xBFF0000000000000, 0x3FDFFFFFFFFFFFF8),
        (0x4008000000000000, 0x0010000000000000, 0x0000000000000000),
        (0x4008000000000001, 0x7FF8000000000000, 0x7FF8000000000000),
        (0x4007FFFFFFFFFFFF, 0x4000000000000001, 0x3FEFFFFFFFFFFFF8),
        (0xC008000000000000, 0x000FFFFFFFFFFFFF, 0x8000000C00000000),
        (0xC007FFFFFFFFFFFF, 0xBFF0000000000000, 0x3CC0000000000000),
        (0xC008000000000001, 0x0010000000000000, 0x8000000000000000),
        (0x0000000000000000, 0x7FF8000000000000, 0x7FF8000000000000),
        (0x0000000000000001, 0x4000000000000001, 0x0000000000000001),
        (0x8000000000000001, 0x000FFFFFFFFFFFFF, 0x8000000000000001),
        (0x7E37E43C8800759C, 0xBFF0000000000000, 0x0000000000000000),
        (0x7E37E43C8800759D, 0x0010000000000000, 0x0000000000000000),
        (0x7E37E43C8800759B, 0x7FF8000000000000, 0x7FF8000000000000),
        (0x0010000000000000, 0x4000000000000001, 0x0010000000000000),
        (0x0010000000000001, 0x000FFFFFFFFFFFFF, 0x0000000000000002),
        (0x000FFFFFFFFFFFFF, 0xBFF0000000000000, 0x000FFFFFFFFFFFFF),
        (0x2C826CB7E8A02595, 0x13805B8E5E6FFE8C, 0x936DD2F38E062290),
        (0x3C8AA40F5B8F9EAB, 0x1C43BC04B9F77993, 0x9BFCF610594BEBA0),
        (0x5E4C69ECB608F742, 0x1ACFE778F3ACD4D8, 0x1A2B909D15F04000),
        (0xD2A2C1A1E137E2CA, 0x146F6F4256904414, 0x1455B1E5D6608B00),
        (0x7D203A05EBD266FC, 0x7126B72716201BF7, 0xF1025D6946AF1D28),
        (0xC9A0AFE9661F217E, 0x2A61C87A1253DB3E, 0xAA4775C11C5AE7D8),
        (0x3B882373BF9AFAE1, 0x6901B8DB498802E8, 0x3B882373BF9AFAE1),
        (0x96C11F5426764AB1, 0x394E68B40C3BE349, 0x96C11F5426764AB1),
        (0x24EE70F9C6B18610, 0x4CE0729284EBAF40, 0x24EE70F9C6B18610),
        (0xB223378C6706E998, 0x50C39F2B4AE43CFC, 0xB223378C6706E998),
        (0x3C2B4942B2CFD42C, 0x6B6A17CAB25B0345, 0x3C2B4942B2CFD42C),
        (0x3109ED07BDAB9CBE, 0x1F0A5184E4309242, 0x1EF92F9E67F73294),
        (0xC8052B48B03C7B52, 0x03C8B647D35D1574, 0x83B475825BFAB350),
        (0xEC225E58F065741D, 0x48231F4346A8E9B4, 0xC7C9D50946355E00),
        (0xCC0900E1D6654010, 0x4D2E406357671164, 0xCC0900E1D6654010),
        (0xC20AB74E835ADC52, 0x4D250EDBDD84136C, 0xC20AB74E835ADC52),
        (0xA8ECD6F34943D85C, 0x150023AFBCE99D1D, 0x94E66401BE879CB0),
        (0xE6A24D63B693EA2C, 0x22A16A1F4782679B, 0x228A5E3E729696E0),
        (0x9745FB173D8465A0, 0x69CDE4F3D3D97B57, 0x9745FB173D8465A0),
        (0x1766A8177C6A082B, 0x25EBA0CE1C8EC7B5, 0x1766A8177C6A082B),
        (0x5ACA10B8607A034D, 0x2A81F6C0326BCA4A, 0xAA6E88B57C8F7B80),
        (0xC5094116A8D347AE, 0x02C2E395703AAD30, 0x82AD72D54DFF6280),
        (0x2720B3F5FA57169C, 0x61063E36CA083FEC, 0x2720B3F5FA57169C),
        (0x5D60CB6E847979D5, 0x380BFEC231FC5D27, 0xB7ABC69C7B33CE80),
        (0x168706C80CAFF565, 0x4F2889C449FFEFD1, 0x168706C80CAFF565),
        (0x28C70E149E44C86D, 0x58A59B73622CA668, 0x28C70E149E44C86D),
        (0x1AEB1D396F7CCD31, 0x4A002FC5D7D7CC11, 0x1AEB1D396F7CCD31),
        (0xAB2CA58638BB5D5E, 0x440EC7124B58EFB3, 0xAB2CA58638BB5D5E),
        (0x868E37B8DA07C0F1, 0x380B2F7E477801D3, 0x868E37B8DA07C0F1),
        (0x702FA15BAA22CF69, 0x7881444AD1245E2B, 0x702FA15BAA22CF69),
        (0xAC0A26D0BFCCCB02, 0x408514CA740FDB7A, 0xAC0A26D0BFCCCB02),
        (0xA9A4BDF64B5C1549, 0x468705F18849904F, 0xA9A4BDF64B5C1549),
        (0xA860A1BE5D1F6269, 0x6A8F976F6296C581, 0xA860A1BE5D1F6269),
        (0x8E6F3FAF25D3BC67, 0x0C0F1B6D88F05FFF, 0x0BF4B89BED0DAB06),
        (0xD7A7D9BCC1F5FD68, 0x2E2808F5835AC086, 0x2DE4E49A63B5F660),
        (0x6DAE044071F29EC4, 0x6FA397964E0F6644, 0x6DAE044071F29EC4),
        (0xBC2BB615F593A0D8, 0x37ED30742101C21E, 0xB7C7C9B4807E2288),
        (0x14608832E3A863CB, 0x1D6B6635ADAF2602, 0x14608832E3A863CB),
        (0x9040F91B2EF48AF6, 0x1C87C1B3DF83975F, 0x9040F91B2EF48AF6),
        (0xDC8A3197370A4A1E, 0x39CAE98F55B8DEC1, 0x39618569778CF6C0),
    ];

    // -----------------------------------------------------------------------
    // The rest of the family. Same discipline as `log` above: bits, not a
    // tolerance. A tolerance here would pass on platform libm and prove
    // nothing — which is exactly how the one-ULP `nextGaussian` divergence
    // survived a green suite for months, behind a `1e-12` whose own comment
    // named this gap as its reason.
    // -----------------------------------------------------------------------

    /// Assert a unary function reproduces HotSpot's `StrictMath` bit for bit.
    ///
    /// NaN is compared as NaN-ness, not as a bit pattern: the spec fixes that
    /// the result is *a* NaN and says nothing about its payload or sign, and
    /// the payload a given machine's default quiet NaN carries is a property of
    /// the hardware. Everything else is an exact `u64` comparison.
    fn check_un(name: &str, f: fn(f64) -> f64, vectors: &[(u64, u64)]) {
        let mut checked = 0usize;
        for &(xb, want) in vectors {
            let x = f64::from_bits(xb);
            let got = f(x);
            if f64::from_bits(want).is_nan() {
                assert!(
                    got.is_nan(),
                    "{name}({x:?}) [{xb:#018x}]: expected NaN, got {got:?}"
                );
            } else {
                assert_eq!(
                    got.to_bits(),
                    want,
                    "{name}({x:?}) [{xb:#018x}]: got {:#018x} ({got:?}), want {want:#018x} ({:?})",
                    got.to_bits(),
                    f64::from_bits(want)
                );
            }
            checked += 1;
        }
        // A table that silently became empty is a test that cannot fail.
        assert!(
            checked >= 40,
            "{name}: only {checked} vectors, table truncated?"
        );
    }

    /// Two-argument form of [`check_un`].
    fn check_bin(name: &str, f: fn(f64, f64) -> f64, vectors: &[(u64, u64, u64)]) {
        let mut checked = 0usize;
        for &(xb, yb, want) in vectors {
            let x = f64::from_bits(xb);
            let y = f64::from_bits(yb);
            let got = f(x, y);
            if f64::from_bits(want).is_nan() {
                assert!(
                    got.is_nan(),
                    "{name}({x:?}, {y:?}) [{xb:#018x}, {yb:#018x}]: expected NaN, got {got:?}"
                );
            } else {
                assert_eq!(
                    got.to_bits(),
                    want,
                    "{name}({x:?}, {y:?}) [{xb:#018x}, {yb:#018x}]: got {:#018x} ({got:?}), \
                     want {want:#018x} ({:?})",
                    got.to_bits(),
                    f64::from_bits(want)
                );
            }
            checked += 1;
        }
        assert!(
            checked >= 40,
            "{name}: only {checked} vectors, table truncated?"
        );
    }

    #[test]
    fn sin_matches_hotspot_strictmath_bit_for_bit() {
        check_un("sin", super::sin, SIN_VECTORS);
    }

    #[test]
    fn cos_matches_hotspot_strictmath_bit_for_bit() {
        check_un("cos", super::cos, COS_VECTORS);
    }

    #[test]
    fn tan_matches_hotspot_strictmath_bit_for_bit() {
        check_un("tan", super::tan, TAN_VECTORS);
    }

    #[test]
    fn asin_matches_hotspot_strictmath_bit_for_bit() {
        check_un("asin", super::asin, ASIN_VECTORS);
    }

    #[test]
    fn acos_matches_hotspot_strictmath_bit_for_bit() {
        check_un("acos", super::acos, ACOS_VECTORS);
    }

    #[test]
    fn atan_matches_hotspot_strictmath_bit_for_bit() {
        check_un("atan", super::atan, ATAN_VECTORS);
    }

    #[test]
    fn exp_matches_hotspot_strictmath_bit_for_bit() {
        check_un("exp", super::exp, EXP_VECTORS);
    }

    #[test]
    fn cbrt_matches_hotspot_strictmath_bit_for_bit() {
        check_un("cbrt", super::cbrt, CBRT_VECTORS);
    }

    #[test]
    fn log10_matches_hotspot_strictmath_bit_for_bit() {
        check_un("log10", super::log10, LOG10_VECTORS);
    }

    #[test]
    fn log1p_matches_hotspot_strictmath_bit_for_bit() {
        check_un("log1p", super::log1p, LOG1P_VECTORS);
    }

    #[test]
    fn expm1_matches_hotspot_strictmath_bit_for_bit() {
        check_un("expm1", super::expm1, EXPM1_VECTORS);
    }

    #[test]
    fn sinh_matches_hotspot_strictmath_bit_for_bit() {
        check_un("sinh", super::sinh, SINH_VECTORS);
    }

    #[test]
    fn cosh_matches_hotspot_strictmath_bit_for_bit() {
        check_un("cosh", super::cosh, COSH_VECTORS);
    }

    #[test]
    fn tanh_matches_hotspot_strictmath_bit_for_bit() {
        check_un("tanh", super::tanh, TANH_VECTORS);
    }

    #[test]
    fn atan2_matches_hotspot_strictmath_bit_for_bit() {
        check_bin("atan2", super::atan2, ATAN2_VECTORS);
    }

    #[test]
    fn pow_matches_hotspot_strictmath_bit_for_bit() {
        check_bin("pow", super::pow, POW_VECTORS);
    }

    #[test]
    fn hypot_matches_hotspot_strictmath_bit_for_bit() {
        check_bin("hypot", super::hypot, HYPOT_VECTORS);
    }

    #[test]
    fn ieee_remainder_matches_hotspot_strictmath_bit_for_bit() {
        check_bin(
            "IEEEremainder",
            super::ieee_remainder,
            IEEEREMAINDER_VECTORS,
        );
    }

    /// `IEEEremainder` rounds the quotient to the NEAREST integer, **ties to
    /// even** — not `round()`, which is ties-away.
    ///
    /// This is the one function in the family whose previous implementation was
    /// wrong by more than a ULP: it computed `x - (x/p).round() * p`. Every
    /// half-integer quotient came out with the wrong sign of remainder, and
    /// `x / p` overflowed to infinity for operands whose remainder is perfectly
    /// ordinary — 49.8% of sampled pairs disagreed with fdlibm, with unbounded
    /// error. Asserted separately from the table so a regression names the
    /// property rather than "vector #17".
    #[test]
    fn ieee_remainder_rounds_the_quotient_to_even() {
        // 1.5/1 and 2.5/1 both tie; ties-to-even sends them to 2 and 2.
        assert_eq!(super::ieee_remainder(1.5, 1.0), -0.5);
        assert_eq!(super::ieee_remainder(2.5, 1.0), 0.5);
        assert_eq!(super::ieee_remainder(0.5, 1.0), 0.5);
        assert_eq!(super::ieee_remainder(-1.5, 1.0), 0.5);
        // The overflow case: x/p is +inf, but the remainder is representable.
        let r = super::ieee_remainder(f64::MAX, f64::MIN_POSITIVE);
        assert!(
            r.is_finite(),
            "remainder of MAX by MIN_NORMAL must be finite, got {r:?}"
        );
        // Spec special cases.
        assert!(super::ieee_remainder(1.0, 0.0).is_nan());
        assert!(super::ieee_remainder(f64::INFINITY, 1.0).is_nan());
        assert_eq!(super::ieee_remainder(1.0, f64::INFINITY), 1.0);
    }

    /// The `sqrt` row of the census is a structural zero, and this test says
    /// why rather than leaving a gap where the other seventeen have coverage.
    ///
    /// IEEE 754 requires `sqrt` to be correctly rounded, so `FdLibm.Sqrt.
    /// compute` and the hardware instruction compute the same function by
    /// construction — there is nothing to port. Confirmed empirically at 100k
    /// samples by `probes/StrictMathOracleDumpProbe.java` (0 disagreements),
    /// and pinned here on the values where a non-conforming implementation
    /// would show first.
    #[test]
    fn sqrt_needs_no_port_because_ieee754_requires_exact_rounding() {
        for &(xb, want) in SQRT_VECTORS {
            let x = f64::from_bits(xb);
            let got = x.sqrt();
            if f64::from_bits(want).is_nan() {
                assert!(got.is_nan(), "sqrt({x:?}) should be NaN, got {got:?}");
            } else {
                assert_eq!(
                    got.to_bits(),
                    want,
                    "sqrt({x:?}) [{xb:#018x}]: hardware sqrt disagrees with fdlibm — \
                     this platform does not round sqrt correctly and DOES need a port"
                );
            }
        }
    }

    /// `StrictMath.sqrt` on values that stress rounding: exact squares, values
    /// straddling a halfway case, subnormals, and the extremes. Captured from
    /// Temurin 25.0.3+9 — deriving them by hand got two of the fourteen wrong.
    const SQRT_VECTORS: &[(u64, u64)] = &[
        (0x0000000000000000, 0x0000000000000000), // +0 -> +0
        (0x8000000000000000, 0x8000000000000000), // -0 -> -0 (sign preserved)
        (0x3FF0000000000000, 0x3FF0000000000000), // 1
        (0x4000000000000000, 0x3FF6A09E667F3BCD), // 2 -> 1.4142135623730951
        (0x4010000000000000, 0x4000000000000000), // 4 -> 2
        (0x4022000000000000, 0x4008000000000000), // 9 -> 3
        (0x0000000000000001, 0x1E60000000000000), // MIN_VALUE (subnormal)
        (0x0010000000000000, 0x2000000000000000), // MIN_NORMAL
        (0x7FEFFFFFFFFFFFFF, 0x5FEFFFFFFFFFFFFF), // MAX_VALUE
        (0x7FF0000000000000, 0x7FF0000000000000), // +inf
        (0xBFF0000000000000, 0xFFF8000000000000), // -1 -> NaN
        (0x7FF8000000000000, 0x7FF8000000000000), // NaN
        (0x3FE0000000000000, 0x3FE6A09E667F3BCD), // 0.5
        (0x3FEFFFFFFFFFFFFF, 0x3FEFFFFFFFFFFFFF), // nextDown(1) — just under a tie
    ];

    // -----------------------------------------------------------------------
    // Overflow sweep — every ported entry point over a wide bit-pattern corpus
    // -----------------------------------------------------------------------
    //
    // fdlibm does signed arithmetic on the extracted exponent/mantissa words
    // and *relies on it wrapping*: C's `hx-0x3ff00000` on an `int32_t` whose
    // sign bit is set, `lx-lp` on words the C declares `uint32_t`. Rust wraps
    // in release and panics in debug, so a direct port is a latent
    // debug-only crash that the golden tables only catch if some vector
    // happens to reach the site — which is exactly how `atan2` and
    // `IEEEremainder` shipped broken while 22 sibling tests stayed green.
    //
    // This sweep exists to make that class of defect impossible to miss: it
    // is a no-panic assertion, run under `debug_assertions` where
    // overflow-checks are on, over a corpus built to reach the sign-bit and
    // extreme-exponent paths the golden tables under-sample. It asserts
    // nothing about VALUES — `check_*` against the HotSpot vectors owns that
    // — only that no operation overflows on the way there.
    fn overflow_sweep_corpus() -> Vec<f64> {
        let mut v: Vec<f64> = Vec::new();
        // Every exponent, at three mantissa positions, both signs. The sign
        // loop is the point: `hi()` is negative for every negative input, and
        // that is what makes `hx - CONST` underflow.
        //
        // The mantissas are chosen for their LOW WORD, not their magnitude.
        // `lo()` reinterprets the low 32 bits as `i32`, so a difference of two
        // low words overflows only when they straddle the `i32` boundary —
        // 0x8000_0000 (the most negative `i32`) against 0x7fff_ffff (the most
        // positive). An earlier version of this corpus used
        // `[0, 1, 0x8_0000_0000_0000, 0xf_ffff_ffff_ffff]`, whose low words are
        // only {0, 1, 0xffff_ffff} = {0, 1, -1}: every difference fits in 2, so
        // the sweep passed against the KNOWN-BROKEN `IEEEremainder` and proved
        // nothing. Verify any change here the same way — put the bug back and
        // watch the test fail.
        for exp in 0u64..=0x7ff {
            for mant in [
                0u64,
                1,
                0x8_0000_0000_0000, // low word 0x0000_0000
                0xf_ffff_ffff_ffff, // low word 0xffff_ffff  (-1)
                0x0_0000_8000_0000, // low word 0x8000_0000  (i32::MIN)
                0x0_0000_7fff_ffff, // low word 0x7fff_ffff  (i32::MAX)
                0xf_ffff_8000_0000, // i32::MIN low word, full high mantissa
                0x7_ffff_7fff_ffff, // i32::MAX low word, other high mantissa
                0x0_0000_c000_0000, // low word 0xc000_0000
                0x0_0000_4000_0000, // low word 0x4000_0000
            ] {
                let bits = (exp << 52) | mant;
                v.push(f64::from_bits(bits));
                v.push(f64::from_bits(bits | 0x8000_0000_0000_0000));
            }
        }
        // The named edges, positive and negative.
        for &b in &[
            0x0000_0000_0000_0000u64, // +0
            0x0000_0000_0000_0001,    // MIN_VALUE (subnormal)
            0x000f_ffff_ffff_ffff,    // max subnormal
            0x0010_0000_0000_0000,    // MIN_NORMAL
            0x3fe0_0000_0000_0000,    // 0.5
            0x3fef_ffff_ffff_ffff,    // nextDown(1)
            0x3ff0_0000_0000_0000,    // 1.0  — atan2's `hx-0x3ff00000` zero case
            0x3ff0_0000_0000_0001,    // nextUp(1)
            0x4000_0000_0000_0000,    // 2.0
            0x7fdf_ffff_ffff_ffff,    // IEEEremainder's `hp <= 0x7fdfffff` edge
            0x7fe0_0000_0000_0000,    // just past it
            0x7fef_ffff_ffff_ffff,    // MAX_VALUE
            0x7ff0_0000_0000_0000,    // +inf
            0x7ff8_0000_0000_0000,    // NaN
            0x7ff0_0000_0000_0001,    // signalling NaN
        ] {
            v.push(f64::from_bits(b));
            v.push(f64::from_bits(b | 0x8000_0000_0000_0000));
        }
        v
    }

    /// The unary family, swept. A panic here is an unwrapped `-`/`+` on an
    /// extracted word, not a value bug.
    #[test]
    #[cfg(debug_assertions)]
    fn no_unary_fdlibm_entry_point_overflows_on_any_exponent() {
        let corpus = overflow_sweep_corpus();
        let unary: &[(&str, fn(f64) -> f64)] = &[
            ("log", super::log),
            ("sin", super::sin),
            ("cos", super::cos),
            ("tan", super::tan),
            ("asin", super::asin),
            ("acos", super::acos),
            ("atan", super::atan),
            ("cbrt", super::cbrt),
            ("exp", super::exp),
            ("log10", super::log10),
            ("log1p", super::log1p),
            ("expm1", super::expm1),
            ("sinh", super::sinh),
            ("cosh", super::cosh),
            ("tanh", super::tanh),
        ];
        for (name, f) in unary {
            for &x in &corpus {
                // Result deliberately unused: this is a no-panic assertion.
                let _ = std::hint::black_box(f(std::hint::black_box(x)));
                let _ = name;
            }
        }
    }

    /// The binary family, swept over the cross product. `atan2` and
    /// `IEEEremainder` both overflowed here before the `wrapping_sub` fix;
    /// the cross product is what reaches a negative `hx` against a positive
    /// `hy` and vice versa.
    #[test]
    #[cfg(debug_assertions)]
    fn no_binary_fdlibm_entry_point_overflows_on_any_exponent_pair() {
        // Stride the corpus for the cross product: the full 2-D sweep is
        // ~16M pairs per function, which is a minute of debug-build time for
        // no extra coverage of the integer paths (they key on the exponent,
        // and every exponent is still represented).
        let corpus = overflow_sweep_corpus();
        let strided: Vec<f64> = corpus.iter().copied().step_by(11).collect();
        let binary: &[(&str, fn(f64, f64) -> f64)] = &[
            ("atan2", super::atan2),
            ("pow", super::pow),
            ("hypot", super::hypot),
            ("IEEEremainder", super::ieee_remainder),
        ];
        for (name, f) in binary {
            for &x in &strided {
                for &y in &strided {
                    let _ =
                        std::hint::black_box(f(std::hint::black_box(x), std::hint::black_box(y)));
                    let _ = name;
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Differential digest against HotSpot StrictMath — millions of points
    // -----------------------------------------------------------------------
    //
    // The `*_VECTORS` tables above are hand-picked and small. They are the
    // right tool for pinning a named edge, and the wrong one for the question
    // "does the RELEASE build, where Rust wraps silently instead of panicking,
    // still produce HotSpot's bits everywhere?".
    //
    // So: build a deterministic corpus, hash every result bit-for-bit, and
    // compare one number against the same number computed by real
    // `StrictMath` (`apps/fdlibm_oracle/FdlibmOracle.java`). 240,960 points per
    // unary function; ~6.2M pairs per binary one. The corpus generator is
    // duplicated verbatim in the Java, so a change to either side must be made
    // to both — the digest will say so loudly.
    //
    // These tests are NOT `#[cfg(debug_assertions)]`: running them in release
    // is the entire point. `cargo test -p cratonvm-types --lib --release`.
    //
    // To localize a mismatch, `java FdlibmOracle --dump <fn>` and diff against
    // the same enumeration here — the digest tells you THAT you diverged, not
    // where.
    const NAN_SENTINEL: u64 = 0x7ff8_0000_0000_0000;
    const STRUCTURED_MANTISSAS: [u64; 10] = [
        0,
        1,
        0x8_0000_0000_0000,
        0xf_ffff_ffff_ffff,
        0x0_0000_8000_0000,
        0x0_0000_7fff_ffff,
        0xf_ffff_8000_0000,
        0x7_ffff_7fff_ffff,
        0x0_0000_c000_0000,
        0x0_0000_4000_0000,
    ];
    const RANDOM_POINTS: usize = 200_000;
    const BIN_STRIDE: usize = 97;

    fn splitmix64(i: u64) -> u64 {
        let x = i.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn corpus_size() -> usize {
        0x800 * STRUCTURED_MANTISSAS.len() * 2 + RANDOM_POINTS
    }

    /// The i-th corpus point. Must match `FdlibmOracle.point` exactly.
    fn point(i: usize) -> f64 {
        let structured = 0x800 * STRUCTURED_MANTISSAS.len() * 2;
        if i < structured {
            let idx = i / 2;
            let neg = i % 2 == 1;
            let exp = (idx / STRUCTURED_MANTISSAS.len()) as u64;
            let mant = STRUCTURED_MANTISSAS[idx % STRUCTURED_MANTISSAS.len()];
            let mut bits = (exp << 52) | mant;
            if neg {
                bits |= 0x8000_0000_0000_0000;
            }
            f64::from_bits(bits)
        } else {
            f64::from_bits(splitmix64((i - structured) as u64))
        }
    }

    fn canon(d: f64) -> u64 {
        if d.is_nan() {
            NAN_SENTINEL
        } else {
            d.to_bits()
        }
    }

    fn fnv(mut h: u64, v: u64) -> u64 {
        for b in 0..8 {
            h ^= (v >> (b * 8)) & 0xff;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h
    }

    fn unary_digest(f: fn(f64) -> f64) -> u64 {
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        for i in 0..corpus_size() {
            h = fnv(h, canon(f(point(i))));
        }
        h
    }

    fn binary_digest(f: fn(f64, f64) -> f64) -> u64 {
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        let n = corpus_size();
        let mut i = 0;
        while i < n {
            let mut j = 0;
            while j < n {
                h = fnv(h, canon(f(point(i), point(j))));
                j += BIN_STRIDE;
            }
            i += BIN_STRIDE;
        }
        h
    }

    /// Every unary entry point, 240,960 points each, against JDK 25
    /// `StrictMath`. Digests produced by `apps/fdlibm_oracle`.
    #[test]
    fn unary_digests_match_hotspot_strictmath() {
        let cases: &[(&str, fn(f64) -> f64, u64)] = &[
            ("log", super::log, 0x850d_59ae_3424_5f2b),
            ("sin", super::sin, 0xff32_e4c2_3073_d95d),
            ("cos", super::cos, 0x1de7_26ca_8273_d984),
            ("tan", super::tan, 0xeb49_07fe_b65f_6bb8),
            ("asin", super::asin, 0xa6ae_71b9_ef8c_7a05),
            ("acos", super::acos, 0x6a30_e215_84f1_b0bc),
            ("atan", super::atan, 0xb99a_080a_58d1_3f24),
            ("cbrt", super::cbrt, 0x951a_6706_50ff_988f),
            ("exp", super::exp, 0x9f52_78e6_653f_288a),
            ("log10", super::log10, 0xa3fd_89d1_687b_0795),
            ("log1p", super::log1p, 0x578c_db90_723a_dfec),
            ("expm1", super::expm1, 0x5829_72f0_847d_0dbb),
            ("sinh", super::sinh, 0x346a_5c2f_3615_e9a0),
            ("cosh", super::cosh, 0x0c83_bbdf_c31b_9b29),
            ("tanh", super::tanh, 0x0f2f_cbf5_dd07_b09c),
        ];
        for &(name, f, want) in cases {
            let got = unary_digest(f);
            assert_eq!(
                got,
                want,
                "{name}: digest {got:#018x} != HotSpot {want:#018x} over {} points — \
                 re-run apps/fdlibm_oracle with --dump {name} to localize",
                corpus_size()
            );
        }
    }

    /// Every binary entry point over the strided cross product (~6.2M pairs
    /// each) against JDK 25 `StrictMath`. `atan2` and `IEEEremainder` are the
    /// two that carried the wrapping-subtraction defect, so their bits in
    /// RELEASE — where the wrap is silent rather than a panic — are the whole
    /// reason this test exists.
    #[test]
    fn binary_digests_match_hotspot_strictmath() {
        let cases: &[(&str, fn(f64, f64) -> f64, u64)] = &[
            ("atan2", super::atan2, 0x8327_323f_2533_8181),
            ("pow", super::pow, 0x5c4c_68f6_e383_2102),
            ("hypot", super::hypot, 0xe1fe_a734_7916_96e1),
            (
                "IEEEremainder",
                super::ieee_remainder,
                0x8aa1_c64f_79b8_b597,
            ),
        ];
        for &(name, f, want) in cases {
            let got = binary_digest(f);
            assert_eq!(
                got, want,
                "{name}: digest {got:#018x} != HotSpot {want:#018x} — \
                 re-run apps/fdlibm_oracle with --dump {name} to localize"
            );
        }
    }
}
