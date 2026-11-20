// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Coarse native EC scalar-multiply for the SunEC point engine —
//! `sun.security.ec.ECOperations.multiply(AffinePoint, byte[])`.
//!
//! GATED default-OFF behind `CRATONVM_NATIVE_EC_MULTIPLY` (the registration is
//! skipped unless the env var is set), so it is zero-risk until explicitly
//! enabled.
//!
//! ## Why
//! The pure-Java SunEC scalar-multiply pays a one-time ~14 s generator-table
//! precompute (`Secp256R1GeneratorMontgomeryMultiplier.<clinit>`, triggered the
//! first time `ECOperations.multiply` runs for P-256) plus interpreted point
//! arithmetic. The byte-identical `mult`/`square` field intrinsics already make
//! steady-state EC ops sub-second, but the one-time table remains. Replacing the
//! whole scalar multiply with the `p256` crate bypasses the table entirely.
//!
//! ## Representation (cracked + verified — ecprobe_tmp/MulGT2..4)
//! - `ECOperations.multiply(affineP, s)` is only ever called for the curves the
//!   SunEC intpoly path supports (P-256/384/521), so no per-receiver decline is
//!   needed — a fully-handled curve set means the native never has to fall back.
//!   All three (P-256/384/521) are implemented via the RustCrypto `p256`/`p384`/
//!   `p521` crates; the curve is detected from the field implementation class.
//! - Base-point coordinates are read via the public `asBigInteger()` accessor
//!   (the internal limb encoding is opaque); the scalar `s` is **little-endian**.
//! - The result is returned as a homogeneous projective point with `Z = 1`,
//!   `X = rx`, `Y = ry` (valid in any projective convention when `Z = 1`), built
//!   through the JDK's own `AffinePoint.fromECPoint` + `ProjectivePoint$Mutable
//!   .setValue` so all field/montgomery encoding is delegated to the JDK.
//!
//! ## Correctness
//! Verified end-to-end by cross-VM signature checks (CratonVM-sign ↔
//! HotSpot-verify, both directions) — a wrong scalar multiply yields a signature
//! an independent JVM rejects.

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ObjectRef, Value};

use p256::elliptic_curve::ff::PrimeField;
use p256::elliptic_curve::group::prime::PrimeCurveAffine;
use p256::elliptic_curve::sec1::{FromSec1Point, ToSec1Point};
use p256::elliptic_curve::CurveAffine;

const EC_OPS: &str = "sun/security/ec/ECOperations";

fn gate_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    // Default-ON whenever EC is routed to the real SunEC path
    // (`crate::route_ec_to_real`, default): the pure-Java P-256 scalar multiply
    // pays a one-time ~14 s generator-table precompute that this `p256`-crate
    // bypass eliminates (SdJwtTest 52 s → 10 s). This native is only ever reached
    // by real SunEC `ECOperations.multiply`, which only runs once EC is routed
    // real — so it stays inert under the `CRATONVM_SYNTHETIC_EC=1` kill-switch.
    // The explicit `CRATONVM_NATIVE_EC_MULTIPLY` env still force-enables it.
    *G.get_or_init(|| {
        let flags = &cratonvm_types::flags().natives;
        flags.native_ec_multiply || !flags.synthetic_ec
    })
}

fn obj(v: Option<Value>) -> Option<ObjectRef> {
    match v {
        Some(Value::Object(o)) => o,
        _ => None,
    }
}

fn internal_err(msg: &str) -> MethodCallFailed {
    RuntimeError::IllegalArgumentException {
        message: msg.to_string(),
    }
    .into()
}

/// `java.security.ProviderException` — the JDK's own unchecked exception for
/// "a provider engine accepted the request and then could not complete it".
/// It is the specification-correct type for an invariant violation reached
/// from inside `sun.security.ec`, and unlike a checked `java.security`
/// exception it does not need to be declared on `ECOperations.multiply`.
const PROVIDER_EXCEPTION: &str = "java/security/ProviderException";

