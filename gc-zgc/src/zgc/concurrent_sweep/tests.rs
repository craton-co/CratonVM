//! zt t21/zc2: the phase split on a real heap. Each test runs both arms
//! (the switch on and off) or carries an explicit positive control, so a
//! split that never engaged cannot pass as "the graph survived".

use super::*;
use cratonvm_gc_core::collector::{GarbageCollector, StopTheWorldToken};
use cratonvm_gc_core::heap::ArrayElementType;
use cratonvm_types::{ClassId, ObjectRef, Value};

/// The VM's monitor table with no monitor in it, as the key probe sees it
/// (zr z5/sw): it wants the dead, and its candidate keys are none. The
/// trait's defaults (`wants_dead_addresses` = yes, no key probe) would ask
/// for the full dead list, which the screen refuses (`WANTS_DEAD`).
struct NoMonitors;
impl MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _: &cratonvm_types::PointerMap) {}
    fn prune_candidate_keys(&self) -> Option<Vec<usize>> {
        Some(Vec::new())
    }
}

/// A 64 MiB heap that holds addresses still (no slide), with the switch
/// set as `on` says.
fn test_heap(on: bool) -> ZgcRealHeap {
    let heap = ZgcRealHeap::with_capacity(64 * 1024 * 1024);
    heap.set_relocation_enabled(false);
    // zt t27/gn5 (gn page item 10): single generation -- the mode refuses
    // the deferred sweep (`refusal::GENERATIONAL`; the refusal test turns
    // it on itself); under `CRATONVM_ZGC_GENERATIONAL=1` every test heap
    // starts generational.
    heap.set_generational_enabled(false);
    heap.counters.csweep.enabled.store(on, Ordering::Relaxed);
    heap
}

/// A chain of `n` two-slot objects plus `garbage` unreferenced ones.
fn graph(heap: &ZgcRealHeap, n: usize, garbage: usize) -> (ObjectRef, Vec<usize>, Vec<usize>) {
    let mut chain: Vec<ObjectRef> = Vec::with_capacity(n);
    for _ in 0..n {
        chain.push(heap.alloc_object(ClassId::new(51), 2));
    }
    for i in 0..n - 1 {
        heap.set_field(chain[i], 0, Value::Object(Some(chain[i + 1])));
    }
    let dead: Vec<usize> = (0..garbage)
        .map(|_| heap.alloc_object(ClassId::new(52), 4).as_ptr() as usize)
        .collect();
    let addrs = chain.iter().map(|o| o.as_ptr() as usize).collect();
    (chain[0], addrs, dead)
}

fn walk(heap: &ZgcRealHeap, head: ObjectRef) -> Vec<usize> {
    let mut out = vec![head.as_ptr() as usize];
    let mut cur = head;
    while let Value::Object(Some(next)) = heap.get_field(cur, 0) {
        out.push(next.as_ptr() as usize);
        cur = next;
    }
    out
}

fn collect(heap: &ZgcRealHeap, head: ObjectRef, armed: bool) {
    if armed {
        heap.arm_concurrent_sweep_runner();
    }
    // SAFETY: single-threaded test; the token stands for the pause.
    let stw = unsafe { StopTheWorldToken::new() };
    let mut roots = [head];
    let _ = heap.collect_garbage(&stw, &mut roots, &NoMonitors);
}


