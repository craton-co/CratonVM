use super::{collect_throwable_chain_lines, is_jdk_module_name, stack_frame_prefix};
use crate::test_utils::{mock_ctx, MockNativeContext};
#[allow(unused_imports)]
use cratonvm_native_api::{
    FieldMetadata, NativeClassAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess,
};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::{ClassId, ObjectRef, Value};

const CAUSE: usize = 2;

/// `getCause()` on a mock throwable answers its `cause` slot.
fn hook(
    ctx: &mut MockNativeContext,
    receiver: ObjectRef,
    method: &str,
    _descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    (method == "getCause").then(|| Ok(Some(ctx.get_field(receiver, CAUSE))))
}

/// A real-layout Throwable class.
fn throwable_class(ctx: &mut MockNativeContext) -> ClassId {
    let tid = ctx
        .ensure_class_initialized("java/lang/Throwable")
        .unwrap_or_else(|e| panic!("no Throwable: {e:?}"));
    let field = |name: &str, desc: &str, slot: usize, is_static: bool| FieldMetadata {
        name: name.to_string(),
        descriptor: desc.to_string(),
        access_flags: if is_static { 0x001A } else { 0x0002 },
        slot_index: slot,
        declaring_class_id: tid,
        is_static,
    };
    ctx.set_declared_fields(
        tid,
        vec![
            field("backtrace", "Ljava/lang/Object;", 0, false),
            field("detailMessage", "Ljava/lang/String;", 1, false),
            field("cause", "Ljava/lang/Throwable;", CAUSE, false),
            field("stackTrace", "[Ljava/lang/StackTraceElement;", 3, false),
            field("depth", "I", 4, false),
            field("suppressedExceptions", "Ljava/util/List;", 5, false),
            field(
                "UNASSIGNED_STACK",
                "[Ljava/lang/StackTraceElement;",
                0,
                true,
            ),
        ],
    );
    let sentinel = ctx.new_ref_array(tid, 0);
    ctx.set_static_field(tid, 0, Value::Object(Some(sentinel)));
    tid
}

/// A throwable wrapping `levels` more below it (the innermost has the
/// `cause == this` sentinel).
fn chain(ctx: &mut MockNativeContext, tid: ClassId, levels: usize) -> ObjectRef {
    let mut t = ctx.alloc_object(tid, 6);
    ctx.set_field(t, CAUSE, Value::Object(Some(t)));
    for _ in 0..levels {
        let outer = ctx.alloc_object(tid, 6);
        ctx.set_field(outer, CAUSE, Value::Object(Some(t)));
        t = outer;
    }
    t
}

fn caused_by_lines(lines: &[String]) -> usize {
    lines
        .iter()
        .filter(|l| l.starts_with("Caused by: "))
        .count()
}


/// A cause cycle still ends in HotSpot's `[CIRCULAR REFERENCE: ...]` line
/// under the loop.
#[test]
fn a_cause_cycle_is_named_once() {
    let mut ctx = mock_ctx();
    ctx.set_invoke_virtual_hook(hook);
    let tid = throwable_class(&mut ctx);
    let a = ctx.alloc_object(tid, 6);
    let b = ctx.alloc_object(tid, 6);
    ctx.set_field(a, CAUSE, Value::Object(Some(b)));
    ctx.set_field(b, CAUSE, Value::Object(Some(a)));
    let lines = collect_throwable_chain_lines(&mut ctx, a);
    assert_eq!(
        lines,
        vec![
            "java.lang.Throwable".to_string(),
            "Caused by: java.lang.Throwable".to_string(),
            "Caused by: [CIRCULAR REFERENCE: java.lang.Throwable]".to_string(),
        ]
    );
}

/// `StackTraceElement.toString()`'s prefix rules.
#[test]
fn the_frame_prefix_follows_stack_trace_element_to_string() {
    assert_eq!(stack_frame_prefix(None, None, None), "");
    assert_eq!(
        stack_frame_prefix(None, Some("java.base"), None),
        "java.base/"
    );
    assert_eq!(
        stack_frame_prefix(None, Some("com.foo"), Some("1.0")),
        "com.foo@1.0/"
    );
    assert_eq!(stack_frame_prefix(Some("mine"), None, None), "mine//");
    assert_eq!(
        stack_frame_prefix(Some("mine"), Some("m"), Some("2")),
        "mine/m@2/"
    );
    // A version without a module, and empty names, print nothing.
    assert_eq!(stack_frame_prefix(None, None, Some("1.0")), "");
    assert_eq!(stack_frame_prefix(Some(""), Some(""), None), "");
    assert!(is_jdk_module_name("java.base"));
    assert!(is_jdk_module_name("jdk.proxy1"));
    assert!(!is_jdk_module_name("com.foo"));
}
