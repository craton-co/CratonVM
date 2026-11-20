//! Round 12 wave 7 (lane ffm4): a real `SegmentVarHandle` over a CratonVM
//! carrier decodes the carrier (it read `readOnly` as the address), and
//! checks scope, bounds, alignment and read-only like the JDK
//! (`r12w6-ffm3-segment-varhandle-has-no-scope-check`).
use super::*;
use crate::phases_late::foreign_ffm::{p67_memory_session, p67_session_slots};
use crate::test_utils::{mock_ctx, MockNativeContext};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

struct Fixture {
    ctx: MockNativeContext,
    vh: ObjectRef,
    seg: ObjectRef,
    session: ObjectRef,
    ptr: *mut u8,
}

fn fixture() -> Fixture {
    let mut ctx = mock_ctx();
    // `get` answers the CANONICAL box from the process-wide cache keyed by
    // `vm_identity()`: under the mock default `0` a full parallel lib run
    // served a box another test's dropped mock heap had filed (the
    // `r15w3_vh4_index_coordinate_tests` failure, i59 L4). Its own row.
    ctx.set_vm_identity(0x5E6A_0001);
    // Production answers an unresolvable field name with null; the
    // carrier's `readOnly` lookup falls back to slot 3 only then.
    ctx.set_absent_field_answers_null(true);
    let vh_cid = match ctx.ensure_class_initialized(SEGMENT_VAR_HANDLE_CLASS) {
        Ok(cid) => cid,
        Err(err) => panic!("no class: {err:?}"),
    };
    let field = |name: &str, desc: &str, slot: usize| cratonvm_native_api::FieldMetadata {
        name: name.to_string(),
        descriptor: desc.to_string(),
        access_flags: 0,
        slot_index: slot,
        declaring_class_id: vh_cid,
        is_static: false,
    };
    ctx.set_declared_fields(
        vh_cid,
        vec![
            field("enclosing", "Ljava/lang/foreign/MemoryLayout;", 0),
            field("offset", "J", 1),
            field("be", "Z", 2),
        ],
    );
    let layout_name = "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl";
    let layout_cid = match ctx.ensure_class_initialized(layout_name) {
        Ok(cid) => cid,
        Err(err) => panic!("no class: {err:?}"),
    };
    let layout = ctx.alloc_object(layout_cid, 3);
    ctx.set_field(layout, 0, Value::Long(4));
    ctx.set_field(layout, 1, Value::Long(4));
    let vh = ctx.alloc_object(vh_cid, 3);
    ctx.set_field(vh, 0, Value::Object(Some(layout)));
    ctx.set_field(vh, 1, Value::Long(0));
    ctx.set_field(vh, 2, Value::Int(0));
    let session = match p67_memory_session(&mut ctx) {
        Ok(Value::Object(Some(session))) => session,
        other => panic!("no session: {other:?}"),
    };
    let (_id, ptr) = ctx
        .allocate_native_memory(16, 8)
        .unwrap_or_else(|| panic!("mock allocation failed"));
    let seg = match crate::panama::alloc_segment_carrier(&mut ctx, 6) {
        Ok(seg) => seg,
        Err(err) => panic!("no carrier: {err:?}"),
    };
    ctx.set_field(seg, 0, Value::Long(ptr as i64));
    ctx.set_field(seg, 1, Value::Long(16));
    ctx.set_field(seg, 2, Value::Object(Some(session)));
    ctx.set_field(seg, 3, Value::Int(0));
    ctx.set_field(seg, 4, Value::Int(1));
    ctx.set_field(seg, 5, Value::Long(0));
    Fixture {
        ctx,
        vh,
        seg,
        session,
        ptr,
    }
}

fn set(f: &mut Fixture, offset: i64, value: i32) -> MethodCallResult {
    let args = [
        Value::Object(Some(f.vh)),
        Value::Object(Some(f.seg)),
        Value::Long(offset),
        Value::Int(value),
    ];
    match segment_vh_set(&mut f.ctx, f.vh, &args) {
        Some(result) => result,
        None => panic!("the SegmentVarHandle fields did not resolve"),
    }
}

