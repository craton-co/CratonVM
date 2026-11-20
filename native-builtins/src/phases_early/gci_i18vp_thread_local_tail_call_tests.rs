use super::*;
use crate::test_utils::MockNativeContext;
use cratonvm_native_api::continuation_pin::transparent_natives;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

const TL_GET_THREAD: &str = "(Ljava/lang/Thread;)Ljava/lang/Object;";

fn gci_i18vp_observe(
    _ctx: &mut MockNativeContext,
    class: &str,
    name: &str,
    desc: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    (class == "java/lang/ThreadLocal" && name == "get" && desc == TL_GET_THREAD)
        .then(|| Ok(Some(Value::Int(transparent_natives() as i32))))
}

fn gci_i18vp_yield(
    _ctx: &mut MockNativeContext,
    class: &str,
    _name: &str,
    desc: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    if class != "java/lang/ThreadLocal" || desc != TL_GET_THREAD {
        return None;
    }
    assert_eq!(transparent_natives(), 1);
    Some(Err(MethodCallFailed::InternalError(
        cratonvm_types::error::VmError::ContinuationYield {
            wake_after_nanos: 0,
        },
    )))
}

/// No layout known (a mock without the JDK classes): every `get()` takes
/// the body, under exactly one transparent frame, answer unchanged.
#[test]
fn gci_i18vp_the_get_decline_is_a_transparent_tail_call() {
    let mut ctx = MockNativeContext::new();
    let tl = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
    let args = [Value::Object(Some(tl))];
    ctx.set_invoke_hook(gci_i18vp_observe);
    assert_eq!(native_tl_real_get(&mut ctx, &args).ok(), Some(Some(Value::Int(1))));
    assert_eq!(transparent_natives(), 0, "withdrawn after the call");
    ctx.set_invoke_hook(gci_i18vp_yield);
    assert!(matches!(
        native_tl_real_get(&mut ctx, &args),
        Err(MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::ContinuationYield { .. }
        ))
    ));
    assert_eq!(transparent_natives(), 0, "withdrawn on the yield too");
}
