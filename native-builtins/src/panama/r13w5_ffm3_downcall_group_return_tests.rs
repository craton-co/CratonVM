//! Round 13 wave 5 (lane ffm3): a by-value group return is copied into the
//! segment the caller's `SegmentAllocator` answers, and that segment is
//! the result (it was a block from `allocate_native_memory` nothing freed).
use super::*;
use crate::test_utils::{mock_ctx, MockNativeContext};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

const ALLOCATE: &str = "(JJ)Ljava/lang/foreign/MemorySegment;";

/// An allocator whose `allocate(size, align)` answers a native carrier.
fn native_allocator(
    ctx: &mut MockNativeContext,
    _receiver: ObjectRef,
    method: &str,
    descriptor: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    if method != "allocate" || descriptor != ALLOCATE {
        return None;
    }
    let size = match args.first() {
        Some(Value::Long(n)) => *n,
        _ => return Some(Ok(None)),
    };
    let (_id, ptr) = ctx.allocate_native_memory(size as usize, 8)?;
    let seg = match alloc_segment_carrier(ctx, 6) {
        Ok(seg) => seg,
        Err(err) => return Some(Err(err)),
    };
    ctx.set_field(seg, 0, Value::Long(ptr as i64));
    ctx.set_field(seg, 1, Value::Long(size));
    ctx.set_field(seg, 2, Value::Object(None));
    ctx.set_field(seg, 3, Value::Int(0));
    ctx.set_field(seg, 4, Value::Int(1));
    ctx.set_field(seg, 5, Value::Long(0));
    Some(Ok(Some(Value::Object(Some(seg)))))
}

/// An allocator that answers a heap (`ofArray`) segment.
fn heap_allocator(
    ctx: &mut MockNativeContext,
    _receiver: ObjectRef,
    method: &str,
    descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    if method != "allocate" || descriptor != ALLOCATE {
        return None;
    }
    let array = ctx.new_array(cratonvm_types::ArrayElementType::Long, 2);
    Some(pe_of_array_alias(ctx, &[Value::Object(Some(array))]))
}

#[test]
fn the_struct_is_copied_into_the_allocators_segment() {
    let mut ctx = mock_ctx();
    ctx.set_invoke_virtual_hook(native_allocator);
    let allocator = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
    // libffi's slot is rounded up; only the layout's 12 bytes are copied.
    let ret: Vec<u8> = (1..=16).collect();
    let seg = match downcall_copy_group_return(&mut ctx, allocator, 12, 4, &ret) {
        Ok(seg) => seg,
        Err(err) => panic!("the copy failed: {err:?}"),
    };
    assert_eq!(ctx.get_field(seg, 1), Value::Long(12));
    let Value::Long(addr) = ctx.get_field(seg, 0) else {
        panic!("no address");
    };
    // SAFETY: the hook allocated 12 bytes at `addr`.
    let got = unsafe { std::slice::from_raw_parts(addr as *const u8, 12) }.to_vec();
    assert_eq!(got, (1..=12).collect::<Vec<u8>>());
}

#[test]
fn a_heap_segment_from_the_allocator_is_refused() {
    let mut ctx = mock_ctx();
    ctx.set_invoke_virtual_hook(heap_allocator);
    let allocator = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
    let ret = [0u8; 16];
    let err = match downcall_copy_group_return(&mut ctx, allocator, 16, 8, &ret) {
        Ok(seg) => panic!("a heap segment took a struct return: {seg:?}"),
        Err(err) => err,
    };
    assert!(
        format!("{err:?}").contains("Heap segment not allowed"),
        "{err:?}"
    );
}

