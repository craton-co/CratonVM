// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Intrinsics for `java.nio.HeapByteBuffer`'s absolute scalar getters.
//!
//! # Why these exist
//!
//! A heap `ByteBuffer` scalar read is one array access behind a lot of Java.
//! `HeapByteBuffer.getInt(int)` runs `checkIndex` → `Objects.checkFromIndexSize`
//! → `byteOffset` → `SCOPED_MEMORY_ACCESS.getIntUnaligned`, and that last hop
//! is a registered native whose generic `byte[]` path allocates a `Vec` and
//! makes one virtual `get_array_element` call **per byte**.
//!
//! MEASURED before this module (`probes/ByteBufferScalarSplitProbe.java`,
//! ns/op, JIT on): `get(int)` 1.54 on HotSpot vs 242.24 here; `getShort(int)`
//! 1.44 vs 1161.77; `getInt(int)` 1.60 vs 1334.90.
//!
//! `ZipContentTests` spends ~18% of its leaf samples in these accessors,
//! because a zip reader pulls every header field through one. That bounds what
//! speeding them up can buy the class at roughly 10%, which is worth knowing
//! before reading anything into a class-level A/B
//! (`known-issues/springboot/zipcontenttests-bytebuffer-accessor-call-cost-20260810.md`).
//!
//! # The contract these must keep
//!
//! Captured from HotSpot 25 per shape with
//! `probes/ByteBufferAccessorMatrixProbe.java` — measured, not assumed:
//!
//! * Bounds are checked against **`limit`**, never `capacity`. A buffer whose
//!   `limit` is below its `capacity` has live backing bytes that Java says are
//!   out of range; returning them would leak across a `slice`/`limit` boundary.
//! * The failure is `java.lang.IndexOutOfBoundsException` with a **null**
//!   message — the plain class, not `ArrayIndexOutOfBoundsException` (its
//!   subclass), because the two are not interchangeable to a `catch`.
//! * `slice()` produces a buffer whose `offset` field is non-zero, so index `i`
//!   reads `hb[offset + i]`. `wrap(array, off, len)` does NOT — it moves
//!   `position`/`limit` and leaves `offset` at 0. Both are in the matrix
//!   because confusing them reads the wrong bytes without failing.
//! * `HeapByteBufferR` (read-only) is a SUBCLASS that inherits these getters,
//!   so it reaches this code and must answer identically.
//! * `DirectByteBuffer` declares its own and is untouched.
//!
//! # Why only the MULTI-BYTE getters
//!
//! `get(int)` and `get()` are left on the Java path, and that is a measurement
//! rather than a judgement call: they are the only accessors here whose Java
//! body makes no native call, so a native crossing makes them SLOWER (0.76x
//! measured for `get(int)`). See [`register_heap_byte_buffer_accessors`].
//! Intrinsifying everything that looked alike would have cost throughput on
//! the commonest accessor of the set.
//!
//! Both the ABSOLUTE (`getInt(int)`) and RELATIVE (`getInt()`) forms are
//! covered. The relative ones were added after the absolute-only version
//! moved `ZipContentTests` not at all: that class's header reader calls
//! `getShort()`/`getInt()` and the absolute forms never, so the first version
//! optimised a shape the hot code does not execute. Reach first, then speed.
//!
//! # There is no "decline" once registered
//!
//! A native that returns `Ok(None)` for a NON-void descriptor does not fall
//! back to the Java body — `invoke_cached_native_callback_impl` simply pushes
//! nothing, and the caller reads whatever was already on the operand stack.
//! So this module must answer every call it accepts, and the switch has to
//! happen at REGISTRATION time. It does; see
//! [`register_heap_byte_buffer_accessors`].
//!
//! # DEFAULT OFF — `CRATONVM_BYTEBUFFER_INTRINSIC=1` enables it
//!
//! It makes the accessors measurably faster and is off because its effect on
//! the JIT configuration is UNPROVEN, not because it is known to hurt. The
//! numbers, the ceiling arithmetic that bounds them, and a mechanism claim that
//! turned out to be wrong are all on
//! [`register_heap_byte_buffer_accessors`]. Read that before enabling this.

use super::*;

/// The fields a heap-buffer read needs. `position` matters only to the
/// relative forms, which consume and then advance it.
struct HeapBufferView {
    array: ObjectRef,
    offset: i32,
    limit: i32,
    position: i32,
    big_endian: bool,
}