/// The split: the pause prunes the registry and frees no byte; the window
/// allocates only above the pause's cursor; the install frees the dead,
/// keeps every survivor (the window's included) and moves the gauge by the
/// reclaim only. Kill arm: the same collection frees in the pause.
#[test]
fn zt21zc2_the_pause_frees_nothing_and_the_install_frees_the_dead() {
    for on in [true, false] {
        let heap = test_heap(on);
        let (head, chain, garbage) = graph(&heap, 2_000, 4_000);
        collect(&heap, head, true);
        let st = &heap.counters.csweep;
        assert_eq!(st.pending(), on, "on={on}: deferred only with the switch");
        for &g in &garbage {
            assert!(
                heap.is_object_address(g).is_none(),
                "on={on}: the registry is pruned in the pause"
            );
        }
        assert_eq!(walk(&heap, head), chain, "on={on}");
        if !on {
            assert_eq!(heap.concurrent_sweep_report().deferred, 0);
            continue;
        }
        let (cursor_at_pause, base) = {
            let a = heap.arena.lock();
            (a.used_low_for_compaction(), a.base_ptr() as usize)
        };
        assert!(
            !heap.conc_bits_known_clear.load(Ordering::Acquire),
            "the mark bits wait for the install"
        );
        // THE WINDOW: allocations land above the pause's cursor (the list
        // is taken), never on a dead object's bytes.
        let window: Vec<ObjectRef> = (0..64)
            .map(|_| heap.alloc_object(ClassId::new(53), 1))
            .collect();
        for (i, o) in window.iter().enumerate() {
            let a = o.as_ptr() as usize;
            assert!(
                a >= base + cursor_at_pause,
                "window object {i} below the cursor"
            );
            heap.set_field(*o, 0, Value::Int(i as i32));
        }
        let at_pause = heap.allocated.load(Ordering::Relaxed);
        assert!(heap.finish_pending_sweep(ZSweepRunner::Initiator));
        assert!(!st.pending());
        assert!(!heap.finish_pending_sweep(ZSweepRunner::Waiter), "ran once");
        assert!(heap.conc_bits_known_clear.load(Ordering::Acquire));
        assert_eq!(
            walk(&heap, head),
            chain,
            "survivors intact after the install"
        );
        for (i, o) in window.iter().enumerate() {
            assert_eq!(
                heap.get_field(*o, 0),
                Value::Int(i as i32),
                "window object {i}"
            );
        }
        let after = heap.allocated.load(Ordering::Relaxed);
        assert!(
            after < at_pause,
            "the gauge dropped by the reclaim: {after} vs {at_pause}"
        );
        let (free_after, cursor_after) = {
            let a = heap.arena.lock();
            (a.free_list_bytes(), a.used_low_for_compaction())
        };
        assert!(
            free_after > 0 || cursor_after < cursor_at_pause,
            "the dead objects' bytes came back"
        );
        let r = heap.concurrent_sweep_report();
        assert_eq!((r.deferred, r.by_initiator, r.refused), (1, 1, 0));
        assert_eq!(
            heap.concurrent_sweep_last_live(),
            Some(heap.counters.csweep.last_live.load(Ordering::Relaxed))
        );
    }
}

/// zt t22/x18: the collecting door re-seals its figures when its cycle
/// deferred, even if another runner (here an allocation's) installed the
/// sweep before the door asked; once per deferral. Controls: a cycle
/// swept in the pause (unarmed) and the kill arm owe the door nothing.
#[test]
fn zt22x18_the_door_reseals_after_another_runner_installed_its_sweep() {
    let heap = test_heap(true);
    let (head, chain, _) = graph(&heap, 200, 1_000);
    collect(&heap, head, true);
    assert!(heap.counters.csweep.pending(), "fixture: deferred");
    assert!(heap.finish_pending_sweep(ZSweepRunner::Allocation));
    assert!(
        heap.run_pending_sweep_after_release(),
        "the door's cycle deferred: re-seal although the allocation ran it"
    );
    assert!(!heap.run_pending_sweep_after_release(), "once per deferral");
    let r = heap.concurrent_sweep_report();
    assert_eq!((r.by_allocation, r.by_initiator), (1, 0));
    // The door that ran its own sweep answers yes, as before.
    collect(&heap, head, true);
    assert!(heap.run_pending_sweep_after_release());
    assert!(!heap.run_pending_sweep_after_release());
    assert_eq!(heap.concurrent_sweep_report().by_initiator, 1);
    assert_eq!(walk(&heap, head), chain);
    // Controls: in-pause sweeps owe nothing.
    collect(&heap, head, false);
    assert!(
        !heap.run_pending_sweep_after_release(),
        "unarmed: no re-seal"
    );
    let off = test_heap(false);
    let (head, _, _) = graph(&off, 10, 10);
    collect(&off, head, true);
    assert!(
        !off.run_pending_sweep_after_release(),
        "kill arm: no re-seal"
    );
}


