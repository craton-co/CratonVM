// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! T12 JDK 25 `jdk/internal/misc/Unsafe` native methods.
//!
//! Implements the remaining Unsafe methods that JDK 25 declares as native but
//! were not yet covered in `lib.rs`:
//!
//! - `addressSize0()I` — returns 8 (64-bit VM)
//! - `isBigEndian0()Z` — returns false (little-endian)
//! - `unalignedAccess0()Z` — returns true (x86-64/ARM64 allow unaligned access)
//! - `loadLoadFence()V` — acquire fence
//! - `storeStoreFence()V` — release fence
//! - `copySwapMemory0(...)V` — copy with byte-order swap per element

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::{ObjectRef, Value};

use crate::{unsafe_obj, unsafe_offset};

// ---------------------------------------------------------------------------
// Native implementations
// ---------------------------------------------------------------------------

/// `Unsafe.loadLoadFence()` — ensures that loads before the fence are not
/// reordered with loads after it. Maps to an acquire fence.
fn native_unsafe_load_load_fence(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
    Ok(None)
}

/// `Unsafe.storeStoreFence()` — ensures that stores before the fence are not
/// reordered with stores after it. Maps to a release fence.
fn native_unsafe_store_store_fence(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
    Ok(None)
}

/// `Unsafe.copySwapMemory0(Object srcBase, long srcOffset, Object destBase,
///   long destOffset, long bytes, long elemSize)` — copies memory from src to
/// dest while swapping byte order per element.
///
/// In our slot-based VM model, "bytes" is treated as the total number of
/// elements to copy (since each slot is one value). For each integer value
/// copied, byte-swapping is applied according to `elemSize`:
/// - 2: 16-bit swap (i16)
/// - 4: 32-bit swap (i32)
/// - 8: 64-bit swap (i64)
///
/// If the source object is `None` (null), this is an off-heap operation which
/// we do not support — we return `Ok(None)` silently.
pub fn native_unsafe_copy_swap_memory(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: [this, srcBase, srcOffset, destBase, destOffset, bytes, elemSize]
    //
    // Copies `bytes` raw bytes from src to dst, byte-swapping every
    // `elemSize`-byte group. This is the path `CharBuffer.getArray` /
    // `IntBuffer.getArray` take when the buffer view's byte order differs
    // from the host's native order (e.g. `ByteBufferAsCharBufferB` — a
    // big-endian char view on a little-endian host — which is exactly how
    // `ICUBinary.getChars`/`getInts` reads the `nfc.nrm` trie tables).
    //
    // The previous implementation copied `bytes` *slots* with `get_field`/
    // `set_field`, treating raw byte offsets as field indices — corrupting
    // memory whenever src/dst were primitive arrays of differing element
    // widths. We now read the source as a byte stream, swap, and write a
    // byte stream to the destination.
    let src_obj = unsafe_obj(args, 1);
    let src_offset = unsafe_offset(args, 2);
    let dest_obj = unsafe_obj(args, 3);
    let dest_offset = unsafe_offset(args, 4);
    let bytes = match args.get(5) {
        Some(Value::Long(b)) => *b as usize,
        Some(Value::Int(b)) => *b as usize,
        _ => 0,
    };
    let elem_size = match args.get(6) {
        Some(Value::Long(s)) => *s as usize,
        Some(Value::Int(s)) => *s as usize,
        _ => 0,
    };
    if bytes == 0 {
        return Ok(None);
    }

    // JIT round 17 wave 4, lane bufcmp: an off-heap side goes through the
    // NativeContext memory bridge, which routes a TAGGED `allocateMemory`
    // handle to the arena store and a REAL pointer to a raw copy -- the same
    // dispatch `copyMemory0` already uses. The arena-only arms below never saw
    // a real pointer, and an `Arena.ofConfined().allocate(..)` block IS one
    // (`allocate_native_memory`), so `seg.asByteBuffer().asIntBuffer()
    // .get(0, int[2])` on a big-endian view (`IntBuffer.getArray` ->
    // `SCOPED_MEMORY_ACCESS.copySwapMemory`) returned without writing: MEASURED
    // `bulk=0/0` against HotSpot's `bulk=168496141/-2` (R17Nio2Typed).
    // `CRATONVM_UNSAFE_COPY_SWAP_NATIVE_POINTER=0` restores the old arms.
    if src_obj.is_none() || dest_obj.is_none() {
        // The RAW `long`s, not `unsafe_offset`: that decoder clamps anything
        // above 1 GiB that is not a live arena handle to 0, i.e. it erases
        // exactly the real pointer this arm exists for.
        let src_raw = raw_long_arg(args, 2);
        let dest_raw = raw_long_arg(args, 4);
        if let Some(done) =
            copy_swap_via_native_bridge(ctx, src_obj, src_raw, dest_obj, dest_raw, bytes, elem_size)
        {
            return done;
        }
    }

    // A null base is an OFF-HEAP address, and returning quietly meant an
    // endian-converting copy between two `allocateMemory` blocks wrote nothing
    // at all. MEASURED, HotSpot 25.0.4+7: elemSize 2 over [1..8] gives
    // [2,1,4,3,6,5,8,7]; CratonVM left the destination as eight zeros -- a
    // silent no-op, which is the failure mode that reads as success.
    //
    // `bytes` here is a real byte count, so the arena's own bounds-checked
    // copy_out/copy_in are the right primitives; anything the arena does not
    // recognise is refused rather than dereferenced.
    if src_obj.is_none() && dest_obj.is_none() {
        let mut buf = vec![0u8; bytes];
        if !crate::unsafe_arena_copy_out(src_offset as i64, &mut buf) {
            return Err(
                cratonvm_types::error::RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Unsafe.copySwapMemory: source 0x{:x} is not in any live arena",
                        src_offset
                    ),
                }
                .into(),
            );
        }
        if elem_size >= 2 {
            for chunk in buf.chunks_mut(elem_size) {
                chunk.reverse();
            }
        }
        if !crate::unsafe_arena_copy_in(dest_offset as i64, &buf) {
            return Err(
                cratonvm_types::error::RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Unsafe.copySwapMemory: destination 0x{:x} is not in any live arena",
                        dest_offset
                    ),
                }
                .into(),
            );
        }
        return Ok(None);
    }

    // SBR3 lane ffm: ONE side an `allocateMemory` block, the other a primitive
    // array -- the shape the real `MemorySegment.copy(seg, layout, off, int[],
    // idx, n)` takes through `ScopedMemoryAccess.copySwapMemory` when the
    // layout's byte order is not the host's (a native segment copied into a
    // heap array answered `0` instead of the swapped values: the `None` arms
    // below returned quietly).
    let is_prim_array = |ctx: &mut dyn NativeContext, o: ObjectRef| {
        ctx.heap_kind_of(o) == cratonvm_types::ObjectKind::Array
            && ctx.heap_element_type_of(o) != cratonvm_types::ArrayElementType::Reference
    };
    let swap_in_place = |buf: &mut Vec<u8>| {
        if elem_size >= 2 {
            for chunk in buf.chunks_mut(elem_size) {
                chunk.reverse();
            }
        }
    };
    match (src_obj, dest_obj) {
        (None, Some(d))
            if crate::unsafe_arena_names_block(src_offset as i64) && is_prim_array(ctx, d) =>
        {
            let mut buf = vec![0u8; bytes];
            if !crate::unsafe_arena_copy_out(src_offset as i64, &mut buf) {
                return Err(
                    cratonvm_types::error::RuntimeError::IllegalArgumentException {
                        message: format!(
                            "Unsafe.copySwapMemory: source 0x{:x} is not in any live arena",
                            src_offset
                        ),
                    }
                    .into(),
                );
            }
            swap_in_place(&mut buf);
            if crate::unsafe_array_write_bytes(ctx, d, dest_offset, &buf) {
                return Ok(None);
            }
        }
        (Some(s), None)
            if crate::unsafe_arena_names_block(dest_offset as i64) && is_prim_array(ctx, s) =>
        {
            if let Some(mut buf) = crate::unsafe_array_read_bytes(ctx, s, src_offset, bytes) {
                swap_in_place(&mut buf);
                if !crate::unsafe_arena_copy_in(dest_offset as i64, &buf) {
                    return Err(
                        cratonvm_types::error::RuntimeError::IllegalArgumentException {
                            message: format!(
                                "Unsafe.copySwapMemory: destination 0x{:x} is not in any live arena",
                                dest_offset
                            ),
                        }
                        .into(),
                    );
                }
                return Ok(None);
            }
        }
        _ => {}
    }

    let src = match src_obj {
        Some(s) => s,
        None => return Ok(None),
    };
    let dst = match dest_obj {
        Some(d) => d,
        None => return Ok(None),
    };

    let src_is_array = ctx.heap_kind_of(src) == cratonvm_types::ObjectKind::Array
        && ctx.heap_element_type_of(src) != cratonvm_types::ArrayElementType::Reference;
    let dst_is_array = ctx.heap_kind_of(dst) == cratonvm_types::ObjectKind::Array
        && ctx.heap_element_type_of(dst) != cratonvm_types::ArrayElementType::Reference;

    if src_is_array && dst_is_array {
        if let Some(mut buf) = crate::unsafe_array_read_bytes(ctx, src, src_offset, bytes) {
            // Byte-swap each elemSize-byte group in place.
            if elem_size >= 2 {
                for chunk in buf.chunks_mut(elem_size) {
                    chunk.reverse();
                }
            }
            if crate::unsafe_array_write_bytes(ctx, dst, dest_offset, &buf) {
                return Ok(None);
            }
        }
    }

    // JIT round 20 wave 8c: under byte field offsets an object-field side is
    // the fields' BYTES (an array side keeps its byte offsets).
    if ctx.unsafe_byte_field_offsets() {
        let mut buf = vec![0u8; bytes];
        for (i, b) in buf.iter_mut().enumerate() {
            let at = src_offset.wrapping_add(i);
            *b = if src_is_array {
                crate::unsafe_array_read_bytes(ctx, src, at, 1).map_or(0, |v| v[0])
            } else {
                crate::unsafe_field_byte_at(&*ctx, src, at).unwrap_or(0)
            };
        }
        if elem_size >= 2 {
            for chunk in buf.chunks_mut(elem_size) {
                chunk.reverse();
            }
        }
        for (i, b) in buf.iter().enumerate() {
            let at = dest_offset.wrapping_add(i);
            if dst_is_array {
                let _ = crate::unsafe_array_write_bytes(ctx, dst, at, &[*b]);
            } else {
                let _ = crate::unsafe_field_set_byte_at(ctx, dst, at, *b);
            }
        }
        return Ok(None);
    }
    // Fallback: legacy slot-by-slot swap for non-array (object-field) targets.
    // HotSpot byte offsets that are outside Craton's small heap-slot range must
    // use the Unsafe synthetic side store; treating them as field indices can
    // write through padded real-JDK mirror layouts and corrupt neighbouring heap
    // Value cells.
    for i in 0..bytes {
        let src_slot = src_offset + i;
        let dst_slot = dest_offset + i;
        let val = if crate::unsafe_offset_is_heap_slot(ctx, src, src_slot) {
            ctx.get_field(src, src_slot)
        } else {
            crate::synthetic_get(ctx, src, src_slot)
        };
        let swapped = swap_value(val, elem_size);
        if crate::unsafe_offset_is_heap_slot(ctx, dst, dst_slot) {
            ctx.set_field(dst, dst_slot, swapped);
        } else {
            crate::synthetic_put(ctx, dst, dst_slot, swapped);
        }
    }

    Ok(None)
}



