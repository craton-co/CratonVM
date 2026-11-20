// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Native implementation of `java.util.zip.CRC32C` (CRC-32C / Castagnoli).
//!
//! # Why this module exists
//!
//! JDK 25's `java.util.zip.CRC32C` is *pure Java* — unlike `CRC32` it has **no
//! native methods at all**. Its hot loop (`private static int updateBytes`)
//! reads memory through `jdk.internal.misc.Unsafe.getInt`/`getLong` for
//! unaligned 8-byte-at-a-time processing. That makes its correctness in this VM
//! contingent on `Unsafe` array-base-offset arithmetic working bit-exactly,
//! which is fragile. To give a future JIT `CRC32C.update` intrinsic a *stable,
//! bit-exact differential oracle*, we register Java-level native overrides for
//! the public `CRC32C` methods here. The overrides compute the real Castagnoli
//! CRC-32C in Rust and are self-consistent: `<init>`, `update`, `reset`, and
//! `getValue` all read/write the single `int crc` instance field at slot 0, so
//! the receiver object never depends on the JDK's Unsafe-based path.
//!
//! # Algorithm
//!
//! CRC-32C is the standard *reflected* CRC with the Castagnoli polynomial
//! `0x1EDC6F41`; reflected, that is `0x82F63B78` (this is exactly
//! `Integer.reverse(0x1EDC6F41)`, the `REVERSED_CRC32C_POLY` constant in the
//! JDK source). The running state starts at `0xFFFFFFFF`, each byte is XORed
//! into the low 8 bits and shifted right 8 times, and the externally visible
//! value is the bit-complement of the running state. This matches RFC 3720
//! (iSCSI) and the JDK's `CRC32C`.
//!
//! # CRC field-layout contract (see crc_layout_contract.md)
//!
//! Both `java.util.zip.CRC32` and `java.util.zip.CRC32C` declare exactly one
//! instance field — `private int crc` — and neither has a synthetic-stub
//! layout in `classloading::class_manager::synthetic_stub_fields`, so both load
//! their layout from the real JDK 25 class file. `java.lang.Object` contributes
//! zero instance fields, therefore `crc` lives at **instance field slot 0**
//! (`first_field_index + 0`). The slot holds the *running* (uncomplemented) CRC
//! state; the public 32-bit checksum is `~crc & 0xFFFFFFFF`.

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{out_of_bounds_message, MethodCallResult, RuntimeError};
use cratonvm_types::{ObjectRef, Value};

/// Reflected Castagnoli polynomial — `Integer.reverse(0x1EDC6F41)`.
/// The JDK source calls this constant `REVERSED_CRC32C_POLY`.
const REVERSED_CRC32C_POLY: u32 = 0x82F6_3B78;

/// Instance field slot of the running `int crc` value on a `CRC32`/`CRC32C`
/// receiver. See the module-level "CRC field-layout contract" note.
pub const CRC_FIELD_SLOT: usize = 0;

/// CRC-32C (Castagnoli) update over `data`, starting from running state `crc`.
///
/// `crc` is the *running* (uncomplemented) state — callers that want the
/// public checksum must complement the result (`!crc`). A fresh CRC32C starts
/// at `0xFFFFFFFF`.
pub fn crc32c_step(mut crc: u32, data: &[u8]) -> u32 {
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            // Branchless reflected-CRC inner step: subtract 1-bit mask so the
            // polynomial is XORed in iff the low bit was set.
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (REVERSED_CRC32C_POLY & mask);
        }
    }
    crc
}

// ---------------------------------------------------------------------------
// Argument helpers (local copies — keep this module self-contained so the
// orchestrator can merge it without touching zip_real.rs internals).
// ---------------------------------------------------------------------------

fn arg_int(args: &[Value], idx: usize) -> i32 {
    match args.get(idx) {
        Some(Value::Int(v)) => *v,
        Some(Value::Long(v)) => *v as i32,
        _ => 0,
    }
}

fn arg_obj(args: &[Value], idx: usize) -> Option<ObjectRef> {
    match args.get(idx) {
        Some(Value::Object(o)) => *o,
        _ => None,
    }
}