/// JIT round 18 wave 11 (lane ffmret; `CRATONVM_FFM_GROUP_RETURN_HEAP_ALLOCATOR`):
/// a group the ABI returned in registers is written through a heap
/// segment, as HotSpot's `BufferStore` writes it; in memory it is still
/// refused (above).
#[test]
fn a_register_returned_group_is_written_into_a_heap_segment() {
    let mut ctx = mock_ctx();
    ctx.set_invoke_virtual_hook(heap_allocator);
    let allocator = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
    let ret: Vec<u8> = (1..=16).collect();
    let seg = match downcall_copy_group_return_in(&mut ctx, allocator, 16, 8, &ret, true, None)
    {
        Ok(seg) => seg,
        Err(err) => panic!("a register-returned group into a heap segment: {err:?}"),
    };
    let view = heap_segment_view(&ctx, seg).expect("the allocator's heap segment");
    assert_eq!(heap_read_bytes(&ctx, &view, 0, 16), Some(ret));
}

/// A six-slot native carrier of `size` zeroed bytes (JIT round 18 wave 11,
/// lane ffmret).
fn ffmret_native_carrier(
    ctx: &mut MockNativeContext,
    size: usize,
    read_only: bool,
) -> (ObjectRef, *mut u8) {
    let (_id, ptr) = ctx
        .allocate_native_memory(size.max(1), 8)
        .expect("native memory");
    // SAFETY: just allocated with room for `size` bytes.
    unsafe { std::ptr::write_bytes(ptr, 0, size) };
    let seg = alloc_segment_carrier(ctx, 6).expect("a carrier");
    ctx.set_field(seg, 0, Value::Long(ptr as i64));
    ctx.set_field(seg, 1, Value::Long(size as i64));
    ctx.set_field(seg, 2, Value::Object(None));
    ctx.set_field(seg, 3, Value::Int(i32::from(read_only)));
    ctx.set_field(seg, 4, Value::Int(1));
    ctx.set_field(seg, 5, Value::Long(0));
    (seg, ptr)
}

/// JIT round 18 wave 11 (lane ffmret; `CRATONVM_FFM_GROUP_RETURN_STORE_CHECKS`):
/// a register-returned group is stored as HotSpot's `BufferStore`
/// bindings store it -- one store per eightbyte (one per member of an
/// AArch64 float aggregate, one in all on Win64), a read-only segment
/// refused before any byte, and a store past the end an
/// `IndexOutOfBoundsException` after the stores before it. An in-memory
/// group keeps the clamped copy.
#[test]
fn register_group_stores_follow_hotspots_buffer_stores() {
    let plan16 = group_register_store_plan(16, None);
    if cfg!(windows) {
        assert_eq!(plan16, vec![(0, 16)]);
    } else {
        assert_eq!(plan16, vec![(0, 8), (8, 8)]);
        assert_eq!(group_register_store_plan(12, None), vec![(0, 8), (8, 4)]);
        assert_eq!(group_register_store_plan(3, None), vec![(0, 3)]);
    }
    assert_eq!(
        group_register_store_plan(16, Some(4)),
        vec![(0, 4), (4, 4), (8, 4), (12, 4)]
    );
    let mut ctx = mock_ctx();
    let ret: Vec<u8> = (1..=16).collect();
    let read = |ptr: *mut u8, n: usize| {
        // SAFETY: the carriers below own `n` bytes at `ptr`.
        unsafe { std::slice::from_raw_parts(ptr, n) }.to_vec()
    };

    let (seg, ptr) = ffmret_native_carrier(&mut ctx, 16, false);
    downcall_store_register_group(&mut ctx, seg, &ret, &plan16).expect("it fits");
    assert_eq!(read(ptr, 16), ret);

    let (small, small_ptr) = ffmret_native_carrier(&mut ctx, 12, false);
    let err = match downcall_store_register_group(&mut ctx, small, &ret, &plan16) {
        Ok(()) => panic!("a 12-byte segment took a 16-byte group"),
        Err(err) => format!("{err:?}"),
    };
    assert!(err.contains("IndexOutOfBounds"), "{err}");
    if cfg!(windows) {
        assert!(err.contains("new offset = 0; new length = 16"), "{err}");
        assert_eq!(read(small_ptr, 12), vec![0u8; 12], "nothing stored");
    } else {
        assert!(err.contains("new offset = 8; new length = 8"), "{err}");
        let mut want = ret[..8].to_vec();
        want.extend_from_slice(&[0u8; 4]);
        assert_eq!(read(small_ptr, 12), want, "the first eightbyte was stored");
    }

    // The VM answers `Object(None)` for the by-name `readOnly` read on this
    // carrier (it declares no such field), so the reader falls back to
    // slot 3; the mock's default `Int(0)` for an unknown name would read
    // as "writable" and hide the flag.
    ctx.set_absent_field_answers_null(true);
    let (ro, ro_ptr) = ffmret_native_carrier(&mut ctx, 16, true);
    let err = match downcall_store_register_group(&mut ctx, ro, &ret, &plan16) {
        Ok(()) => panic!("a read-only segment took a group"),
        Err(err) => format!("{err:?}"),
    };
    ctx.set_absent_field_answers_null(false);
    assert!(
        err.contains("Attempt to write a read-only segment"),
        "{err}"
    );
    assert_eq!(read(ro_ptr, 16), vec![0u8; 16]);

    // In memory: the clamped copy, no exception.
    let (clamped, clamped_ptr) = ffmret_native_carrier(&mut ctx, 12, false);
    downcall_store_group(&mut ctx, clamped, 16, &ret, false, None).expect("clamped");
    assert_eq!(read(clamped_ptr, 12), ret[..12].to_vec());
}