/// Read the backing-array view of a heap buffer.
///
/// `hb` and `limit` are REQUIRED: without the array there is nothing to read,
/// and without the limit there is no bound to enforce. The other two are
/// defaulted to the values the JDK itself starts from, which is not a guess:
///
/// * `offset` defaults to 0 — what every non-`slice` buffer has.
/// * `bigEndian` defaults to `true` — `ByteBuffer`'s documented initial order.
///   CratonVM's own synthetic `java/nio/HeapByteBuffer` instances (see
///   `charset.rs`) set `hb`/`offset`/`limit` by name but never `bigEndian`,
///   and they are big-endian precisely because nothing has called `order()`
///   on them.
fn view(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<HeapBufferView> {
    let array = match ctx.get_field_by_name(this, "hb") {
        Value::Object(Some(a)) => a,
        _ => return None,
    };
    let limit = match ctx.get_field_by_name(this, "limit") {
        Value::Int(v) if v >= 0 => v,
        _ => return None,
    };
    let offset = match ctx.get_field_by_name(this, "offset") {
        Value::Int(v) if v >= 0 => v,
        _ => 0,
    };
    let position = match ctx.get_field_by_name(this, "position") {
        Value::Int(v) if v >= 0 => v,
        _ => return None,
    };
    let big_endian = match ctx.get_field_by_name(this, "bigEndian") {
        Value::Int(v) => v != 0,
        _ => true,
    };
    Some(HeapBufferView {
        array,
        offset,
        limit,
        position,
        big_endian,
    })
}

/// `java.lang.IndexOutOfBoundsException` with a null message — what
/// `Objects.checkFromIndexSize` throws through `Buffer`'s index check, and
/// what the matrix records for every absolute out-of-range read on HotSpot.
fn index_out_of_bounds() -> MethodCallFailed {
    MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::IndexOutOfBoundsException {
        message: None,
    }))
}

/// A receiver this module cannot read.
///
/// Loud on purpose. Since a registered native cannot hand the call back to the
/// Java body (see the module header), the alternative to raising here is
/// pushing nothing and letting the caller consume a stale operand — a wrong
/// number with no symptom. A `HeapByteBuffer` with no readable `hb`/`limit` is
/// a VM-side layout problem, and this names it.
fn unreadable_receiver(ctx: &mut dyn NativeContext, this: ObjectRef) -> MethodCallFailed {
    let class = ctx
        .class_name_of_id(ctx.class_id_of_object(this))
        .unwrap_or_else(|| "<unknown>".to_string());
    MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::IllegalStateException {
        message: format!(
            "HeapByteBuffer intrinsic: receiver of class {class} has no readable \
             `hb`/`limit` field pair; unset CRATONVM_BYTEBUFFER_INTRINSIC to \
             run the Java accessors instead"
        ),
    }))
}

/// Copy `WIDTH` bytes out of the backing array at buffer index `index`,
/// enforcing the `limit` bound first.
///
/// `checked_add` rather than `index + WIDTH`: `getInt(Integer.MAX_VALUE)` must
/// raise, not wrap to a small index that passes the check and reads elsewhere.
/// The matrix pins both `MAX_VALUE` and `MIN_VALUE`.
fn read_bytes<const WIDTH: usize>(
    ctx: &mut dyn NativeContext,
    view: &HeapBufferView,
    index: i32,
) -> Result<[u8; WIDTH], MethodCallFailed> {
    if index < 0 {
        return Err(index_out_of_bounds());
    }
    match index.checked_add(WIDTH as i32) {
        Some(end) if end <= view.limit => {}
        _ => return Err(index_out_of_bounds()),
    }
    let src = match view.offset.checked_add(index) {
        Some(src) if src >= 0 => src as usize,
        _ => return Err(index_out_of_bounds()),
    };
    let mut bytes = [0u8; WIDTH];
    // `read_byte_array_into` is a `memcpy` from the array payload in the VM
    // override, and returns a short count on a bounds problem. A buffer whose
    // `limit` reaches past its own backing array is malformed; refuse rather
    // than return whatever was copied.
    if ctx.read_byte_array_into(view.array, src, &mut bytes) != WIDTH {
        return Err(index_out_of_bounds());
    }
    Ok(bytes)
}

