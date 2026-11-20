use super::native_throwable_get_stack_trace_array;
use crate::test_utils::{mock_ctx, MockNativeContext};
#[allow(unused_imports)]
use cratonvm_native_api::{
    FieldMetadata, NativeClassAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess,
};
use cratonvm_types::{ClassId, ObjectRef, Value};

const BACKTRACE: usize = 0;
const STACK_TRACE: usize = 3;

/// A real-layout Throwable class; answers `(class, sentinel)`.
fn real_throwable(ctx: &mut MockNativeContext) -> (ClassId, ObjectRef) {
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
            field("backtrace", "Ljava/lang/Object;", BACKTRACE, false),
            field("detailMessage", "Ljava/lang/String;", 1, false),
            field("cause", "Ljava/lang/Throwable;", 2, false),
            field(
                "stackTrace",
                "[Ljava/lang/StackTraceElement;",
                STACK_TRACE,
                false,
            ),
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
    (tid, sentinel)
}

fn get(ctx: &mut MockNativeContext, t: ObjectRef) -> ObjectRef {
    match native_throwable_get_stack_trace_array(ctx, &[Value::Object(Some(t))]) {
        Ok(Some(Value::Object(Some(arr)))) => arr,
        other => panic!("getStackTrace answered {other:?}"),
    }
}

/// A set trace: off, the stored array itself (as before); on, a new
/// array holding the same elements every call (HotSpot's clone).
#[test]
fn a_set_trace_is_answered_as_a_copy_with_the_switch_on() {
    let mut ctx = mock_ctx();
    let (tid, _sentinel) = real_throwable(&mut ctx);
    let t = ctx.alloc_object(tid, 6);
    ctx.set_field(t, BACKTRACE, Value::Object(Some(t)));
    let element = ctx.alloc_object(ClassId::new(1), 1);
    let set = ctx.new_ref_array(ClassId::new(1), 1);
    ctx.set_array_element(set, 0, Value::Object(Some(element)));
    ctx.set_field(t, STACK_TRACE, Value::Object(Some(set)));

    ctx.compat_stack_trace_keeps_array_flag.set(false);
    assert_eq!(get(&mut ctx, t), set, "off: the stored array, as before");

    ctx.compat_stack_trace_keeps_array_flag.set(true);
    let a = get(&mut ctx, t);
    let b = get(&mut ctx, t);
    assert_ne!(a, set);
    assert_ne!(a, b);
    for arr in [a, b] {
        assert_eq!(ctx.array_length(arr), 1);
        assert_eq!(ctx.get_array_element(arr, 0), Value::Object(Some(element)));
    }
    assert_eq!(ctx.get_field(t, STACK_TRACE), Value::Object(Some(set)));
}

/// An unassigned trace (the sentinel, or `null` as the native
/// constructors leave it) with a `backtrace`: on, the built array is
/// stored in `stackTrace` and a copy answered; off, nothing is stored.
#[test]
fn an_unassigned_trace_keeps_the_built_array_with_the_switch_on() {
    let mut ctx = mock_ctx();
    let (tid, sentinel) = real_throwable(&mut ctx);
    for unassigned in [Some(sentinel), None] {
        let t = ctx.alloc_object(tid, 6);
        ctx.set_field(t, BACKTRACE, Value::Object(Some(t)));
        ctx.set_field(t, STACK_TRACE, Value::Object(unassigned));

        ctx.compat_stack_trace_keeps_array_flag.set(false);
        get(&mut ctx, t);
        assert_eq!(
            ctx.get_field(t, STACK_TRACE),
            Value::Object(unassigned),
            "off: nothing stored"
        );

        ctx.compat_stack_trace_keeps_array_flag.set(true);
        let first = get(&mut ctx, t);
        let Value::Object(Some(kept)) = ctx.get_field(t, STACK_TRACE) else {
            panic!("on: the built array is stored");
        };
        assert_ne!(Some(kept), unassigned);
        assert_ne!(first, kept, "a copy is answered, not the kept array");
        let second = get(&mut ctx, t);
        assert_ne!(second, kept);
        assert_ne!(second, first);
        assert_eq!(ctx.get_field(t, STACK_TRACE), Value::Object(Some(kept)));
    }
}

/// No `backtrace` (the JDK body fills only then): nothing is stored.
#[test]
fn a_throwable_without_a_backtrace_keeps_nothing() {
    let mut ctx = mock_ctx();
    let (tid, sentinel) = real_throwable(&mut ctx);
    ctx.compat_stack_trace_keeps_array_flag.set(true);
    let t = ctx.alloc_object(tid, 6);
    ctx.set_field(t, BACKTRACE, Value::Object(None));
    ctx.set_field(t, STACK_TRACE, Value::Object(Some(sentinel)));
    get(&mut ctx, t);
    assert_eq!(ctx.get_field(t, STACK_TRACE), Value::Object(Some(sentinel)));
}

/// Interpreter round i3 wave 82, lane L2: a read names what `backtrace`
/// holds -- the throwable itself (the registry marker), the original a
/// clone copied the marker from, or a throwable-owned holder -- and the
/// throwable itself when `backtrace` is `null`.
#[test]
fn a_trace_read_goes_through_the_backtrace_field() {
    let mut ctx = mock_ctx();
    let (tid, _sentinel) = real_throwable(&mut ctx);
    let original = ctx.alloc_object(tid, 6);
    ctx.set_field(original, BACKTRACE, Value::Object(Some(original)));
    assert_eq!(super::throwable_trace_source(&mut ctx, original), original);
    let clone = ctx.alloc_object(tid, 6);
    ctx.set_field(clone, BACKTRACE, Value::Object(Some(original)));
    assert_eq!(super::throwable_trace_source(&mut ctx, clone), original);
    let bare = ctx.alloc_object(tid, 6);
    ctx.set_field(bare, BACKTRACE, Value::Object(None));
    assert_eq!(super::throwable_trace_source(&mut ctx, bare), bare);
}
