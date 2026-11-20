use super::*;
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// JIT round 16 wave 11 (lane ffmcost): the plain-downcall cell writer
/// produces `marshal_arg`'s bytes for every primitive kind and every
/// `Value` shape either can be handed, and refuses ADDRESS and groups.
#[test]
fn plain_cell_matches_marshal_arg() {
    let mut ctx = mock_ctx();
    let cid = ctx
        .ensure_class_initialized("java/lang/foreign/ValueLayout")
        .unwrap();
    let values = [
        Value::Int(0),
        Value::Int(-1),
        Value::Int(0x1_2345),
        Value::Int(i32::MIN),
        Value::Long(-7),
        Value::Long(i64::MAX),
        Value::Float(-1.5),
        Value::Float(f32::NAN),
        Value::Double(2.25),
        Value::Double(-0.0),
        Value::Object(None),
    ];
    for kind in [
        LAYOUT_BYTE,
        LAYOUT_BOOLEAN,
        LAYOUT_SHORT,
        LAYOUT_CHAR,
        LAYOUT_INT,
        LAYOUT_LONG,
        LAYOUT_FLOAT,
        LAYOUT_DOUBLE,
    ] {
        let layout = ctx.alloc_object(cid, 2);
        ctx.set_field(layout, 0, Value::Int(kind));
        for v in &values {
            let want = match marshal_arg(&ctx, layout, v) {
                Ok(slot) => slot.bytes.clone(),
                Err(_) => panic!("kind {kind} refused {v:?}"),
            };
            let mut cell = u64::MAX;
            assert!(marshal_primitive_into(kind, v, &mut cell), "kind {kind}");
            let got = cell.to_ne_bytes();
            assert_eq!(&got[..want.len()], &want[..], "kind {kind} value {v:?}");
            assert!(got[want.len()..].iter().all(|b| *b == 0), "kind {kind}");
        }
    }
    for kind in [
        LAYOUT_ADDRESS,
        LAYOUT_STRUCT,
        LAYOUT_UNION,
        LAYOUT_SEQUENCE,
        LAYOUT_UNKNOWN,
    ] {
        let mut cell = 0x55u64;
        assert!(!marshal_primitive_into(kind, &Value::Long(1), &mut cell));
        assert_eq!(cell, 0x55, "a refused kind leaves the cell untouched");
    }
}

#[test]
fn primitive_types_translate() {
    assert!(primitive_kind_to_ffi_type(LAYOUT_INT).is_some());
    assert!(primitive_kind_to_ffi_type(LAYOUT_LONG).is_some());
    assert!(primitive_kind_to_ffi_type(LAYOUT_FLOAT).is_some());
    assert!(primitive_kind_to_ffi_type(LAYOUT_DOUBLE).is_some());
    assert!(primitive_kind_to_ffi_type(LAYOUT_ADDRESS).is_some());
    assert!(primitive_kind_to_ffi_type(LAYOUT_BYTE).is_some());
    assert!(primitive_kind_to_ffi_type(LAYOUT_BOOLEAN).is_some());
    assert!(primitive_kind_to_ffi_type(LAYOUT_SHORT).is_some());
    assert!(primitive_kind_to_ffi_type(LAYOUT_CHAR).is_some());
    // Compound types must return None.
    assert!(primitive_kind_to_ffi_type(LAYOUT_STRUCT).is_none());
    assert!(primitive_kind_to_ffi_type(LAYOUT_UNION).is_none());
    assert!(primitive_kind_to_ffi_type(LAYOUT_SEQUENCE).is_none());
}

#[test]
fn arg_slot_round_trip_int() {
    let slot = ArgSlot {
        bytes: 0x1234_5678i32.to_ne_bytes().to_vec(),
        translated: 0,
        span: 0,
    };
    assert_eq!(
        slot.handle_for(0x1234_5678),
        None,
        "an untranslated slot maps nothing"
    );
    let real = 0x7000_0000usize;
    let mapped = ArgSlot {
        bytes: real.to_ne_bytes().to_vec(),
        translated: ARENA_HANDLE_TAG | 0x40,
        span: 6,
    };
    assert_eq!(
        mapped.handle_for(real as i64 + 2),
        Some(ARENA_HANDLE_TAG | 0x42)
    );
    assert_eq!(
        mapped.handle_for(real as i64 + 6),
        Some(ARENA_HANDLE_TAG | 0x46),
        "one past the end"
    );
    assert_eq!(mapped.handle_for(real as i64 + 7), None, "past the block");
    assert_eq!(
        mapped.handle_for(real as i64 - 1),
        None,
        "before the pointer passed"
    );
    std::mem::forget(mapped); // its handle was never translated
                              // The pointer should be usable.
    assert!(!slot.as_ptr().is_null());
}

