use super::{set_thread_field, thread_field};
use crate::test_utils::mock_ctx;
use cratonvm_native_api::{FieldMetadata, NativeHeapAccess};
use cratonvm_types::{ClassId, Value};

fn field(name: &str, slot_index: usize, declaring: ClassId) -> FieldMetadata {
    FieldMetadata {
        name: name.to_string(),
        descriptor: "Ljava/lang/Object;".to_string(),
        access_flags: 0x0002,
        slot_index,
        declaring_class_id: declaring,
        is_static: false,
    }
}

/// `class P extends Thread { Object holder; Object target; }` on a JDK
/// 21+ `Thread` (a `holder`, no `target`). Failed before: the served
/// `Thread.<init>` wrote P's own `holder` / `target` by name, and the
/// getters read P's `holder`.
#[test]
fn a_subclass_field_of_the_same_name_is_neither_written_nor_read() {
    let mut ctx = mock_ctx();
    ctx.set_absent_field_answers_null(true);
    let thread = ctx.declare_loaded_class("java/lang/Thread");
    let sub = ctx.declare_loaded_class("probe/P");
    ctx.set_superclass(sub, thread);
    ctx.set_declared_fields(thread, vec![field("holder", 1, thread)]);
    ctx.set_declared_fields(sub, vec![field("holder", 5, sub), field("target", 6, sub)]);
    let p = ctx.alloc_object(sub, 8);
    let holder = ctx.alloc_object(thread, 8);
    set_thread_field(&ctx, p, "holder", Value::Object(Some(holder)));
    set_thread_field(&ctx, p, "target", Value::Object(Some(holder)));
    // `Thread.holder` got it; P's two fields kept their zeroed slots.
    assert_eq!(ctx.get_field(p, 1), Value::Object(Some(holder)));
    assert_eq!(ctx.get_field(p, 5), Value::Int(0));
    assert_eq!(ctx.get_field(p, 6), Value::Int(0));
    assert_eq!(thread_field(&ctx, p, "holder"), Value::Object(Some(holder)));
    // `Thread` declares no `target`: nothing, not P's.
    ctx.set_field(p, 6, Value::Object(Some(holder)));
    assert_eq!(thread_field(&ctx, p, "target"), Value::Object(None));
    // A `java.lang.Thread` itself: by name, as before.
    let t = ctx.alloc_object(thread, 8);
    set_thread_field(&ctx, t, "holder", Value::Object(Some(holder)));
    assert_eq!(ctx.get_field(t, 1), Value::Object(Some(holder)));
    assert_eq!(thread_field(&ctx, t, "holder"), Value::Object(Some(holder)));
}
