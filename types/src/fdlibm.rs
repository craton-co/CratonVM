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
//! Only `log` is ported here so far. The rest of the `StrictMath`
//! transcendental surface (`sin`/`cos`/`tan`/`asin`/`acos`/`atan`/`atan2`/
//! `exp`/`log10`/`cbrt`/`pow`/`sinh`/`cosh`/`tanh`/`hypot`/`expm1`/`log1p`)
//! still delegates to platform libm and still violates the same contract.

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
