use super::{ThrowableLayoutCache, UNRESOLVED_THROWABLE_FIELD_INDEX};
use std::sync::atomic::Ordering;

#[test]
fn throwable_layout_slots_are_not_shared_between_vms() {
    let first = ThrowableLayoutCache::new();
    let second = ThrowableLayoutCache::new();
    first.detail_message.store(7, Ordering::Relaxed);

    assert_eq!(first.detail_message.load(Ordering::Relaxed), 7);
    assert_eq!(
        second.detail_message.load(Ordering::Relaxed),
        UNRESOLVED_THROWABLE_FIELD_INDEX,
        "a second VM must not inherit the first VM's Throwable layout"
    );
}

/// Round 15 wave 4 (lane threadvm; TR4W3-1): the `Thread` layout memo
/// answers what it was told, per class, per VM, and refuses what it
/// cannot answer exactly.
#[test]
fn thread_layout_memo_answers_per_class_and_refuses_a_second_thread_class() {
    use super::ThreadLayoutCache;
    use cratonvm_types::ClassId;
    let cache = ThreadLayoutCache::new();
    let thread = ClassId::new(7);
    let sub = ClassId::new(40);
    // 40 + 16 shares 40's way: it evicts, never aliases.
    let rival = ClassId::new(56);
    assert_eq!(cache.thread_class_above(sub), None);
    cache.remember_thread_class_above(sub, Some(thread));
    assert_eq!(cache.thread_class_above(sub), Some(Some(thread)));
    assert_eq!(cache.thread_class_above(rival), None);
    cache.remember_thread_class_above(rival, None);
    assert_eq!(cache.thread_class_above(rival), Some(None));
    assert_eq!(cache.thread_class_above(sub), None);

    let mut resolved = 0;
    let mut resolve = |slot: Option<usize>| {
        resolved += 1;
        slot
    };
    assert_eq!(
        cache.declared_slot(thread, "holder", || resolve(Some(5))),
        Some(Some(5))
    );
    assert_eq!(
        cache.declared_slot(thread, "holder", || resolve(Some(99))),
        Some(Some(5))
    );
    assert_eq!(
        cache.declared_slot(thread, "target", || resolve(None)),
        Some(None)
    );
    assert_eq!(
        cache.declared_slot(thread, "target", || resolve(Some(1))),
        Some(None)
    );
    assert_eq!(resolved, 2);
    // Not a kept name, or another `java.lang.Thread` id: "no memo".
    assert_eq!(cache.declared_slot(thread, "priority", || Some(3)), None);
    assert_eq!(
        cache.declared_slot(ClassId::new(8), "holder", || Some(3)),
        None
    );
    // A second VM starts empty.
    let other = ThreadLayoutCache::new();
    assert_eq!(other.thread_class_above(sub), None);
    assert_eq!(
        other.declared_slot(ClassId::new(8), "holder", || Some(3)),
        Some(Some(3))
    );
}