fn get(f: &mut Fixture, offset: i64) -> MethodCallResult {
    let args = [
        Value::Object(Some(f.vh)),
        Value::Object(Some(f.seg)),
        Value::Long(offset),
    ];
    match segment_vh_get(&mut f.ctx, f.vh, &args) {
        Some(result) => result,
        None => panic!("the SegmentVarHandle fields did not resolve"),
    }
}

fn word_at(f: &Fixture, offset: usize) -> u32 {
    // SAFETY: the fixture's block is 16 bytes, live for the mock's life.
    unsafe { (f.ptr.add(offset) as *const u32).read_unaligned() }
}

/// Before the fix the carrier's `readOnly` slot was read as the address,
/// so every access was `MemorySegment access via null address`.
#[test]
fn a_carrier_is_written_and_read_at_its_own_address() {
    let mut f = fixture();
    assert!(set(&mut f, 4, 0x1122_3344).is_ok());
    assert_eq!(word_at(&f, 4), 0x1122_3344);
    let got = match get(&mut f, 4) {
        Ok(Some(Value::Object(Some(boxed)))) => crate::lang_class::unbox_value(&f.ctx, boxed),
        Ok(Some(v)) => v,
        other => panic!("get gave {other:?}"),
    };
    assert_eq!(got, Value::Int(0x1122_3344));
}

/// The use-after-close the page records: a closed scope refuses both.
#[test]
fn a_closed_scope_is_already_closed_for_get_and_set() {
    let mut f = fixture();
    assert!(set(&mut f, 0, 7).is_ok());
    let slots = p67_session_slots(&f.ctx, f.session);
    f.ctx.set_field(f.session, slots.state, Value::Int(0));
    let err = match set(&mut f, 0, 9) {
        Ok(v) => panic!("a closed segment was written: {v:?}"),
        Err(err) => err,
    };
    assert!(format!("{err:?}").contains("Already closed"), "{err:?}");
    assert_eq!(word_at(&f, 0), 7, "nothing was written");
    let err = match get(&mut f, 0) {
        Ok(v) => panic!("a closed segment was read: {v:?}"),
        Err(err) => err,
    };
    assert!(format!("{err:?}").contains("Already closed"), "{err:?}");    }

/// Bounds are `IndexOutOfBoundsException`, a misaligned native address is
/// `IllegalArgumentException`, and a read-only segment refuses the write.
#[test]
fn bounds_alignment_and_read_only_raise_the_jdk_exceptions() {
    let mut f = fixture();
    let err = match get(&mut f, 16) {
        Ok(v) => panic!("an out-of-bounds read answered: {v:?}"),
        Err(err) => err,
    };
    assert!(format!("{err:?}").contains("IndexOutOfBounds"), "{err:?}");
    // j27 wave 4 (lane FFMMSG): HotSpot's `checkEnclosingLayout` text, not
    // the byte-array view handles' `Misaligned access`. The `(of ..)`
    // clause is the layout's `toString()`, which this mock answers with
    // nothing, so none here (j27 wave 6: see the next test for it).
    let message = match get(&mut f, 2) {
        Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::IllegalArgumentException { message },
        ))) => message,
        other => panic!("a misaligned read answered: {other:?}"),
    };
    assert_eq!(
        message,
        format!(
            "Target offset 2 is incompatible with alignment constraint 4 for segment \
             MemorySegment{{ kind: native, address: 0x{:x}, byteSize: 16 }}",
            f.ptr as usize
        )
    );
    // An out-of-bounds read names the segment too (it said `offset=..,
    // width=.., size=..`).
    let message = match get(&mut f, 16) {
        Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::IndexOutOfBoundsException { message: Some(message) },
        ))) => message,
        other => panic!("an out-of-bounds read answered: {other:?}"),
    };
    assert_eq!(
        message,
        format!(
            "Out of bound access on segment MemorySegment{{ kind: native, address: 0x{:x}, \
             byteSize: 16 }}; new offset = 16; new length = 4",
            f.ptr as usize
        )
    );
    f.ctx.set_field(f.seg, 3, Value::Int(1));
    let err = match set(&mut f, 0, 5) {
        Ok(v) => panic!("a read-only segment was written: {v:?}"),
        Err(err) => err,
    };
    assert!(format!("{err:?}").contains("read-only"), "{err:?}");
    assert_eq!(word_at(&f, 0), 0);
}

