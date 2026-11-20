use super::*;
use crate::test_utils::MockNativeContext;
use cratonvm_native_api::continuation_pin::transparent_natives;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

fn gci_i18vp_observe(
    _ctx: &mut MockNativeContext,
    class: &str,
    name: &str,
    desc: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    (class == JDK_LOADER_RESOURCE && name == "getBytes" && desc == "()[B")
        .then(|| Ok(Some(Value::Int(transparent_natives() as i32))))
}

fn gci_i18vp_yield(
    _ctx: &mut MockNativeContext,
    class: &str,
    _name: &str,
    _desc: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    if class != JDK_LOADER_RESOURCE {
        return None;
    }
    assert_eq!(transparent_natives(), 1);
    Some(Err(MethodCallFailed::InternalError(
        cratonvm_types::error::VmError::ContinuationYield {
            wake_after_nanos: 0,
        },
    )))
}

#[test]
fn gci_i18vp_the_resource_body_is_a_transparent_tail_call() {
    let mut ctx = MockNativeContext::new();
    let resource = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
    let args = [Value::Object(Some(resource))];
    ctx.set_invoke_hook(gci_i18vp_observe);
    assert_eq!(resource_get_bytes_body(&mut ctx, &args).ok(), Some(Some(Value::Int(1))));
    assert_eq!(transparent_natives(), 0, "withdrawn after the call");
    ctx.set_invoke_hook(gci_i18vp_yield);
    assert!(matches!(
        resource_get_bytes_body(&mut ctx, &args),
        Err(MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::ContinuationYield { .. }
        ))
    ));
    assert_eq!(transparent_natives(), 0, "withdrawn on the yield too");
}
