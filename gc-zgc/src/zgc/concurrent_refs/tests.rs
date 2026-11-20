//! zt t22/zc3: the hidden-slot protocol on a real heap. Every test runs the
//! kill arm beside the switch (no set handed in = the referent is traced),
//! so a hole that never engaged cannot pass as "the referent died".

use super::*;
use cratonvm_gc_core::collector::{GarbageCollector, MonitorCleanup, StopTheWorldToken};
use cratonvm_types::ClassId;

struct NoMonitors;
impl MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _: &cratonvm_types::PointerMap) {}
    fn prune_candidate_keys(&self) -> Option<Vec<usize>> {
        Some(Vec::new())
    }
}

fn test_heap(on: bool) -> ZgcRealHeap {
    let heap = ZgcRealHeap::with_capacity(64 * 1024 * 1024);
    heap.set_relocation_enabled(false);
    // zt t27/gn5b (gn page item 10): single generation -- the mode refuses
    // the switch per cycle (`concurrent_refs_skip_mode`; the refusal test
    // turns the mode on itself), and `CRATONVM_ZGC_GENERATIONAL=1` makes
    // every test heap generational.
    heap.set_generational_enabled(false);
    heap.counters.crefs.enabled.store(on, Ordering::Relaxed);
    heap
}


/// A holder (the root) names two `Reference`-shaped objects and a strong
/// copy of the second one's referent. With the rows handed in, the first
/// referent (reachable only through its slot) dies, its slot is null after
/// the collection and its storage is unregistered; the second keeps its
/// referent. Kill arm (no set): both referents are traced and kept.
#[test]
fn zt22zc3_a_hidden_slot_is_cleared_in_the_pause_when_its_referent_dies() {
    for on in [true, false] {
        let heap = test_heap(on);
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
        let dead_addr = dead.as_ptr() as usize;
        assert!(heap.concurrent_refs_skip_mode() == on);
        if on {
            heap.set_stw_vm_reference_skip(vec![
                weak_dead.as_ptr() as usize,
                weak_live.as_ptr() as usize,
            ]);
        }
        // SAFETY: single-threaded test; the token stands for the pause.
        let stw = unsafe { StopTheWorldToken::new() };
        let mut roots = [holder];
        let _ = heap.collect_garbage(&stw, &mut roots, &NoMonitors);
        heap.clear_stw_vm_reference_skip();
        assert_eq!(
            heap.get_field(weak_live, 0),
            Value::Object(Some(live)),
            "on={on}: a live referent stays in its slot"
        );
        if on {
            assert_eq!(
                heap.get_field(weak_dead, 0),
                Value::Object(None),
                "the dead referent's slot is cleared in the pause"
            );
            assert!(
                heap.is_object_address(dead_addr).is_none(),
                "and its storage was reclaimed"
            );
            assert_eq!(heap.counters.crefs.cycle_cleared.load(Ordering::Relaxed), 1);
        } else {
            assert_eq!(
                heap.get_field(weak_dead, 0),
                Value::Object(Some(dead)),
                "kill arm: the referent is traced through its slot"
            );
            assert!(heap.is_object_address(dead_addr).is_some());
        }
        assert!(
            heap.counters.crefs.stw_skip.lock().is_empty(),
            "on={on}: the collection took the set"
        );
    }
}

/// A dead `Reference` is left to the sweep (its slot is never written),
/// and a `Reference` whose slot the application already cleared is left
/// alone.
#[test]
fn zt22zc3_a_dead_or_already_cleared_reference_is_not_written() {
    let heap = test_heap(true);
    let holder = heap.alloc_object(ClassId::new(61), 1);
    let cleared = heap.alloc_object(ClassId::new(62), 2);
    let orphan = heap.alloc_object(ClassId::new(62), 2);
    let target = heap.alloc_object(ClassId::new(63), 1);
    heap.set_field(holder, 0, Value::Object(Some(cleared)));
    // The application already cleared `cleared`'s referent: an unwritten
    // slot of this layout-less test object would read back as `Int(0)`.
    heap.set_field(cleared, 0, Value::Object(None));
    heap.set_field(orphan, 0, Value::Object(Some(target)));
    let orphan_addr = orphan.as_ptr() as usize;
    heap.set_stw_vm_reference_skip(vec![cleared.as_ptr() as usize, orphan_addr]);
    // SAFETY: single-threaded test; the token stands for the pause.
    let stw = unsafe { StopTheWorldToken::new() };
    let mut roots = [holder];
    let _ = heap.collect_garbage(&stw, &mut roots, &NoMonitors);
    assert_eq!(heap.get_field(cleared, 0), Value::Object(None));
    assert!(heap.is_object_address(orphan_addr).is_none());
    assert_eq!(
        heap.counters.crefs.cycle_cleared.load(Ordering::Relaxed),
        0,
        "neither the dead Reference nor the empty slot is a clear"
    );
}

