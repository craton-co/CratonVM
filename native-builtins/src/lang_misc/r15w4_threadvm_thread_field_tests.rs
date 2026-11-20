use super::served_thread_run_task;
use crate::test_utils::mock_ctx;
use cratonvm_native_api::{FieldMetadata, NativeHeapAccess};
use cratonvm_types::{ClassId, Value};

fn field(name: &str, descriptor: &str, slot_index: usize, declaring: ClassId) -> FieldMetadata {
    FieldMetadata {
        name: name.to_string(),
        descriptor: descriptor.to_string(),
        access_flags: 0x0002,
        slot_index,
        declaring_class_id: declaring,
        is_static: false,
    }
}

/// `class N extends Thread { String name; long tid; }` on the real
/// layout (`Thread`: `tid` 1, `name` 2). Failed before: `getName()` read
/// N's own `name` field by name.
#[test]
fn get_name_reads_threads_own_name_not_a_subclass_field() {
    let mut ctx = mock_ctx();
    ctx.set_absent_field_answers_null(true);
    let thread = ctx.declare_loaded_class("java/lang/Thread");
    let sub = ctx.declare_loaded_class("probe/N");
    ctx.set_superclass(sub, thread);
    ctx.set_declared_fields(
        thread,
        vec![
            field("tid", "J", 1, thread),
            field("name", "Ljava/lang/String;", 2, thread),
        ],
    );
    ctx.set_declared_fields(
        sub,
        vec![
            field("name", "Ljava/lang/String;", 20, sub),
            field("tid", "J", 21, sub),
        ],
    );
    let n = ctx.alloc_object(sub, 24);
    let real = ctx.alloc_object(thread, 1);
    let decoy = ctx.alloc_object(thread, 1);
    ctx.set_field(n, 2, Value::Object(Some(real)));
    ctx.set_field(n, 20, Value::Object(Some(decoy)));
    let got = crate::lang_system::native_thread_get_name(&mut ctx, &[Value::Object(Some(n))])
        .expect("getName");
    assert_eq!(got, Some(Value::Object(Some(real))));
}

/// A synthetic-layout `Thread` (no `tid` of its own here) whose subclass
/// declares `long tid`: not the real layout. Failed before: the by-name
/// read found the subclass's `long`.
#[test]
fn a_subclass_long_tid_does_not_make_the_real_layout() {
    let mut ctx = mock_ctx();
    ctx.set_absent_field_answers_null(true);
    let thread = ctx.declare_loaded_class("java/lang/Thread");
    let sub = ctx.declare_loaded_class("probe/S");
    ctx.set_superclass(sub, thread);
    ctx.set_declared_fields(thread, vec![field("name", "Ljava/lang/String;", 0, thread)]);
    ctx.set_declared_fields(sub, vec![field("tid", "J", 6, sub)]);
    let s = ctx.alloc_object(sub, 8);
    ctx.set_field(s, 6, Value::Long(5));
    assert!(!crate::has_real_jdk_thread_layout(&ctx, s));
}

/// The served `Thread.run` of `class R extends Thread { Runnable target;
/// }` on a JDK 21+ `Thread` (`holder`, no `target`) runs `holder.task`.
#[test]
fn served_run_task_takes_holder_task_not_a_subclass_target() {
    let mut ctx = mock_ctx();
    ctx.set_absent_field_answers_null(true);
    let thread = ctx.declare_loaded_class("java/lang/Thread");
    let holder_class = ctx.declare_loaded_class("java/lang/Thread$FieldHolder");
    let sub = ctx.declare_loaded_class("probe/R");
    ctx.set_superclass(sub, thread);
    ctx.set_declared_fields(
        thread,
        vec![field("holder", "Ljava/lang/Thread$FieldHolder;", 5, thread)],
    );
    ctx.set_declared_fields(
        holder_class,
        vec![field("task", "Ljava/lang/Runnable;", 0, holder_class)],
    );
    ctx.set_declared_fields(sub, vec![field("target", "Ljava/lang/Runnable;", 9, sub)]);
    let r = ctx.alloc_object(sub, 12);
    let holder = ctx.alloc_object(holder_class, 2);
    let task = ctx.alloc_object(thread, 1);
    let decoy = ctx.alloc_object(thread, 1);
    ctx.set_field(holder, 0, Value::Object(Some(task)));
    ctx.set_field(r, 5, Value::Object(Some(holder)));
    ctx.set_field(r, 9, Value::Object(Some(decoy)));
    assert_eq!(
        served_thread_run_task(&mut ctx, r),
        Value::Object(Some(task))
    );
}
