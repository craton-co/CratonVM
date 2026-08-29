// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Native intrinsic for the SunEC P-256 Montgomery field multiply/square —
//! `sun.security.util.math.intpoly.MontgomeryIntegerPolynomialP256.{mult,square}`.
//!
//! These two methods dominate EC keygen/sign/verify time: a single keygen pays a
//! one-time generator-table precompute (`Secp256R1GeneratorMontgomeryMultiplier
//! .<clinit>`) that calls them hundreds of thousands of times, and the JDK
//! bytecode is a ~2.4 kB fully-unrolled limb routine the interpreter runs slowly
//! (and which the JIT does not compile beneficially — see
//! `gaps/ec-nojit-unsignedmultiplyhigh-intrinsic.md`). HotSpot intrinsifies the
//! field arithmetic in native code; this mirrors that.
//!
//! ## Representation (verified against JDK 25 — `ecprobe_tmp/MontGT2`)
//! A field element is stored as `NUM_LIMBS = 5` little-endian limbs of
//! `BITS_PER_LIMB = 52` bits, holding the integer
//! `X = Σ limb[i]·2^(52·i)`  with  `X ≡ value·R (mod p)` — i.e. Montgomery form
//! with `R = 2^260` and `p` the P-256 prime. `MAX_ADDS = 0`, so every stored
//! element is fully reduced: each limb is in `[0, 2^52)` and `X < p`.
//!
//! `mult(a, b, r)` writes `r` such that `decode(r) ≡ decode(a)·decode(b)·R⁻¹
//! (mod p)`. Because the inputs are canonical and the JDK's own output is
//! canonical (`r < p`, every limb `< 2^52`), decoding the limbs to an integer,
//! computing the Montgomery product, and re-encoding in canonical form yields a
//! limb array **byte-identical** to the JDK's — not merely value-equivalent.
//! `square(a, r) == mult(a, a, r)` (verified), so square delegates to mult.
//!
//! The modular arithmetic uses `num-bigint` for auditability; correctness is
//! pinned by `tests::*` against limb vectors captured from the real JDK.

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallResult, RuntimeError};
use cratonvm_types::{ObjectRef, Value};
use num_bigint::BigUint;
use std::sync::OnceLock;

const CLASS: &str = "sun/security/util/math/intpoly/MontgomeryIntegerPolynomialP256";
const NUM_LIMBS: usize = 5;
const BITS: u32 = 52;
const LIMB_MASK: u64 = (1u64 << BITS) - 1;

/// P-256 field prime `p = 2^256 − 2^224 + 2^192 + 2^96 − 1`, big-endian.
const P256_PRIME_BE: [u8; 32] = [
    0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
];

fn p() -> &'static BigUint {
    static P: OnceLock<BigUint> = OnceLock::new();
    P.get_or_init(|| BigUint::from_bytes_be(&P256_PRIME_BE))
}

/// `R⁻¹ mod p` with `R = 2^260`. Computed once via Fermat (`R^(p−2) mod p`,
/// `p` prime) so we avoid an extended-GCD dependency.
fn r_inv() -> &'static BigUint {
    static R_INV: OnceLock<BigUint> = OnceLock::new();
    R_INV.get_or_init(|| {
        let p = p();
        let r_mod = (BigUint::from(1u8) << (NUM_LIMBS as u32 * BITS)) % p;
        let exp = p - 2u32;
        r_mod.modpow(&exp, p)
    })
}

/// Reconstruct `Σ limb[i]·2^(52·i)` as a `BigUint`. Limbs are canonical
/// (`MAX_ADDS = 0` ⇒ each in `[0, 2^52)`), so they are read as unsigned.
#[inline]
fn decode(limbs: &[u64; NUM_LIMBS]) -> BigUint {
    let mut x = BigUint::from(0u8);
    for i in (0..NUM_LIMBS).rev() {
        x = (x << BITS) + BigUint::from(limbs[i]);
    }
    x
}

/// Split a canonical field value (`< p < 2^260`) into `NUM_LIMBS` base-2^52
/// little-endian limbs.
#[inline]
fn encode(mut x: BigUint) -> [i64; NUM_LIMBS] {
    let mask = BigUint::from(LIMB_MASK);
    let mut out = [0i64; NUM_LIMBS];
    for slot in out.iter_mut() {
        // AUDITED (crypto-failure-contract.md): this `unwrap_or(0)` is **not**
        // an error being swallowed. `BigUint` stores zero as an empty digit
        // vector, so `iter_u64_digits().next()` is `None` for exactly one
        // value — zero — and `0` is that value. There is no input for which
        // this substitutes a default for a failure. Verified by
        // `tests::mult_matches_jdk_byte_for_byte`, whose first vector is
        // `0 · x` and whose expected output is all-zero limbs.
        let lo = (&x & &mask).iter_u64_digits().next().unwrap_or(0);
        *slot = lo as i64;
        x >>= BITS;
    }
    out
}

