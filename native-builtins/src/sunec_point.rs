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
//!   (This file currently implements **P-256 only**; other curves throw a clear
//!   error under the gate — see TODO.)
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

use p256::elliptic_curve::sec1::{FromEncodedPoint, ToEncodedPoint};
use p256::elliptic_curve::ff::PrimeField;
use p256::elliptic_curve::group::prime::PrimeCurveAffine;

const EC_OPS: &str = "sun/security/ec/ECOperations";

fn gate_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| std::env::var_os("CRATONVM_NATIVE_EC_MULTIPLY").is_some())
}

fn obj(v: Option<Value>) -> Option<ObjectRef> {
    match v {
        Some(Value::Object(o)) => o,
        _ => None,
    }
}

fn internal_err(msg: &str) -> MethodCallFailed {
    RuntimeError::IllegalArgumentException { message: msg.to_string() }.into()
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

/// Read a `java.math.BigInteger`'s value as a big-endian unsigned 32-byte array
/// (left-zero-padded), via `toByteArray()`. Errors if the value exceeds 32 bytes.
fn read_bigint_be32(
    ctx: &mut dyn NativeContext,
    bigint: ObjectRef,
) -> Result<[u8; 32], MethodCallFailed> {
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
    // positive, so strip any leading 0x00 sign byte, then left-pad to 32.
    let start = if signed.len() > 1 && signed[0] == 0 { 1 } else { 0 };
    let mag = &signed[start..];
    if mag.len() > 32 {
        return Err(internal_err("coordinate exceeds 32 bytes"));
    }
    let mut out = [0u8; 32];
    out[32 - mag.len()..].copy_from_slice(mag);
    Ok(out)
}

/// Build a Java `byte[]` of length 32 from `bytes`.
fn make_byte_array(
    ctx: &mut dyn NativeContext,
    bytes: &[u8; 32],
) -> Result<ObjectRef, MethodCallFailed> {
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 32);
    for (i, b) in bytes.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(*b as i8 as i32));
    }
    Ok(arr)
}

/// Construct a positive `java.math.BigInteger` from a 32-byte big-endian magnitude.
fn make_bigint(
    ctx: &mut dyn NativeContext,
    bytes: &[u8; 32],
) -> Result<ObjectRef, MethodCallFailed> {
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
        let cls = ctx.class_name_of_id(cid).ok_or_else(|| internal_err("no ecOps class"))?;
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

    // Curve detection from the field's class. P-256 only for now.
    let field_cls = ctx
        .class_name_of_id(ctx.class_id_of_object(field))
        .unwrap_or_default();
    let is_p256 = field_cls.contains("P256");
    if !is_p256 {
        ctx.unpin_native_roots(pin_base);
        return Err(internal_err(&format!(
            "native EC multiply: curve {field_cls} not yet implemented (P-256 only)"
        )));
    }

    // Read base point coordinates via the public asBigInteger() accessor.
    let affine_p = ctx.read_native_pin(p_affine, affine_p);
    let ex = invoke_virtual_obj(ctx, affine_p, "getX", "()Lsun/security/util/math/ImmutableIntegerModuloP;")?
        .ok_or_else(|| internal_err("getX null"))?;
    let bx_bi = invoke_virtual_obj(ctx, ex, "asBigInteger", "()Ljava/math/BigInteger;")?
        .ok_or_else(|| internal_err("x.asBigInteger null"))?;
    let bx = read_bigint_be32(ctx, bx_bi)?;
    let affine_p = ctx.read_native_pin(p_affine, affine_p);
    let ey = invoke_virtual_obj(ctx, affine_p, "getY", "()Lsun/security/util/math/ImmutableIntegerModuloP;")?
        .ok_or_else(|| internal_err("getY null"))?;
    let by_bi = invoke_virtual_obj(ctx, ey, "asBigInteger", "()Ljava/math/BigInteger;")?
        .ok_or_else(|| internal_err("y.asBigInteger null"))?;
    let by = read_bigint_be32(ctx, by_bi)?;

    // ---- pure p256 math (no Java refs held) ----
    let (rx, ry) = p256_scalar_mul(&bx, &by, &scalar_le)
        .ok_or_else(|| internal_err("p256 scalar multiply failed (bad point/scalar)"))?;

    // ---- construct result: ProjectivePoint$Mutable from AffinePoint(rx,ry) ----
    let field = ctx.read_native_pin(p_field, field);
    let rx_bi = make_bigint(ctx, &rx)?;
    let p_rx = ctx.pin_native_root(rx_bi);
    let ry_bi = make_bigint(ctx, &ry)?;
    let rx_bi = ctx.read_native_pin(p_rx, rx_bi);
    let field = ctx.read_native_pin(p_field, field);

    let ecpoint = ctx.new_object_initialized(
        "java/security/spec/ECPoint",
        "(Ljava/math/BigInteger;Ljava/math/BigInteger;)V",
        &[Value::Object(Some(rx_bi)), Value::Object(Some(ry_bi))],
    )?;
    let ecpoint = obj(ecpoint).ok_or_else(|| internal_err("new ECPoint null"))?;
    let p_ecp = ctx.pin_native_root(ecpoint);
    let field = ctx.read_native_pin(p_field, field);

    // affine = AffinePoint.fromECPoint(ecpoint, field)  (static)
    let affine_res = ctx.invoke(
        "sun/security/ec/point/AffinePoint",
        "fromECPoint",
        "(Ljava/security/spec/ECPoint;Lsun/security/util/math/IntegerFieldModuloP;)Lsun/security/ec/point/AffinePoint;",
        &[Value::Object(Some(ctx.read_native_pin(p_ecp, ecpoint))), Value::Object(Some(ctx.read_native_pin(p_field, field)))],
    )?;
    let affine = obj(affine_res).ok_or_else(|| internal_err("fromECPoint null"))?;
    let p_aff = ctx.pin_native_root(affine);
    let field = ctx.read_native_pin(p_field, field);

    // result = new ProjectivePoint$Mutable(field)
    let result = ctx.new_object_initialized(
        "sun/security/ec/point/ProjectivePoint$Mutable",
        "(Lsun/security/util/math/IntegerFieldModuloP;)V",
        &[Value::Object(Some(ctx.read_native_pin(p_field, field)))],
    )?;
    let result = obj(result).ok_or_else(|| internal_err("new ProjectivePoint$Mutable null"))?;
    let p_res = ctx.pin_native_root(result);
    let affine = ctx.read_native_pin(p_aff, affine);

    // result.setValue(affine)
    ctx.invoke(
        "sun/security/ec/point/ProjectivePoint$Mutable",
        "setValue",
        "(Lsun/security/ec/point/AffinePoint;)Lsun/security/ec/point/ProjectivePoint$Mutable;",
        &[Value::Object(Some(ctx.read_native_pin(p_res, result))), Value::Object(Some(affine))],
    )?;
    let result = ctx.read_native_pin(p_res, result);

    ctx.unpin_native_roots(pin_base);
    Ok(Some(Value::Object(Some(result))))
}