fn assemble_u16(bytes: [u8; 2], big_endian: bool) -> u16 {
    if big_endian {
        u16::from_be_bytes(bytes)
    } else {
        u16::from_le_bytes(bytes)
    }
}

fn assemble_u32(bytes: [u8; 4], big_endian: bool) -> u32 {
    if big_endian {
        u32::from_be_bytes(bytes)
    } else {
        u32::from_le_bytes(bytes)
    }
}

fn assemble_u64(bytes: [u8; 8], big_endian: bool) -> u64 {
    if big_endian {
        u64::from_be_bytes(bytes)
    } else {
        u64::from_le_bytes(bytes)
    }
}

/// Shared body for every absolute getter. `decode` turns the raw bytes into
/// the `Value` the descriptor promises.
fn absolute<const WIDTH: usize>(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    decode: fn([u8; WIDTH], bool) -> Value,
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        // No receiver at all: a `NullPointerException` is what the Java body
        // would have produced on the first field access.
        _ => {
            return Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NullPointerException {
                    message: Some("HeapByteBuffer intrinsic: null receiver".to_string()),
                },
            )))
        }
    };
    let index = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => return Err(index_out_of_bounds()),
    };
    let Some(view) = view(ctx, this) else {
        return Err(unreadable_receiver(ctx, this));
    };
    let bytes = read_bytes::<WIDTH>(ctx, &view, index)?;
    Ok(Some(decode(bytes, view.big_endian)))
}

/// `java.nio.BufferUnderflowException` — what `Buffer.nextGetIndex(nb)` throws
/// when fewer than `nb` bytes remain. A DIFFERENT type from the absolute
/// forms' `IndexOutOfBoundsException`, and not interchangeable to a `catch`.
fn buffer_underflow(ctx: &mut dyn NativeContext) -> MethodCallFailed {
    match ctx.new_object_initialized("java/nio/BufferUnderflowException", "()V", &[]) {
        Ok(Some(Value::Object(Some(exc)))) => MethodCallFailed::ExceptionThrown(exc),
        // Could not build the class. Fail the read rather than answer it —
        // returning a value for a read Java says is short is the one outcome
        // that must not happen.
        Ok(_) => index_out_of_bounds(),
        Err(e) => e,
    }
}

/// Shared body for every relative getter.
///
/// `Buffer.nextGetIndex(nb)` is the contract being reproduced:
///
/// ```java
/// int p = position;
/// if (limit - p < nb) throw new BufferUnderflowException();
/// position = p + nb;
/// return p;
/// ```
///
/// Two details the matrix pins and this must not get wrong: the bound is
/// `limit - position < width` (so it cannot be replaced by the absolute form's
/// check without changing which exception comes out), and `position` advances
/// ONLY on success — HotSpot leaves it at 30 after a failed `getInt()` at
/// position 30.
fn relative<const WIDTH: usize>(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    decode: fn([u8; WIDTH], bool) -> Value,
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NullPointerException {
                    message: Some("HeapByteBuffer intrinsic: null receiver".to_string()),
                },
            )))
        }
    };
    let Some(view) = view(ctx, this) else {
        return Err(unreadable_receiver(ctx, this));
    };
    // `limit - position` as i64: both are non-negative i32s, so the difference
    // cannot overflow, but computing it in i32 and comparing against a width
    // invites a wrap the moment either field is ever allowed to be larger.
    if (view.limit as i64) - (view.position as i64) < WIDTH as i64 {
        return Err(buffer_underflow(ctx));
    }
    let bytes = read_bytes::<WIDTH>(ctx, &view, view.position)?;
    ctx.set_field_by_name(this, "position", Value::Int(view.position + WIDTH as i32));
    Ok(Some(decode(bytes, view.big_endian)))
}

fn decode_short(bytes: [u8; 2], big_endian: bool) -> Value {
    Value::Int(assemble_u16(bytes, big_endian) as i16 as i32)
}

fn decode_char(bytes: [u8; 2], big_endian: bool) -> Value {
    Value::Int(assemble_u16(bytes, big_endian) as i32)
}

fn decode_int(bytes: [u8; 4], big_endian: bool) -> Value {
    Value::Int(assemble_u32(bytes, big_endian) as i32)
}

fn decode_long(bytes: [u8; 8], big_endian: bool) -> Value {
    Value::Long(assemble_u64(bytes, big_endian) as i64)
}

