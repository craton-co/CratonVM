//! Interpreter round i4 wave 95, lane L6 (`CRATONVM_VTHREAD_ONE_PUBLISH`,
//! `CRATONVM_VTHREAD_DEATH_SWEEP_BY_COUNT`): the one-publish registration,
//! mount and death of a JDK continuation thread, the kept dead count, and
//! the per-thread count of unstructured monitor enters.
use super::*;
use crate::threading::jvm_thread::ThreadSharedArcs;

fn mirror(addr: usize) -> ObjectRef {
    // SAFETY: never dereferenced; the registry keys and compares addresses.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn walked_dead(registry: &ThreadRegistry) -> usize {
    registry
        .threads
        .read()
        .values()
        .filter(|e| !e.alive.load(Ordering::Acquire))
        .count()
}

#[test]
fn a_prepared_registration_is_complete_in_one_publish() {
    let registry = ThreadRegistry::new();
    let tid = registry.next_thread_id();
    let arcs = ThreadSharedArcs::fresh();
    let park = arcs.park_state.clone();
    let flag = arcs.interrupted.clone();
    let vthread = mirror(0x1_0000);
    registry.register_prepared_continuation_thread(tid, "vt", vthread, 77, arcs, 0x40, 0x80);
    assert!(registry.is_alive(tid));
    assert!(registry.is_daemon(tid));
    assert_eq!(registry.java_thread_obj(tid), Some(vthread));
    assert_eq!(registry.find_thread_id_by_java_tid(77), Some(tid));
    assert!(Arc::ptr_eq(&registry.get_park_state(tid).unwrap(), &park));
    assert!(Arc::ptr_eq(&registry.get_interrupted_flag(tid).unwrap(), &flag));
    assert!(Arc::ptr_eq(
        &registry.find_park_state_by_thread_obj(vthread).unwrap(),
        &park
    ));
    assert_eq!(registry.own_jvm_thread_addr(tid), Some(0x80));
    // Not STW-ready until its first slice marks it.
    let threads = registry.threads.read();
    let entry = threads.get(&tid).unwrap();
    assert!(!entry.stw_ready.load(Ordering::Acquire));
    assert_eq!(entry.tlab_addr.load(Ordering::Acquire), 0x40);
}

#[test]
fn a_mount_publishes_and_answers_the_mirror() {
    let registry = ThreadRegistry::new();
    let tid = registry.next_thread_id();
    let vthread = mirror(0x2_0000);
    registry.register_prepared_continuation_thread(
        tid,
        "vt",
        vthread,
        0,
        ThreadSharedArcs::fresh(),
        0,
        0,
    );
    registry.set_mounted(tid, false);
    assert_eq!(
        registry.mount_continuation_thread(tid, 0x100, 0x200),
        Some(Some(vthread))
    );
    assert!(registry.is_mounted(tid));
    assert_eq!(registry.own_jvm_thread_addr(tid), Some(0x100));
    assert_eq!(registry.mount_continuation_thread(ThreadId(9999), 1, 2), None);
}

#[test]
fn the_kept_dead_count_matches_the_walk_and_bounds_the_tail() {
    let registry = ThreadRegistry::new();
    let slack = 64;
    let total = TERMINATED_THREAD_ENTRY_CAP + 3 * slack;
    for i in 0..total {
        let tid = registry.next_thread_id();
        registry.register_prepared_continuation_thread(
            tid,
            "vt",
            mirror(0x10_0000 + i * 0x100),
            1000 + i as u64,
            ThreadSharedArcs::fresh(),
            0,
            0,
        );
        let retired = registry.retire_continuation_thread(tid).unwrap();
        assert!(!registry.is_alive(tid));
        assert_eq!(registry.own_jvm_thread_addr(tid), None);
        registry.finish_retired_continuation_thread(tid, &retired, slack);
        let dead = registry.dead_entries.load(Ordering::Relaxed);
        assert_eq!(dead, walked_dead(&registry));
        assert!(dead <= TERMINATED_THREAD_ENTRY_CAP + slack, "{dead}");
        // A second death of the same id (the newest, never purged) is not
        // counted again.
        assert!(registry.retire_continuation_thread(tid).is_some());
        assert_eq!(
            registry.dead_entries.load(Ordering::Relaxed),
            walked_dead(&registry)
        );
    }
    // The oldest ids were purged, with their `tid` index entries.
    assert_eq!(registry.find_thread_id_by_java_tid(1000), None);
    assert!(registry.find_thread_id_by_java_tid(1000 + total as u64 - 1).is_some());
    // The default death keeps the same count.
    let platform = registry.next_thread_id();
    registry.register(platform, "p", None);
    registry.mark_dead(platform);
    assert_eq!(
        registry.dead_entries.load(Ordering::Relaxed),
        walked_dead(&registry)
    );
    assert_eq!(registry.count() - registry.alive_count(), walked_dead(&registry));
}