/// j27 wave 6 (lane T1LOOPFFM): JDK 25 checks the alignment
/// (`checkEnclosingLayout`) BEFORE the session, so the misaligned refusal
/// names its layout (`(of i4)`, which the old scope-first road dropped)
/// and a closed session does not turn it into `Already closed`. An
/// aligned access to the closed segment is still the session's refusal.
#[test]
fn a_misaligned_access_refuses_before_the_session_and_names_its_layout() {
    let mut f = fixture();
    let expected = format!(
        "Target offset 2 is incompatible with alignment constraint 4 (of i4) for segment \
         MemorySegment{{ kind: native, address: 0x{:x}, byteSize: 16 }}",
        f.ptr as usize
    );
    let misaligned = |f: &mut Fixture, write: bool| {
        let i4 = f.ctx.create_string("i4");
        f.ctx.set_invoke_virtual_result(Ok(Some(Value::Object(Some(i4)))));
        match if write { set(f, 2, 1) } else { get(f, 2) } {
            Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::IllegalArgumentException { message },
            ))) => message,
            other => panic!("a misaligned access (write {write}) answered: {other:?}"),
        }
    };
    for write in [false, true] {
        assert_eq!(misaligned(&mut f, write), expected, "open, write {write}");
    }
    let slots = p67_session_slots(&f.ctx, f.session);
    f.ctx.set_field(f.session, slots.state, Value::Int(0));
    for write in [false, true] {
        assert_eq!(misaligned(&mut f, write), expected, "closed, write {write}");
    }
    let err = match get(&mut f, 0) {
        Ok(v) => panic!("a closed segment was read: {v:?}"),
        Err(err) => err,
    };
    assert!(format!("{err:?}").contains("IllegalState"), "{err:?}");
    assert_eq!(word_at(&f, 0), 0, "nothing written");
}

/// The layout-shape handle never asked either: `seg.asReadOnly()` written
/// through `JAVA_INT.varHandle()` was written.
#[test]
fn a_layout_varhandle_refuses_to_write_a_read_only_segment() {
    let mut f = fixture();
    f.ctx.set_field(f.seg, 3, Value::Int(1));
    let shape = SegmentVhShape {
        width: 4,
        carrier: b'I',
        little_endian: cfg!(target_endian = "little"),
        base_offset: 0,
        stride: 0,
        alignment: 4,
        target_size: 0,
        target_align: 1,
        index_bound: None,
        scaled_index: false,
        enclosing_size: 0,
    };
    let args = [
        Value::Object(Some(f.vh)),
        Value::Object(Some(f.seg)),
        Value::Long(0),
        Value::Int(3),
    ];
    let err = match layout_vh_set(&mut f.ctx, shape, &args) {
        Ok(v) => panic!("a read-only segment was written: {v:?}"),
        Err(err) => err,
    };
    assert!(format!("{err:?}").contains("read-only"), "{err:?}");
    assert_eq!(word_at(&f, 0), 0);
}