/// zt t23/x19 (review of t22/zc3): a deferral forgets the previous
/// collection's VM reference round, so a runner that installs before the
/// door notes this cycle's round prints `cp_vm_refs_pause_us=0`, not the
/// previous figure. Control: a cycle swept in the pause keeps it.
#[test]
fn zt23x19_a_deferral_forgets_the_previous_vm_reference_round() {
    let heap = test_heap(true);
    let (head, _, _) = graph(&heap, 50, 100);
    heap.note_vm_reference_round(10, 20, 3);
    assert_eq!(heap.last_vm_reference_round_us(), 33, "fixture");
    collect(&heap, head, false);
    assert_eq!(
        heap.last_vm_reference_round_us(),
        33,
        "control: swept in the pause, nothing forgotten"
    );
    collect(&heap, head, true);
    assert!(heap.counters.csweep.pending(), "fixture: deferred");
    assert_eq!(heap.last_vm_reference_round_us(), 0, "forgotten at the deferral");
    heap.note_vm_reference_round(1, 2, 3);
    assert!(heap.run_pending_sweep_after_release());
    assert_eq!(heap.last_vm_reference_round_us(), 6);
}

/// zt t32 (`CRATONVM_ZGC_CONCURRENT_REFERENT_CLEAR`): a holder names two
/// `Reference`-shaped rows (hidden from the mark) and a strong copy of the
/// second one's referent; the first referent dies. Returns the heap after
/// the collection, the two rows and their referents.
fn referent_clear_cycle(csweep: bool, after_release: bool) -> (ZgcRealHeap, [ObjectRef; 4]) {
    let heap = test_heap(csweep);
    heap.dbg_set_concurrent_referent_clear(true, after_release);
    let holder = heap.alloc_object(ClassId::new(61), 3);
    let weak_dead = heap.alloc_object(ClassId::new(62), 2);
    let weak_live = heap.alloc_object(ClassId::new(62), 2);
    let dead = heap.alloc_object(ClassId::new(63), 1);
    let live = heap.alloc_object(ClassId::new(63), 1);
    heap.set_field(weak_dead, 0, Value::Object(Some(dead)));
    heap.set_field(weak_live, 0, Value::Object(Some(live)));
    heap.set_field(holder, 0, Value::Object(Some(weak_dead)));
    heap.set_field(holder, 1, Value::Object(Some(weak_live)));
    heap.set_field(holder, 2, Value::Object(Some(live)));
    assert!(heap.concurrent_refs_skip_mode());
    heap.set_stw_vm_reference_skip(vec![
        weak_dead.as_ptr() as usize,
        weak_live.as_ptr() as usize,
    ]);
    collect(&heap, holder, true);
    heap.clear_stw_vm_reference_skip();
    (heap, [weak_dead, weak_live, dead, live])
}

/// zt t32 (steps 2b / 2d): a cycle that parks its sweep leaves the dead
/// referent in its slot, arms the gate (which blocks that read and passes
/// the live one), and the clear after the release nulls the slot and
/// disarms; the install then frees the referent. Kill arm: the pause
/// clears, and no gate is ever armed.
#[test]
fn zt32_a_parked_sweep_clears_the_dead_referents_after_the_release() {
    use super::super::concurrent_refs::{
        ZGC_REFERENT_GATE_ARMED, ZGC_REFERENT_GATE_DISARMED, ZGC_REFERENT_GATE_NEVER,
    };
    for after_release in [true, false] {
        let (heap, [weak_dead, weak_live, dead, live]) =
            referent_clear_cycle(true, after_release);
        let (wd, wl, d, l) = (
            weak_dead.as_ptr() as usize,
            weak_live.as_ptr() as usize,
            dead.as_ptr() as usize,
            live.as_ptr() as usize,
        );
        assert!(heap.counters.csweep.pending(), "the sweep is parked");
        assert_eq!(heap.get_field(weak_live, 0), Value::Object(Some(live)));
        if !after_release {
            let slot = heap.get_field(weak_dead, 0);
            assert_eq!(slot, Value::Object(None), "kill: the pause clears");
            assert_eq!(heap.referent_gate_state(), ZGC_REFERENT_GATE_NEVER);
            assert_eq!(heap.run_staged_referent_clear(), 0, "nothing staged");
            continue;
        }
        assert_eq!(
            heap.get_field(weak_dead, 0),
            Value::Object(Some(dead)),
            "the slot still names the dead referent at the release"
        );
        assert_eq!(heap.referent_gate_state(), ZGC_REFERENT_GATE_ARMED);
        assert!(heap.referent_read_blocked(wd, d), "dead: blocked");
        assert!(!heap.referent_read_blocked(wl, l), "the live read passes");
        assert!(!heap.referent_read_blocked(l, d), "not a row: passes");
        assert_eq!(heap.run_staged_referent_clear(), 1);
        assert_eq!(heap.referent_gate_state(), ZGC_REFERENT_GATE_DISARMED);
        assert_eq!(heap.get_field(weak_dead, 0), Value::Object(None));
        assert_eq!(heap.run_staged_referent_clear(), 0, "idempotent");
        assert!(heap.finish_pending_sweep(ZSweepRunner::Initiator));
        assert!(heap.is_object_address(d).is_none(), "the install freed it");
        assert_eq!(heap.get_field(weak_live, 0), Value::Object(Some(live)));
        let r = heap.concurrent_refs_report();
        assert_eq!(
            (r.deferred_clears, r.after_release_cleared, r.gate_blocked),
            (1, 1, 1)
        );
    }
}

