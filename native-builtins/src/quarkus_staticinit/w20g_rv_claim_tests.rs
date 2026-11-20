//! gc-common w20-g (`common-w19f-runtime-value-inflight-set-is-address-keyed`):
//! the supplier claim lives in the RuntimeValue itself, so it moves with
//! it. The hook stands in for a moving collection inside `supplier.get()`:
//! it re-addresses the native pins from the old copy to the new one and
//! gives the new copy the moved field contents (the claim is a
//! self-reference, which the collector forwards), then looks at the new
//! copy the way a second `getValue()` would.
use super::*;
use crate::test_utils::{mock_ctx, MockNativeContext};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use std::cell::Cell;

thread_local! {
    static W20G_MOVE: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
    static W20G_RESULT: Cell<usize> = const { Cell::new(0) };
    static W20G_GETS: Cell<usize> = const { Cell::new(0) };
    static W20G_SECOND_BUSY: Cell<bool> = const { Cell::new(false) };
}

fn obj(addr: usize) -> ObjectRef {
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn w20g_moving_supplier_hook(
    ctx: &mut MockNativeContext,
    _receiver: ObjectRef,
    method_name: &str,
    _descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    if method_name != "get" {
        return None;
    }
    W20G_GETS.with(|g| g.set(g.get() + 1));
    let (old, new) = W20G_MOVE.with(|m| m.get());
    ctx.remap_native_pin_addr_for_test(old, new);
    // The moved copy carries the claim, forwarded to its new address.
    let new = obj(new);
    ctx.set_field(new, RV_FIELD_SUPPLIER, Value::Object(Some(new)));
    // A second getValue() on the moved RuntimeValue must see it busy,
    // not claim (and invoke) the supplier a second time.
    let second = rv_step(ctx, new);
    W20G_SECOND_BUSY.with(|b| b.set(second == RvStep::Busy));
    Some(Ok(Some(Value::Object(Some(obj(
        W20G_RESULT.with(|r| r.get())
    ))))))
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
fn w20g_a_moved_runtime_value_is_still_claimed() {
    let mut ctx = mock_ctx();
    let supplier = ctx.fresh_object_ref();
    let rv_old = runtime_value(&mut ctx, supplier);
    let rv_new = runtime_value(&mut ctx, supplier);
    let result = ctx.create_string("materialized-once");
    W20G_MOVE.with(|m| m.set((rv_old.as_ptr() as usize, rv_new.as_ptr() as usize)));
    W20G_RESULT.with(|r| r.set(result.as_ptr() as usize));
    W20G_GETS.with(|g| g.set(0));
    W20G_SECOND_BUSY.with(|b| b.set(false));
    ctx.set_invoke_virtual_hook(w20g_moving_supplier_hook);

    let got = native_rv_get_value(&mut ctx, &[Value::Object(Some(rv_old))]).unwrap();
    assert_eq!(got, Some(Value::Object(Some(result))));
    assert!(
        W20G_SECOND_BUSY.with(|b| b.get()),
        "a getValue() on the moved copy must wait for the claim, not take it"
    );
    // Published into the current copy; a later getValue() is a plain read.
    let again = native_rv_get_value(&mut ctx, &[Value::Object(Some(rv_new))]).unwrap();
    assert_eq!(again, Some(Value::Object(Some(result))));
    assert_eq!(W20G_GETS.with(|g| g.get()), 1, "supplier.get() ran once");
}

/// A supplier that throws releases the claim: the supplier is back, and
/// the next getValue() invokes it again.
#[test]
fn w20g_a_throwing_supplier_releases_the_claim() {
    let mut ctx = mock_ctx();
    let supplier = ctx.fresh_object_ref();
    let rv = runtime_value(&mut ctx, supplier);
    let exc = ctx.fresh_object_ref();
    ctx.set_invoke_virtual_result(Err(
        cratonvm_types::error::MethodCallFailed::ExceptionThrown(exc),
    ));
    assert!(native_rv_get_value(&mut ctx, &[Value::Object(Some(rv))]).is_err());
    assert_eq!(
        ctx.get_field(rv, RV_FIELD_SUPPLIER),
        Value::Object(Some(supplier)),
        "the supplier is restored, not left claimed"
    );
    let result = ctx.create_string("second-try");
    ctx.set_invoke_virtual_result(Ok(Some(Value::Object(Some(result)))));
    assert_eq!(
        native_rv_get_value(&mut ctx, &[Value::Object(Some(rv))]).unwrap(),
        Some(Value::Object(Some(result)))
    );
    assert_eq!(ctx.get_field(rv, RV_FIELD_SUPPLIER), Value::Object(None));
}

/// A RuntimeValue whose slot names itself is busy; an unclaimed one is
/// claimed exactly once.
#[test]
fn w20g_rv_step_claims_once() {
    let mut ctx = mock_ctx();
    let supplier = ctx.fresh_object_ref();
    let rv = runtime_value(&mut ctx, supplier);
    assert_eq!(rv_step(&mut ctx, rv), RvStep::Claimed(supplier));
    assert_eq!(rv_step(&mut ctx, rv), RvStep::Busy);
    assert_eq!(
        ctx.get_field(rv, RV_FIELD_SUPPLIER),
        Value::Object(Some(rv))
    );
}