/// P-256 scalar multiply `s · (x, y)` → result affine `(rx, ry)` as big-endian
/// 32-byte arrays. Inputs: `x`/`y` big-endian 32-byte affine coords, `s_le`
/// little-endian scalar. Returns `None` on an invalid point, or if the result
/// is the identity (point at infinity) — which the caller surfaces as an error
/// (it does not occur for valid keygen/sign scalars).
fn p256_scalar_mul(x: &[u8; 32], y: &[u8; 32], s_le: &[u8]) -> Option<([u8; 32], [u8; 32])> {
    use p256::{AffinePoint, EncodedPoint, ProjectivePoint, Scalar};
    use p256::elliptic_curve::generic_array::GenericArray;

    let ep = EncodedPoint::from_affine_coordinates(
        GenericArray::from_slice(x),
        GenericArray::from_slice(y),
        false,
    );
    let affine = AffinePoint::from_encoded_point(&ep);
    let affine = if affine.is_some().into() { affine.unwrap() } else { return None };

    // scalar: little-endian → big-endian, reduced mod n.
    let mut be = [0u8; 32];
    for (i, b) in s_le.iter().take(32).enumerate() {
        be[31 - i] = *b;
    }
    let scalar = {
        let ct = Scalar::from_repr(GenericArray::clone_from_slice(&be));
        if ct.is_some().into() {
            ct.unwrap()
        } else {
            // s >= n: reduce. (Rare; SunEC scalars are < n.)
            use p256::elliptic_curve::ops::Reduce;
            use p256::U256;
            <Scalar as Reduce<U256>>::reduce_bytes(GenericArray::from_slice(&be))
        }
    };

    let prod = (ProjectivePoint::from(affine) * scalar).to_affine();
    if prod.is_identity().into() {
        return None;
    }
    let enc = prod.to_encoded_point(false);
    let rx = enc.x()?;
    let ry = enc.y()?;
    let mut ox = [0u8; 32];
    let mut oy = [0u8; 32];
    ox.copy_from_slice(rx);
    oy.copy_from_slice(ry);
    Some((ox, oy))
}

/// Register the gated coarse native EC scalar-multiply. No-op unless
/// `CRATONVM_NATIVE_EC_MULTIPLY` is set.
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
