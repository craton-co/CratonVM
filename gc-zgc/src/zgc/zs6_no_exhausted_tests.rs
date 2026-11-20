//! zs s6/no: the opt-in exhausted verdict (`CRATONVM_ZGC_EXHAUSTED_FREE_FLOOR`).
//! (zt t12/rt: the fitted near-full floor's switch was retired.)
use super::*;

const MIB: usize = 1024 * 1024;

struct NoMonitors;
impl MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _: &cratonvm_types::PointerMap) {}
}

fn collect(heap: &ZgcRealHeap, roots: &mut Vec<ObjectRef>) {
    // SAFETY: single-threaded unit test.
    let stw = unsafe { StopTheWorldToken::new() };
    heap.collect_garbage(&stw, roots, &NoMonitors);
}

/// A 64 MiB heap whose live set (32 KiB arrays held by `roots`) is what
/// one collection left: at least `live_bytes`, with the warm-up, budget
/// and pause-target clauses out of the way.
fn heap_with_live(live_bytes: usize, floor: usize) -> (ZgcRealHeap, Vec<ObjectRef>) {
    let heap = ZgcRealHeap::with_capacity(64 * MIB);
    heap.counters.warmup_enabled.store(false, Ordering::Relaxed);
    heap.alloc_trigger_bytes.store(0, Ordering::Relaxed);
    heap.set_pause_target_ms(0);
    heap.counters
        .exhausted_free_floor
        .store(floor, Ordering::Relaxed);
    let mut roots = Vec::new();
    while heap.allocated.load(Ordering::Relaxed) < live_bytes {
        roots.push(heap.alloc_array(ClassId::new(0), ArrayElementType::Byte, 32 * 1024));
    }
    collect(&heap, &mut roots);
    (heap, roots)
}

fn small(heap: &ZgcRealHeap) -> Option<ObjectRef> {
    heap.try_alloc_array(ClassId::new(0), ArrayElementType::Int, 4)
}

#[test]
fn zs6no_an_exhausted_refusal_refuses_this_threads_attempts_until_the_next_collection() {
    let page = director::EXHAUSTED_FREE_FLOOR_HOTSPOT_KIB * 1024;
    let (heap, mut roots) = heap_with_live(64 * MIB - 3 * MIB / 2, page);
    let free = heap.heap_capacity() - heap.cycle_alloc_watermark.load(Ordering::Relaxed);
    assert!(free < page, "fixture: {free} bytes free");
    assert!(small(&heap).is_some(), "nothing armed yet");
    assert!(heap.arm_exhausted_verdict());
    assert!(
        small(&heap).is_none(),
        "the armed thread's attempt is refused"
    );
    assert!(small(&heap).is_none(), "until the next collection");
    // Another thread never reads this thread's verdict.
    std::thread::scope(|s| {
        let other = s.spawn(|| small(&heap).is_some()).join().unwrap();
        assert!(other, "another thread's attempt is served");
    });
    // A collection lapses it.
    collect(&heap, &mut roots);
    assert!(small(&heap).is_some(), "a collection since the verdict");
    // The infallible entry never reads it.
    assert!(heap.arm_exhausted_verdict());
    let _ = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 4);
    // The ladder's OOME build lifts it ...
    heap.lift_exhausted_verdict();
    assert!(small(&heap).is_some());
    // ... and spends it (zs s9/no2): no second verdict until a collection
    // leaves at least the floor free, so the rest of the fill runs the
    // default ladder down to object granularity.
    assert!(!heap.arm_exhausted_verdict(), "spent");
    assert!(small(&heap).is_some());
    roots.truncate(roots.len() / 2);
    collect(&heap, &mut roots);
    assert!(!heap.arm_exhausted_verdict(), "room: nothing to arm");
    let (armed, refused) = heap.exhausted_verdict_stats();
    assert_eq!((armed, refused), (2, 2));
    assert_eq!(heap.exhausted_verdict_spent_count(), 1);
    // Recovered: refill to under the floor and it arms again.
    while heap.allocated.load(Ordering::Relaxed) < 64 * MIB - 3 * MIB / 2 {
        roots.push(heap.alloc_array(ClassId::new(0), ArrayElementType::Byte, 32 * 1024));
    }
    collect(&heap, &mut roots);
    assert!(heap.arm_exhausted_verdict(), "re-armed after the recovery");
    heap.lift_exhausted_verdict();
}