/// A tagged arena handle must never reach native code.
///
/// `0x4000_0010_0000_0000` is not a hypothetical: it is the address
/// `MemorySegment.ofBuffer(ByteBuffer.allocateDirect(32))` reports on this
/// VM, measured. It is non-null and 16-aligned, so every pre-existing
/// screen passed it through to be dereferenced.
#[test]
fn tagged_arena_handle_is_refused() {
    let tagged = ARENA_HANDLE_TAG | 0x10_0000_0000;
    let err = checked_foreign_addr(tagged, "downcall pointer argument")
        .expect_err("a tagged arena handle must be refused, not dereferenced");
    match err {
        MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
            RuntimeError::IllegalStateException { message },
        )) => {
            assert!(
                message.contains("arena handle"),
                "message must name the cause: {message}"
            );
        }
        other => panic!("expected IllegalStateException, got {other:?}"),
    }
}

/// The exact shape that segfaulted: `Arena.allocateFrom("abcd")` handed
/// `strlen` the pointer `0x5` — the segment's byte SIZE read out of the
/// slot its base pointer belongs in.
#[test]
fn unmappable_low_address_is_refused() {
    assert!(
        checked_foreign_addr(5, "downcall pointer argument").is_err(),
        "an address in the reserved low window must be refused"
    );
    assert!(
        checked_foreign_addr(MIN_MAPPED_ADDR - 1, "downcall pointer argument").is_err(),
        "the top of the reserved low window must still be refused"
    );
}

/// The screen must not fire on the values a working downcall uses: a null
/// pointer is a legitimate C argument, and a real mapping passes.
#[test]
fn null_and_real_addresses_pass() {
    assert_eq!(
        checked_foreign_addr(0, "downcall pointer argument").ok(),
        Some(0),
        "NULL is a legitimate C pointer argument"
    );
    let buf = [0u8; 8];
    let real = buf.as_ptr() as i64;
    assert_eq!(
        checked_foreign_addr(real, "downcall pointer argument").ok(),
        Some(real),
        "a genuine mapping must pass the screen"
    );
    assert_eq!(
        checked_foreign_addr(MIN_MAPPED_ADDR, "downcall pointer argument").ok(),
        Some(MIN_MAPPED_ADDR),
        "the screen is exclusive at its lower bound"
    );
}

// -----------------------------------------------------------------
// F27 — the layout carrier, read in the ONE encoding
// -----------------------------------------------------------------
//
// Every number asserted below is `java` on Microsoft build 25.0.3+9-LTS,
// quoted at the assertion. Nothing here was run on CratonVM.

/// The real JDK's own value-layout carriers, which `--real-jdk` is the ONLY
/// producer of (`vm_util.rs::make_prepared_value_layout` is dropped under
/// `CompatibilityMode::JdkOnly`, so the real `ValueLayout.<clinit>` runs).
///
/// Before this lane every one of these fell to `_ => LAYOUT_LONG`. That is
/// the mutation to make if this test ever looks redundant: replace the
/// delegation with the old nine-string list and all nine `…Impl` rows go to
/// `LAYOUT_LONG`.
#[test]
fn real_jdk_value_layout_carriers_are_classified() {
    for (class_name, expected) in [
        (
            "jdk/internal/foreign/layout/ValueLayouts$OfBooleanImpl",
            LAYOUT_BOOLEAN,
        ),
        (
            "jdk/internal/foreign/layout/ValueLayouts$OfByteImpl",
            LAYOUT_BYTE,
        ),
        (
            "jdk/internal/foreign/layout/ValueLayouts$OfCharImpl",
            LAYOUT_CHAR,
        ),
        (
            "jdk/internal/foreign/layout/ValueLayouts$OfShortImpl",
            LAYOUT_SHORT,
        ),
        (
            "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
            LAYOUT_INT,
        ),
        (
            "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
            LAYOUT_LONG,
        ),
        (
            "jdk/internal/foreign/layout/ValueLayouts$OfFloatImpl",
            LAYOUT_FLOAT,
        ),
        (
            "jdk/internal/foreign/layout/ValueLayouts$OfDoubleImpl",
            LAYOUT_DOUBLE,
        ),
        (
            "jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl",
            LAYOUT_ADDRESS,
        ),
        // and this VM's own spellings, which already worked
        ("java/lang/foreign/ValueLayout$OfInt", LAYOUT_INT),
        ("java/lang/foreign/ValueLayout$OfFloat", LAYOUT_FLOAT),
        ("java/lang/foreign/AddressLayout", LAYOUT_ADDRESS),
    ] {
        assert_eq!(
            layout_kind_of_class(class_name),
            expected,
            "{class_name} must classify as {expected}, not default to LAYOUT_LONG"
        );
    }
}