/// Construct and throw a real Java exception of `class_name` (internal form)
/// carrying `msg`.
///
/// Mirrors the facade's throw helper — `native-builtins/src/phases_early.rs:14672`
/// (`throw_jca_exc`) and `native-builtins/src/phases_late/bouncycastle.rs:6040`
/// (`bc_gost_throw_crypto_exception`) — which this crate cannot call directly
/// (`native-builtins` depends on us, not the other way round).
///
/// Note the fallback: if the exception class itself cannot be constructed
/// (synthetic-JDK mode, class absent from the boot path) we still raise an
/// unchecked `IllegalStateException`. There is deliberately no arm that
/// returns a value — a security engine that cannot report its failure must
/// still fail, not answer.
fn throw_jca(ctx: &mut dyn NativeContext, class_name: &str, msg: &str) -> MethodCallFailed {
    let detail = ctx.create_string(msg);
    match ctx.new_object_initialized(
        class_name,
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(detail))],
    ) {
        Ok(Some(Value::Object(Some(exc)))) => MethodCallFailed::ExceptionThrown(exc),
        _ => RuntimeError::IllegalStateException {
            message: format!("{class_name}: {msg}"),
        }
        .into(),
    }
}

/// Invoke an instance method whose concrete class is taken from `receiver`'s
/// runtime type (so interface/abstract methods like `asBigInteger` dispatch
/// correctly), returning the (object) result.
fn invoke_virtual_obj(
    ctx: &mut dyn NativeContext,
    receiver: ObjectRef,
    method: &str,
    desc: &str,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let cid = ctx.class_id_of_object(receiver);
    let cls = ctx
        .class_name_of_id(cid)
        .ok_or_else(|| internal_err("no class name for receiver"))?;
    let r = ctx.invoke(&cls, method, desc, &[Value::Object(Some(receiver))])?;
    Ok(obj(r))
}

/// One of the SunEC weierstrass prime curves we accelerate, with its field/
/// coordinate byte length (P-256=32, P-384=48, P-521=66).
#[derive(Copy, Clone)]
enum Curve {
    P256,
    P384,
    P521,
}

impl Curve {
    /// Detect from the `IntegerFieldModuloP` implementation class name
    /// (`sun.security.util.math.intpoly.IntegerPolynomialP256` etc.).
    fn from_field_class(cls: &str) -> Option<Curve> {
        if cls.contains("P256") {
            Some(Curve::P256)
        } else if cls.contains("P384") {
            Some(Curve::P384)
        } else if cls.contains("P521") {
            Some(Curve::P521)
        } else {
            None
        }
    }
    fn byte_len(self) -> usize {
        match self {
            Curve::P256 => 32,
            Curve::P384 => 48,
            Curve::P521 => 66,
        }
    }
}

/// Read a `java.math.BigInteger`'s value as a big-endian unsigned `len`-byte
/// magnitude (left-zero-padded), via `toByteArray()`. Errors if it exceeds `len`.
fn read_bigint_be(
    ctx: &mut dyn NativeContext,
    bigint: ObjectRef,
    len: usize,
) -> Result<Vec<u8>, MethodCallFailed> {
    let arr = invoke_virtual_obj(ctx, bigint, "toByteArray", "()[B")?
        .ok_or_else(|| internal_err("toByteArray returned null"))?;
    let n = ctx.array_length(arr);
    let mut signed = Vec::with_capacity(n);
    for i in 0..n {
        match ctx.get_array_element(arr, i) {
            Value::Int(b) => signed.push(b as u8),
            other => return Err(internal_err(&format!("byte[] element not int: {other:?}"))),
        }
    }
    // toByteArray is signed big-endian two's complement; coordinates are
    // positive, so strip any leading 0x00 sign byte, then left-pad to `len`.
    let start = if signed.len() > 1 && signed[0] == 0 {
        1
    } else {
        0
    };
    let mag = &signed[start..];
    if mag.len() > len {
        return Err(internal_err("coordinate exceeds field byte length"));
    }
    let mut out = vec![0u8; len];
    out[len - mag.len()..].copy_from_slice(mag);
    Ok(out)
}