/// zt t2/al (t1/sp finding 9): a verdict refusal records its request in
/// the refusal record, sized as `alloc_raw` would have been asked for it,
/// at the current collection count -- so `note_final_refusal` and
/// `room_for_last_refusal` read THIS refusal. The kill arm
/// (`CRATONVM_ZGC_VERDICT_RECORDS_REFUSAL=0`) leaves the record alone,
/// and with the verdict off (the default) nothing records either.
#[test]
fn zt2al_a_verdict_refusal_records_its_request() {
    let page = director::EXHAUSTED_FREE_FLOOR_HOTSPOT_KIB * 1024;
    for records in [true, false] {
        let (heap, _roots) = heap_with_live(64 * MIB - 3 * MIB / 2, page);
        heap.counters
            .verdict_records_refusal
            .store(records, Ordering::Relaxed);
        let before = (
            heap.counters.last_refusal_size.load(Ordering::Relaxed),
            heap.counters.last_refusal_epoch.load(Ordering::Relaxed),
        );
        assert!(heap.arm_exhausted_verdict());
        assert!(small(&heap).is_none(), "the verdict refuses");
        let after = (
            heap.counters.last_refusal_size.load(Ordering::Relaxed),
            heap.counters.last_refusal_epoch.load(Ordering::Relaxed),
        );
        if records {
            // `int[4]`: the array header plus 16 bytes of data.
            let want =
                ARRAY_DATA_OFFSET + array_data_size(4, ArrayElementType::Int).expect("int[4]");
            assert_eq!(after, (want, heap.gc_count.load(Ordering::Relaxed)));
        } else {
            assert_eq!(after, before, "the kill arm records nothing");
        }
        heap.lift_exhausted_verdict();
    }
    // The positive control's negative: the verdict off records nothing.
    let (heap, _roots) = heap_with_live(64 * MIB - 3 * MIB / 2, 0);
    let before = heap.counters.last_refusal_size.load(Ordering::Relaxed);
    assert!(!heap.arm_exhausted_verdict());
    assert!(small(&heap).is_some());
    assert_eq!(
        heap.counters.last_refusal_size.load(Ordering::Relaxed),
        before
    );
}

/// zt t2/rv (zs10rv7 item 4): a VM-raised error that is not the heap
/// refusal DROPS the verdict (its throwable allocates) without spending
/// it; the heap refusal's lift is the positive control that spends.
#[test]
fn zt2rv_a_non_heap_oome_drops_the_verdict_without_spending_it() {
    let page = director::EXHAUSTED_FREE_FLOOR_HOTSPOT_KIB * 1024;
    let (heap, _roots) = heap_with_live(64 * MIB - 3 * MIB / 2, page);
    assert!(heap.arm_exhausted_verdict());
    assert!(small(&heap).is_none(), "armed: refused");
    heap.drop_exhausted_verdict();
    assert!(small(&heap).is_some(), "dropped: the throwable allocates");
    assert_eq!(heap.exhausted_verdict_spent_count(), 0, "not spent");
    assert!(heap.arm_exhausted_verdict(), "still free to arm again");
    heap.lift_exhausted_verdict();
    assert_eq!(heap.exhausted_verdict_spent_count(), 1, "the lift spends");
    assert!(!heap.arm_exhausted_verdict(), "spent");
}

/// zs s9/no2 (rv6 X1): the verdict key is a per-heap serial. Two live
/// heaps get different keys, and a verdict armed on heap A is not read
/// on heap B, even at the same `gc_count` (the address-reuse case itself
/// cannot be forced portably; equal epochs are its precondition).
#[test]
fn zs9no2_a_verdict_armed_on_one_heap_is_not_read_on_another() {
    let page = director::EXHAUSTED_FREE_FLOOR_HOTSPOT_KIB * 1024;
    let (a, _ra) = heap_with_live(64 * MIB - 3 * MIB / 2, page);
    let (b, _rb) = heap_with_live(64 * MIB - 3 * MIB / 2, page);
    assert_ne!(a.exhausted_verdict_key(), b.exhausted_verdict_key());
    assert_ne!(a.exhausted_verdict_key(), 0);
    assert_eq!(
        a.gc_count.load(Ordering::Relaxed),
        b.gc_count.load(Ordering::Relaxed),
        "fixture: equal epochs"
    );
    assert!(a.arm_exhausted_verdict());
    assert!(small(&a).is_none(), "A reads its own verdict");
    assert!(small(&b).is_some(), "B never reads A's verdict");
    assert_eq!(b.exhausted_verdict_stats().1, 0);
    // Lifting on B leaves A's verdict alone; and B's spent state is its own.
    b.lift_exhausted_verdict();
    assert!(small(&a).is_none());
    assert_eq!(b.exhausted_verdict_spent_count(), 0);
    a.lift_exhausted_verdict();
    assert_eq!(a.exhausted_verdict_spent_count(), 1);
    assert!(
        b.arm_exhausted_verdict(),
        "A's spent verdict does not spend B's"
    );
    b.lift_exhausted_verdict();
}

#[test]
fn zs6no_a_collection_that_leaves_a_page_free_arms_nothing() {
    let page = director::EXHAUSTED_FREE_FLOOR_HOTSPOT_KIB * 1024;
    let (heap, _roots) = heap_with_live(52 * MIB, page);
    assert!(!heap.arm_exhausted_verdict());
    assert!(small(&heap).is_some());
    assert_eq!(heap.exhausted_verdict_stats(), (0, 0));
}

#[test]
fn zs6no_the_verdict_is_off_by_default() {
    let fresh = ZgcRealHeap::with_capacity(64 * MIB);
    assert_eq!(
        fresh.counters.exhausted_free_floor.load(Ordering::Relaxed),
        0,
        "unset is off"
    );
    let (heap, _roots) = heap_with_live(64 * MIB - 3 * MIB / 2, 0);
    assert!(!heap.arm_exhausted_verdict());
    assert!(small(&heap).is_some());
}