/// `r = a·b·R⁻¹ mod p`, the SunEC Montgomery field product, in canonical
/// limb form (byte-identical to the JDK's `mult` output).
#[inline]
fn mont_mult(a: &[u64; NUM_LIMBS], b: &[u64; NUM_LIMBS]) -> [i64; NUM_LIMBS] {
    let p = p();
    let prod = (decode(a) * decode(b)) % p;
    let xr = (prod * r_inv()) % p;
    encode(xr)
}

fn require_long_array(
    v: Option<&Value>,
    name: &str,
) -> Result<ObjectRef, cratonvm_types::error::MethodCallFailed> {
    match v {
        Some(Value::Object(Some(o))) => Ok(*o),
        _ => Err(RuntimeError::NullPointerException {
            message: Some(format!("{name} must not be null")),
        }
        .into()),
    }
}

/// Read exactly `NUM_LIMBS` `long` elements from `arr` as `u64`.
fn read_limbs(
    ctx: &dyn NativeContext,
    arr: ObjectRef,
    name: &str,
) -> Result<[u64; NUM_LIMBS], cratonvm_types::error::MethodCallFailed> {
    if ctx.array_length(arr) < NUM_LIMBS {
        return Err(RuntimeError::aioobe_index_only(NUM_LIMBS as i32).into());
    }
    let mut out = [0u64; NUM_LIMBS];
    for (i, slot) in out.iter_mut().enumerate() {
        match ctx.get_array_element(arr, i) {
            Value::Long(v) => *slot = v as u64,
            // A non-long element means the caller passed the wrong array kind.
            _ => return Err(RuntimeError::aioobe_index_only(i as i32).into()),
        }
    }
    Ok(out)
}

fn write_limbs(
    ctx: &dyn NativeContext,
    arr: ObjectRef,
    limbs: &[i64; NUM_LIMBS],
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    if ctx.array_length(arr) < NUM_LIMBS {
        return Err(RuntimeError::aioobe_index_only(NUM_LIMBS as i32).into());
    }
    for (i, v) in limbs.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Long(*v));
    }
    Ok(())
}

/// `protected void mult(long[] a, long[] b, long[] r)` — args = [this, a, b, r].
fn native_mont_mult(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let a = require_long_array(args.get(1), "a")?;
    let b = require_long_array(args.get(2), "b")?;
    let r = require_long_array(args.get(3), "r")?;
    let av = read_limbs(ctx, a, "a")?;
    let bv = read_limbs(ctx, b, "b")?;
    let rv = mont_mult(&av, &bv);
    write_limbs(ctx, r, &rv)?;
    Ok(None)
}

/// `protected void square(long[] a, long[] r)` — args = [this, a, r].
/// `square(a) == mult(a, a)` (verified against the JDK).
fn native_mont_square(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let a = require_long_array(args.get(1), "a")?;
    let r = require_long_array(args.get(2), "r")?;
    let av = read_limbs(ctx, a, "a")?;
    let rv = mont_mult(&av, &av);
    write_limbs(ctx, r, &rv)?;
    Ok(None)
}

