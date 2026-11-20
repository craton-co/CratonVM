use super::*;

fn body_inlining(classes: &[&str]) -> CompiledMethod {
    let buf = ExecutableBuffer::new(64).expect("executable buffer");
    let mut compiled = CompiledMethod::new(buf);
    for class in classes {
        compiled
            .inlined_methods
            .push((class.to_string(), "m".to_string(), "()V".to_string()));
    }
    compiled
}

/// One bucket per (class, body) pair at publication; a class change
/// checks each dependency on that class; the withdrawn body's buckets go
/// stale, and deallocate once nothing owns the body.
#[test]
fn a_withdrawn_body_stales_then_frees_one_bucket_per_class() {
    let cache = JitCache::new();
    let (class, method, desc): (Arc<str>, Arc<str>, Arc<str>) =
        (Arc::from("app/Caller"), Arc::from("hot"), Arc::from("()V"));
    let cid = cratonvm_types::ClassId::new(4343);
    cache.put(
        Arc::clone(&class),
        Arc::clone(&method),
        Arc::clone(&desc),
        cid,
        body_inlining(&["app/Shape", "app/Shape", "app/Other"]),
    );
    let v = cache.dependency_counter_values();
    assert_eq!(v.buckets_allocated, 2, "{v:?}");
    assert_eq!((v.buckets_stale, v.buckets_deallocated), (0, 0), "{v:?}");
    let held = cache.get(&class, &method, &desc, cid).expect("published");
    assert_eq!(cache.invalidate_for_class_change("app/Shape"), 1);
    let v = cache.dependency_counter_values();
    assert_eq!(v.find_witness_in, 2, "two dependencies on the class: {v:?}");
    assert_eq!(
        (
            v.buckets_stale,
            v.buckets_stale_accumulated,
            v.buckets_deallocated
        ),
        (2, 2, 0),
        "held by this test: {v:?}"
    );
    drop(held);
    let _ = reclaim_retired_jit_code();
    let v = cache.dependency_counter_values();
    assert_eq!(v.buckets_stale_accumulated, 2, "{v:?}");
    assert_eq!(v.buckets_stale + v.buckets_deallocated, 2, "{v:?}");
    // A change of a class nobody inlined checks nothing.
    cache.invalidate_for_class_change("app/Unrelated");
    assert_eq!(cache.dependency_counter_values().find_witness_in, 2);
}

/// A body that inlined nothing has no bucket.
#[test]
fn a_body_that_inlined_nothing_allocates_no_bucket() {
    let cache = JitCache::new();
    cache.put(
        Arc::from("app/Leaf"),
        Arc::from("run"),
        Arc::from("()V"),
        cratonvm_types::ClassId::new(4344),
        body_inlining(&[]),
    );
    assert_eq!(
        cache.dependency_counter_values(),
        DependencyCounterValues::default()
    );
}
