use super::*;
use crate::test_utils::MockNativeContext;
use cratonvm_native_api::continuation_pin::transparent_natives;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

const AES: &str = "java/util/concurrent/AbstractExecutorService";
const ES: &str = "java/util/concurrent/ExecutorService";

/// The body answers the transparent-frame count it ran under.
fn gci_i18vp_observe(
    _ctx: &mut MockNativeContext,
    class: &str,
    _name: &str,
    _desc: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    (class == AES).then(|| Ok(Some(Value::Int(transparent_natives() as i32))))
}

/// The body yields (a park that unmounts).
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
    assert_eq!(transparent_natives(), 1, "the bridge's frame is transparent for the body");
    Some(Err(cratonvm_types::error::MethodCallFailed::InternalError(
        cratonvm_types::error::VmError::ContinuationYield {
            wake_after_nanos: 0,
        },
    )))
}

/// `invokeAny(Collection)` and `submit(Runnable, T)` on a real executor
/// (any class other than the synthetic placeholders, `executor_is_real`):
/// one transparent frame for the body, the answer and a yield handed back
/// unchanged, the declaration withdrawn after.
#[test]
fn gci_i18vp_a_real_executors_body_is_a_transparent_tail_call() {
    let mut registry = NativeMethodRegistry::new();
    register_executor_natives(&mut registry);
    let rows = [
        ("invokeAny", "(Ljava/util/Collection;)Ljava/lang/Object;"),
        (
            "submit",
            "(Ljava/lang/Runnable;Ljava/lang/Object;)Ljava/util/concurrent/Future;",
        ),
    ];
    for (name, desc) in rows {
        let f = registry
            .find(ES, name, desc)
            .unwrap_or_else(|| panic!("ExecutorService.{name}{desc} registered"));
        let mut ctx = MockNativeContext::new();
        let pool = ctx.ensure_class_initialized("p/GciI18vpRealPool").unwrap();
        let executor = ctx.alloc_object(pool, 2);
        let arg = ctx.alloc_object(ClassId::new(0), 1);
        let args = [
            Value::Object(Some(executor)),
            Value::Object(Some(arg)),
            Value::Object(None),
        ];
        ctx.set_invoke_hook(gci_i18vp_observe);
        assert_eq!(f(&mut ctx, &args).ok(), Some(Some(Value::Int(1))), "{name}");
        assert_eq!(transparent_natives(), 0, "{name}: withdrawn after the call");
        ctx.set_invoke_hook(gci_i18vp_yield);
        assert!(
            matches!(
                f(&mut ctx, &args),
                Err(cratonvm_types::error::MethodCallFailed::InternalError(
                    cratonvm_types::error::VmError::ContinuationYield { .. }
                ))
            ),
            "{name}: the yield reaches the interpreter unchanged"
        );
        assert_eq!(transparent_natives(), 0, "{name}: withdrawn on the yield too");
    }
}