/// The engine's skip snapshot (the parallel stop-the-world mark and the
/// concurrent mark end's refresh) carries the collection's rows; with no
/// set it does not.
#[test]
fn zt22zc3_the_engine_snapshot_carries_the_hidden_rows() {
    let heap = test_heap(true);
    let r = heap.alloc_object(ClassId::new(62), 2).as_ptr() as usize;
    let snap = heap.begin_concurrent_mark_cycle();
    assert!(!snap.contains(&r), "control: no set, no row");
    heap.end_concurrent_mark_cycle();
    heap.set_stw_vm_reference_skip(vec![r]);
    let snap = heap.begin_concurrent_mark_cycle();
    assert!(snap.contains(&r));
    heap.end_concurrent_mark_cycle();
    assert!(heap.stw_vm_reference_skip_set().contains(&r));
    heap.clear_stw_vm_reference_skip();
    assert!(heap.stw_vm_reference_skip_set().is_empty());
}

/// The census accumulates every noted round, with the switch off too, and
/// resets the per-cycle figures; generational mode refuses the switch.
#[test]
fn zt22zc3_the_census_counts_rounds_with_the_switch_off_too() {
    let heap = test_heap(false);
    heap.note_vm_reference_round(10, 200, 5);
    heap.note_vm_reference_round(3, 50, 1);
    let r = heap.concurrent_refs_report();
    assert_eq!((r.enabled, r.rounds), (false, 2));
    assert_eq!(
        (r.pre_us, r.post_us, r.jni_us, r.max_round_us),
        (13, 250, 6, 215)
    );
    assert_eq!(heap.last_vm_reference_round_us(), 54);
    assert_eq!(r.skip_cycles, 0);
    // zt t31/zc11: a round run after the release counts in the summary,
    // not in the pause figure.
    heap.note_vm_reference_round_after_release(2, 4, 1, 1000);
    let r = heap.concurrent_refs_report();
    assert_eq!((r.post_us, r.max_round_us), (1254, 1007));
    assert_eq!(heap.last_vm_reference_round_us(), 7);

    let heap = test_heap(true);
    heap.set_stw_vm_reference_skip(vec![8, 16]);
    heap.note_vm_reference_round(1, 1, 1);
    assert!(!heap.counters.crefs.cycle_skip.load(Ordering::Relaxed));
    assert_eq!(heap.counters.crefs.cycle_rows.load(Ordering::Relaxed), 0);
    let r = heap.concurrent_refs_report();
    assert_eq!((r.skip_cycles, r.skip_rows), (1, 2));
    heap.clear_stw_vm_reference_skip();
    heap.generational_enabled.store(true, Ordering::Relaxed);
    assert!(
        heap.concurrent_refs_skip_mode(),
        "generational major cycle allows concurrent refs skip"
    );
    heap.set_gen_bitmap_fallback_major(false);
    heap.has_old_objects.store(true, Ordering::Relaxed);
    assert!(
        !heap.concurrent_refs_skip_mode(),
        "generational young cycle refuses"
    );
    assert_eq!(heap.concurrent_refs_report().refused, 1);
}

/// j27 wave 5 (lane REFS): the read gate may arm exactly on a heap that
/// clears after the release; a heap that clears in the pause (the default)
/// never arms it, so a compare-only referent read owes it nothing there.
#[test]
fn j27_refs_the_gate_may_arm_only_with_the_after_release_clear() {
    let heap = test_heap(true);
    heap.dbg_set_concurrent_referent_clear(true, false);
    assert!(!heap.referent_gate_may_arm(), "pause clear: never armed");
    heap.dbg_set_concurrent_referent_clear(true, true);
    assert!(heap.referent_gate_may_arm(), "after-release clear: may arm");
    heap.dbg_set_concurrent_referent_clear(false, true);
    assert!(!heap.referent_gate_may_arm(), "the clear needs the refs switch");
    // An armed (or once-armed) gate answers `true` whatever the seed says.
    heap.counters
        .crefs
        .gate
        .store(ZGC_REFERENT_GATE_DISARMED, Ordering::Release);
    assert!(heap.referent_gate_may_arm(), "a gate that armed once");
}