/// Group, sequence and padding carriers in both spellings. No group class
/// was on the old list at all, which is why `layout_total_size` answered 8
/// for a struct of any width.
#[test]
fn group_and_sequence_carriers_are_classified() {
    for (class_name, expected) in [
        ("java/lang/foreign/StructLayout", LAYOUT_STRUCT),
        (
            "jdk/internal/foreign/layout/StructLayoutImpl",
            LAYOUT_STRUCT,
        ),
        ("java/lang/foreign/GroupLayout", LAYOUT_STRUCT),
        ("java/lang/foreign/UnionLayout", LAYOUT_UNION),
        ("jdk/internal/foreign/layout/UnionLayoutImpl", LAYOUT_UNION),
        ("java/lang/foreign/SequenceLayout", LAYOUT_SEQUENCE),
        (
            "jdk/internal/foreign/layout/SequenceLayoutImpl",
            LAYOUT_SEQUENCE,
        ),
        ("java/lang/foreign/PaddingLayout", LAYOUT_PADDING),
        (
            "jdk/internal/foreign/layout/PaddingLayoutImpl",
            LAYOUT_PADDING,
        ),
    ] {
        assert_eq!(
            layout_kind_of_class(class_name),
            expected,
            "{class_name} must classify as {expected}"
        );
        assert_ne!(
            layout_kind_of_class(class_name),
            LAYOUT_LONG,
            "{class_name} must not fall to the old LAYOUT_LONG default"
        );
    }
}

/// The unknown case must be LOUD, not plausible.
#[test]
fn an_unrecognised_carrier_is_unknown_not_an_eight_byte_integer() {
    for class_name in [
        "java/lang/String",
        "java/lang/foreign/MemorySegment$Scope",
        "",
    ] {
        assert_eq!(
            layout_kind_of_class(class_name),
            LAYOUT_UNKNOWN,
            "{class_name:?} is not a layout carrier and must say so"
        );
    }
    assert!(
        primitive_kind_to_ffi_type(LAYOUT_UNKNOWN).is_none(),
        "LAYOUT_UNKNOWN must not translate to any ffi type"
    );
    assert!(
        !(0..10).contains(&LAYOUT_UNKNOWN),
        "LAYOUT_UNKNOWN must not be admitted by a `is this a value layout` range test"
    );
}

/// `structLayout(JAVA_LONG, JAVA_INT)` on the oracle:
/// `byteSize=12 byteAlignment=8` — the total is NOT rounded up to 16.
///
/// Before this lane both readers ignored the carrier: `layout_total_size`
/// answered `layout_byte_size(LAYOUT_LONG)` = 8 and `layout_align` the same
/// 8, for a group layout of ANY width.
#[test]
fn group_layout_size_and_alignment_come_from_the_carrier() {
    let mut ctx = mock_ctx();
    let cid = ctx
        .ensure_class_initialized("java/lang/foreign/StructLayout")
        .unwrap();
    let members = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 2);
    let sl = ctx.alloc_object(cid, 4);
    ctx.set_field(sl, 0, Value::Long(12));
    ctx.set_field(sl, 1, Value::Long(8));
    ctx.set_field(sl, 2, Value::Object(Some(members)));
    ctx.set_field(sl, 3, Value::Object(None));

    assert_eq!(read_layout_kind(&ctx, sl), LAYOUT_STRUCT);
    assert_eq!(
        layout_total_size(&ctx, sl).unwrap(),
        12,
        "structLayout(JAVA_LONG, JAVA_INT).byteSize() is 12 on 25.0.3+9-LTS"
    );
    assert_eq!(
        layout_align(&ctx, sl),
        8,
        "structLayout(JAVA_LONG, JAVA_INT).byteAlignment() is 8"
    );
}