/// The `long` argument at `pos`, undecoded (no field-slot clamp).
fn raw_long_arg(args: &[Value], pos: usize) -> i64 {
    match args.get(pos) {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => i64::from(*v),
        _ => 0,
    }
}

/// Largest single `copySwapMemory0` this path buffers (the 256 MiB cap the
/// other off-heap `Unsafe` copies use). Anything larger takes the legacy arms.
const COPY_SWAP_MAX_BYTES: usize = 256 * 1024 * 1024;

/// `copySwapMemory0` with at least one OFF-HEAP side (a null base), through the
/// `NativeContext` memory bridge: a tagged `Unsafe.allocateMemory` handle goes
/// to the arena store, a real pointer (an FFM arena block, an `allocateDirect`
/// buffer) to a raw copy; null/negative addresses and freed handles are
/// refused there and surface as `IllegalArgumentException`. The heap side must
/// be a primitive array (`Unsafe` byte offset including the array base).
///
/// `None` means "not a shape this path owns" (a heap side that is not a
/// primitive array, or an oversize length) and the caller runs its legacy arms.
fn copy_swap_via_native_bridge(
    ctx: &mut dyn NativeContext,
    src_obj: Option<ObjectRef>,
    src_raw: i64,
    dest_obj: Option<ObjectRef>,
    dest_raw: i64,
    bytes: usize,
    elem_size: usize,
) -> Option<MethodCallResult> {
    if bytes > COPY_SWAP_MAX_BYTES {
        return None;
    }
    let is_prim_array = |ctx: &mut dyn NativeContext, o: ObjectRef| {
        ctx.heap_kind_of(o) == cratonvm_types::ObjectKind::Array
            && ctx.heap_element_type_of(o) != cratonvm_types::ArrayElementType::Reference
    };
    if let Some(s) = src_obj {
        if !is_prim_array(ctx, s) {
            return None;
        }
    }
    if let Some(d) = dest_obj {
        if !is_prim_array(ctx, d) {
            return None;
        }
    }
    let iae = |what: &str, addr: i64| -> MethodCallResult {
        Err(
            cratonvm_types::error::RuntimeError::IllegalArgumentException {
                message: format!(
                    "Unsafe.copySwapMemory: {what} address 0x{addr:x} is not addressable"
                ),
            }
            .into(),
        )
    };
    let mut buf = match src_obj {
        None => {
            let mut buf = vec![0u8; bytes];
            if !ctx.copy_from_native_memory(src_raw, &mut buf) {
                return Some(iae("source", src_raw));
            }
            buf
        }
        Some(s) => {
            if src_raw < 0 {
                return Some(Err(cratonvm_types::error::RuntimeError::aioobe_index_only(
                    src_raw as i32,
                )
                .into()));
            }
            match crate::unsafe_array_read_bytes(ctx, s, src_raw as usize, bytes) {
                Some(b) => b,
                None => {
                    return Some(Err(cratonvm_types::error::RuntimeError::aioobe_index_only(
                        src_raw as i32,
                    )
                    .into()))
                }
            }
        }
    };
    if elem_size >= 2 {
        for chunk in buf.chunks_mut(elem_size) {
            chunk.reverse();
        }
    }
    match dest_obj {
        None => {
            if !ctx.copy_to_native_memory(dest_raw, &buf) {
                return Some(iae("destination", dest_raw));
            }
        }
        Some(d) => {
            if dest_raw < 0 || !crate::unsafe_array_write_bytes(ctx, d, dest_raw as usize, &buf) {
                return Some(Err(cratonvm_types::error::RuntimeError::aioobe_index_only(
                    dest_raw as i32,
                )
                .into()));
            }
        }
    }
    Some(Ok(None))
}