/// Round 14 wave 6 (lane ffm6): the shape
/// `p67_memory_layout_array_element_var_handle` mints for
/// `JAVA_INT.arrayElementVarHandle()` stores at `offset + 4 * index`, and
/// refuses the way JDK 25's `AbstractLayout.scale` does -- before the
/// segment's own checks (here: before the read-only refusal).
#[test]
fn an_array_element_layout_varhandle_scales_and_checks_its_index() {
    let mut f = fixture();
    let shape = SegmentVhShape {
        width: 4,
        carrier: b'I',
        little_endian: cfg!(target_endian = "little"),
        base_offset: 0,
        stride: 4,
        alignment: 4,
        target_size: 0,
        target_align: 1,
        index_bound: None,
        scaled_index: true,
        enclosing_size: 0,
    };
    let vh = Value::Object(Some(f.vh));
    let seg = Value::Object(Some(f.seg));
    let stored = layout_vh_set(
        &mut f.ctx,
        shape,
        &[vh, seg, Value::Long(4), Value::Long(2), Value::Int(42)],
    );
    assert!(matches!(stored, Ok(None)), "{stored:?}");
    assert_eq!(word_at(&f, 12), 42);
    assert_eq!(word_at(&f, 0), 0);
    let refusal = |f: &mut Fixture, offset: i64, index: i64| -> String {
        let args = [vh, seg, Value::Long(offset), Value::Long(index)];
        match layout_vh_get(&mut f.ctx, shape, &args) {
            Ok(v) => panic!("offset {offset} index {index} answered: {v:?}"),
            Err(err) => format!("{err:?}"),
        }
    };
    let err = refusal(&mut f, 0, -1);
    assert!(err.contains("The provided index is negative: -1"), "{err}");
    let err = refusal(&mut f, -4, -1);
    assert!(err.contains("The provided offset is negative: -4"), "{err}");
    let err = refusal(&mut f, 0, i64::MAX);
    assert!(err.contains("long overflow"), "{err}");
    // In range for `scale`, past the segment: the segment's own refusal.
    let err = refusal(&mut f, 0, 4);
    assert!(err.contains("IndexOutOfBounds"), "{err}");
    // The index refusal precedes the read-only one.
    f.ctx.set_field(f.seg, 3, Value::Int(1));
    let err = match layout_vh_set(
        &mut f.ctx,
        shape,
        &[vh, seg, Value::Long(0), Value::Long(-1), Value::Int(7)],
    ) {
        Ok(v) => panic!("a negative index was written: {v:?}"),
        Err(err) => format!("{err:?}"),
    };
    assert!(err.contains("The provided index is negative"), "{err}");
}

/// Round 14 wave 6 (lane ffm6): a `sequenceElement()` index is
/// `Objects.checkIndex(index, elementCount)`, even where the address it
/// names is still inside the segment.
#[test]
fn a_sequence_layout_varhandle_checks_its_index_against_the_element_count() {
    let mut f = fixture();
    let shape = SegmentVhShape {
        width: 4,
        carrier: b'I',
        little_endian: cfg!(target_endian = "little"),
        base_offset: 0,
        stride: 4,
        alignment: 4,
        target_size: 0,
        target_align: 1,
        index_bound: Some(2),
        scaled_index: false,
        enclosing_size: 0,
    };
    let vh = Value::Object(Some(f.vh));
    let seg = Value::Object(Some(f.seg));
    let stored = layout_vh_set(
        &mut f.ctx,
        shape,
        &[vh, seg, Value::Long(0), Value::Long(1), Value::Int(9)],
    );
    assert!(matches!(stored, Ok(None)), "{stored:?}");
    assert_eq!(word_at(&f, 4), 9);
    for index in [2_i64, -1] {
        let err = match layout_vh_set(
            &mut f.ctx,
            shape,
            &[vh, seg, Value::Long(0), Value::Long(index), Value::Int(5)],
        ) {
            Ok(v) => panic!("index {index} was written: {v:?}"),
            Err(err) => format!("{err:?}"),
        };
        assert!(
            err.contains(&format!("Index {index} out of bounds for length 2")),
            "{err}"
        );
    }
    assert_eq!(word_at(&f, 8), 0);
    // `sequenceLayout(10, JAVA_INT)` (40 bytes) does not fit the 16-byte
    // segment at base 0, so even index 0 is refused (`checkEnclosingLayout`).
    let tall = SegmentVhShape {
        index_bound: Some(10),
        enclosing_size: 40,
        ..shape
    };
    let err = match layout_vh_set(
        &mut f.ctx,
        tall,
        &[vh, seg, Value::Long(0), Value::Long(0), Value::Int(5)],
    ) {
        Ok(v) => panic!("a sequence larger than the segment was written: {v:?}"),
        Err(err) => format!("{err:?}"),
    };
    assert!(err.contains("Out of bound access"), "{err}");
    assert_eq!(word_at(&f, 0), 0);
}