/// `sequenceLayout(10, JAVA_INT)` is `byteSize=40 byteAlignment=4`. The
/// carrier records the TOTAL, never the element count — the count is
/// `byteSize / element byteSize`. Reading slot 1 as the count (what
/// `layout_to_ffi_type` did) yields 4.
#[test]
fn sequence_carrier_reports_its_total_not_its_alignment() {
    let mut ctx = mock_ctx();
    let cid = ctx
        .ensure_class_initialized("java/lang/foreign/SequenceLayout")
        .unwrap();
    let elem_cid = ctx
        .ensure_class_initialized("java/lang/foreign/ValueLayout$OfInt")
        .unwrap();
    let elem = ctx.alloc_object(elem_cid, 4);
    ctx.set_field(elem, 0, Value::Long(4));
    ctx.set_field(elem, 1, Value::Long(4));
    let seq = ctx.alloc_object(cid, 4);
    ctx.set_field(seq, 0, Value::Long(40));
    ctx.set_field(seq, 1, Value::Long(4));
    ctx.set_field(seq, 2, Value::Object(Some(elem)));
    ctx.set_field(seq, 3, Value::Object(None));

    assert_eq!(read_layout_kind(&ctx, seq), LAYOUT_SEQUENCE);
    assert_eq!(layout_total_size(&ctx, seq).unwrap(), 40);
    assert_eq!(layout_align(&ctx, seq), 4);
    assert_eq!(layout_total_size(&ctx, elem).unwrap(), 4);
    assert_eq!(
        layout_total_size(&ctx, seq).unwrap() / layout_total_size(&ctx, elem).unwrap(),
        10,
        "the element count is derived, and it is 10"
    );
}

/// NOM-3 (G6-1). The five-slot sibling of the test above: a carrier
/// minted with a STORED count at slot 4 must have that count preferred
/// over the total/element-size division, exactly like
/// `SequenceLayout.elementCount()`. G6-1 §8.2's own motivating case
/// (`sequenceLayout(3, structLayout())`, where the division answers 0
/// because the element's `byteSize()` is 0) is not reachable through
/// *this* function specifically — `layout_to_ffi_type` refuses a
/// zero-member `StructLayout` outright as unmarshallable by value,
/// independent of NOM-2 — so this test proves the same precedence with a
/// marshallable `JAVA_INT` element instead: the carrier's OWN `byteSize`
/// (slot 0) is deliberately wrong for its stored count (40 bytes would
/// divide to 10 elements of a 4-byte int) so a division-based reader and
/// a slot-4-preferring one give different, checkable answers.
#[test]
fn sequence_carrier_with_a_stored_count_prefers_it_over_the_division() {
    let mut ctx = mock_ctx();
    let cid = ctx
        .ensure_class_initialized("java/lang/foreign/SequenceLayout")
        .unwrap();
    let elem_cid = ctx
        .ensure_class_initialized("java/lang/foreign/ValueLayout$OfInt")
        .unwrap();
    let elem = ctx.alloc_object(elem_cid, 4);
    ctx.set_field(elem, 0, Value::Long(4));
    ctx.set_field(elem, 1, Value::Long(4));

    let seq = ctx.alloc_object(cid, 5);
    ctx.set_field(seq, 0, Value::Long(40));
    ctx.set_field(seq, 1, Value::Long(4));
    ctx.set_field(seq, 2, Value::Object(Some(elem)));
    ctx.set_field(seq, 3, Value::Object(None));
    ctx.set_field(seq, 4, Value::Long(3));

    assert_eq!(read_layout_kind(&ctx, seq), LAYOUT_SEQUENCE);
    assert_eq!(
        layout_total_size(&ctx, seq).unwrap() / layout_total_size(&ctx, elem).unwrap(),
        10,
        "the division alone would answer 10, not the stored 3"
    );
    let ffi = layout_to_ffi_type(&ctx, seq, 0).expect("a stored count must be readable");
    // `libffi::middle::Type` has no public element-count accessor; walk
    // the null-terminated `elements` array on the raw `ffi_type` it
    // wraps, the same shape `ffi_type_array_create` builds it in.
    let raw = ffi.as_raw_ptr();
    let mut count = 0usize;
    unsafe {
        let mut p = (*raw).elements;
        while !(*p).is_null() {
            count += 1;
            p = p.add(1);
        }
    }
    assert_eq!(
        count, 3,
        "the stored count (slot 4) must win over the 0/0 division"
    );
}

