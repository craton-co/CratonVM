use crate::collector::GarbageCollector;
use cratonvm_types::*;
use cratonvm_gc_core::MonitorCleanup;
use std::sync::atomic::Ordering;
use crate::g1::*;

struct NoMonitors;
impl MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

fn stw() -> cratonvm_gc_core::collector::StopTheWorldToken {
    // SAFETY: these tests run the heap single-threaded; no mutator runs.
    unsafe { cratonvm_gc_core::collector::StopTheWorldToken::new() }
}

/// 8 MiB, fully committed, 1 MiB regions, promotion after one survival.
fn config() -> G1CollectorConfig {
    G1CollectorConfig {
        heap_size: 8 * 1024 * 1024,
        initial_heap_size: 8 * 1024 * 1024,
        region_size: 1024 * 1024,
        ihop_percent: 45,
        promotion_age: 1,
        gc_worker_threads: 1,
        heap_waste_percent: 0,
        ..G1CollectorConfig::default()
    }
}

/// `gch-h3os-g1-remark-unloads-classes-before-it-resurrects-soft-retained-referents`:
/// the VM's remark callback marks its keep list itself
/// (`interpreter::g1_remark_mark_kept_closure`) and then judges classes
/// through the predicate the heap hands it. That is sound only if (1)
/// `resurrect_after_remark` may run INSIDE the callback (no lock of the
/// heap is held there) and (2) the predicate is a live read of the bitmap,
/// so the kept object AND what it reaches through a field read live
/// afterwards. Both are pinned here, with the control: before the call
/// both read dead.
#[test]
fn gch_h4g1_a_keep_list_marked_inside_the_remark_callback_reads_live_there() {
    let heap = crate::vm_heap::VmHeap::from_inner(crate::vm_heap::VmHeapInner::G1(crate::vm_heap::G1State::new(config())));
    let crate::vm_heap::VmHeapInner::G1(state) = &heap.inner else {
        unreachable!("built as G1 above")
    };
    let gc: &G1Collector = &state.collector;
    let holder = gc.alloc_object(ClassId::new(1), 1);
    let child = gc.alloc_object(ClassId::new(2), 0);
    gc.set_field(holder, 0, Value::Object(Some(child)));
    // Promote both (promotion_age 1): Old objects below TAMS, so the
    // completed mark's verdict, not TAMS, decides them.
    let mut roots = vec![holder];
    gc.young_collection(&mut roots, &NoMonitors);
    gc.young_collection(&mut roots, &NoMonitors);
    let holder = roots[0];
    let child = match gc.get_field(holder, 0) {
        Value::Object(Some(c)) => c,
        other => panic!("the holder lost its child across promotion: {other:?}"),
    };
    let (h, c) = (holder.as_ptr() as usize, child.as_ptr() as usize);

    // A cycle with NO roots: both are dead by mark until the keep list
    // names the holder.
    heap.g1_start_concurrent_mark(&stw());
    let mut seen: Option<((bool, bool), (bool, bool))> = None;
    let mut process = |is_live: &dyn Fn(usize) -> bool| -> Vec<usize> {
        let before = (is_live(h), is_live(c));
        if let crate::vm_heap::VmHeapInner::G1(g1) = &heap.inner {
            g1.resurrect_after_remark(&[h]);
        }
        seen = Some((before, (is_live(h), is_live(c))));
        vec![h]
    };
    assert!(
        heap.g1_final_remark_and_cleanup(&stw(), &[], Some(&mut process)),
        "the cycle opened above must complete"
    );
    let (before, after) = seen.expect("the remark ran its reference callback");
    assert_eq!(
        before,
        (false, false),
        "control: with no root and no keep, the holder and its child are dead by mark"
    );
    assert_eq!(
        after,
        (true, true),
        "marking the keep list inside the callback must make the kept object and \
         its field-reached child read live through the callback's own predicate"
    );
}

/// Leave exactly `tail` bytes in the current Eden region of this thread's
/// stripe; returns that region's index.
fn leave_tail(gc: &G1Collector, tail: usize) -> usize {
    let regions = gc.regions.read();
    let cur = gc.eden_slots[gc.eden_slot()].load(Ordering::Relaxed);
    let rem = regions[cur].remaining();
    assert!(
        rem > tail,
        "fixture: the current Eden region has {rem} bytes left"
    );
    regions[cur]
        .bump_alloc(rem - tail, 8, "gch-h4g1-test")
        .expect("fixture: eat the region down to its tail");
    cur
}

/// `gengc-r5w5-sizer9-refill-can-carve-less-than-the-object` (G1 half): a
/// refill with a floor never hands out a chunk below it. With an 8 KiB tail
/// in the current Eden region, the floor of 20 KiB is served from a fresh
/// region and the tail is left uncarved; the plain refill (floor 0, the
/// control) carves the 8 KiB tail, the short buffer the page describes. A
/// floor above the TLAB bound (half a region) has no chunk.
#[test]
fn gch_h4g1_refill_at_least_never_hands_out_less_than_the_floor() {
    const TAIL: usize = 8 * 1024;
    const FLOOR: usize = 20 * 1024;

    let control = G1Collector::new(config());
    control
        .refill_tlab(4096)
        .expect("a fresh heap refills a TLAB");
    leave_tail(&control, TAIL);
    let (_, short) = control.refill_tlab(64 * 1024).expect("the tail is carved");
    assert_eq!(
        short, TAIL,
        "control: the plain refill carves the short tail"
    );

    let gc = G1Collector::new(config());
    gc.refill_tlab(4096).expect("a fresh heap refills a TLAB");
    let old = leave_tail(&gc, TAIL);
    let (ptr, len) = gc
        .refill_tlab_at_least(64 * 1024, FLOOR)
        .expect("a fresh region serves the floor");
    assert!(
        len >= FLOOR,
        "a {len}-byte chunk under a {FLOOR}-byte floor"
    );
    let regions = gc.regions.read();
    assert_eq!(
        regions[old].remaining(),
        TAIL,
        "the short region must be passed over, not carved"
    );
    assert_ne!(
        gc.region_for_ptr(&regions, ptr),
        Some(old),
        "the chunk must come from another region"
    );
    drop(regions);
    assert!(
        gc.refill_tlab_at_least(4096, gc.config.region_size / 2 + 8)
            .is_none(),
        "a floor above the TLAB bound has no chunk"
    );
}

/// `r15w4-mh2-address-memo-epoch-misses-g1-cleanup-patch`: a completed
/// mark cycle's `cleanup` (which can free whole Old regions in place)
/// moves `address_epoch()` and books no collection.
#[test]
fn gch_h4g1_cleanup_moves_the_address_epoch_not_the_collection_count() {
    let gc = G1Collector::new(config());
    let collections = gc.collection_count();
    let epoch = gc.address_epoch();
    gc.start_concurrent_mark(&stw());
    gc.remark(&stw(), &[]);
    while !gc.concurrent_mark_step(usize::MAX) {}
    gc.cleanup(&stw());
    assert_eq!(
        gc.collection_count(),
        collections,
        "cleanup books no collection (the premise of the page)"
    );
    assert_ne!(
        gc.address_epoch(),
        epoch,
        "cleanup can recycle regions, so the address epoch must move"
    );
}
