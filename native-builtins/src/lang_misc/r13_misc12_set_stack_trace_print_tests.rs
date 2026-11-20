use super::{native_throwable_set_stack_trace, throwable_frame_text};
use crate::test_utils::{mock_ctx, MockNativeContext};
#[allow(unused_imports)]
use cratonvm_native_api::{
    FieldMetadata, NativeClassAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess,
};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::{ClassId, ObjectRef, Value};

const BACKTRACE: usize = 0;
const STACK_TRACE: usize = 3;

/// `toString()` on a mock element answers the string in its slot 0.
fn hook(
    ctx: &mut MockNativeContext,
    receiver: ObjectRef,
    method: &str,
    _descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    (method == "toString").then(|| Ok(Some(ctx.get_field(receiver, 0))))
}

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

/// A `StackTraceElement[]` whose elements print as `texts`.
fn trace(ctx: &mut MockNativeContext, texts: &[&str]) -> ObjectRef {
    // A typed producer (`typed_array_producer_ratchet`).
    let arr = ctx.new_ref_array(ClassId::new(1), texts.len());
    for (i, text) in texts.iter().enumerate() {
        let s = ctx.create_string_uninterned(text);
        let elem = ctx.alloc_object(ClassId::new(1), 1);
        ctx.set_field(elem, 0, Value::Object(Some(s)));
        ctx.set_array_element(arr, i, Value::Object(Some(elem)));        }
    arr    }