/// The union row that separates "size" from "alignment". Measured on
/// 25.0.3+9-LTS:
///
/// ```text
/// unionLayout(sequenceLayout(7, JAVA_BYTE), JAVA_INT)
///     -> byteSize=7 byteAlignment=4   [[7:b1]|i4]
/// ```
///
/// The two other unions in the tree's tests are 8/8 and 4/4, where size and
/// alignment coincide — which is why reading slot 1 (the alignment) as the
/// union's SIZE in `layout_to_ffi_type` passed every test there was.
#[test]
fn a_unions_size_is_not_its_alignment() {
    let mut ctx = mock_ctx();
    let cid = ctx
        .ensure_class_initialized("java/lang/foreign/UnionLayout")
        .unwrap();
    let members = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 2);
    let u = ctx.alloc_object(cid, 4);
    ctx.set_field(u, 0, Value::Long(7));
    ctx.set_field(u, 1, Value::Long(4));
    ctx.set_field(u, 2, Value::Object(Some(members)));
    ctx.set_field(u, 3, Value::Object(None));

    assert_eq!(read_layout_kind(&ctx, u), LAYOUT_UNION);
    assert_eq!(layout_total_size(&ctx, u).unwrap(), 7);
    assert_eq!(layout_align(&ctx, u), 4);
    assert_ne!(
        layout_total_size(&ctx, u).unwrap(),
        layout_align(&ctx, u),
        "this row exists precisely because the two differ"
    );
}

/// **The out-of-bounds WRITE.** `panama.rs` hands this Vec's pointer to
/// `ffi_call` as the result address and libffi writes `rtype->size` bytes
/// there — the C size of the aggregate, which rounds up to the alignment.
/// `structLayout(JAVA_LONG, JAVA_INT)` is 12 to the JDK and 16 to C.
///
/// Before this lane the buffer was `8` for every aggregate return, because
/// `layout_total_size` answered 8.
#[test]
fn aggregate_return_slot_covers_the_c_size_not_just_the_jdk_byte_size() {
    let mut ctx = mock_ctx();
    let cid = ctx
        .ensure_class_initialized("java/lang/foreign/StructLayout")
        .unwrap();
    let members = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 2);
    let sl = ctx.alloc_object(cid, 4);
    ctx.set_field(sl, 0, Value::Long(12));
    ctx.set_field(sl, 1, Value::Long(8));
    ctx.set_field(sl, 2, Value::Object(Some(members)));
    ctx.set_field(sl, 3, Value::Object(None));

    let slot = alloc_return_slot(&ctx, Some(sl)).unwrap();
    assert_eq!(
        slot.len(),
        16,
        "libffi writes align_up(12, 8) = 16 bytes for struct {{ long; int; }}"
    );
    assert!(
        slot.len() >= layout_total_size(&ctx, sl).unwrap(),
        "the slot can never be shorter than the layout it is sized for"
    );
}

/// A primitive return still gets at least `sizeof(ffi_arg)`.
#[test]
fn primitive_return_slot_is_still_widened_to_ffi_arg() {
    let mut ctx = mock_ctx();
    let cid = ctx
        .ensure_class_initialized("java/lang/foreign/ValueLayout$OfByte")
        .unwrap();
    let vl = ctx.alloc_object(cid, 4);
    ctx.set_field(vl, 0, Value::Long(1));
    ctx.set_field(vl, 1, Value::Long(1));
    assert_eq!(
        alloc_return_slot(&ctx, Some(vl)).unwrap().len(),
        std::mem::size_of::<usize>()
    );
    assert!(alloc_return_slot(&ctx, None).unwrap().is_empty());
}

/// A carrier this VM cannot classify must not be sized as an eight-byte
/// integer on the way into a downcall.
#[test]
fn an_unclassifiable_return_layout_is_refused() {
    let mut ctx = mock_ctx();
    let cid = ctx.ensure_class_initialized("java/lang/Object").unwrap();
    let junk = ctx.alloc_object(cid, 2);
    ctx.set_field(junk, 0, Value::Object(None));
    let err = alloc_return_slot(&ctx, Some(junk))
        .expect_err("an unclassifiable return layout must be refused, not sized as a long");
    match err {
        MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
            RuntimeError::IllegalStateException { message },
        )) => assert!(
            message.contains("java/lang/Object"),
            "the refusal must name the class: {message}"
        ),
        other => panic!("expected IllegalStateException, got {other:?}"),
    }
}

