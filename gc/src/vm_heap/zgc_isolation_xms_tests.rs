use crate::collector::GarbageCollector;
use cratonvm_types::*;
use cratonvm_gc_core::MonitorCleanup;
use std::sync::atomic::Ordering;
use crate::zgc::*;
struct NoMonitors;
impl MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _: &cratonvm_types::PointerMap) {}
}

fn gate(addr: usize) -> u8 {
    assert_ne!(addr, 0);
    unsafe { (*(addr as *const std::sync::atomic::AtomicU8)).load(Ordering::Acquire) }
}

/// t1 wave 3 lane Q (`CRATONVM_ZGC_JIT_GATE_CELL`): a placed copy takes
/// the Box's bytes, the plan names it, every gate write reaches both
/// blocks, and a second placement changes nothing. Per heap: another
/// heap's plan still names its own Box.
#[test]
fn t1q_a_placed_gate_cell_mirrors_the_box_and_is_per_heap() {
    let a = ZgcRealHeap::with_capacity(32 * 1024 * 1024);
    let b = ZgcRealHeap::with_capacity(32 * 1024 * 1024);
    let boxed = a.jit_ref_store_plan();
    let cell: &'static crate::gen_heap::JitRefStoreGates =
        Box::leak(Box::new(crate::gen_heap::JitRefStoreGates::unpublished()));
    assert!(a.place_jit_ref_store_gates(cell));
    let plan = a.jit_ref_store_plan();
    assert_eq!(plan.0, &cell.pre_active as *const _ as usize);
    assert_eq!(plan.1, plan.0 + 1, "pre and post stay adjacent (the fused gate)");
    assert_eq!(gate(plan.0), gate(boxed.0));
    assert_eq!(gate(plan.1), gate(boxed.1));
    assert_eq!(gate(plan.2), gate(boxed.2));
    assert_eq!(cell.published.load(Ordering::Acquire), 1);
    a.set_mark_active(true);
    assert_eq!((gate(plan.0), gate(boxed.0)), (1, 1));
    a.set_mark_active(false);
    assert_eq!((gate(plan.0), gate(boxed.0)), (0, 0));
    a.store_jit_gate(|g| &g.post_active, 1);
    assert_eq!((gate(plan.1), gate(boxed.1)), (1, 1));
    let other: &'static crate::gen_heap::JitRefStoreGates =
        Box::leak(Box::new(crate::gen_heap::JitRefStoreGates::unpublished()));
    assert!(!a.place_jit_ref_store_gates(other));
    assert_eq!(a.jit_ref_store_plan(), plan);
    assert_ne!(b.jit_ref_store_plan().0, plan.0);
    b.set_mark_active(true);
    assert_eq!(gate(plan.0), 0, "another heap's mark does not arm this cell");
    b.set_mark_active(false);
}

