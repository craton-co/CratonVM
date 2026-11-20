use super::*;
use crate::config::VmConfig;
use std::sync::Arc;

#[test]
fn both_object_doors_make_the_same_object_on_every_layout_arm() {
    let shared = Arc::new(SharedVm::new(crate::VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "w8b-try-alloc");
    let mut ctx = NativeContextImpl {
        shared: &shared,
        thread: &mut thread,
    };
    // First: the fallible door on the SLOW layout arm (the per-VM
    // `AnonymousObject$5` cache is empty, so this substitutes and fills it).
    let slow = ctx
        .try_alloc_object(ClassId::new(0), 5)
        .expect("five slots fit a fresh heap");
    // Then both doors on the cache-hit arm.
    let infallible = ctx.alloc_object(ClassId::new(0), 5);
    let cached = ctx
        .try_alloc_object(ClassId::new(0), 5)
        .expect("five slots fit a fresh heap");
    let class_of = |o: ObjectRef| shared.mem.heap.class_id_of(o);
    assert_ne!(
        class_of(slow),
        ClassId::new(0),
        "the ClassId(0) request with slots must be substituted, as alloc_object does"
    );
    assert_eq!(class_of(slow), class_of(infallible));
    assert_eq!(class_of(cached), class_of(infallible));
    for obj in [slow, infallible, cached] {
        assert_eq!(ctx.object_num_fields(obj), 5);
    }
    // A plain field-less Object is not substituted by either door.
    let plain = ctx.try_alloc_object(ClassId::new(0), 0).expect("fits");
    assert_eq!(class_of(plain), ClassId::new(0));
}
