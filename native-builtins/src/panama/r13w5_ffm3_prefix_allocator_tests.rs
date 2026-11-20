//! Round 13 wave 5 (lane ffm3, hand-back): `SegmentAllocator.prefixAllocator`
//! over a carrier is the carrier itself, so the carrier answers
//! `allocate(long, long)` as `AbstractMemorySegmentImpl` does.
use super::*;
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn a_carrier_allocates_a_prefix_slice_with_the_jdks_checks() {
    let mut ctx = mock_ctx();
    let (_id, ptr) = ctx
        .allocate_native_memory(16, 8)
        .unwrap_or_else(|| panic!("mock allocation failed"));
    let seg = match alloc_segment_carrier(&mut ctx, 6) {
        Ok(seg) => seg,
        Err(err) => panic!("no carrier: {err:?}"),
    };
    ctx.set_field(seg, 0, Value::Long(ptr as i64));
    ctx.set_field(seg, 1, Value::Long(16));
    ctx.set_field(seg, 2, Value::Object(None));
    ctx.set_field(seg, 3, Value::Int(0));
    ctx.set_field(seg, 4, Value::Int(1));
    ctx.set_field(seg, 5, Value::Long(0));
    let this = Value::Object(Some(seg));

    let slice =
        match pe_segment_allocate_prefix(&mut ctx, &[this, Value::Long(8), Value::Long(4)]) {
            Ok(Some(Value::Object(Some(slice)))) => slice,
            other => panic!("allocate gave {other:?}"),
        };
    assert_eq!(
        crate::panama_libffi::segment_address(&ctx, slice),
        ptr as i64
    );
    assert_eq!(crate::panama_libffi::segment_byte_size(&ctx, slice), 8);

    let err =
        match pe_segment_allocate_prefix(&mut ctx, &[this, Value::Long(8), Value::Long(3)]) {
            Ok(v) => panic!("alignment 3 was accepted: {v:?}"),
            Err(err) => err,
        };
    assert!(
        format!("{err:?}").contains("Invalid alignment constraint"),
        "{err:?}"
    );
    let err =
        match pe_segment_allocate_prefix(&mut ctx, &[this, Value::Long(32), Value::Long(1)]) {
            Ok(v) => panic!("a 32-byte prefix of 16 bytes was accepted: {v:?}"),
            Err(err) => err,
        };
    assert!(format!("{err:?}").contains("IndexOutOfBounds"), "{err:?}");    }