/// Round 14 wave 7 (lane review7): the ROOT layout's alignment is checked
/// at the handle's base, as JDK 25's `checkEnclosingLayout` does -- member
/// `b` at offset 4 of an 8-aligned `struct { int a; int b; }` is written
/// at base 0 (it was refused: 8 was asked of offset 4), and base 4 is
/// refused by the struct's alignment.
#[test]
fn a_member_handle_checks_the_root_alignment_at_its_base() {
    let mut f = fixture();
    let shape = SegmentVhShape {
        width: 4,
        carrier: b'I',
        little_endian: cfg!(target_endian = "little"),
        base_offset: 4,
        stride: 0,
        alignment: 8,
        target_size: 0,
        target_align: 1,
        index_bound: None,
        scaled_index: false,
        enclosing_size: 8,
    };
    let vh = Value::Object(Some(f.vh));
    let seg = Value::Object(Some(f.seg));
    let stored = layout_vh_set(
        &mut f.ctx,
        shape,
        &[vh, seg, Value::Long(0), Value::Int(77)],
    );
    assert!(matches!(stored, Ok(None)), "{stored:?}");
    assert_eq!(word_at(&f, 4), 77);
    let err = match layout_vh_get(&mut f.ctx, shape, &[vh, seg, Value::Long(4)]) {
        Ok(v) => panic!("a base the struct's alignment refuses was read: {v:?}"),
        Err(err) => format!("{err:?}"),
    };
    assert!(
        err.contains("Target offset 4 is incompatible with alignment constraint 8"),
        "{err}"
    );
}

/// Round 14 wave 1 (lane ffm): `ADDRESS.withTargetLayout(JAVA_INT)
/// .varHandle().get(..)` answers a segment of the target's size (it was 0,
/// so following the pointer was `IndexOutOfBoundsException`), and a
/// pointer the target's alignment does not divide is refused
/// (`r13w13-ffm7-address-read-ignores-target-layout`).
#[test]
fn an_address_layout_varhandle_read_is_sized_and_aligned_by_its_target() {
    let mut f = fixture();
    let shape = SegmentVhShape {
        width: 8,
        carrier: FFM_ADDRESS_CARRIER,
        little_endian: cfg!(target_endian = "little"),
        base_offset: 0,
        stride: 0,
        alignment: 8,
        target_size: 4,
        target_align: 4,
        index_bound: None,
        scaled_index: false,
        enclosing_size: 0,
    };
    let args = [
        Value::Object(Some(f.vh)),
        Value::Object(Some(f.seg)),
        Value::Long(0),
    ];
    // SAFETY: the fixture's block is 16 bytes, live for the mock's life.
    unsafe { (f.ptr as *mut u64).write_unaligned(0x1000) };
    let read = match layout_vh_get(&mut f.ctx, shape, &args) {
        Ok(Some(Value::Object(Some(seg)))) => seg,
        other => panic!("no segment: {other:?}"),
    };
    assert_eq!(f.ctx.get_field(read, 0), Value::Long(0x1000));
    assert_eq!(f.ctx.get_field(read, 1), Value::Long(4));
    // SAFETY: as above.
    unsafe { (f.ptr as *mut u64).write_unaligned(0x1002) };
    let err = match layout_vh_get(&mut f.ctx, shape, &args) {
        Ok(v) => panic!("a misaligned pointer was answered: {v:?}"),
        Err(err) => err,
    };
    assert!(
        format!("{err:?}").contains("Invalid alignment constraint for address: 0x1002"),
        "{err:?}"
    );
    // Without a target (or with the switch off) the answer stays zero-length.
    let untargeted = SegmentVhShape {
        target_size: 0,
        target_align: 1,
        ..shape
    };
    let read = match layout_vh_get(&mut f.ctx, untargeted, &args) {
        Ok(Some(Value::Object(Some(seg)))) => seg,
        other => panic!("no segment: {other:?}"),
    };
    assert_eq!(f.ctx.get_field(read, 1), Value::Long(0));    }

