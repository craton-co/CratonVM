use super::*;
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn a_constructor_link_offers_its_receiver_to_finalizer_registration() {
    let mut ctx = mock_ctx();
    let fresh = ctx.alloc_object(ClassId::new(51), 1);
    let offered = link_to_special_constructor_registers(
        &mut ctx,
        "<init>",
        &[Value::Object(Some(fresh)), Value::Int(3)],
    );
    assert_eq!(*ctx.register_if_finalizable_calls.borrow(), vec![fresh]);
    // JIT round 21 wave 10, lane finalize: the offered receiver comes back
    // for the withdrawal of a constructor that throws before Object.<init>.
    assert_eq!(offered.map(|(obj, _)| obj), Some(fresh));
}

#[test]
fn a_method_link_or_a_null_receiver_offers_nothing() {
    let mut ctx = mock_ctx();
    let obj = ctx.alloc_object(ClassId::new(52), 1);
    assert!(
        link_to_special_constructor_registers(&mut ctx, "toString", &[Value::Object(Some(obj))])
            .is_none()
    );
    assert!(link_to_special_constructor_registers(&mut ctx, "<init>", &[Value::Object(None)])
        .is_none());
    assert!(link_to_special_constructor_registers(&mut ctx, "<init>", &[]).is_none());
    assert!(ctx.register_if_finalizable_calls.borrow().is_empty());
}