/// zt t32 (R6, 6b): the install's clip drops every byte inside a selected
/// region and keeps the rest, in pieces, ascending.
#[test]
fn zt32_the_install_lists_no_span_inside_a_planned_region() {
    let holes = [(100, 200), (300, 400)];
    assert_eq!(
        clip_spans_outside(&[(350, 100), (0, 50), (150, 200)], &holes),
        vec![(0, 50), (200, 100), (400, 50)]
    );
    assert_eq!(clip_spans_outside(&[(100, 100)], &holes), vec![]);
    assert_eq!(
        clip_spans_outside(&[(50, 400)], &holes),
        vec![(50, 50), (200, 100), (400, 50)]
    );
    assert_eq!(clip_spans_outside(&[(10, 5)], &[]), vec![(10, 5)]);
}

/// zt t32: a runner of the parked sweep that is not the round (here the
/// initiator straight away) runs the staged clear before its walk, so no
/// referent is freed while a slot names it.
#[test]
fn zt32_the_sweep_runner_runs_the_staged_clear_first() {
    let (heap, [weak_dead, _, dead, _]) = referent_clear_cycle(true, true);
    assert!(heap.finish_pending_sweep(ZSweepRunner::Initiator));
    assert_eq!(heap.get_field(weak_dead, 0), Value::Object(None));
    assert!(heap.is_object_address(dead.as_ptr() as usize).is_none());
    assert_eq!(heap.concurrent_refs_report().after_release_cleared, 1);
}

/// zt t32: a cycle that sweeps in its pause (the concurrent sweep off) runs
/// the staged clear there, before its sweep; no gate is armed.
#[test]
fn zt32_a_cycle_that_sweeps_in_its_pause_clears_there() {
    let (heap, [weak_dead, weak_live, dead, live]) = referent_clear_cycle(false, true);
    assert!(!heap.counters.csweep.pending());
    assert_eq!(heap.get_field(weak_dead, 0), Value::Object(None));
    assert_eq!(heap.get_field(weak_live, 0), Value::Object(Some(live)));
    assert!(heap.is_object_address(dead.as_ptr() as usize).is_none());
    assert_eq!(
        heap.referent_gate_state(),
        super::super::concurrent_refs::ZGC_REFERENT_GATE_NEVER
    );
    assert_eq!(heap.concurrent_refs_report().deferred_clears, 0);
}