/// j27 wave 4 (lane FFMMSG): `ADDRESS.varHandle().set(seg, 0L,
/// MemorySegment.ofArray(new int[2]))` is the `unboxSegment` filter's
/// `IllegalArgumentException: Heap segment not allowed: <segment>` on
/// HotSpot 25; this named the carrier class (`class: ..`). Nothing stored.
#[test]
fn an_address_layout_varhandle_refuses_a_heap_value_with_hotspot_text() {
    let mut f = fixture();
    let array = f.ctx.new_array(cratonvm_types::ArrayElementType::Int, 2);
    let heap =
        match crate::panama::pe_of_array_alias(&mut f.ctx, &[Value::Object(Some(array))]) {
            Ok(Some(Value::Object(Some(heap)))) => heap,
            other => panic!("ofArray answered {other:?}"),
        };
    let shape = SegmentVhShape {
        width: 8,
        carrier: FFM_ADDRESS_CARRIER,
        little_endian: cfg!(target_endian = "little"),
        base_offset: 0,
        stride: 0,
        alignment: 8,
        target_size: 0,
        target_align: 1,
        index_bound: None,
        scaled_index: false,
        enclosing_size: 0,
    };
    let args = [
        Value::Object(Some(f.vh)),
        Value::Object(Some(f.seg)),
        Value::Long(0),
        Value::Object(Some(heap)),
    ];
    let message = match layout_vh_set(&mut f.ctx, shape, &args) {
        Err(MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
            RuntimeError::IllegalArgumentException { message },
        ))) => message,
        other => panic!("expected IllegalArgumentException, got {other:?}"),
    };
    assert_eq!(
        message,
        format!(
            "Heap segment not allowed: MemorySegment{{ kind: heap, heapBase: [I@{:x}, address: 0x0, byteSize: 8 }}",
            f.ctx.identity_hash_code(array) as u32
        )
    );
    assert_eq!((word_at(&f, 0), word_at(&f, 4)), (0, 0), "the refused store landed");
}

/// A `byte[16]` alias segment and its HotSpot `toString()` text.
fn heap_bytes(f: &mut Fixture) -> (ObjectRef, String) {
    let array = f.ctx.new_array(cratonvm_types::ArrayElementType::Byte, 16);
    let seg =
        match crate::panama::pe_of_array_alias(&mut f.ctx, &[Value::Object(Some(array))]) {
            Ok(Some(Value::Object(Some(seg)))) => seg,
            other => panic!("ofArray answered {other:?}"),
        };
    let text = format!(
        "MemorySegment{{ kind: heap, heapBase: [B@{:x}, address: 0x0, byteSize: 16 }}",
        f.ctx.identity_hash_code(array) as u32
    );
    (seg, text)
}

fn message_of(result: Result<impl std::fmt::Debug, MethodCallFailed>) -> String {
    match result {
        Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::IndexOutOfBoundsException { message: Some(message) },
        ))) => message,
        Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::IllegalArgumentException { message },
        ))) => message,
        other => panic!("expected IOOBE or IAE, got {other:?}"),
    }
}

/// j27 wave 4 (lane FFMMSG): a CratonVM carrier reaching
/// `checkEnclosingLayout`'s two refusals gets HotSpot's text, the segment
/// from `panama::segment_to_string` and the layout from its own
/// `toString()`. It said `Out of bound access on segment: offset=..,
/// length=.., size=..` and `.. for segment` with nothing after.
#[test]
fn a_carrier_through_check_enclosing_names_segment_and_layout() {
    let mut f = fixture();
    let (seg, text) = heap_bytes(&mut f);
    assert_eq!(
        message_of(segment_vh_check_enclosing(&mut f.ctx, seg, f.vh, 4, 4, 14)),
        format!("Out of bound access on segment {text}; new offset = 14; new length = 4")
    );
    let i4 = f.ctx.create_string("i4");
    f.ctx.set_invoke_virtual_result(Ok(Some(Value::Object(Some(i4)))));
    assert_eq!(
        message_of(segment_vh_check_enclosing(&mut f.ctx, seg, f.vh, 4, 4, 0)),
        format!(
            "Target offset 0 is incompatible with alignment constraint 4 (of i4) for segment {text}"
        )
    );
}

