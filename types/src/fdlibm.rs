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
fn kernel_rem_pio2(x: &[f64], y: &mut [f64], e0: i32, nx: usize, prec: usize, ipio2: &[i32]) -> i32 {
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
    if ((hx - 0x3ff0_0000) | lx) == 0 {
        return atan(y); // x = 1.0
    }
    let m = ((hy >> 31) & 1) | ((hx >> 30) & 2); // 2*sign(x) + sign(y)

    if (iy | ly) == 0 {
        return match m {
            0 | 1 => y,                                 // atan(+-0, +anything) = +-0
            2 => std::f64::consts::PI + tiny,           // atan(+0, -anything) = pi
            _ => -std::f64::consts::PI - tiny,          // atan(-0, -anything) = -pi
        };
    }
    if (ix | lx) == 0 {
        return if hy < 0 { -pi_o_2 - tiny } else { pi_o_2 + tiny };
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
        return if hy < 0 { -pi_o_2 - tiny } else { pi_o_2 + tiny };
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
        0 => z,                                    // atan(+, +)
        1 => -z,                                   // atan(-, +)
        2 => std::f64::consts::PI - (z - pi_lo),   // atan(+, -)
        _ => (z - pi_lo) - std::f64::consts::PI,   // atan(-, -)
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
#[allow(clippy::excessive_precision, clippy::neg_multiply, clippy::assign_op_pattern)]
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
        return if jx >= 0 { 1.0 / x + 1.0 } else { 1.0 / x - 1.0 };
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
    if ((hx - hp) | (lx - lp)) == 0 {
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
                assert!(got.is_nan(), "log({x:?}) [{xb:#018x}] should be NaN, got {got:?}");
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
}
