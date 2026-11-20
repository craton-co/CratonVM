use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn gci_i27vt_a_non_positive_park_nanos_parks_not_even_when_interrupted() {
    use LockSupportEntry::*;
    assert_eq!(lock_support_relative_entry(0, true), NoPark);
    assert_eq!(lock_support_relative_entry(-5, true), NoPark);
    assert_eq!(lock_support_relative_entry(0, false), NoPark);
    assert_eq!(lock_support_relative_entry(i64::MIN, false), NoPark);
    assert_eq!(lock_support_relative_entry(1, true), AtOnce);
    assert_eq!(lock_support_relative_entry(1, false), Park);
}

#[test]
fn gci_i27vt_park_until_always_reaches_the_park() {
    use LockSupportEntry::*;
    assert_eq!(lock_support_absolute_entry(0, 0, false), AtOnce);
    assert_eq!(lock_support_absolute_entry(-1, 0, false), AtOnce);
    assert_eq!(lock_support_absolute_entry(1_000, 0, false), AtOnce, "elapsed");
    assert_eq!(lock_support_absolute_entry(1_000, 5, true), AtOnce, "interrupted");
    assert_eq!(lock_support_absolute_entry(1_000, 5, false), Park);
}

/// The mock's current thread, with `parkBlocker` declared at slot 1.
fn thread_with_park_blocker(ctx: &mut MockNativeContext) -> ObjectRef {
    let thread = ctx.stable_current_thread();
    let cid = ctx.class_id_of_object(thread);
    ctx.set_declared_fields(
        cid,
        vec![cratonvm_native_api::FieldMetadata {
            name: "parkBlocker".to_string(),
            descriptor: "Ljava/lang/Object;".to_string(),
            access_flags: 0,
            slot_index: 1,
            declaring_class_id: cid,
            is_static: false,
        }],
    );
    thread
}

/// Observed through `parkBlocker`: an interrupted `parkNanos(b, 0)` never
/// reaches the park (the field keeps its sentinel); an interrupted
/// `parkNanos(b, 5)` and an uninterrupted `parkUntil(b, 0)` do (the
/// field is set and cleared around the at-once park).
#[test]
fn gci_i27vt_the_stand_ins_reach_the_park_in_the_jdk_order() {
    let mut ctx = MockNativeContext::new();
    let thread = thread_with_park_blocker(&mut ctx);
    let blocker = ctx.alloc_object(ClassId::new(0), 1);
    let sentinel = ctx.alloc_object(ClassId::new(0), 1);

    ctx.set_interrupted(true);
    ctx.set_field(thread, 1, Value::Object(Some(sentinel)));
    native_lock_support_park_nanos(&mut ctx, &[Value::Object(Some(blocker)), Value::Long(0)])
        .unwrap();
    assert_eq!(
        ctx.get_field(thread, 1),
        Value::Object(Some(sentinel)),
        "parkNanos(b, 0): no park at all"
    );
    native_lock_support_park_nanos(&mut ctx, &[Value::Object(Some(blocker)), Value::Long(5)])
        .unwrap();
    assert_eq!(
        ctx.get_field(thread, 1),
        Value::Object(None),
        "interrupted parkNanos(b, 5): the at-once park, blocker set and cleared"
    );
    assert!(ctx.is_interrupted(false), "the flag is not cleared");

    ctx.set_interrupted(false);
    ctx.set_field(thread, 1, Value::Object(Some(sentinel)));
    native_lock_support_park_until(&mut ctx, &[Value::Object(Some(blocker)), Value::Long(0)])
        .unwrap();
    assert_eq!(
        ctx.get_field(thread, 1),
        Value::Object(None),
        "parkUntil(b, 0): U.park(true, 0) runs"
    );
}