/// Apply byte-swap to a Value according to the element size.
fn swap_value(val: Value, elem_size: usize) -> Value {
    match val {
        Value::Int(v) => {
            let swapped = match elem_size {
                2 => {
                    // Swap the low 16 bits, sign-extend back to i32
                    let half = v as i16;
                    half.swap_bytes() as i32
                }
                4 => v.swap_bytes(),
                8 => v.swap_bytes(), // i32 within a 64-bit context; swap all 4 bytes
                _ => v,
            };
            Value::Int(swapped)
        }
        Value::Long(v) => {
            let swapped = match elem_size {
                2 => {
                    let half = v as i16;
                    half.swap_bytes() as i64
                }
                4 => {
                    let word = v as i32;
                    word.swap_bytes() as i64
                }
                8 => v.swap_bytes(),
                _ => v,
            };
            Value::Long(swapped)
        }
        // Non-numeric values pass through unchanged
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register the T12 JDK 25 Unsafe native methods that are not yet covered in
/// `lib.rs`. Call this from the main registration function after
/// `register_essential_natives`.
pub fn register_t12_unsafe_natives(registry: &mut NativeMethodRegistry) {
            let class = "jdk/internal/misc/Unsafe";

    // `addressSize0` / `isBigEndian0` / `unalignedAccess0` were DELETED here
    // 2026-09-12: `javap -p jdk.internal.misc.Unsafe` declares none of the
    // three on 17, 21 or 25. They were the private native primitives behind
    // `addressSize()` / `isBigEndian()` / `unalignedAccess()` in older JDKs;
    // modern images answer those from constants and the `*0` methods are gone.
    // Six registrations — this registrar runs twice — that no caller on any
    // supported image could name.
    registry.register(class, "loadLoadFence", "()V", native_unsafe_load_load_fence, cratonvm_native_api::NativeKind::Bridge);
    registry.register(
        class,
        "storeStoreFence",
        "()V",
        native_unsafe_store_store_fence, cratonvm_native_api::NativeKind::Bridge,
    );
    // copySwapMemory0 is registered in lib.rs (replacing the previous stub)
    // so we do NOT re-register it here to avoid double-registration.
    }

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