/// Build a Java `byte[]` from `bytes`.
fn make_byte_array(
    ctx: &mut dyn NativeContext,
    bytes: &[u8],
) -> Result<ObjectRef, MethodCallFailed> {
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
    for (i, b) in bytes.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(*b as i8 as i32));
    }
    Ok(arr)
}

/// Construct a positive `java.math.BigInteger` from a big-endian magnitude.
fn make_bigint(ctx: &mut dyn NativeContext, bytes: &[u8]) -> Result<ObjectRef, MethodCallFailed> {
    let arr = make_byte_array(ctx, bytes)?;
    let r = ctx.new_object_initialized(
        "java/math/BigInteger",
        "(I[B)V",
        &[Value::Int(1), Value::Object(Some(arr))],
    )?;
    obj(r).ok_or_else(|| internal_err("new BigInteger returned null"))
}

/// `public MutablePoint multiply(AffinePoint affineP, byte[] s)` —
/// args = [ecOps(this), affineP, scalarBytes].
fn native_ec_multiply(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let ec_ops = obj(args.first().copied()).ok_or_else(|| internal_err("null ECOperations"))?;
    let affine_p = obj(args.get(1).copied()).ok_or_else(|| internal_err("null AffinePoint"))?;
    let scalar_arr = obj(args.get(2).copied()).ok_or_else(|| internal_err("null scalar"))?;

    // Read scalar bytes (little-endian) — array read, no re-entrant call.
    //
    // The `if let Value::Int(..)` this replaces left `*dst` at its initialised
    // zero whenever an element read did not yield an `Int` (wrong array kind,
    // or a heap read that could not be serviced). That is the worst possible
    // shape for a private-key scalar: the multiply then proceeds with silently
    // zeroed limbs and returns a perfectly well-formed *wrong* point, which
    // the caller signs or key-agrees with as though nothing happened. Refuse
    // instead — a byte we could not read is not a byte worth guessing.
    let slen = ctx.array_length(scalar_arr);
    let mut scalar_le = vec![0u8; slen];
    for (i, dst) in scalar_le.iter_mut().enumerate() {
        match ctx.get_array_element(scalar_arr, i) {
            Value::Int(b) => *dst = b as u8,
            other => {
                return Err(internal_err(&format!(
                    "native EC multiply: scalar byte[{i}] is not a byte ({other:?})"
                )))
            }
        }
    }

    // Pin args across the re-entrant invokes below (a moving GC during any
    // invoke would otherwise leave these ObjectRefs stale).
    //
    // The pinned region is delimited here and nowhere else. Everything that
    // can fail lives in `ec_multiply_pinned`, so **every** exit — the two
    // explicit refusals and the dozen `?` early returns alike — passes through
    // the single `unpin_native_roots` below. Previously only the refusals
    // unpinned and each `?` leaked its pins into the root set for the life of
    // the VM; a caller that repeatedly hit `getX`-returned-null would grow the
    // root set without bound. This is a root-set leak, not a cryptographic
    // degradation, but it lives on the same paths.
    let _scrub = cratonvm_native_builtins_crypto::scrub::ScrubOnDrop;
    let pin_base = ctx.pin_native_root(ec_ops);
    let result = ec_multiply_pinned(ctx, pin_base, ec_ops, affine_p, &scalar_le);
    ctx.unpin_native_roots(pin_base);
    result
}