/// Wave 96 (lane L6): a collection that moves the mirror between the
/// two halves of a death re-keys the park index to the new address; the
/// second half used to look the entry up by the old address only and
/// leave the dead state in the index. The default death (`mark_dead`)
/// had the same window.
#[test]
fn a_death_that_races_a_mirror_move_leaves_no_park_index_entry() {
    // The index itself: `find_park_state_by_thread_obj` falls back to a
    // walk of the entries, dead ones included, by design.
    fn indexed(registry: &ThreadRegistry, obj: ObjectRef) -> bool {
        registry
            .thread_obj_to_park
            .lock()
            .contains_key(&(obj.as_ptr() as usize))
    }
    let registry = ThreadRegistry::new();
    let tid = registry.next_thread_id();
    let old = mirror(0x30_0000);
    let new = mirror(0x31_0000);
    registry.register_prepared_continuation_thread(
        tid,
        "vt",
        old,
        0,
        ThreadSharedArcs::fresh(),
        0,
        0,
    );
    let retired = registry.retire_continuation_thread(tid).unwrap();
    let mut pointer_map = cratonvm_types::PointerMap::default();
    pointer_map.insert(0x30_0000usize, 0x31_0000usize);
    registry.update_thread_objs_after_gc(&pointer_map);
    assert!(indexed(&registry, new));
    registry.finish_retired_continuation_thread(tid, &retired, 64);
    assert!(!indexed(&registry, new));
    assert!(!indexed(&registry, old));

    // `mark_dead` with the stale address read before the move: the
    // registry's own read is under its lock, so drive the helper the way
    // `mark_dead` calls it after a re-key.
    let platform = registry.next_thread_id();
    let p_old = mirror(0x40_0000);
    let p_new = mirror(0x41_0000);
    registry.register(platform, "p", Some(p_old));
    let state = registry.get_park_state(platform).unwrap();
    let mut pointer_map = cratonvm_types::PointerMap::default();
    pointer_map.insert(0x40_0000usize, 0x41_0000usize);
    registry.update_thread_objs_after_gc(&pointer_map);
    assert!(indexed(&registry, p_new));
    registry.drop_dead_park_index_entry(p_old, &state, true);
    assert!(!indexed(&registry, p_new));
    // A later death of the same thread (a joiner's) does not search, and
    // a newer thread's state at the address is never dropped.
    let other = registry.next_thread_id();
    registry.register(other, "o", Some(p_new));
    registry.drop_dead_park_index_entry(p_old, &state, false);
    registry.drop_dead_park_index_entry(p_new, &state, true);
    assert!(indexed(&registry, p_new));
}

#[test]
fn unstructured_monitor_enters_are_counted_per_thread() {
    let registry = ThreadRegistry::new();
    let tid = registry.next_thread_id();
    registry.register(tid, "t", None);
    assert_eq!(registry.unstructured_monitor_enters_of(tid), 0);
    registry.note_unstructured_monitor_enter(tid);
    registry.note_unstructured_monitor_enter(tid);
    registry.note_unstructured_monitor_exit(tid);
    assert_eq!(registry.unstructured_monitor_enters_of(tid), 1);
    registry.note_unstructured_monitor_exit(tid);
    // Never below zero.
    registry.note_unstructured_monitor_exit(tid);
    assert_eq!(registry.unstructured_monitor_enters_of(tid), 0);
    // An unknown id never reads "holds nothing".
    assert_eq!(registry.unstructured_monitor_enters_of(ThreadId(4242)), u32::MAX);
}
