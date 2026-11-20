use super::*;

/// t1 wave 6 (lane V5): the object-observer record withdraws an in-flight
/// body (publication) but never an installed one (a superseded callee);
/// every other record judges both the same.
#[test]
fn t1v5_observer_gain_spares_installed_bodies_only() {
    let cache = JitCache::new();
    let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
    buf.emit(&[0xC3]); // RET
    let mut cm = CompiledMethod::new(buf);
    cm.inlined_methods = vec![("t1v5/A".into(), "m".into(), "()V".into())];
    let key = JitKey {
        class_name: Arc::from("t1v5/B"),
        method_name: Arc::from("n"),
        descriptor: Arc::from("()V"),
        declaring_class_id: cratonvm_types::ClassId::new(4245),
    };
    let gain = InvalidationRecord::ObjectObserverGained;
    assert!(gain.withdraws_in(&cache, &key, &cm), "in flight: refused");
    assert!(!gain.withdraws_installed_in(&cache, &key, &cm), "installed: stays");
    let change = InvalidationRecord::InlinedClass(Arc::from("t1v5/A"));
    assert!(change.withdraws_in(&cache, &key, &cm));
    assert!(change.withdraws_installed_in(&cache, &key, &cm));
}

/// t1 wave 7 (lane NBA, page `t1-V6-osr-memo-recheck-...`): the optimizing
/// OSR memo's re-check of an installed body uses the installed rule; the
/// first (publication) check keeps refusing a body in flight at the gain.
#[test]
fn t1nba_the_installed_osr_recheck_survives_an_observer_gain() {
    let cache = JitCache::new();
    let mut buf = ExecutableBuffer::new(64).expect("alloc failed");
    buf.emit(&[0xC3]); // RET
    let mut cm = CompiledMethod::new(buf);
    cm.inlined_methods = vec![("t1nba/A".into(), "m".into(), "()V".into())];
    let id = cratonvm_types::ClassId::new(4246);
    let before = jit_invalidation_epoch();
    cache.log_invalidation(InvalidationRecord::ObjectObserverGained);
    assert!(
        !cache.unpublished_body_dependencies_are_current("t1nba/B", "n", "()V", id, &cm, before),
        "in flight at the gain: refused"
    );
    assert!(
        cache.installed_body_dependencies_are_current("t1nba/B", "n", "()V", id, &cm, before),
        "installed before the gain: kept"
    );
    let mid = jit_invalidation_epoch();
    cache.log_invalidation(InvalidationRecord::InlinedClass(Arc::from("t1nba/A")));
    assert!(
        !cache.installed_body_dependencies_are_current("t1nba/B", "n", "()V", id, &cm, mid),
        "a hierarchy change still withdraws an installed body"
    );
}