/// JIT round 18 wave 11 (lane ffmret; `CRATONVM_FFM_GROUP_RETURN_ALLOCATE_BEFORE_CALL`):
/// the in-memory allocation runs before the general path (and so before
/// the C call), the general path stores into that buffer, and the memo
/// path leaves in-memory groups to it. Source ratchet; the probe
/// `R18FfmretShapes` checks the behaviour.
#[test]
fn in_memory_groups_allocate_before_the_general_path() {
    let src = concat!(include_str!("../panama/mod.rs"), "\n", include_str!("../panama/alloc_segment_carrier.rs"), "\n", include_str!("../panama/register_pe_memory_segment_on.rs"), "\n", include_str!("../panama/pe_of_array_alias.rs"), "\n", include_str!("../panama/pe_swap_width.rs")).replace("\r\n", "\n");
    let body = |sig: &str| {
        let start = src.find(sig).unwrap_or_else(|| panic!("{sig} not found"));
        let rest = &src[start + 1..];
        rest[..rest.find("\n}\n").unwrap_or(rest.len())].to_string()
    };
    let wrapper = body("\npub(in crate::panama) fn pe_downcall_invoke_with(");
    let pre = wrapper
        .find("downcall_imr_preallocate(ctx, args)")
        .expect("the allocation");
    let general = wrapper
        .find("pe_downcall_invoke_general(ctx, &fresh_args")
        .expect("the general path with the fresh arguments");
    assert!(pre < general);
    let general_body = body("\npub(in crate::panama) fn pe_downcall_invoke_general(");
    let call = general_body
        .find("libffi::raw::ffi_call(")
        .expect("the call");
    let store = general_body
        .find("downcall_store_group(ctx, buffer, total, &ret_slot, false, None)")
        .expect("the store into the buffer");
    assert!(call < store);
    let memo = body("\npub(in crate::panama) fn pe_downcall_invoke_memo_with(");
    assert!(memo.contains("if !in_registers {"));
    let store_body = body("\npub(in crate::panama) fn downcall_store_group(");
    assert!(store_body.contains("if in_registers {"));
    assert!(store_body.contains("downcall_store_register_group("));
}

#[test]
fn an_allocator_answering_null_is_a_null_pointer() {
    let mut ctx = mock_ctx();
    // No hook: the mock's `invoke_virtual` answers `Ok(None)`.
    let allocator = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
    let ret = [0u8; 8];
    let err = match downcall_copy_group_return(&mut ctx, allocator, 8, 8, &ret) {
        Ok(seg) => panic!("no segment, yet an answer: {seg:?}"),
        Err(err) => err,
    };
    assert!(format!("{err:?}").contains("NullPointer"), "{err:?}");    }