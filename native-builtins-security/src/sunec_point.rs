// SPDX-License-Identifier: Apache-2.0
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
use p256::elliptic_curve::sec1::{FromEncodedPoint, ToEncodedPoint};

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
    let slen = ctx.array_length(scalar_arr);
    let mut scalar_le = vec![0u8; slen];
    for (i, dst) in scalar_le.iter_mut().enumerate() {
        if let Value::Int(b) = ctx.get_array_element(scalar_arr, i) {
            *dst = b as u8;
        }
    }

    // Pin args across the re-entrant invokes below (a moving GC during any
    // invoke would otherwise leave these ObjectRefs stale).
    let pin_base = ctx.pin_native_root(ec_ops);
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
    let field_cls = ctx
        .class_name_of_id(ctx.class_id_of_object(field))
        .unwrap_or_default();
    let curve = match Curve::from_field_class(&field_cls) {
        Some(c) => c,
        None => {
            ctx.unpin_native_roots(pin_base);
            return Err(internal_err(&format!(
                "native EC multiply: unsupported curve field {field_cls}"
            )));
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
        Curve::P256 => scalar_mul_p256(&bx, &by, &scalar_le),
        Curve::P384 => scalar_mul_p384(&bx, &by, &scalar_le),
        Curve::P521 => scalar_mul_p521(&bx, &by, &scalar_le),
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

    let result = ctx.read_native_pin(p_res, result);
    ctx.unpin_native_roots(pin_base);
    Ok(Some(Value::Object(Some(result))))
}

/// Generate a curve `scalar_mul` over the RustCrypto curve crate `$krate` with
/// field/scalar byte length `$nbytes`: `s · (x, y)` → affine `(rx, ry)` as
/// big-endian `$nbytes`-byte vectors. `x`/`y` are big-endian affine coords
/// (`$nbytes` bytes), `s_le` the little-endian scalar. Returns `None` on an
/// invalid point or an identity (point-at-infinity) result — which the caller
/// surfaces as an error (it does not occur for valid keygen/sign scalars, which
/// are always in `[1, n)`).
macro_rules! impl_curve_scalar_mul {
    ($name:ident, $krate:ident, $nbytes:literal) => {
        fn $name(x: &[u8], y: &[u8], s_le: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
            use $krate::elliptic_curve::ff::PrimeField;
            use $krate::elliptic_curve::generic_array::GenericArray;
            use $krate::elliptic_curve::group::prime::PrimeCurveAffine;
            use $krate::elliptic_curve::sec1::{FromEncodedPoint, ToEncodedPoint};
            use $krate::{AffinePoint, EncodedPoint, ProjectivePoint, Scalar};

            if x.len() != $nbytes || y.len() != $nbytes {
                return None;
            }
            let ep = EncodedPoint::from_affine_coordinates(
                GenericArray::from_slice(x),
                GenericArray::from_slice(y),
                false,
            );
            let affine = AffinePoint::from_encoded_point(&ep);
            let affine = if affine.is_some().into() {
                affine.unwrap()
            } else {
                return None;
            };

            // scalar: little-endian → big-endian (n bytes).
            let mut be = [0u8; $nbytes];
            for (i, b) in s_le.iter().take($nbytes).enumerate() {
                be[$nbytes - 1 - i] = *b;
            }
            let ct = Scalar::from_repr(GenericArray::clone_from_slice(&be));
            let scalar = if ct.is_some().into() {
                ct.unwrap()
            } else {
                // A non-canonical scalar (>= the group order `n`). SunEC reaches
                // this ONLY in `ECDHKeyAgreement.validate`'s public-key order
                // check, which multiplies the (already on-curve, verified just
                // above) point by `n` and expects the neutral element: `n·P = O`.
                // Signal identity so the caller returns SunEC's neutral
                // `MutablePoint`. (Keygen/sign/real-ECDH scalars are always < n
                // and take the canonical branch above.)
                return Some((Vec::new(), Vec::new()));
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
            let enc = prod.to_encoded_point(false);
            Some((enc.x()?.to_vec(), enc.y()?.to_vec()))
        }
    };
}

impl_curve_scalar_mul!(scalar_mul_p256, p256, 32);
impl_curve_scalar_mul!(scalar_mul_p384, p384, 48);
impl_curve_scalar_mul!(scalar_mul_p521, p521, 66);

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
        native_ec_multiply,
    );
}