/// The body of [`native_ec_multiply`], running inside the pinned region opened
/// by its caller. Must not unpin: the caller does that on every exit path.
fn ec_multiply_pinned(
    ctx: &mut dyn NativeContext,
    pin_base: usize,
    ec_ops: ObjectRef,
    affine_p: ObjectRef,
    scalar_le: &[u8],
) -> MethodCallResult {
    let p_affine = ctx.pin_native_root(affine_p);

    // field = ecOps.getField()
    let field = {
        let cid = ctx.class_id_of_object(ec_ops);
        let cls = ctx
            .class_name_of_id(cid)
            .ok_or_else(|| internal_err("no ecOps class"))?;
        let r = ctx.invoke(
            &cls,
            "getField",
            "()Lsun/security/util/math/IntegerFieldModuloP;",
            &[Value::Object(Some(ctx.read_native_pin(pin_base, ec_ops)))],
        )?;
        obj(r).ok_or_else(|| internal_err("getField returned null"))?
    };
    let affine_p = ctx.read_native_pin(p_affine, affine_p);
    let p_field = ctx.pin_native_root(field);

    // Curve detection from the field's implementation class (P-256/384/521).
    //
    // The `unwrap_or_default()` this replaces turned an unresolvable class id
    // into the empty string, which then fell into the "unsupported curve" arm
    // with a blank curve name in the message. Both conditions are refusals, but
    // they are different refusals and the diagnostic must say which.
    let field_cls = ctx.class_name_of_id(ctx.class_id_of_object(field));
    let field_cls = match field_cls {
        Some(c) => c,
        None => {
            return Err(throw_jca(
                ctx,
                PROVIDER_EXCEPTION,
                "native EC multiply: field object has no resolvable class name",
            ))
        }
    };
    // SUPPORTED vs REJECTED: exactly P-256, P-384 and P-521 are implemented
    // (`Curve::from_field_class`). Every other curve — including the ones the
    // JDK itself still ships, e.g. secp256k1 — is refused here. There is no
    // "closest match" fallback: multiplying on the wrong curve would produce a
    // point that is structurally valid and cryptographically meaningless.
    let curve = match Curve::from_field_class(&field_cls) {
        Some(c) => c,
        None => {
            return Err(throw_jca(
                ctx,
                PROVIDER_EXCEPTION,
                &format!("native EC multiply: unsupported curve field {field_cls}"),
            ))
        }
    };
    let n = curve.byte_len();

    // Read base point coordinates via the public asBigInteger() accessor.
    let affine_p = ctx.read_native_pin(p_affine, affine_p);
    let ex = invoke_virtual_obj(
        ctx,
        affine_p,
        "getX",
        "()Lsun/security/util/math/ImmutableIntegerModuloP;",
    )?
    .ok_or_else(|| internal_err("getX null"))?;
    let bx_bi = invoke_virtual_obj(ctx, ex, "asBigInteger", "()Ljava/math/BigInteger;")?
        .ok_or_else(|| internal_err("x.asBigInteger null"))?;
    let bx = read_bigint_be(ctx, bx_bi, n)?;
    let affine_p = ctx.read_native_pin(p_affine, affine_p);
    let ey = invoke_virtual_obj(
        ctx,
        affine_p,
        "getY",
        "()Lsun/security/util/math/ImmutableIntegerModuloP;",
    )?
    .ok_or_else(|| internal_err("getY null"))?;
    let by_bi = invoke_virtual_obj(ctx, ey, "asBigInteger", "()Ljava/math/BigInteger;")?
        .ok_or_else(|| internal_err("y.asBigInteger null"))?;
    let by = read_bigint_be(ctx, by_bi, n)?;

    // ---- pure Rust EC math (no Java refs held) ----
    let (rx, ry) = match curve {
        Curve::P256 => scalar_mul_p256(&bx, &by, scalar_le),
        Curve::P384 => scalar_mul_p384(&bx, &by, scalar_le),
        Curve::P521 => scalar_mul_p521(&bx, &by, scalar_le),
    }
    .ok_or_else(|| internal_err("native EC scalar multiply failed (bad point/scalar)"))?;
    // Empty coordinates = point-at-infinity (e.g. the `n·P` order check in
    // `ECDHKeyAgreement.validate`). A freshly-constructed `ProjectivePoint
    // $Mutable(field)` already has X=Y=Z=0, and `ECOperations.isNeutral` tests
    // `Z == 0`, so the bare Mutable IS the neutral element — return it without
    // `setValue` (which would set an affine Z=1, no longer neutral).
    let is_identity = rx.is_empty() && ry.is_empty();

    // ---- result = new ProjectivePoint$Mutable(field) ----
    let field = ctx.read_native_pin(p_field, field);
    let result = ctx.new_object_initialized(
        "sun/security/ec/point/ProjectivePoint$Mutable",
        "(Lsun/security/util/math/IntegerFieldModuloP;)V",
        &[Value::Object(Some(field))],
    )?;
    let result = obj(result).ok_or_else(|| internal_err("new ProjectivePoint$Mutable null"))?;
    let p_res = ctx.pin_native_root(result);

    if !is_identity {
        // result.setValue(AffinePoint.fromECPoint(ECPoint(rx,ry), field))
        let field = ctx.read_native_pin(p_field, field);
        let rx_bi = make_bigint(ctx, &rx)?;
        let p_rx = ctx.pin_native_root(rx_bi);
        let ry_bi = make_bigint(ctx, &ry)?;
        let rx_bi = ctx.read_native_pin(p_rx, rx_bi);

        let ecpoint = ctx.new_object_initialized(
            "java/security/spec/ECPoint",
            "(Ljava/math/BigInteger;Ljava/math/BigInteger;)V",
            &[Value::Object(Some(rx_bi)), Value::Object(Some(ry_bi))],
        )?;
        let ecpoint = obj(ecpoint).ok_or_else(|| internal_err("new ECPoint null"))?;
        let p_ecp = ctx.pin_native_root(ecpoint);
        let field = ctx.read_native_pin(p_field, field);

        let affine_res = ctx.invoke(
            "sun/security/ec/point/AffinePoint",
            "fromECPoint",
            "(Ljava/security/spec/ECPoint;Lsun/security/util/math/IntegerFieldModuloP;)Lsun/security/ec/point/AffinePoint;",
            &[Value::Object(Some(ctx.read_native_pin(p_ecp, ecpoint))), Value::Object(Some(field))],
        )?;
        let affine = obj(affine_res).ok_or_else(|| internal_err("fromECPoint null"))?;
        let p_aff = ctx.pin_native_root(affine);

        ctx.invoke(
            "sun/security/ec/point/ProjectivePoint$Mutable",
            "setValue",
            "(Lsun/security/ec/point/AffinePoint;)Lsun/security/ec/point/ProjectivePoint$Mutable;",
            &[
                Value::Object(Some(ctx.read_native_pin(p_res, result))),
                Value::Object(Some(ctx.read_native_pin(p_aff, affine))),
            ],
        )?;
    }

    // Read the pin *before* returning: the caller unpins immediately after,
    // and this ObjectRef has to be the post-GC one.
    let result = ctx.read_native_pin(p_res, result);
    Ok(Some(Value::Object(Some(result))))
}