/// Read `len` bytes starting at `off` from a Java `byte[]`.
fn read_byte_array(ctx: &dyn NativeContext, arr: ObjectRef, off: usize, len: usize) -> Vec<u8> {
    let arr_len = ctx.array_length(arr);
    let end = (off + len).min(arr_len);
    let start = off.min(arr_len);
    let mut out = Vec::with_capacity(end.saturating_sub(start));
    for i in start..end {
        match ctx.get_array_element(arr, i) {
            Value::Int(v) => out.push(v as u8),
            _ => out.push(0),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Native overrides for java.util.zip.CRC32C public instance methods.
//
// All four operate on instance field slot 0 (`int crc`) as a Value::Int,
// mirroring the real JDK `private int crc` field. The slot stores the running
// (uncomplemented) state; getValue() returns the complemented public value.
// ---------------------------------------------------------------------------

/// `CRC32C()` — constructor. JDK source sets `crc = -1` (0xFFFFFFFF).
fn crc32c_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match arg_obj(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Object(None))),
    };
    ctx.set_field(this, CRC_FIELD_SLOT, Value::Int(-1));
    Ok(Some(Value::Object(None)))
}

/// `void update(int b)` — fold one byte into the running CRC.
fn crc32c_update_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match arg_obj(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Object(None))),
    };
    let crc = ctx.get_field(this, CRC_FIELD_SLOT).as_int().unwrap_or(-1) as u32;
    let b = (arg_int(args, 1) & 0xFF) as u8;
    let new_crc = crc32c_step(crc, &[b]);
    ctx.set_field(this, CRC_FIELD_SLOT, Value::Int(new_crc as i32));
    Ok(Some(Value::Object(None)))
}



/// The JDK 25 `Checksum.update(byte[] b)` default body is
/// `update(b, 0, b.length)`: a null `b` fails at `arraylength`, with the
/// helpful-NPE text HotSpot prints for it (the JDK is compiled with local
/// variable names).
const CHECKSUM_UPDATE_NULL_ARRAY_MESSAGE: &str =
    "Cannot read the array length because \"b\" is null";

/// `CRC32C.update(byte[] b, int off, int len)`'s precondition,
/// `Preconditions.checkFromIndexSize(off, len, b.length, AIOOBE_FORMATTER)`:
/// `None` when `[off, off + len)` lies inside `[0, length)`, otherwise the
/// `ArrayIndexOutOfBoundsException` the JDK body throws. Computed in `i64`
/// so `off + len` cannot overflow.
fn crc32c_range_failure(off: i32, len: i32, length: usize) -> Option<RuntimeError> {
    let length = i64::try_from(length).unwrap_or(i64::MAX);
    let (from, size) = (i64::from(off), i64::from(len));
    if from < 0 || size < 0 || size > length - from {
        return Some(RuntimeError::aioobe_with_message(
            off,
            out_of_bounds_message::check_from_index_size(from, size, length),
        ));
    }
    None
}

/// `void update(byte[] b)` — Checksum default; delegates to the (b,off,len)
/// form over the whole array. Registered explicitly for robustness.
fn crc32c_update_bytes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    crc32c_update_bytes_with(ctx, args, true)
}

fn crc32c_update_bytes_with(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    checked: bool,
) -> MethodCallResult {
    let this = match arg_obj(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Object(None))),
    };
    let arr = match arg_obj(args, 1) {
        Some(a) => a,
        None if checked => {
            return Err(RuntimeError::NullPointerException {
                message: Some(CHECKSUM_UPDATE_NULL_ARRAY_MESSAGE.to_string()),            }
            .into());        }
        None => return Ok(Some(Value::Object(None))),
    };
    let len = ctx.array_length(arr);
    let bytes = read_byte_array(ctx, arr, 0, len);
    let crc = ctx.get_field(this, CRC_FIELD_SLOT).as_int().unwrap_or(-1) as u32;
    let new_crc = crc32c_step(crc, &bytes);
    ctx.set_field(this, CRC_FIELD_SLOT, Value::Int(new_crc as i32));
    Ok(Some(Value::Object(None)))
}