/// No runner armed (a direct call, a test elsewhere in the crate): the
/// cycle sweeps in the pause and the refusal is booked.
#[test]
fn zt21zc2_an_unarmed_collection_sweeps_in_the_pause() {
    let heap = test_heap(true);
    let (head, chain, _garbage) = graph(&heap, 100, 100);
    collect(&heap, head, false);
    assert!(!heap.counters.csweep.pending());
    let r = heap.concurrent_sweep_report();
    assert_eq!(
        (r.deferred, r.refused, r.last_refusal),
        (0, 1, refusal::NO_RUNNER)
    );
    assert_eq!(walk(&heap, head), chain);
    // Generational young cycle (and pre-promotion cycles) refuse.
    let g = test_heap(true);
    g.set_generational_enabled(true);
    g.set_gen_bitmap_fallback_major(false);
    g.has_old_objects.store(true, Ordering::Relaxed);
    let (head, _, _) = graph(&g, 10, 10);
    collect(&g, head, true);
    assert!(!g.counters.csweep.pending());
    assert_eq!(
        g.concurrent_sweep_report().last_refusal,
        refusal::GENERATIONAL
    );
}

/// `needs_gc` holds while a sweep is pending (the cycle has not ended) and
/// answers again after the install (positive control: the same gauge then
/// asks for a collection).
#[test]
fn zt21zc2_needs_gc_holds_while_the_sweep_is_pending() {
    let heap = test_heap(true);
    heap.counters.warmup_enabled.store(false, Ordering::Relaxed);
    let (head, _, _) = graph(&heap, 100, 1_000);
    collect(&heap, head, true);
    assert!(heap.counters.csweep.pending());
    heap.allocated
        .fetch_add(heap.gc_threshold.saturating_mul(2), Ordering::Relaxed);
    assert!(!heap.needs_gc(), "pending: no new cycle");
    assert!(heap.finish_pending_sweep(ZSweepRunner::Initiator));
    heap.allocated
        .fetch_add(heap.gc_threshold.saturating_mul(2), Ordering::Relaxed);
    assert!(
        heap.needs_gc(),
        "positive control: installed, the gauge collects"
    );
}

/// The next collection finishes a pending sweep before it touches the
/// arena, and a fresh cycle then defers again.
#[test]
fn zt21zc2_the_next_collection_finishes_the_pending_sweep_first() {
    let heap = test_heap(true);
    let (head, chain, _) = graph(&heap, 500, 2_000);
    collect(&heap, head, true);
    assert!(heap.counters.csweep.pending());
    collect(&heap, head, true);
    let r = heap.concurrent_sweep_report();
    assert_eq!((r.by_cycle_start, r.deferred), (1, 2));
    assert!(
        heap.counters.csweep.pending(),
        "the second cycle deferred too"
    );
    assert!(heap.finish_pending_sweep(ZSweepRunner::Initiator));
    assert_eq!(walk(&heap, head), chain);
}

/// An allocation the window cannot serve finishes the sweep and retries
/// instead of failing (the allocation stall). Control: the same request
/// after the install needs no retry.
#[test]
fn zt21zc2_an_allocation_short_of_space_finishes_the_sweep_and_retries() {
    let heap = ZgcRealHeap::with_capacity(16 * 1024 * 1024);
    heap.set_relocation_enabled(false);
    // zt t27/gn5b: single generation, as `test_heap` -- the mode refuses
    // the deferred sweep (`refusal::GENERATIONAL`).
    heap.set_generational_enabled(false);
    heap.counters.csweep.enabled.store(true, Ordering::Relaxed);
    let (head, chain, _) = graph(&heap, 10, 0);
    // Fill the arena with garbage arrays, leaving little bump space.
    let mut filled = 0usize;
    while filled < 13 * 1024 * 1024 {
        if heap
            .try_alloc_array(ClassId::new(54), ArrayElementType::Byte, 4096)
            .is_none()
        {
            break;
        }
        filled += 4096;
    }
    // The fill may have raised the urgent latches; this test is about the
    // window, not about an urgent cycle (which sweeps in the pause).
    heap.headroom_low.store(false, Ordering::Relaxed);
    heap.hard_alloc_failure.store(false, Ordering::Relaxed);
    collect(&heap, head, true);
    assert!(heap.counters.csweep.pending());
    let big = heap.try_alloc_array(ClassId::new(55), ArrayElementType::Byte, 6 * 1024 * 1024);
    assert!(big.is_some(), "served after the sweep it finished");
    let r = heap.concurrent_sweep_report();
    assert!(r.alloc_retries >= 1 && r.by_allocation == 1, "{r:?}");
    assert!(!heap.counters.csweep.pending());
    assert_eq!(walk(&heap, head), chain);
}