/// **W7-89 §7.1, diagnosed.** A real `HeapMemorySegmentImpl` has five
/// fields, no `min`, and `AbstractMemorySegmentImpl.length` at slot 0
/// (javap, 25.0.3+9-LTS). `segment_address` used to fall through to
/// `get_field(seg, 0)` and hand out that LENGTH as a machine address:
/// `MemorySegment.ofArray(new byte[16]).set(...)` died with
/// `EXCEPTION_ACCESS_VIOLATION … read at address 0x10`, and 0x10 is 16.
#[test]
fn a_real_heap_segment_has_no_machine_address() {
    let mut ctx = mock_ctx();
    let cid = ctx
        .ensure_class_initialized("jdk/internal/foreign/HeapMemorySegmentImpl$OfByte")
        .unwrap();
    let seg = ctx.alloc_object(cid, 5);
    ctx.set_field(seg, 0, Value::Long(16)); // length
    ctx.set_field(seg, 1, Value::Int(0)); // readOnly
    ctx.set_field(seg, 2, Value::Object(None)); // scope
    ctx.set_field(seg, 3, Value::Long(0)); // offset

    assert!(is_real_heap_segment(&ctx, seg));
    assert_ne!(
        segment_address(&ctx, seg),
        16,
        "the segment's byte LENGTH must never be answered as its address"
    );
    assert_eq!(
        segment_address(&ctx, seg),
        0,
        "a heap segment has no machine address; 0 is what every caller refuses on"
    );
    assert_eq!(
        segment_byte_size(&ctx, seg),
        16,
        "its SIZE is still slot 0 and must keep answering"
    );
}

/// The two carriers that were already right must stay right — this is the
/// negative control for the arm above.
#[test]
fn native_and_synthetic_segment_addresses_are_unchanged() {
    let mut ctx = mock_ctx();
    let native_cid = ctx
        .ensure_class_initialized("jdk/internal/foreign/NativeMemorySegmentImpl")
        .unwrap();
    let native = ctx.alloc_object(native_cid, 4);
    ctx.set_field(native, 0, Value::Long(276)); // length
    ctx.set_field_by_name(native, "min", Value::Long(0x7f00_0000));
    assert!(!is_real_heap_segment(&ctx, native));
    assert_eq!(segment_address(&ctx, native), 0x7f00_0000);

    let synth_cid = ctx
        .ensure_class_initialized("java/lang/foreign/MemorySegment")
        .unwrap();
    let synth = ctx.alloc_object(synth_cid, 6);
    ctx.set_field(synth, 0, Value::Long(0x1000)); // base
    ctx.set_field(synth, 1, Value::Long(64)); // size
    ctx.set_field(synth, 5, Value::Long(0x20)); // offset
    assert!(!is_real_heap_segment(&ctx, synth));
    assert_eq!(
        segment_address(&ctx, synth),
        0x1020,
        "the synthetic (base@0 + offset@5) carrier is untouched"
    );
}

