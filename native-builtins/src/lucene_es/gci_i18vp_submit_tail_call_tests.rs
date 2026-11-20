use super::*;
use crate::test_utils::MockNativeContext;
use cratonvm_native_api::continuation_pin::transparent_natives;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

const AES: &str = "java/util/concurrent/AbstractExecutorService";

fn gci_i18vp_observe(
    _ctx: &mut MockNativeContext,
    class: &str,
    name: &str,
    _desc: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    (class == AES && name == "submit")
        .then(|| Ok(Some(Value::Int(transparent_natives() as i32))))
}

fn gci_i18vp_yield(
    _ctx: &mut MockNativeContext,
    class: &str,
    _name: &str,
    _desc: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    if class != AES {
        return None;
    }
    assert_eq!(transparent_natives(), 1);
    Some(Err(cratonvm_types::error::MethodCallFailed::InternalError(
        cratonvm_types::error::VmError::ContinuationYield {
            wake_after_nanos: 0,
        },
    )))
}

#[test]
fn gci_i18vp_a_real_executors_submit_is_a_transparent_tail_call() {
    type Native = fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult;
    let natives: [(&str, Native); 2] = [
        ("runnable", native_es_submit_runnable),
        ("callable", native_es_submit_callable),
    ];
    for (label, f) in natives {
        let mut ctx = MockNativeContext::new();
        let pool = ctx.ensure_class_initialized("p/GciI18vpRealPool").unwrap();
        let executor = ctx.alloc_object(pool, 2);
        let task = ctx.alloc_object(ClassId::new(0), 1);
        let args = [Value::Object(Some(executor)), Value::Object(Some(task))];
        ctx.set_invoke_hook(gci_i18vp_observe);
        assert_eq!(f(&mut ctx, &args).ok(), Some(Some(Value::Int(1))), "{label}");
        assert_eq!(transparent_natives(), 0, "{label}: withdrawn after the call");
        ctx.set_invoke_hook(gci_i18vp_yield);
        assert!(
            matches!(
                f(&mut ctx, &args),
                Err(cratonvm_types::error::MethodCallFailed::InternalError(
                    cratonvm_types::error::VmError::ContinuationYield { .. }
                ))
            ),
            "{label}: the yield passes through unchanged"
        );
        assert_eq!(transparent_natives(), 0, "{label}: withdrawn on the yield too");
    }
}
