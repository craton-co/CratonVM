// SPDX-License-Identifier: Apache-2.0
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

/// `Unsafe.addressSize0()` — returns the size of a native pointer in bytes.
/// Derived from the host pointer width instead of a hard-coded 8, so it agrees
/// with `unsafe_natives_ext::native_unsafe_address_size` and stays correct on a
/// 32-bit build.
fn native_unsafe_address_size0(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(std::mem::size_of::<usize>() as i32)))
}

/// `Unsafe.isBigEndian0()` — returns whether the platform is big-endian.
///
/// Read from the compile target rather than hard-coded to little-endian.
/// `ByteBuffer`/`VarHandle` byte-order handling and `ScopedMemoryAccess`'s
/// unaligned accessors branch on this, so a wrong answer silently byte-swaps
/// every multi-byte off-heap read on a big-endian host.
fn native_unsafe_is_big_endian0(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(i32::from(cfg!(target_endian = "big")))))
}

/// `Unsafe.unalignedAccess0()` — returns whether unaligned memory access is
/// supported. True on x86/x86-64 and on AArch64 (which permits unaligned
/// accesses to normal memory); conservatively false elsewhere, which only
/// costs a slower byte-at-a-time path in the JDK callers.
fn native_unsafe_unaligned_access0(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let unaligned = cfg!(any(
        target_arch = "x86",
        target_arch = "x86_64",
        target_arch = "aarch64"
    ));
    Ok(Some(Value::Int(i32::from(unaligned))))
}

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
            return Err(cratonvm_types::error::RuntimeError::IllegalArgumentException {
                message: format!(
                    "Unsafe.copySwapMemory: source 0x{:x} is not in any live arena",
                    src_offset
                ),
            }
            .into());
        }
        if elem_size >= 2 {
            for chunk in buf.chunks_mut(elem_size) {
                chunk.reverse();
            }
        }
        if !crate::unsafe_arena_copy_in(dest_offset as i64, &buf) {
            return Err(cratonvm_types::error::RuntimeError::IllegalArgumentException {
                message: format!(
                    "Unsafe.copySwapMemory: destination 0x{:x} is not in any live arena",
                    dest_offset
                ),
            }
            .into());
        }
        return Ok(None);
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
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    let class = "jdk/internal/misc/Unsafe";

    registry.register(class, "addressSize0", "()I", native_unsafe_address_size0);
    registry.register(class, "isBigEndian0", "()Z", native_unsafe_is_big_endian0);
    registry.register(
        class,
        "unalignedAccess0",
        "()Z",
        native_unsafe_unaligned_access0,
    );
    registry.register(class, "loadLoadFence", "()V", native_unsafe_load_load_fence);
    registry.register(
        class,
        "storeStoreFence",
        "()V",
        native_unsafe_store_store_fence,
    );
    // copySwapMemory0 is registered in lib.rs (replacing the previous stub)
    // so we do NOT re-register it here to avoid double-registration.
    registry.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess};
    use super::*;
    use crate::test_utils::MockNativeContext;
    use crate::{
        native_unsafe_allocate_memory, native_unsafe_allocate_memory_realloc,
        native_unsafe_array_base_offset, native_unsafe_array_index_scale, native_unsafe_cas_int,
        native_unsafe_fence, native_unsafe_get_int, native_unsafe_get_int_volatile,
        native_unsafe_get_long, native_unsafe_put_int, native_unsafe_put_int_volatile,
        native_unsafe_put_long,
    };
    use cratonvm_types::{ClassId, ObjectRef, Value};

    /// Create a test object with `num_fields` fields pre-filled with Int(0).
    fn make_test_obj(ctx: &mut MockNativeContext, num_fields: usize) -> ObjectRef {
        let obj = ctx.alloc_object(ClassId::new(1), num_fields);
        for i in 0..num_fields {
            ctx.set_field(obj, i, Value::Int(0));
        }
        obj
    }

    /// Dummy "this" value used as args[0] for Unsafe instance methods.
    fn dummy_this() -> Value {
        Value::Object(None)
    }

    // -----------------------------------------------------------------------
    // Test 1: alloc/free round-trip
    // -----------------------------------------------------------------------
    #[test]
    fn t12_unsafe_alloc_free_round_trip() {
        let mut ctx = MockNativeContext::new();
        // allocateMemory(this, size=64)
        let result =
            native_unsafe_allocate_memory(&mut ctx, &[dummy_this(), Value::Long(64)]).unwrap();
        let addr = match result {
            Some(Value::Long(a)) => a,
            other => panic!("expected Long, got {:?}", other),
        };
        assert_ne!(addr, 0, "allocated address must be non-zero");

        // reallocateMemory(this, oldAddr, newSize=128)
        let result2 = native_unsafe_allocate_memory_realloc(
            &mut ctx,
            &[dummy_this(), Value::Long(addr), Value::Long(128)],
        )
        .unwrap();
        let addr2 = match result2 {
            Some(Value::Long(a)) => a,
            other => panic!("expected Long from realloc, got {:?}", other),
        };
        assert_ne!(addr2, 0, "reallocated address must be non-zero");
    }

    // -----------------------------------------------------------------------
    // Test 2: get/put Int and Long round-trip
    // -----------------------------------------------------------------------
    #[test]
    fn t12_unsafe_get_put_int_round_trip() {
        let mut ctx = MockNativeContext::new();
        let obj = make_test_obj(&mut ctx, 4);

        // Put Int(42) at offset 2
        native_unsafe_put_int(
            &mut ctx,
            &[
                dummy_this(),
                Value::Object(Some(obj)),
                Value::Long(2),
                Value::Int(42),
            ],
        )
        .unwrap();

        // Get it back
        let result = native_unsafe_get_int(
            &mut ctx,
            &[dummy_this(), Value::Object(Some(obj)), Value::Long(2)],
        )
        .unwrap();
        assert_eq!(result, Some(Value::Int(42)));

        // Put Long(123456789) at offset 1
        native_unsafe_put_long(
            &mut ctx,
            &[
                dummy_this(),
                Value::Object(Some(obj)),
                Value::Long(1),
                Value::Long(123_456_789),
            ],
        )
        .unwrap();

        // Get it back
        let result2 = native_unsafe_get_long(
            &mut ctx,
            &[dummy_this(), Value::Object(Some(obj)), Value::Long(1)],
        )
        .unwrap();
        assert_eq!(result2, Some(Value::Long(123_456_789)));
    }

    // -----------------------------------------------------------------------
    // Test 3: volatile ordering
    // -----------------------------------------------------------------------
    #[test]
    fn t12_unsafe_volatile_ordering() {
        let mut ctx = MockNativeContext::new();
        let obj = make_test_obj(&mut ctx, 4);

        // Volatile put Int(99) at offset 0
        native_unsafe_put_int_volatile(
            &mut ctx,
            &[
                dummy_this(),
                Value::Object(Some(obj)),
                Value::Long(0),
                Value::Int(99),
            ],
        )
        .unwrap();

        // Volatile read
        let result = native_unsafe_get_int_volatile(
            &mut ctx,
            &[dummy_this(), Value::Object(Some(obj)), Value::Long(0)],
        )
        .unwrap();
        assert_eq!(result, Some(Value::Int(99)));
    }

    // -----------------------------------------------------------------------
    // Test 4: CAS int under contention
    // -----------------------------------------------------------------------
    #[test]
    fn t12_cas_int_atomic_under_contention() {
        let mut ctx = MockNativeContext::new();
        let obj = make_test_obj(&mut ctx, 4);
        ctx.set_field(obj, 0, Value::Int(10));

        // CAS expected=10, update=20 => should succeed (return 1)
        let result = native_unsafe_cas_int(
            &mut ctx,
            &[
                dummy_this(),
                Value::Object(Some(obj)),
                Value::Long(0),
                Value::Int(10),
                Value::Int(20),
            ],
        )
        .unwrap();
        assert_eq!(result, Some(Value::Int(1)));

        // Verify field is now 20
        assert_eq!(ctx.get_field(obj, 0), Value::Int(20));

        // CAS expected=10 (wrong), update=30 => should fail (return 0)
        let result2 = native_unsafe_cas_int(
            &mut ctx,
            &[
                dummy_this(),
                Value::Object(Some(obj)),
                Value::Long(0),
                Value::Int(10),
                Value::Int(30),
            ],
        )
        .unwrap();
        assert_eq!(result2, Some(Value::Int(0)));

        // Field should still be 20
        assert_eq!(ctx.get_field(obj, 0), Value::Int(20));
    }

    // -----------------------------------------------------------------------
    // Test 5: field offset / layout queries
    // -----------------------------------------------------------------------
    #[test]
    fn t12_unsafe_field_offset_matches_layout() {
        let mut ctx = MockNativeContext::new();

        // arrayBaseOffset returns Int(16) — HotSpot 64-bit object header size.
        let base = native_unsafe_array_base_offset(&mut ctx, &[dummy_this()]).unwrap();
        assert_eq!(base, Some(Value::Int(16)));

        // arrayIndexScale with no class mirror defaults to Int(1).
        let scale = native_unsafe_array_index_scale(&mut ctx, &[dummy_this()]).unwrap();
        assert_eq!(scale, Some(Value::Int(1)));

        // addressSize0 returns Int(8)
        let addr_sz = native_unsafe_address_size0(&mut ctx, &[dummy_this()]).unwrap();
        assert_eq!(addr_sz, Some(Value::Int(8)));

        // isBigEndian0 returns Int(0) (false)
        let big_endian = native_unsafe_is_big_endian0(&mut ctx, &[dummy_this()]).unwrap();
        assert_eq!(big_endian, Some(Value::Int(0)));

        // unalignedAccess0 returns Int(1) (true)
        let unaligned = native_unsafe_unaligned_access0(&mut ctx, &[dummy_this()]).unwrap();
        assert_eq!(unaligned, Some(Value::Int(1)));
    }

    // -----------------------------------------------------------------------
    // Test 5b (C39): arrayIndexScale derives element size from the Class
    // mirror's name. JCTools' UnsafeRefArrayAccess.<clinit> threw
    // `IllegalStateException: Unknown pointer size: 1` because we always
    // returned 1 for Object[]. The fix reads the Class name (e.g. `[I`,
    // `[Ljava/lang/Object;`) and returns the corresponding JVM scale.
    // -----------------------------------------------------------------------
    #[test]
    fn c39_array_index_scale_derives_from_class_mirror() {
        use crate::array_index_scale_for_name;

        // Name-derivation helper: covers every JVMS array descriptor.
        assert_eq!(array_index_scale_for_name("[Z"), 1);
        assert_eq!(array_index_scale_for_name("[B"), 1);
        assert_eq!(array_index_scale_for_name("[S"), 2);
        assert_eq!(array_index_scale_for_name("[C"), 2);
        assert_eq!(array_index_scale_for_name("[I"), 4);
        assert_eq!(array_index_scale_for_name("[F"), 4);
        assert_eq!(array_index_scale_for_name("[J"), 8);
        assert_eq!(array_index_scale_for_name("[D"), 8);
        assert_eq!(array_index_scale_for_name("[Ljava/lang/Object;"), 8);
        assert_eq!(array_index_scale_for_name("[Ljava/lang/String;"), 8);
        assert_eq!(array_index_scale_for_name("[[I"), 8);
        // Non-array or unknown → 0, which is what
        // `sun.misc.Unsafe.arrayIndexScale`'s javadoc specifies and what
        // callers guard on with `if (scale == 0) throw`.
        //
        // This assertion said 1 until 2026-08-29 and called it a "defensive
        // default". It was not defensive: 1 is a plausible element width, so a
        // caller computing `base + i * scale` over a class that has no
        // elements got a plausible address instead of a refusal -- the
        // dangerous direction in this family. The value was left alone by an
        // earlier session because its blast radius was unmeasured; measured
        // over the whole 117-vector corpus in both modes,
        // `array_index_scale_for_name` is asked 161/175 times in every vector
        // and NOT ONCE with a non-array, so taking the specified answer costs
        // nothing. See the L1 record, section 9.2.
        assert_eq!(array_index_scale_for_name("java/lang/Object"), 0);
        assert_eq!(array_index_scale_for_name(""), 0);

        // End-to-end: synthesise a Class mirror with `name` at slot 1 and
        // invoke the native. The native should read the name and derive
        // the scale — the critical path that unblocks
        // jctools.UnsafeRefArrayAccess.<clinit>.
        let mut ctx = MockNativeContext::new();
        // FIX(class_id-0 name shadow): mirror_class_name checks the
        // class-id reverse-map before slot 1's stored name; a mirror
        // whose field 0 is the never-registered placeholder `0` reads
        // back empty. Register a real class id via ensure_class_initialized
        // so the reverse-map agrees with the name in slot 1.
        let cid = ctx.ensure_class_initialized("[Ljava/lang/Object;").unwrap();
        let name_obj = ctx.create_string("[Ljava/lang/Object;");
        let mirror = ctx.alloc_object(ClassId::new(0), 2);
        ctx.set_field(mirror, 0, Value::Int(cid.as_u32() as i32));
        ctx.set_field(mirror, 1, Value::Object(Some(name_obj)));
        let scale =
            native_unsafe_array_index_scale(&mut ctx, &[dummy_this(), Value::Object(Some(mirror))])
                .unwrap();
        assert_eq!(scale, Some(Value::Int(8)));

        // And for int[] → 4.
        let int_cid = ctx.ensure_class_initialized("[I").unwrap();
        let int_name = ctx.create_string("[I");
        let int_mirror = ctx.alloc_object(ClassId::new(0), 2);
        ctx.set_field(int_mirror, 0, Value::Int(int_cid.as_u32() as i32));
        ctx.set_field(int_mirror, 1, Value::Object(Some(int_name)));
        let int_scale = native_unsafe_array_index_scale(
            &mut ctx,
            &[dummy_this(), Value::Object(Some(int_mirror))],
        )
        .unwrap();
        assert_eq!(int_scale, Some(Value::Int(4)));

        // byte[] → 1.
        //
        // Field 0 was the never-registered placeholder `0` here, which is the
        // class-id-0 name shadow the comment above this block warns about: the
        // name in slot 1 is never read, the derivation returns its non-array
        // answer, and that answer USED TO BE 1 -- the same value `[B` derives.
        // So this case asserted the catch-all and would have passed for a
        // mirror naming anything at all. It only surfaced when the non-array
        // answer became 0 (the javadoc's), which is what a default that
        // collides with a real value costs: it hides the case it stands in for.
        // Registered properly, as the `[Ljava/lang/Object;` and `[I` cases
        // above already were.
        let byte_cid = ctx.ensure_class_initialized("[B").unwrap();
        let byte_name = ctx.create_string("[B");
        let byte_mirror = ctx.alloc_object(ClassId::new(0), 2);
        ctx.set_field(byte_mirror, 0, Value::Int(byte_cid.as_u32() as i32));
        ctx.set_field(byte_mirror, 1, Value::Object(Some(byte_name)));
        let byte_scale = native_unsafe_array_index_scale(
            &mut ctx,
            &[dummy_this(), Value::Object(Some(byte_mirror))],
        )
        .unwrap();
        assert_eq!(byte_scale, Some(Value::Int(1)));
    }

    // -----------------------------------------------------------------------
    // Test 6: fence functions do not panic
    // -----------------------------------------------------------------------
    #[test]
    fn t12_fence_does_not_panic() {
        let mut ctx = MockNativeContext::new();
        let args: &[Value] = &[dummy_this()];

        // loadFence (SeqCst via native_unsafe_fence)
        let r1 = native_unsafe_fence(&mut ctx, args).unwrap();
        assert_eq!(r1, None);

        // storeFence (SeqCst via native_unsafe_fence)
        let r2 = native_unsafe_fence(&mut ctx, args).unwrap();
        assert_eq!(r2, None);

        // fullFence (SeqCst via native_unsafe_fence)
        let r3 = native_unsafe_fence(&mut ctx, args).unwrap();
        assert_eq!(r3, None);

        // loadLoadFence (Acquire)
        let r4 = native_unsafe_load_load_fence(&mut ctx, args).unwrap();
        assert_eq!(r4, None);

        // storeStoreFence (Release)
        let r5 = native_unsafe_store_store_fence(&mut ctx, args).unwrap();
        assert_eq!(r5, None);
    }

    // -----------------------------------------------------------------------
    // Test 7: copySwapMemory basic copy with byte-swap
    // -----------------------------------------------------------------------
    #[test]
    fn t12_copy_swap_memory_basic() {
        let mut ctx = MockNativeContext::new();
        let src = make_test_obj(&mut ctx, 4);
        let dst = make_test_obj(&mut ctx, 4);

        // Fill src with known values
        ctx.set_field(src, 0, Value::Int(1));
        ctx.set_field(src, 1, Value::Int(2));
        ctx.set_field(src, 2, Value::Int(3));
        ctx.set_field(src, 3, Value::Int(4));

        // copySwapMemory0(this, src, 0, dst, 0, 4, elemSize=4)
        let result = native_unsafe_copy_swap_memory(
            &mut ctx,
            &[
                dummy_this(),
                Value::Object(Some(src)),
                Value::Long(0),
                Value::Object(Some(dst)),
                Value::Long(0),
                Value::Long(4), // bytes (= element count in our model)
                Value::Long(4), // elemSize = 4 bytes (i32 swap)
            ],
        )
        .unwrap();
        assert_eq!(result, None);

        // Verify dest has byte-swapped values
        for i in 0..4usize {
            let original = (i as i32) + 1;
            let expected = original.swap_bytes();
            assert_eq!(
                ctx.get_field(dst, i),
                Value::Int(expected),
                "field {} mismatch: expected swap_bytes({}) = {}",
                i,
                original,
                expected
            );
        }
    }

    // -----------------------------------------------------------------------
    // Test 8: copySwapMemory with a NULL base and an address the arena does not
    // know is REFUSED -- it used to be a silent no-op, and this test pinned
    // that.
    //
    // A null base means an OFF-HEAP address. Returning `Ok(None)` meant an
    // endian-converting copy between two `allocateMemory` blocks wrote nothing
    // at all, and the destination was already zero, so the failure read as
    // success. MEASURED against HotSpot 25.0.4+7 (`probes/UnsafeShadowSweep`,
    // off-heap section): elemSize 2 over [1..8] gives [2,1,4,3,6,5,8,7];
    // CratonVM produced eight zeros.
    //
    // This assertion is the one that has to change with the fix. It was
    // written from the implementation rather than from the contract -- the
    // name said "is a noop", which is a description of the code, not of what
    // `Unsafe.copySwapMemory` promises. Address 0 is in no live arena, so the
    // contract-correct outcome is the same IllegalArgumentException the
    // arena's other accessors already produce for an unknown address.
    // -----------------------------------------------------------------------
    #[test]
    fn t12_copy_swap_memory_null_base_unknown_address_is_refused() {
        let mut ctx = MockNativeContext::new();

        let result = native_unsafe_copy_swap_memory(
            &mut ctx,
            &[
                dummy_this(),
                Value::Object(None), // off-heap source
                Value::Long(0),      // ... at an address no arena owns
                Value::Object(None), // off-heap destination
                Value::Long(0),
                Value::Long(16),
                Value::Long(4),
            ],
        );
        match result {
            Err(cratonvm_types::error::MethodCallFailed::InternalError(
                cratonvm_types::error::VmError::Runtime(
                    cratonvm_types::error::RuntimeError::IllegalArgumentException { .. },
                ),
            )) => {}
            other => panic!(
                "expected IllegalArgumentException for an off-heap copySwapMemory at an                  address no arena owns, got {other:?}"
            ),
        }
    }

    // -----------------------------------------------------------------------
    // Test 9: copySwapMemory clamps an over-long `bytes` to the destination's
    // real field count — a too-large length must NOT write past the dst object
    // (the OOB-field-write fix).
    // -----------------------------------------------------------------------
    #[test]
    fn t12_copy_swap_memory_clamps_oversize_to_dst_fields() {
        let mut ctx = MockNativeContext::new();
        let src = make_test_obj(&mut ctx, 8);
        let dst = make_test_obj(&mut ctx, 2);
        // A neighbour allocated AFTER dst, pre-filled with a sentinel; if the
        // loop ran past dst's 2 slots it would scribble into adjacent storage.
        let neighbour = make_test_obj(&mut ctx, 2);
        ctx.set_field(neighbour, 0, Value::Int(0x5151_5151));
        ctx.set_field(neighbour, 1, Value::Int(0x5252_5252));

        for i in 0..8usize {
            ctx.set_field(src, i, Value::Int((i as i32) + 1));
        }

        // bytes=8 but dst only has 2 slots → must copy exactly 2, no panic.
        let result = native_unsafe_copy_swap_memory(
            &mut ctx,
            &[
                dummy_this(),
                Value::Object(Some(src)),
                Value::Long(0),
                Value::Object(Some(dst)),
                Value::Long(0),
                Value::Long(8), // oversize
                Value::Long(4),
            ],
        )
        .unwrap();
        assert_eq!(result, None);

        // The two in-range slots are written (byte-swapped); the neighbour's
        // sentinel values are untouched.
        assert_eq!(ctx.get_field(dst, 0), Value::Int(1i32.swap_bytes()));
        assert_eq!(ctx.get_field(dst, 1), Value::Int(2i32.swap_bytes()));
        assert_eq!(ctx.get_field(neighbour, 0), Value::Int(0x5151_5151));
        assert_eq!(ctx.get_field(neighbour, 1), Value::Int(0x5252_5252));
        // The loop must NOT have addressed slots past dst's original count.
        // (Under the production VM that would be an out-of-bounds field write
        // into a neighbouring object; under the auto-resizing mock it would
        // instead grow dst from 2 to 8 slots. Either way, the field count must
        // stay 2 with the clamp in place.)
        assert_eq!(
            ctx.object_num_fields(dst),
            2,
            "copySwapMemory wrote past dst's field count (OOB-write guard failed)"
        );
    }

    #[test]
    fn p58_charset_coder_includes_new_decoder() {
        use cratonvm_native_api::NativeMethodRegistry;
        let mut r = NativeMethodRegistry::new();
        crate::phases_late::register_p58_charset_coder(&mut r);
        let total = r.len();
        assert!(
            total > 10,
            "expected > 10 charset coder natives, got {total}"
        );
        assert!(
            r.find(
                "java/nio/charset/Charset",
                "newDecoder",
                "()Ljava/nio/charset/CharsetDecoder;",
            )
            .is_some(),
            "newDecoder must be registered"
        );
        assert!(
            r.find(
                "java/nio/charset/Charset",
                "newEncoder",
                "()Ljava/nio/charset/CharsetEncoder;",
            )
            .is_some(),
            "newEncoder must be registered"
        );
    }
}
