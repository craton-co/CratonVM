//! VH2-3: what the mock can judge. Its hierarchy answers no by-name walk
//! (`class_assignable_to_name` is `None`), so a class-typed site fails
//! open here and the class half is the probe's (`R15Vh2RefReturnCast`).
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};

fn cce_message(e: &MethodCallFailed) -> Option<String> {
    match e {
        MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::ClassCastException { message, .. },
        )) => Some(message.clone()),
        _ => None,
    }
}

#[test]
fn a_non_array_returned_into_an_array_site_is_cast() {
    if !vh_reference_return_checkcast_enabled() {
        return; // an explicit `0` in the test environment wins
    }
    let mut ctx = MockNativeContext::new();
    let s = match ctx.new_object("java/lang/String") {
        Ok(Some(Value::Object(Some(o)))) => o,
        other => panic!("new_object: {other:?}"),
    };
    let refusal =
        varhandle_reference_return_refusal(&mut ctx, Value::Object(Some(s)), "[I", Some(true));
    assert_eq!(
        refusal.as_ref().and_then(cce_message).as_deref(),
        Some("Cannot cast java.lang.String to [I")
    );
    let ints = ctx.new_array(ArrayElementType::Int, 1);
    assert!(varhandle_reference_return_refusal(
        &mut ctx,
        Value::Object(Some(ints)),
        "[I",
        Some(true)
    )
    .is_none());
}

#[test]
fn null_and_object_sites_are_never_refused() {
    let mut ctx = MockNativeContext::new();
    let s = match ctx.new_object("java/lang/String") {
        Ok(Some(Value::Object(Some(o)))) => o,
        other => panic!("new_object: {other:?}"),
    };
    for site in ["[I", "Ljava/lang/Integer;", "Ljava/lang/Object;"] {
        assert!(
            varhandle_reference_return_refusal(&mut ctx, Value::Object(None), site, None)
                .is_none()
        );
    }
    assert!(varhandle_reference_return_refusal(
        &mut ctx,
        Value::Object(Some(s)),
        "Ljava/lang/Object;",
        Some(true)
    )
    .is_none());
}