/// A heap segment handed to a downcall as a pointer must be refused BY
/// NAME. `segment_address` answers 0 for one, and 0 is a legitimate C null
/// that `checked_foreign_addr` deliberately passes — so without the arm in
/// `marshal_arg` the call would silently become `NULL`.
///
/// The exception TYPE and wording are pinned to the oracle, not just the
/// intent — MEASURED on HotSpot 25.0.3+9-LTS (`FfmRepro2.java`):
/// `strlen.invoke(MemorySegment.ofArray(new byte[]{'h','i',0}))` throws
/// `java.lang.IllegalArgumentException: Heap segment not allowed:
/// MemorySegment{ kind: heap, heapBase: [B@…, address: 0x0, byteSize: 3 }`.
/// This test used to assert `IllegalStateException`, which nobody had
/// checked against the oracle for this exact call site; corrected here
/// alongside widening the refusal past H1.
#[test]
fn a_heap_segment_pointer_argument_is_refused_by_name() {
    let mut ctx = mock_ctx();
    let layout_cid = ctx
        .ensure_class_initialized("java/lang/foreign/AddressLayout")
        .unwrap();
    let layout = ctx.alloc_object(layout_cid, 4);
    ctx.set_field(layout, 0, Value::Long(8));
    ctx.set_field(layout, 1, Value::Long(8));

    let seg_cid = ctx
        .ensure_class_initialized("jdk/internal/foreign/HeapMemorySegmentImpl$OfByte")
        .unwrap();
    let seg = ctx.alloc_object(seg_cid, 5);
    ctx.set_field(seg, 0, Value::Long(16));

    // `expect_err` would require `ArgSlot: Debug`; it holds the raw outgoing
    // frame and deliberately does not derive it. Destructure instead so the
    // assertion stays on the error without widening the type's API.
    let Err(err) = marshal_arg(&ctx, layout, &Value::Object(Some(seg))) else {
        panic!("a heap MemorySegment has no address to pass to native code");
    };
    match err {
        MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
            RuntimeError::IllegalArgumentException { message },
        )) => assert!(
            message.contains("Heap segment not allowed"),
            "the refusal must match the oracle's own wording: {message}"
        ),
        other => panic!("expected IllegalArgumentException, got {other:?}"),
    }

    // Control: the same layout with a plain long address still marshals.
    assert_eq!(
        marshal_arg(&ctx, layout, &Value::Long(0x7f00_0000))
            .unwrap()
            .bytes
            .len(),
        std::mem::size_of::<usize>()
    );
}

/// H2 (this VM's `ofArray(byte[]|short[]|char[])` ALIAS carrier) must be
/// refused exactly like H1: before `is_alias_heap_segment` existed,
/// `marshal_arg`'s downcall-pointer screen checked only
/// `is_real_heap_segment`, so this shape fell through to
/// `segment_address` (which answers 0 for it — a legitimate C null) and a
/// downcall silently received `NULL` instead of a refusal. The eight-slot
/// layout mirrors production exactly: see `panama::pe_of_array_alias` and
/// its own test-module twin `panama::tests::heap_alias_segment`.
#[test]
fn an_alias_heap_segment_pointer_argument_is_also_refused() {
    let mut ctx = mock_ctx();
    let layout_cid = ctx
        .ensure_class_initialized("java/lang/foreign/AddressLayout")
        .unwrap();
    let layout = ctx.alloc_object(layout_cid, 4);
    ctx.set_field(layout, 0, Value::Long(8));
    ctx.set_field(layout, 1, Value::Long(8));

    let array = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 3);

    let seg_cid = ctx
        .ensure_class_initialized("java/lang/foreign/MemorySegment")
        .unwrap();
    let seg = ctx.alloc_object(seg_cid, 8);
    ctx.set_field(seg, 0, Value::Long(0)); // no machine address
    ctx.set_field(seg, 1, Value::Long(3)); // byteSize
    ctx.set_field(seg, 2, Value::Object(None)); // scope
    ctx.set_field(seg, 3, Value::Int(0)); // readOnly
    ctx.set_field(seg, 4, Value::Int(1)); // alive
    ctx.set_field(seg, 5, Value::Long(0)); // offset
    ctx.set_field(seg, 6, Value::Object(Some(array))); // heap base
    ctx.set_field(seg, 7, Value::Long(0)); // start within array

    let Err(err) = marshal_arg(&ctx, layout, &Value::Object(Some(seg))) else {
        panic!("an alias heap MemorySegment has no address to pass to native code");
    };
    match err {
        MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
            RuntimeError::IllegalArgumentException { message },
        )) => assert!(
            message.contains("Heap segment not allowed"),
            "the refusal must match the oracle's own wording: {message}"
        ),
        other => panic!("expected IllegalArgumentException, got {other:?}"),
    }
}