fn decode_float(bytes: [u8; 4], big_endian: bool) -> Value {
    // `from_bits`, not a numeric cast: a NaN payload must survive, which the
    // matrix checks through `floatToRawIntBits`.
    Value::Float(f32::from_bits(assemble_u32(bytes, big_endian)))
}

fn decode_double(bytes: [u8; 8], big_endian: bool) -> Value {
    Value::Double(f64::from_bits(assemble_u64(bytes, big_endian)))
}

/// Register the heap-buffer absolute scalar getters.
///
/// Registered on `java/nio/HeapByteBuffer` only. `HeapByteBufferR` inherits
/// them (it overrides only the mutators), so a read-only buffer resolves to
/// these and is served here; `DirectByteBuffer` declares its own and is
/// untouched.
///
/// # DEFAULT OFF — opt in with `CRATONVM_BYTEBUFFER_INTRINSIC=1`
///
/// This is faster than the Java accessors and still not worth turning on by
/// default, because the two statements are about different configurations.
///
/// MEASURED, `ZipContentTests` (29 tests), one host, interleaved:
///
/// | arm | intrinsic ON | intrinsic OFF | |
/// |---|---:|---:|---|
/// | `--nojit` | **187.0s** | 206.5s | intrinsic 9.4% FASTER |
/// | JIT, round 1 | 464.3s | 413.7s | intrinsic 12.2% slower |
/// | JIT, round 2 | 375.8s | 353.8s | intrinsic 6.2% slower |
/// | JIT, earlier pair | 477.3s | 450.7s | intrinsic 5.9% slower |
///
/// The microbenchmark agrees with the interpreter and not with the JIT:
/// `getShort()` 1.42x, `getInt()` 1.5x, `getShort(int)` 2.09x, `getInt(int)`
/// 2.14x (`probes/ByteBufferScalarSplitProbe.java`).
///
/// THE MECHANISM IS NOT ESTABLISHED, and an earlier version of this comment
/// asserted one that is wrong. It said a registered native loses because the
/// JIT can no longer INLINE the accessor. It never inlined it: the single-pass
/// emitter "bails on any callee invoke that is not a resolver-proven elidable
/// super-`<init>`" (`jit/src/lib.rs:5239`), and `HeapByteBuffer.getShort()` is
/// four invokes. The JIT does COMPILE those methods, which is a different
/// thing.
///
/// The JIT-arm numbers also carry less weight than they look. The SAME
/// configuration (intrinsic OFF, JIT) measured 315.5s / 353.8s / 413.7s /
/// 450.7s across one session — ±20%, wider than the 6-12% deltas. And the
/// ceiling was always small: the accessors were ~18% of leaf samples, so a
/// 2.4x on them removes at most `18% x (1 - 1/2.4)` ≈ 10% of total time. An
/// experiment with a 10% ceiling and ±20% noise cannot resolve its own sign.
///
/// The `--nojit` arm lands exactly where that arithmetic predicts — 9.4%
/// against a ~10% ceiling — which is the best evidence that the code does what
/// it claims.
///
/// So this stays OFF because its benefit under the JIT is unproven, NOT
/// because it is known to hurt. What lands here is the measurement, the
/// HotSpot-diffed contract (`probes/ByteBufferAccessorMatrixProbe.java`, 119
/// lines identical in both arms), and a working implementation. Settling the
/// JIT arm needs a properly powered measurement (repeated pairs, or
/// per-process CPU time instead of wall clock), not another single pair.
///
/// The flag is read once, at registration: a registered native has no way to
/// decline an individual call (module header), so the switch has to be here.
pub fn register_heap_byte_buffer_accessors(registry: &mut NativeMethodRegistry) {
    let enabled = cratonvm_types::flags::runtime_var_os("CRATONVM_BYTEBUFFER_INTRINSIC")
        .is_some_and(|v| !v.is_empty() && v != "0");
    if !enabled {
        return;
    }

    let class = "java/nio/HeapByteBuffer";
    // `get(int)` is deliberately NOT registered. Alone among these, its Java
    // body makes no native call — it is `hb[ix(checkIndex(i))]`, four compiled
    // Java calls and an array load — so replacing it with a native crossing
    // costs more than it saves. MEASURED, min of 3 interleaved rounds:
    //
    //   buffer.get(int)      intrinsic 413.69 ns vs Java 315.47 ns -> 0.76x
    //   buffer.getShort(int) intrinsic 602.28 ns vs Java 1535.44 ns -> 2.55x
    //   buffer.getInt(int)   intrinsic 679.26 ns vs Java 1627.59 ns -> 2.40x
    //
    // The multi-byte getters win because their Java path ALREADY paid a
    // `ScopedMemoryAccess` native crossing on top of the index arithmetic;
    // this replaces two layers with one. `get(int)` has only the one layer,
    // and it is cheaper than the funnel.
    registry.register(class, "getShort", "(I)S", |ctx, args| {
        absolute::<2>(ctx, args, decode_short)
    });
    registry.register(class, "getChar", "(I)C", |ctx, args| {
        absolute::<2>(ctx, args, decode_char)
    });
    registry.register(class, "getInt", "(I)I", |ctx, args| {
        absolute::<4>(ctx, args, decode_int)
    });
    registry.register(class, "getLong", "(I)J", |ctx, args| {
        absolute::<8>(ctx, args, decode_long)
    });
    registry.register(class, "getFloat", "(I)F", |ctx, args| {
        absolute::<4>(ctx, args, decode_float)
    });
    registry.register(class, "getDouble", "(I)D", |ctx, args| {
        absolute::<8>(ctx, args, decode_double)
    });

    // The RELATIVE forms. These are the ones that actually carry the zip
    // reader: `ZipCentralDirectoryFileHeaderRecord.load` calls `getShort()`
    // eleven times and `getInt()` six times per header, and the absolute forms
    // not at all — which is why intrinsifying only the absolute set moved that
    // class not at all. Reach, then speed.
    //
    // `get()` (one byte) is excluded for the same measured reason as
    // `get(int)`: no native on its Java path, so a crossing makes it slower.
    registry.register(class, "getShort", "()S", |ctx, args| {
        relative::<2>(ctx, args, decode_short)
    });
    registry.register(class, "getChar", "()C", |ctx, args| {
        relative::<2>(ctx, args, decode_char)
    });
    registry.register(class, "getInt", "()I", |ctx, args| {
        relative::<4>(ctx, args, decode_int)
    });
    registry.register(class, "getLong", "()J", |ctx, args| {
        relative::<8>(ctx, args, decode_long)
    });
    registry.register(class, "getFloat", "()F", |ctx, args| {
        relative::<4>(ctx, args, decode_float)
    });
    registry.register(class, "getDouble", "()D", |ctx, args| {
        relative::<8>(ctx, args, decode_double)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Byte assembly is the half that needs no VM, and the half that returns a
    /// plausible WRONG number when it is wrong.
    #[test]
    fn assembly_follows_the_buffer_byte_order() {
        assert_eq!(assemble_u16([0x01, 0x02], true), 0x0102);
        assert_eq!(assemble_u16([0x01, 0x02], false), 0x0201);
        assert_eq!(assemble_u32([1, 2, 3, 4], true), 0x0102_0304);
        assert_eq!(assemble_u32([1, 2, 3, 4], false), 0x0403_0201);
        assert_eq!(
            assemble_u64([1, 2, 3, 4, 5, 6, 7, 8], true),
            0x0102_0304_0506_0708
        );
        assert_eq!(
            assemble_u64([1, 2, 3, 4, 5, 6, 7, 8], false),
            0x0807_0605_0403_0201
        );
    }

    /// `getShort` is signed and `getChar` is not, off the same two bytes.
    #[test]
    fn short_is_signed_and_char_is_not() {
        assert_eq!(decode_short([0xFF, 0xFE], true), Value::Int(-2));
        assert_eq!(decode_char([0xFF, 0xFE], true), Value::Int(0xFFFE));
    }

    /// A NaN payload must survive; a numeric cast would not preserve it.
    #[test]
    fn float_decoding_preserves_the_raw_bit_pattern() {
        let bits: u32 = 0x7FC0_0001;
        let Value::Float(f) = decode_float(bits.to_be_bytes(), true) else {
            panic!("expected a float");
        };
        assert_eq!(f.to_bits(), bits);
        let dbits: u64 = 0x7FF8_0000_0000_0001;
        let Value::Double(d) = decode_double(dbits.to_be_bytes(), true) else {
            panic!("expected a double");
        };
        assert_eq!(d.to_bits(), dbits);
    }
}