/// j27 wave 4 (lane FFMMSG): the same two refusals on the layout-handle
/// road (`layout_vh_locate`): `Out of bound access on segment of 16
/// bytes` and a bare `.. for segment` before.
#[test]
fn a_carrier_through_the_layout_handle_names_segment_and_layout() {
    let mut f = fixture();
    let (seg, text) = heap_bytes(&mut f);
    let shape = SegmentVhShape {
        width: 4,
        carrier: b'I',
        little_endian: cfg!(target_endian = "little"),
        base_offset: 0,
        stride: 0,
        alignment: 4,
        target_size: 0,
        target_align: 1,
        index_bound: None,
        scaled_index: false,
        enclosing_size: 4,
    };
    let vh = f.vh;
    let at = |base: i64| [Value::Object(Some(vh)), Value::Object(Some(seg)), Value::Long(base)];
    let args = at(14);
    assert_eq!(
        message_of(layout_vh_locate(&mut f.ctx, shape, &args, seg).map(|_| ())),
        format!("Out of bound access on segment {text}; new offset = 14; new length = 4")
    );
    // Without a modelled enclosing layout the access's own bounds check
    // answers the same text (it said `VarHandle access of 4 bytes at
    // offset 14 is out of bounds for a segment of 16 bytes`).
    let unenclosed = SegmentVhShape {
        enclosing_size: 0,
        ..shape
    };
    assert_eq!(
        message_of(layout_vh_locate(&mut f.ctx, unenclosed, &args, seg).map(|_| ())),
        format!("Out of bound access on segment {text}; new offset = 14; new length = 4")
    );
    let args = at(0);
    let i4 = f.ctx.create_string("i4");
    f.ctx.set_invoke_virtual_result(Ok(Some(Value::Object(Some(i4)))));
    assert_eq!(
        message_of(layout_vh_locate(&mut f.ctx, shape, &args, seg).map(|_| ())),
        format!(
            "Target offset 0 is incompatible with alignment constraint 4 (of i4) for segment {text}"
        )
    );
}

/// Round 13 wave 1 (lane mhffm): turn the fixture's handle into a JDK 25
/// PATH handle -- `enclosing` a 8-byte, 4-aligned struct, the member a
/// `short` at offset 2, the value type in the `VarForm`
/// (`VarHandleSegmentAsShorts`).
fn make_struct_path_handle(f: &mut Fixture) {
    let vh_cid = f.ctx.class_id_of_object(f.vh);
    let field = |name: &str, desc: &str, slot: usize, owner: ClassId| {
        cratonvm_native_api::FieldMetadata {
            name: name.to_string(),
            descriptor: desc.to_string(),
            access_flags: 0,
            slot_index: slot,
            declaring_class_id: owner,
            is_static: false,
        }
    };
    f.ctx.set_declared_fields(
        vh_cid,
        vec![
            field("enclosing", "Ljava/lang/foreign/MemoryLayout;", 0, vh_cid),
            field("offset", "J", 1, vh_cid),
            field("be", "Z", 2, vh_cid),
            field("vform", "Ljava/lang/invoke/VarForm;", 3, vh_cid),
        ],
    );
    let form_cid = match f.ctx.ensure_class_initialized("java/lang/invoke/VarForm") {
        Ok(cid) => cid,
        Err(err) => panic!("no class: {err:?}"),
    };
    f.ctx.set_declared_fields(
        form_cid,
        vec![field("implClass", "Ljava/lang/Class;", 0, form_cid)],
    );
    let impl_cid = match f
        .ctx
        .ensure_class_initialized("java/lang/invoke/VarHandleSegmentAsShorts")
    {
        Ok(cid) => cid,
        Err(err) => panic!("no class: {err:?}"),
    };
    let mirror = f.ctx.alloc_object(impl_cid, 1);
    f.ctx
        .set_field(mirror, 0, Value::Int(impl_cid.as_u32() as i32));
    let form = f.ctx.alloc_object(form_cid, 1);
    f.ctx.set_field(form, 0, Value::Object(Some(mirror)));
    let struct_cid = match f
        .ctx
        .ensure_class_initialized("jdk/internal/foreign/layout/StructLayoutImpl")
    {
        Ok(cid) => cid,
        Err(err) => panic!("no class: {err:?}"),
    };
    let layout = f.ctx.alloc_object(struct_cid, 3);
    f.ctx.set_field(layout, 0, Value::Long(8));
    f.ctx.set_field(layout, 1, Value::Long(4));
    f.ctx.set_field(f.vh, 0, Value::Object(Some(layout)));
    f.ctx.set_field(f.vh, 1, Value::Long(2));
    f.ctx.set_field(f.vh, 3, Value::Object(Some(form)));
}

fn short_at(f: &Fixture, offset: usize) -> u16 {
    // SAFETY: the fixture's block is 16 bytes, live for the mock's life.
    unsafe { (f.ptr.add(offset) as *const u16).read_unaligned() }
}

