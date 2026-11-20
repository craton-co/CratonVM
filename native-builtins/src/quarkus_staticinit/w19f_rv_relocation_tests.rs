//! gc-common w19-f: `RuntimeValue.getValue()` publishes the supplier's
//! result through the receiver's CURRENT address. The hook stands in for a
//! moving collection inside `supplier.get()` (Java): it re-addresses the
//! mock's native pins from the old copy to the new one.
use super::*;
use crate::test_utils::{mock_ctx, MockNativeContext};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use std::cell::Cell;

thread_local! {
    static W19F_MOVE: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
    static W19F_RESULT: Cell<usize> = const { Cell::new(0) };
}

fn w19f_supplier_hook(
    ctx: &mut MockNativeContext,
    _receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    if method_name != "get" {
        return None;
    }
    let (old, new) = W19F_MOVE.with(|m| m.get());
    ctx.remap_native_pin_addr_for_test(old, new);
    let result = unsafe { ObjectRef::from_raw(W19F_RESULT.with(|r| r.get()) as *mut u8) };
    Some(Ok(Some(Value::Object(Some(result)))))
}

fn runtime_value(ctx: &mut MockNativeContext, supplier: ObjectRef) -> ObjectRef {
    let cid = ctx.ensure_class_initialized(CLS_RUNTIME_VALUE).unwrap();
    let rv = ctx.alloc_object(cid, 2);
    native_rv_init_supplier(
        ctx,
        &[Value::Object(Some(rv)), Value::Object(Some(supplier))],
    )
    .unwrap();
    rv
}

#[test]
fn w19f_get_value_publishes_into_the_moved_receiver() {
    let mut ctx = mock_ctx();
    let supplier = ctx.fresh_object_ref();
    let rv_old = runtime_value(&mut ctx, supplier);
    let rv_new = runtime_value(&mut ctx, supplier);
    let result = ctx.create_string("materialized");
    W19F_MOVE.with(|m| m.set((rv_old.as_ptr() as usize, rv_new.as_ptr() as usize)));
    W19F_RESULT.with(|r| r.set(result.as_ptr() as usize));
    ctx.set_invoke_virtual_hook(w19f_supplier_hook);
    let pins_before = ctx.native_pin_count_for_test();

    let got = native_rv_get_value(&mut ctx, &[Value::Object(Some(rv_old))]).unwrap();

    assert_eq!(got, Some(Value::Object(Some(result))));
    assert_eq!(
        ctx.get_field(rv_new, RV_FIELD_VALUE),
        Value::Object(Some(result)),
        "the value must land in the receiver's current copy"
    );
    assert_eq!(
        ctx.get_field(rv_new, RV_FIELD_SUPPLIER),
        Value::Object(None),
        "the supplier must be cleared in the receiver's current copy"
    );
    assert_eq!(
        ctx.get_field(rv_old, RV_FIELD_VALUE),
        Value::Object(None),
        "nothing may be written through the vacated address"
    );
    assert_eq!(
        ctx.native_pin_count_for_test(),
        pins_before,
        "no pin left behind"
    );
}