/// `void update(byte[] b, int off, int len)` — the hot-loop entry point.
fn crc32c_update_bytes_off_len(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    crc32c_update_bytes_off_len_with(ctx, args, true)
}

fn crc32c_update_bytes_off_len_with(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    checked: bool,
) -> MethodCallResult {
    let this = match arg_obj(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Object(None))),
    };
    let arr = match arg_obj(args, 1) {
        Some(a) => a,
        // The JDK body's explicit `if (b == null) throw new
        // NullPointerException();`: no message.
        None if checked => {
            return Err(RuntimeError::NullPointerException { message: None }.into());        }
        None => return Ok(Some(Value::Object(None))),
    };
    if checked {
        if let Some(err) =
            crc32c_range_failure(arg_int(args, 2), arg_int(args, 3), ctx.array_length(arr))
        {
            return Err(err.into());
        }
    }
    let off = arg_int(args, 2).max(0) as usize;
    let len = arg_int(args, 3).max(0) as usize;
    let bytes = read_byte_array(ctx, arr, off, len);
    let crc = ctx.get_field(this, CRC_FIELD_SLOT).as_int().unwrap_or(-1) as u32;
    let new_crc = crc32c_step(crc, &bytes);
    ctx.set_field(this, CRC_FIELD_SLOT, Value::Int(new_crc as i32));
    Ok(Some(Value::Object(None)))
}

/// `long getValue()` — the public checksum is `~crc & 0xFFFFFFFF`.
fn crc32c_get_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match arg_obj(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Long(0))),
    };
    let crc = ctx.get_field(this, CRC_FIELD_SLOT).as_int().unwrap_or(-1) as u32;
    let value = (!crc) as u64 & 0xFFFF_FFFF;
    Ok(Some(Value::Long(value as i64)))
}

/// `void reset()` — JDK source sets `crc = -1`.
fn crc32c_reset(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match arg_obj(args, 0) {
        Some(o) => o,
        None => return Ok(Some(Value::Object(None))),
    };
    ctx.set_field(this, CRC_FIELD_SLOT, Value::Int(-1));
    Ok(Some(Value::Object(None)))
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register the `java.util.zip.CRC32C` native overrides.
///
/// `CRC32C` has no native methods in JDK 25, so these are *Java-method
/// overrides* (the registry intercepts the public `update`/`getValue`/`reset`
/// bytecode bodies). Registered unconditionally — works in both real-JDK and
/// synthetic-JDK modes because every method consistently uses field slot 0.
pub fn register_crc32c_natives(r: &mut NativeMethodRegistry) {
    // JIT round 22 wave 3, lane intrinsics (AGENTS.md "NativeKind is ambient"):
    // the six rows used to inherit the caller's category -- `Bridge` from
    // `register_essential_natives_with_shims`' own `set_category` -- and now
    // state it and restore it. Kind unchanged: production registered them as
    // `Bridge` already, so the registry census and `--dump-native-registry` read
    // the same. (Real JDK 25 `CRC32C` is pure Java, so under `--real-jdk` its
    // concrete bytecode outranks these rows either way.)
            let crc32c = "java/util/zip/CRC32C";
    r.register(crc32c, "<init>", "()V", crc32c_init, cratonvm_native_api::NativeKind::Bridge);
    r.register(crc32c, "update", "(I)V", crc32c_update_int, cratonvm_native_api::NativeKind::Bridge);
    r.register(crc32c, "update", "([B)V", crc32c_update_bytes, cratonvm_native_api::NativeKind::Bridge);
    r.register(crc32c, "update", "([BII)V", crc32c_update_bytes_off_len, cratonvm_native_api::NativeKind::Bridge);
    r.register(crc32c, "getValue", "()J", crc32c_get_value, cratonvm_native_api::NativeKind::Bridge);
    r.register(crc32c, "reset", "()V", crc32c_reset, cratonvm_native_api::NativeKind::Bridge);
    }

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
