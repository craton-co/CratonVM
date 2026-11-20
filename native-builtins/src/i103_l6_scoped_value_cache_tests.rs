use super::*;
use cratonvm_native_api::{FieldMetadata, NativeClassAccess as _, NativeHeapAccess as _};

/// A mock whose current thread is one stable object with a `tid` field
/// (slot 0) holding `tid`.
fn ctx_with_tid(vm: usize, tid: i64) -> crate::test_utils::MockNativeContext {
    let mut c = crate::test_utils::mock_ctx();
    c.set_vm_identity(vm);
    let thread = c.stable_current_thread();
    let class = c.class_id_of_object(thread);
    c.set_declared_fields(
        class,
        vec![FieldMetadata {
            name: "tid".to_string(),
            descriptor: "J".to_string(),
            access_flags: 0,
            slot_index: 0,
            declaring_class_id: class,
            is_static: false,
        }],
    );
    c.set_field(thread, 0, Value::Long(tid));
    c
}

/// The ending thread's entry and global root go; another thread's stay;
/// a second call is a no-op.
#[test]
fn a_thread_end_drops_its_scoped_value_cache_root_only() {
    const VM: usize = 0xB29_3103;
    let mut c = ctx_with_tid(VM, 4103);
    let thread = c.stable_current_thread();
    assert_eq!(
        c.get_field_by_name(thread, "tid"),
        Value::Long(4103),
        "premise: the mock thread answers its tid"
    );
    let mine = c.alloc_object(ClassId::new(0), 1);
    let other = c.alloc_object(ClassId::new(0), 1);
    let before = c.global_root_count();
    let h_mine = c.add_global_root(mine);
    let h_other = c.add_global_root(other);
    SCOPED_VALUE_CACHES.with(VM, |m| {
        m.insert(4103, h_mine);
        m.insert(9999, h_other);
    });
    assert_eq!(c.global_root_count(), before + 2, "premise");

    forget_current_thread_scoped_value_cache(&mut c);
    assert_eq!(c.global_root_count(), before + 1, "the dead thread's root stayed");
    assert!(c.resolve_global_root(h_mine).is_none());
    assert_eq!(c.resolve_global_root(h_other), Some(other), "another thread's root went");
    assert!(SCOPED_VALUE_CACHES.with(VM, |m| !m.contains_key(&4103) && m.contains_key(&9999)));

    forget_current_thread_scoped_value_cache(&mut c);
    assert_eq!(c.global_root_count(), before + 1, "not idempotent");
    SCOPED_VALUE_CACHES.with(VM, |m| m.clear());
}

/// The thread-end funnel every platform exit / detach / continuation
/// death runs reaches it.
#[test]
fn release_current_thread_locals_drops_the_scoped_value_cache() {
    const VM: usize = 0xB29_3104;
    std::thread::spawn(|| {
        let mut c = ctx_with_tid(VM, 4104);
        let cache = c.alloc_object(ClassId::new(0), 1);
        let before = c.global_root_count();
        let h = c.add_global_root(cache);
        SCOPED_VALUE_CACHES.with(VM, |m| {
            m.insert(4104, h);
        });
        crate::phases_early::release_current_thread_locals(&mut c);
        assert_eq!(c.global_root_count(), before, "the cache stayed a global root");
        assert!(SCOPED_VALUE_CACHES.with(VM, |m| m.is_empty()));
    })
    .join()
    .expect("test thread must not panic");
}
