//! Round 13 wave 13 (lane ffm7): an ADDRESS read through a layout with a
//! target answers `Utils.longToAddress(raw, pointeeByteSize,
//! pointeeByteAlign)` (`r13w13-ffm7-address-read-ignores-target-layout`).
use super::*;
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// A fabricated `AddressLayout` carrier (`[0] size, [1] align, .., [4]
/// target`), with `target` or none.
fn address_layout(ctx: &mut dyn NativeContext, target: Option<ObjectRef>) -> ObjectRef {
    let layout = match try_alloc_concurrent_synthetic(ctx, "cratonvm/test/R13AddressLayout", 5)
    {
        Ok(layout) => layout,
        Err(err) => panic!("no carrier: {err:?}"),
    };
    ctx.set_field(layout, 0, Value::Long(8));
    ctx.set_field(layout, 1, Value::Long(8));
    ctx.set_field(layout, 4, Value::Object(target));
    layout
}

#[test]
fn an_address_read_is_sized_and_aligned_by_its_target() {
    let mut ctx = mock_ctx();
    let int_layout =
        match try_alloc_concurrent_synthetic(&mut ctx, "cratonvm/test/R13IntLayout", 4) {
            Ok(layout) => layout,
            Err(err) => panic!("no carrier: {err:?}"),
        };
    ctx.set_field(int_layout, 0, Value::Long(4));
    ctx.set_field(int_layout, 1, Value::Long(4));
    let targeted = address_layout(&mut ctx, Some(int_layout));
    let bare = address_layout(&mut ctx, None);

    let seg = match pe_address_read_segment(&mut ctx, targeted, 0x1000) {
        Ok(seg) => seg,
        Err(err) => panic!("an aligned pointer was refused: {err:?}"),
    };
    assert_eq!(ctx.get_field(seg, 0), Value::Long(0x1000));
    assert_eq!(ctx.get_field(seg, 1), Value::Long(4), "the target's size");

    let refused = pe_address_read_segment(&mut ctx, targeted, 0x1002);
    assert!(
        format!("{refused:?}").contains("Invalid alignment constraint for address: 0x1002"),
        "{refused:?}"
    );

    let seg = match pe_address_read_segment(&mut ctx, bare, 0x1003) {
        Ok(seg) => seg,
        Err(err) => panic!("no target, no alignment: {err:?}"),
    };
    assert_eq!(
        ctx.get_field(seg, 1),
        Value::Long(0),
        "zero-length without a target"
    );
}