// JDK-ONLY-CLASSIFY: unknown — needs census. Substantively this is an
// intrinsic, not a bridge and not a stub: the real
// `MontgomeryIntegerPolynomialP256.{mult,square}` have concrete bytecode, and
// HotSpot intrinsifies the same field arithmetic. The classification hazard is
// structural, not semantic — neither of the two registrations below carries a
// category. Both inherit `Intrinsic` from the ONE caller,
// `native-builtins/src/lib.rs`'s `with_category(NativeKind::Intrinsic, …)`
// wrapper. Delete or move that wrapper and these silently become
// `SyntheticStub` and vanish under `--jdk-only`. Evidence still owed per
// jdk-only-native-review.md §6: parity is argued from JDK 25 limb vectors in
// this file's tests, but the speedup is asserted, not measured in-repo.
/// Register the byte-identical P-256 Montgomery field multiply/square intrinsics.
pub fn register_sunec_intpoly_intrinsics(registry: &mut NativeMethodRegistry) {
    registry.register(CLASS, "mult", "([J[J[J)V", native_mont_mult);
    registry.register(CLASS, "square", "([J[J)V", native_mont_square);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    // Limb vectors captured from JDK 25's real
    // MontgomeryIntegerPolynomialP256.{mult,square} (ecprobe_tmp/MontSamples.java).
    // Each MULT entry is (a_limbs, b_limbs, expected_r_limbs).
    #[rustfmt::skip]
    const MULT_VECTORS: &[(&[i64; 5], &[i64; 5], &[i64; 5])] = &[
        // zero * x
        (&[0x0, 0x0, 0x0, 0x0, 0x0],
         &[0xdeadbeefd, 0x300000000000, 0xffffff2152411, 0x1102fffffffff, 0xeadbeeef1524],
         &[0x0, 0x0, 0x0, 0x0, 0x0]),
        // one(mont) * x
        (&[0x10, 0xf000000000000, 0xfffffffffffff, 0xffeffffffffff, 0xfffff],
         &[0x99998962fc961, 0x68acf1356999, 0x66666676af37c, 0xf1377530eca96, 0xfc962fc868ac],
         &[0x99998962fc961, 0x68acf1356999, 0x66666676af37c, 0xf1377530eca96, 0xfc962fc868ac]),
        // x * one(mont)
        (&[0xba997530eca87, 0x790000000fedc, 0x2345668acf135, 0x3578fffffff01, 0x654320efacf1],
         &[0x10, 0xf000000000000, 0xfffffffffffff, 0xffeffffffffff, 0xfffff],
         &[0xba997530eca87, 0x790000000fedc, 0x2345668acf135, 0x3578fffffff01, 0x654320efacf1]),
        // (p-1) * (p-1)
        (&[0xfffffffffffef, 0x10fffffffffff, 0x0, 0x11000000000, 0xffffffef0000],
         &[0xfffffffffffef, 0x10fffffffffff, 0x0, 0x11000000000, 0xffffffef0000],
         &[0x10, 0xf000000000000, 0xfffffffffffff, 0xffeffffffffff, 0xfffff]),
        // random 1
        (&[0x842eff98a9759, 0xe0996c9949873, 0xadd036e077ccf, 0x9c4a28b369013, 0x9b18fd15a035],
         &[0xfdeef74613ef0, 0xd1abc768a0775, 0x7492792376809, 0x871743f7451f6, 0x51d467691430],
         &[0x5d345a7ca4667, 0xb48fc3b761b32, 0x3eb9eb427c5d3, 0xf10a7242677fc, 0x1c5bea89843c]),
        // random 2
        (&[0xfffee789abcc6, 0x54aaaaaab7fff, 0x21876543, 0x431f555555400, 0x789abcbf8765],
         &[0xaaa899999996e, 0x5eccccccaaaaa, 0x5555575555555, 0x77e5333333755, 0xeeeeeee57777],
         &[0xca82d7fcf7571, 0x251b27abfacdd, 0x1eb83d0c8db0e, 0x8632fde04ab05, 0xb44cc347d916]),
    ];

    // SQUARE entries (a_limbs, expected_r_limbs) from the real JDK square().
    #[rustfmt::skip]
    const SQUARE_VECTORS: &[(&[i64; 5], &[i64; 5])] = &[
        (&[0x842eff98a9759, 0xe0996c9949873, 0xadd036e077ccf, 0x9c4a28b369013, 0x9b18fd15a035],
         &[0x4f4424402354b, 0xb23fcdfd72be7, 0xf1bea8600adff, 0x68720d8f836c7, 0x7366dace0f4b]),
        (&[0x10, 0xf000000000000, 0xfffffffffffff, 0xffeffffffffff, 0xfffff],
         &[0x10, 0xf000000000000, 0xfffffffffffff, 0xffeffffffffff, 0xfffff]),
        (&[0xfffffffffffef, 0x10fffffffffff, 0x0, 0x11000000000, 0xffffffef0000],
         &[0x10, 0xf000000000000, 0xfffffffffffff, 0xffeffffffffff, 0xfffff]),
    ];

    fn u(a: &[i64; 5]) -> [u64; 5] {
        let mut o = [0u64; 5];
        for i in 0..5 {
            o[i] = a[i] as u64;
        }
        o
    }

    #[test]
    fn mult_matches_jdk_byte_for_byte() {
        for (a, b, expect) in MULT_VECTORS {
            let got = mont_mult(&u(a), &u(b));
            assert_eq!(&got, *expect, "mult mismatch for a={a:x?} b={b:x?}");
        }
    }

    #[test]
    fn square_matches_jdk_and_equals_mult_self() {
        for (a, expect) in SQUARE_VECTORS {
            let got = mont_mult(&u(a), &u(a));
            assert_eq!(&got, *expect, "square mismatch for a={a:x?}");
        }
    }

    #[test]
    fn output_is_canonical() {
        // Every output limb must be in [0, 2^52) — the canonical form the JDK
        // emits and downstream ops assume (MAX_ADDS = 0).
        for (a, b, _) in MULT_VECTORS {
            for limb in mont_mult(&u(a), &u(b)) {
                assert!(
                    (0..(1i64 << 52)).contains(&limb),
                    "non-canonical limb {limb:#x}"
                );
            }
        }
    }
}