// ── Group orders ──────────────────────────────────────────────────────────
//
// The order `n` of each curve's base-point group, big-endian, left-padded to
// the curve's field byte width. These exist for exactly one purpose: to tell
// the *one* legitimate non-canonical scalar (`s == n`, the public-key order
// check in `ECDHKeyAgreement.validate`) apart from every other `s >= n`, which
// is a scalar this code cannot represent and must therefore refuse.
//
// Hard-coding them means no new dependency on a `crypto-bigint` re-export whose
// integer width differs per curve. The values are not trusted on their own:
// `tests::group_order_constants_are_exact` proves each one equals the backend's
// own modulus, using only `Scalar::from_repr`. `from_repr(v)` is `Some` iff
// `v < n`, so `from_repr(ORDER) == None` gives `ORDER >= n` and
// `from_repr(ORDER - 1) == Some` gives `ORDER <= n`. Together: `ORDER == n`.
// A mistyped digit cannot survive that test.

/// P-256 group order (`n`), big-endian, 32 bytes.
const P256_ORDER_BE: [u8; 32] = [
    0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xbc, 0xe6, 0xfa, 0xad, 0xa7, 0x17, 0x9e, 0x84, 0xf3, 0xb9, 0xca, 0xc2, 0xfc, 0x63, 0x25, 0x51,
];