/// Wave 7 read a group `enclosing` as an 8-byte ADDRESS at `offset`:
/// the member at base 8 was an out-of-bounds 8-byte read at 10.
#[test]
fn a_path_handle_reads_the_forms_type_at_the_members_offset() {
    let mut f = fixture();
    make_struct_path_handle(&mut f);
    assert!(set(&mut f, 8, 0x1234).is_ok());
    assert_eq!(short_at(&f, 10), 0x1234);
    let got = match get(&mut f, 8) {
        Ok(Some(Value::Object(Some(boxed)))) => crate::lang_class::unbox_value(&f.ctx, boxed),
        Ok(Some(v)) => v,
        other => panic!("get gave {other:?}"),
    };
    assert_eq!(got, Value::Int(0x1234));
}

/// `checkEnclosingLayout`: the whole struct must fit at the coordinate
/// (base 10 leaves 6 of its 8 bytes), and the coordinate must meet the
/// struct's alignment (base 2), even where the member alone would fit.
#[test]
fn a_path_handle_checks_the_enclosing_layout_not_the_member() {
    let mut f = fixture();
    make_struct_path_handle(&mut f);
    let err = match get(&mut f, 10) {
        Ok(v) => panic!("a struct past the end answered: {v:?}"),
        Err(err) => err,
    };
    assert!(format!("{err:?}").contains("IndexOutOfBounds"), "{err:?}");
    let err = match get(&mut f, 2) {
        Ok(v) => panic!("a misaligned struct answered: {v:?}"),
        Err(err) => err,
    };
    assert!(
        format!("{err:?}").contains("alignment constraint 4"),
        "{err:?}"
    );
}

/// HotSpot checks bounds before the session: a CLOSED segment accessed
/// out of bounds is `IndexOutOfBoundsException`, not `Already closed`.
#[test]
fn bounds_come_before_the_session_check() {
    let mut f = fixture();
    make_struct_path_handle(&mut f);
    let slots = p67_session_slots(&f.ctx, f.session);
    f.ctx.set_field(f.session, slots.state, Value::Int(0));
    let err = match get(&mut f, 10) {
        Ok(v) => panic!("a closed out-of-bounds read answered: {v:?}"),
        Err(err) => err,
    };
    assert!(format!("{err:?}").contains("IndexOutOfBounds"), "{err:?}");
    let err = match get(&mut f, 8) {
        Ok(v) => panic!("a closed segment was read: {v:?}"),
        Err(err) => err,
    };
    assert!(format!("{err:?}").contains("Already closed"), "{err:?}");    }

#[test]
fn the_form_class_names_the_value_type() {
    let t = segment_vh_shape_of_form_class;
    assert!(t("java/lang/invoke/VarHandleSegmentAsInts") == Some(SegShape::Int));
    assert!(t("java/lang/invoke/VarHandleSegmentAsIntsAligned") == Some(SegShape::Int));
    assert!(t("java/lang/invoke/VarHandleSegmentAsLongs") == Some(SegShape::Long));
    assert!(t("java/lang/invoke/VarHandleSegmentAsBooleans") == Some(SegShape::Boolean));
    assert!(t("java/lang/invoke/VarHandleSegmentAsDoublesAligned") == Some(SegShape::Double));
    assert!(t("java/lang/invoke/VarHandleByteArrayAsInts").is_none());
}

/// JIT round 20 wave 8 (lane huntFFM): JDK 25's unaligned segment forms
/// declare `get` / `set` only; the `...Aligned` subclasses and the byte /
/// boolean forms (no subclass) add the ordered modes.
#[test]
fn only_aligned_and_byte_forms_define_the_ordered_modes() {
    let t = segment_vh_form_class_is_aligned;
    assert!(t("java/lang/invoke/VarHandleSegmentAsIntsAligned"));
    assert!(t("java/lang/invoke/VarHandleSegmentAsDoublesAligned"));
    assert!(t("java/lang/invoke/VarHandleSegmentAsBytes"));
    assert!(t("java/lang/invoke/VarHandleSegmentAsBooleans"));
    assert!(!t("java/lang/invoke/VarHandleSegmentAsInts"));
    assert!(!t("java/lang/invoke/VarHandleSegmentAsShorts"));
    assert!(!t("java/lang/invoke/VarHandleSegmentAsLongs"));
}
