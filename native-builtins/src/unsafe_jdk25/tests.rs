use super::*;
use crate::test_utils::MockNativeContext;
use crate::{
    native_unsafe_allocate_memory, native_unsafe_allocate_memory_realloc,
    native_unsafe_array_base_offset, native_unsafe_array_index_scale, native_unsafe_cas_int,
    native_unsafe_fence, native_unsafe_get_int, native_unsafe_get_int_volatile,
    native_unsafe_get_long, native_unsafe_put_int, native_unsafe_put_int_volatile,
    native_unsafe_put_long,
};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
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
// Test 5c (2026-09-20): a NAMED non-array class is REFUSED by both
// accessors, and a mirror whose name cannot be read is not.
//
// This is the residual of
// `unsafe-objectfieldoffset-accepted-a-static-and-the-jdk-refusal-that-is-itself-broken-20260826-RETIRED-20260920.md`
// section 3. HotSpot refuses a non-array on all four doors and its refusal
// is broken (`java/lang/InvalidClassException` does not exist, so the
// throw fails to link and the caller gets a `NoClassDefFoundError`), which
// is why this VM throws the REAL `java.io.InvalidClassException` rather
// than reproducing the oracle. See `refuse_non_array_class`.
//
// The second half is the part worth a test of its own: an unreadable
// mirror keeps the legacy number. The refusal is scoped to a positively
// identified non-array, so no internal path holding a mirror shape the
// name reader does not model can be turned into a throw by it.
// -----------------------------------------------------------------------
#[test]
fn c5c_non_array_class_is_refused_by_both_accessors() {
    let mut ctx = MockNativeContext::new();
    let cid = ctx.ensure_class_initialized("java/lang/String").unwrap();
    let name_obj = ctx.create_string("java/lang/String");
    let mirror = ctx.alloc_object(ClassId::new(0), 2);
    ctx.set_field(mirror, 0, Value::Int(cid.as_u32() as i32));
    ctx.set_field(mirror, 1, Value::Object(Some(name_obj)));

    let base =
        native_unsafe_array_base_offset(&mut ctx, &[dummy_this(), Value::Object(Some(mirror))]);
    assert!(
        matches!(
            base,
            Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(_))
        ),
        "arrayBaseOffset of a non-array must refuse, got {base:?}"
    );
    let scale =
        native_unsafe_array_index_scale(&mut ctx, &[dummy_this(), Value::Object(Some(mirror))]);
    assert!(
        matches!(
            scale,
            Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(_))
        ),
        "arrayIndexScale of a non-array must refuse, got {scale:?}"
    );

    // An array is untouched by the refusal.
    let acid = ctx.ensure_class_initialized("[I").unwrap();
    let aname = ctx.create_string("[I");
    let amirror = ctx.alloc_object(ClassId::new(0), 2);
    ctx.set_field(amirror, 0, Value::Int(acid.as_u32() as i32));
    ctx.set_field(amirror, 1, Value::Object(Some(aname)));
    assert_eq!(
        native_unsafe_array_base_offset(
            &mut ctx,
            &[dummy_this(), Value::Object(Some(amirror))]
        )
        .unwrap(),
        Some(Value::Int(16))
    );
    assert_eq!(
        native_unsafe_array_index_scale(
            &mut ctx,
            &[dummy_this(), Value::Object(Some(amirror))]
        )
        .unwrap(),
        Some(Value::Int(4))
    );

    // No argument at all keeps the legacy answers: the refusal reads
    // `args[1]`, and a door that never supplied one is not a caller naming
    // a non-array.
    assert_eq!(
        native_unsafe_array_base_offset(&mut ctx, &[dummy_this()]).unwrap(),
        Some(Value::Int(16))
    );
    assert_eq!(
        native_unsafe_array_index_scale(&mut ctx, &[dummy_this()]).unwrap(),
        Some(Value::Int(1))
    );
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
            "expected IllegalArgumentException for an off-heap copySwapMemory at an \
             address no arena owns, got {other:?}"
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

// -----------------------------------------------------------------------
#[test]
fn r17_copy_swap_memory_real_pointer_to_real_pointer() {
    let mut ctx = MockNativeContext::new();
    let source: Vec<u8> = vec![0x0A, 0x0B, 0x0C, 0x0D, 0xFF, 0xFF, 0xFF, 0xFE];
    let mut destination = vec![0u8; 8];
    copy_swap_via_native_bridge(
        &mut ctx,
        None,
        source.as_ptr() as i64,
        None,
        destination.as_mut_ptr() as i64,
        8,
        4,
    )
    .expect("native-to-native shape")
    .unwrap();
    assert_eq!(destination, [0x0D, 0x0C, 0x0B, 0x0A, 0xFE, 0xFF, 0xFF, 0xFF]);
}

// -----------------------------------------------------------------------
// Test 10 (JIT round 17 wave 4, lane bufcmp): a REAL off-heap pointer (an
// FFM arena block is one) on either side of copySwapMemory is copied, not
// dropped. `seg.asByteBuffer().asIntBuffer().get(0, int[2])` answered
// `0/0` where HotSpot answers `168496141/-2`.
// -----------------------------------------------------------------------
#[test]
fn r17_copy_swap_memory_real_pointer_to_int_array_and_back() {
    let mut ctx = MockNativeContext::new();
    // Big-endian 0x0A0B0C0D, -2 at a real (untagged) address.
    let native: Vec<u8> = vec![0x0A, 0x0B, 0x0C, 0x0D, 0xFF, 0xFF, 0xFF, 0xFE];
    let addr = native.as_ptr() as i64;
    let dst = ctx.new_array(cratonvm_types::ArrayElementType::Int, 2);
    native_unsafe_copy_swap_memory(
        &mut ctx,
        &[
            dummy_this(),
            Value::Object(None),
            Value::Long(addr),
            Value::Object(Some(dst)),
            Value::Long(16),
            Value::Long(8),
            Value::Long(4),
        ],
    )
    .unwrap();
    assert_eq!(ctx.get_array_element(dst, 0), Value::Int(0x0A0B_0C0D));
    assert_eq!(ctx.get_array_element(dst, 1), Value::Int(-2));

    // And back: int[] -> real pointer, swapped to big-endian.
    let mut out: Vec<u8> = vec![0u8; 8];
    let out_addr = out.as_mut_ptr() as i64;
    native_unsafe_copy_swap_memory(
        &mut ctx,
        &[
            dummy_this(),
            Value::Object(Some(dst)),
            Value::Long(16),
            Value::Object(None),
            Value::Long(out_addr),
            Value::Long(8),
            Value::Long(4),
        ],
    )
    .unwrap();
    assert_eq!(out, native);
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