/// P-384 group order (`n`), big-endian, 48 bytes.
const P384_ORDER_BE: [u8; 48] = [
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xc7, 0x63, 0x4d, 0x81, 0xf4, 0x37, 0x2d, 0xdf,
    0x58, 0x1a, 0x0d, 0xb2, 0x48, 0xb0, 0xa7, 0x7a, 0xec, 0xec, 0x19, 0x6a, 0xcc, 0xc5, 0x29, 0x73,
];

/// P-521 group order (`n`), big-endian, left-padded to the 66-byte field width.
const P521_ORDER_BE: [u8; 66] = [
    0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xfa, 0x51, 0x86, 0x87, 0x83, 0xbf, 0x2f, 0x96, 0x6b, 0x7f, 0xcc, 0x01, 0x48, 0xf7, 0x09,
    0xa5, 0xd0, 0x3b, 0xb5, 0xc9, 0xb8, 0x89, 0x9c, 0x47, 0xae, 0xbb, 0x6f, 0xb7, 0x1e, 0x91, 0x38,
    0x64, 0x09,
];

/// Generate a curve `scalar_mul` over the RustCrypto curve crate `$krate` with
/// field/scalar byte length `$nbytes`: `s · (x, y)` → affine `(rx, ry)` as
/// big-endian `$nbytes`-byte vectors. `x`/`y` are big-endian affine coords
/// (`$nbytes` bytes), `s_le` the little-endian scalar.
///
/// Returns `None` — which the caller surfaces as a thrown exception — for a
/// wrong-length coordinate, a point that is not on the curve, a scalar with
/// significant bytes beyond the field width, or a scalar that is
/// non-canonical for any reason other than being exactly the group order.
/// `Some((empty, empty))` is the point-at-infinity signal. There is no input
/// for which this returns a coordinate pair it is not confident in.
///
/// `$order_be` is the curve's group order `n`, big-endian, `$nbytes` long.
macro_rules! impl_curve_scalar_mul {
    ($name:ident, $krate:ident, $nbytes:literal, $order_be:ident) => {
        fn $name(x: &[u8], y: &[u8], s_le: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
            let _scrub = cratonvm_native_builtins_crypto::scrub::ScrubOnDrop;
            use $krate::elliptic_curve::ff::PrimeField;
            use $krate::elliptic_curve::group::prime::PrimeCurveAffine;
            use $krate::elliptic_curve::sec1::{FromSec1Point, ToSec1Point};
            use $krate::{AffinePoint, ProjectivePoint, Scalar, Sec1Point};

            let ep =
                Sec1Point::from_affine_coordinates(x.try_into().ok()?, y.try_into().ok()?, false);
            let affine = AffinePoint::from_sec1_point(&ep);
            let affine = if affine.is_some().into() {
                affine.unwrap()
            } else {
                return None;
            };

            // scalar: little-endian → big-endian (n bytes).
            //
            // REFUSE rather than truncate. `take($nbytes)` on its own silently
            // discards the high-order bytes of an over-long little-endian
            // scalar, so `s` and `s mod 2^(8·nbytes)` become indistinguishable
            // and the multiply answers a question the caller never asked — with
            // a well-formed point, so nothing downstream notices. A trailing
            // zero byte (BigInteger's sign padding, harmless) is still allowed;
            // any *significant* excess byte is a hard refusal.
            if s_le.len() > $nbytes && s_le[$nbytes..].iter().any(|b| *b != 0) {
                return None;
            }
            let mut be = [0u8; $nbytes];
            for (i, b) in s_le.iter().take($nbytes).enumerate() {
                be[$nbytes - 1 - i] = *b;
            }
            let ct = Scalar::from_repr(be.into());
            let scalar = if ct.is_some().into() {
                ct.unwrap()
            } else {
                // The scalar is non-canonical (`>= n`), so the backend will not
                // build it and no multiply can be performed. Exactly ONE such
                // scalar has a defined, expected answer:
                //
                //   * `s == n` — `ECDHKeyAgreement.validate`'s public-key order
                //     check. It multiplies the (already on-curve, verified just
                //     above) point by the group order and requires `n·P = O`.
                //     Returning the neutral element is the *right* answer, and
                //     the JDK depends on it.
                //
                // Everything else `>= n` is a scalar this code cannot honour.
                // It used to receive the identity too — and identity is a
                // *plausible* answer, so the caller could not tell "your scalar
                // was rejected" from "the product really is the point at
                // infinity". That is the failure-as-output shape this audit
                // exists to remove, on a private-key operand. Refuse instead;
                // the native caller turns `None` into a thrown exception.
                //
                // Keygen / sign / real-ECDH scalars are always in `[1, n)` and
                // take the canonical branch above, so this narrowing cannot
                // reject a legitimate multiply.
                if be == $order_be {
                    return Some((Vec::new(), Vec::new()));
                }
                return None;
            };

            let prod = (ProjectivePoint::from(affine) * scalar).to_affine();
            if prod.is_identity().into() {
                // Point-at-infinity. This is the EXPECTED result of the order
                // check `n·P` in `ECDHKeyAgreement.validate` (a valid public key
                // satisfies `n·P = O`). Signal it with empty coordinate vecs so
                // the caller returns SunEC's neutral `MutablePoint` (Z=0) instead
                // of failing — only a truly invalid point/scalar yields `None`.
                return Some((Vec::new(), Vec::new()));
            }
            let enc = prod.to_sec1_point(false);
            Some((enc.x()?.to_vec(), enc.y()?.to_vec()))
        }
    };
}