/// t1 wave 5 lane Q (`t1-V3-zgc-gate-cell-late-placement-*` item 1): a
/// placement racing gate writes on another thread leaves the cell equal to
/// the Box, byte for byte, whatever the interleaving. The writer toggles
/// every gate byte and ends on a known value; after the join both blocks
/// must agree (a lost re-copy shows as a lowered cell under an armed Box).
#[test]
fn t1q_a_late_placement_racing_gate_writes_keeps_the_cell_equal_to_the_box() {
    for round in 0..24u8 {
        let heap = ZgcRealHeap::with_capacity(32 * 1024 * 1024);
        let cell: &'static crate::gen_heap::JitRefStoreGates =
            Box::leak(Box::new(crate::gen_heap::JitRefStoreGates::unpublished()));
        let go = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|s| {
            s.spawn(|| {
                while !go.load(Ordering::Acquire) {
                    std::hint::spin_loop();
                }
                for i in 0..2000u32 {
                    let v = u8::from(i % 2 == 0);
                    heap.store_jit_gate(|g| &g.pre_active, v);
                    heap.store_jit_gate(|g| &g.post_active, v);
                    heap.store_jit_gate(|g| &g.young_floor, v << 4);
                }
                // End armed (the direction a lost copy would undo).
                heap.store_jit_gate(|g| &g.pre_active, 1);
                heap.store_jit_gate(|g| &g.post_active, 1);
                heap.store_jit_gate(|g| &g.young_floor, round & 0x30);
            });
            go.store(true, Ordering::Release);
            // Placed somewhere inside the writer's run.
            for _ in 0..(u32::from(round) * 37) {
                std::hint::spin_loop();
            }
            assert!(heap.place_jit_ref_store_gates(cell));
        });
        let own = &heap.jit_ref_store_gates;
        for (b, c) in [
            (&own.pre_active, &cell.pre_active),
            (&own.post_active, &cell.post_active),
            (&own.young_floor, &cell.young_floor),
            (&own.post_skip_mask, &cell.post_skip_mask),
        ] {
            assert_eq!(b.load(Ordering::SeqCst), c.load(Ordering::SeqCst), "round {round}");
        }
        assert_eq!(cell.pre_active.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn zgc_plans_survive_another_heaps_reset_mark_and_drop() {
    let a = ZgcRealHeap::with_capacity(32 * 1024 * 1024);
    a.set_generational_enabled(true);
    let holder = a.alloc_object(ClassId::new(1), 1);
    a.header_ref(holder.as_ptr()).set_gc_age(a.promotion_age());
    // The promoting sweep's order: the JIT post gate first, then the truth
    // (zs s3/bt: the gate no longer starts armed).
    a.store_jit_gate(|g| &g.post_active, 1);
    a.has_old_objects.store(true, Ordering::Relaxed);
    let plan = a.jit_ref_store_plan();
    assert_eq!(
        gate(plan.0),
        0,
        "an unmarked heap must retain its pre-barrier fast path"
    );
    a.set_mark_active(true);
    {
        let b = ZgcRealHeap::with_capacity(32 * 1024 * 1024);
        assert_ne!(plan.1, b.jit_ref_store_plan().1);
        b.reset_generational_state();
        b.set_mark_active(true);
        b.set_mark_active(false);
        assert_eq!(gate(plan.0), 1);
        assert_eq!(gate(plan.1), 1);
    }
    assert_eq!(a.jit_ref_store_plan(), plan);
    if gate(plan.1) != 0 {
        a.note_ref_store(holder.as_ptr() as usize);
    }
    assert!(
        a.remembered
            .snapshot()
            .iter()
            .map(|s| s.bits_set())
            .sum::<usize>()
            > 0,
        "the owning heap's old receiver must be carded"
    );
    assert_eq!(gate(plan.0), 1);
    assert_eq!(gate(plan.1), 1);
    a.set_mark_active(false);
    assert_eq!(gate(plan.0), 0);
}


#[test]
fn zgc_promotions_rearm_own_gate_after_a_reset() {
    let a = ZgcRealHeap::with_capacity(32 * 1024 * 1024);
    a.set_tlab_enabled(false);
    a.set_generational_enabled(true);
    let holder = a.alloc_object(ClassId::new(1), 1);
    a.header_ref(holder.as_ptr())
        .set_gc_age(a.promotion_age() - 1);
    a.reset_generational_state();
    let plan = a.jit_ref_store_plan();
    assert_eq!(gate(plan.1), 0);
    let stw = unsafe { crate::StopTheWorldToken::new() };
    let mut roots = [holder];
    a.collect_garbage(&stw, &mut roots, &NoMonitors);
    assert!(a.has_old_objects.load(Ordering::Relaxed));
    assert_eq!(gate(plan.1), 1);
    let b = ZgcRealHeap::with_capacity(32 * 1024 * 1024);
    b.reset_generational_state();
    assert_eq!(gate(plan.1), 1);
}

#[test]
fn zgc_heaps_keep_independent_uncommit_policies() {
    use cratonvm_gc_core::reservation::GRANULE;
    let a = ZgcRealHeap::with_capacity_and_initial(32 * GRANULE, 8 * GRANULE);
    let b = ZgcRealHeap::with_capacity_and_initial(32 * GRANULE, 4 * GRANULE);
    a.set_uncommit_options(true, 0);
    b.set_uncommit_options(false, 0);
    a.set_readable_give_back(false);
    b.set_readable_give_back(false);
    a.arena.lock().commit_initial_prefix(16 * GRANULE);
    b.arena.lock().commit_initial_prefix(16 * GRANULE);
    let stw = unsafe { crate::StopTheWorldToken::new() };
    for _ in 0..3 {
        a.collect_garbage(&stw, &mut [], &NoMonitors);
        b.collect_garbage(&stw, &mut [], &NoMonitors);
    }
    assert!(a.os_committed_bytes() >= 8 * GRANULE);
    assert!(b.os_committed_bytes() >= 16 * GRANULE);
    if a.arena.lock().commit_bits().is_some() {
        assert_eq!(a.os_committed_bytes(), 8 * GRANULE);
    }
}

#[test]
fn zgc_empty_collections_preserve_xms_floor() {
    let heap = ZgcRealHeap::with_capacity_and_initial(64 * 1024 * 1024, 16 * 1024 * 1024);
    heap.set_uncommit_options(true, 0);
    heap.set_readable_give_back(false);
    let stw = unsafe { crate::StopTheWorldToken::new() };
    for _ in 0..3 {
        heap.collect_garbage(&stw, &mut [], &NoMonitors);
        assert!(heap.os_committed_bytes() >= 16 * 1024 * 1024);
    }
}