/// `is_alias_heap_segment` classifies by FIELD COUNT alone (`> 7`, i.e.
/// exactly the real alias carrier's width), which is deliberately
/// narrower than "any heap-shaped carrier" — a seven-field object is not
/// wide enough to match, whatever it holds. This shape briefly
/// corresponded to a real carrier — `ofArray(int[]|long[]|float[]|
/// double[])`'s retired native-mirror allocation, "H3" — and is kept as
/// a hand-built boundary case now that nothing mints it: a segment
/// `marshal_arg` doesn't recognize as alias-only must still marshal its
/// slot-0 value as a plain pointer, not be refused like a real H1/H2
/// heap segment is (the two tests above).
#[test]
fn a_seven_field_carrier_below_the_alias_width_still_marshals() {
    let mut ctx = mock_ctx();
    let layout_cid = ctx
        .ensure_class_initialized("java/lang/foreign/AddressLayout")
        .unwrap();
    let layout = ctx.alloc_object(layout_cid, 4);
    ctx.set_field(layout, 0, Value::Long(8));
    ctx.set_field(layout, 1, Value::Long(8));

    let array = ctx.new_array(cratonvm_types::ArrayElementType::Int, 2);

    // The retired seven-slot mirror shape, preserved here only as a
    // field-count boundary case: [0]=native ptr, [1]=byteSize,
    // [2]=array, [3]=?, [4]=kind, [5]=offset, [6]=session.
    let seg_cid = ctx
        .ensure_class_initialized("java/lang/foreign/MemorySegment")
        .unwrap();
    let seg = ctx.alloc_object(seg_cid, 7);
    ctx.set_field(seg, 0, Value::Long(0x7f00_0000)); // native mirror address
    ctx.set_field(seg, 1, Value::Long(8)); // byteSize
    ctx.set_field(seg, 3, Value::Int(0)); // read-write
    ctx.set_field(seg, 4, Value::Int(1)); // alive
    ctx.set_field(seg, 5, Value::Long(0)); // offset
    ctx.set_field(seg, 2, Value::Object(Some(array))); // SEG_BACKING_ARRAY_FIELD
    ctx.set_field(seg, 6, Value::Object(None)); // SEG_MIRROR_SESSION_FIELD

    assert!(
        !crate::panama::is_alias_heap_segment(&ctx, seg),
        "a mirror carrier must not classify as an alias-only heap segment"
    );
    let slot = marshal_arg(&ctx, layout, &Value::Object(Some(seg)))
        .expect("a mirror carrier's real native buffer must still marshal");
    assert_eq!(
        slot.bytes,
        (0x7f00_0000usize).to_ne_bytes().to_vec(),
        "must marshal the mirror's real native pointer, not a refusal"
    );
}

/// `JAVA_INT` must marshal FOUR bytes. On `--real-jdk` the receiver is a
/// `ValueLayouts$OfIntImpl`, which used to classify as `LAYOUT_LONG` — so
/// the argument slot was eight bytes and the CIF said `i64`.
#[test]
fn a_real_jdk_int_layout_marshals_four_bytes() {
    let mut ctx = mock_ctx();
    let cid = ctx
        .ensure_class_initialized("jdk/internal/foreign/layout/ValueLayouts$OfIntImpl")
        .unwrap();
    let vl = ctx.alloc_object(cid, 5);
    ctx.set_field(vl, 0, Value::Long(4));
    ctx.set_field(vl, 1, Value::Long(4));
    assert_eq!(read_layout_kind(&ctx, vl), LAYOUT_INT);
    assert_eq!(layout_total_size(&ctx, vl).unwrap(), 4);
    assert_eq!(
        marshal_arg(&ctx, vl, &Value::Int(7)).unwrap().bytes.len(),
        4,
        "a JAVA_INT argument occupies four bytes, not eight"
    );
}

#[test]
fn unmarshal_int_widens_correctly() {
    let mut slot = vec![0u8; std::mem::size_of::<usize>()];
    slot[..4].copy_from_slice(&(-1i32).to_ne_bytes());
    // For a 32-bit int, libffi widens to ffi_arg. The high bytes
    // of the widened slot will be sign-extended on real calls;
    // unmarshal must read the low 32 bits and reinterpret as i32.
    let v = unmarshal_return_primitive(LAYOUT_INT, &slot);
    assert!(matches!(v, Value::Int(_)));
}

/// Round 12 wave 7 (lane upcall): a `JAVA_CHAR` return is unsigned. The
/// widened slot of `(char) 0xFFFE` answered -2; a `short` keeps its sign.
#[test]
fn unmarshal_char_is_zero_extended_and_short_is_not() {
    let slot = 0xFFFEusize.to_ne_bytes();
    if char_return_unsigned() {
        assert_eq!(
            unmarshal_return_primitive(LAYOUT_CHAR, &slot),
            Value::Int(0xFFFE)
        );
    }
    assert_eq!(
        unmarshal_return_primitive(LAYOUT_SHORT, &slot),
        Value::Int(-2)
    );
}