impl_curve_scalar_mul!(scalar_mul_p256, p256, 32, P256_ORDER_BE);
impl_curve_scalar_mul!(scalar_mul_p384, p384, 48, P384_ORDER_BE);
impl_curve_scalar_mul!(scalar_mul_p521, p521, 66, P521_ORDER_BE);

// JDK-ONLY-CLASSIFY: unknown — needs census. Same structural hazard as
// `sunec_intpoly`: the single registration below carries no category and
// inherits `Intrinsic` from its ONE caller in `native-builtins/src/lib.rs`.
// Unlike `sunec_intpoly` the equivalence claim here is weaker — the comment at
// the call site describes this as a *coarse* scalar-multiply that bypasses the
// JDK's generator-table precompute, i.e. a different algorithm reaching the
// same point. Under native-promotion-review.md §4 that is an intrinsic only if
// exception ordering and side effects match on every path; nothing in-repo
// proves that. Additionally `gate_enabled()` means the registration may not
// happen at all, so a source census cannot see it — only `invocations` can.
/// Register the coarse native EC scalar-multiply (P-256/384/521). Active when
/// EC is routed real (`gate_enabled`: `route_ec_to_real` default, or the
/// `CRATONVM_NATIVE_EC_MULTIPLY` env force-on).
pub fn register_sunec_point_intrinsics(registry: &mut NativeMethodRegistry) {
    if !gate_enabled() {
        return;
    }
    registry.register(
        EC_OPS,
        "multiply",
        "(Lsun/security/ec/point/AffinePoint;[B)Lsun/security/ec/point/MutablePoint;",
        native_ec_multiply, cratonvm_native_api::NativeKind::Intrinsic,
    );
}

#[cfg(test)]
mod tests;
