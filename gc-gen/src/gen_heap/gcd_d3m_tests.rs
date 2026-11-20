use super::*;

struct NoMonitors;
impl cratonvm_gc_core::collector::MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

/// One young collection with the moving path's preconditions set, as
/// `gcd_d2h_tests` sets them (no compiled frame is live on a test thread).
fn moving_cycle(heap: &GenerationalHeap, roots: &mut [ObjectRef]) -> GcResult {
    cratonvm_gc_core::gc_quiescence::publish_moving_young_enabled(true);
    cratonvm_gc_core::gc_quiescence::begin_moving_young_coverage_cycle();
    cratonvm_gc_core::gc_quiescence::clear_force_non_moving_jit_roots();
    cratonvm_gc_core::gc_quiescence::clear_unregistered_jit_frame_on_stack();
    heap.collect_garbage_inner(roots, &[], &NoMonitors).0
}

/// Heap A's evacuator refuses an unrecorded root; heap B's moving cycle
/// then runs. A's post-GC verifier must still read A's refusal (it read
/// `NEVER-OFFERED` through the old process static), and B must not read
/// A's.
#[test]
fn the_refusal_ledger_is_per_heap() {
    let a = GenerationalHeap::with_sizes(4 * 1024 * 1024, 4 * 1024 * 1024);
    let y = a.alloc_object(ClassId::new(7), 4);
    a.set_field(y, 1, Value::Int(0x3d3));
    let base = y.as_ptr() as usize;
    // SAFETY: 8-aligned, inside a live 4-slot object; never dereferenced
    // as an object by this test.
    let bad = unsafe { ObjectRef::from_raw((base + 16) as *mut u8) };
    let mut roots = vec![y, bad];
    // The first cycle is refused before any forward (gcd d2/h's latch);
    // the second runs, and its evacuator refuses `bad`.
    let _ = moving_cycle(&a, &mut roots);
    let _ = moving_cycle(&a, &mut roots);
    assert!(!a.young_cycle_skipped.load(Ordering::Relaxed));
    assert_eq!(a.get_field(roots[0], 1), Value::Int(0x3d3));
    assert_eq!(
        a.forward_refusal_reason(base + 16),
        Some("not-an-object-start")
    );
    assert_eq!(
        a.forward_refusal_reason(base),
        None,
        "the live object was offered"
    );

    let b = GenerationalHeap::with_sizes(4 * 1024 * 1024, 4 * 1024 * 1024);
    let z = b.alloc_object(ClassId::new(7), 2);
    let mut b_roots = vec![z];
    let _ = moving_cycle(&b, &mut b_roots);
    assert!(!b.young_cycle_skipped.load(Ordering::Relaxed));
    assert_eq!(
        a.forward_refusal_reason(base + 16),
        Some("not-an-object-start"),
        "another heap's moving cycle must not clear this heap's refusals"
    );
    assert_eq!(b.forward_refusal_reason(base + 16), None);
}

/// The ledger's own rules: one entry per address, the first reason kept,
/// capped, and cleared by the next cycle's reset.
#[test]
fn the_refusal_ledger_dedups_caps_and_resets() {
    let l = ForwardRefusalLedger::default();
    l.record(0x1000, "suspect-header");
    l.record(0x1000, "not-an-object-start");
    assert_eq!(l.reason(0x1000), Some("suspect-header"));
    for i in 0..(REFUSAL_LEDGER_CAP + 8) {
        l.record(0x2000 + 8 * i, "bad-forwarding-target");
    }
    assert_eq!(l.refusals.lock().len(), REFUSAL_LEDGER_CAP);
    l.reset();
    assert_eq!(l.reason(0x1000), None);
}

/// gcj j1/yg: the pinned copy's survivor reserve is HotSpot's survivor
/// space, or 5/4 of the last pinned cycle's survivors when larger, and
/// never more than half the semi.
#[test]
fn gcj_j1yg_the_pinned_survivor_reserve_is_a_survivor_space() {
    const M: usize = 1 << 20;
    assert_eq!(pinned_survivor_reserve(128 * M, 17 * M, 0), 17 * M);
    assert_eq!(pinned_survivor_reserve(128 * M, 17 * M, 8 * M), 17 * M);
    assert_eq!(pinned_survivor_reserve(128 * M, 17 * M, 24 * M), 30 * M);
    assert_eq!(pinned_survivor_reserve(128 * M, 17 * M, 200 * M), 64 * M);
    assert_eq!(pinned_survivor_reserve(128 * M, 0, 0), 0);
}

/// gcj j1/yg item 1: the unproven-frame arm takes a term-3 cycle whose
/// recorded reasons are all frames the ledger holds, and nothing else.
#[test]
fn gcj_j1yg_the_unproven_frame_arm_takes_only_ledger_held_reasons() {
    use cratonvm_gc_core::gc_quiescence::incomplete_reason as r;
    let bit = |c: usize| 1usize << c;
    let unreg = bit(r::UNREGISTERED_JIT_FRAME);
    let take = |mask: usize| pinned_copy_takes_unproven_cycle(true, false, mask, false, false, false);
    assert!(take(unreg));
    assert!(take(unreg | bit(r::UNPUBLISHED_FRAME_OOP) | bit(r::CROSS_THREAD_JIT_PEER)));
    // A reason that distrusts a map or a band refuses the cycle.
    for refused in [
        r::NO_PRECISE_MAP,
        r::MISSING_EXACT_RBP,
        r::ACTIVE_FRAME_MAP,
        r::PARENT_FRAME_MAP,
        r::OSR_SHADOW,
        r::UNBOUNDED_FRAME_BAND,
        r::FOREIGN_INNERMOST_RBP,
        r::JIT_RELOCATION_UNSUPPORTED,
        r::COVERAGE_ORACLE_REFUTED,
    ] {
        assert!(!take(unreg | bit(refused)), "reason {refused}");
    }
    // No recorded reason: nothing to judge, refused.
    assert!(!take(0));
    // Moving young off, a forced in-place promise, another divert term.
    assert!(!pinned_copy_takes_unproven_cycle(false, false, unreg, false, false, false));
    assert!(!pinned_copy_takes_unproven_cycle(true, true, unreg, false, false, false));
    assert!(!pinned_copy_takes_unproven_cycle(true, false, unreg, true, false, false));
    // A take-over licence that forbids an unpinnable move needs the
    // pinned verdict.
    let xt = unreg | bit(r::XT_TAKEOVER);
    assert!(!pinned_copy_takes_unproven_cycle(true, false, xt, false, true, false));
    assert!(pinned_copy_takes_unproven_cycle(true, false, xt, false, true, true));
}

/// The take-over arm's candidacy: term 3 through the take-over licence
/// alone, with a verdict a pinning backend could act on.
#[test]
fn the_takeover_arm_takes_only_a_takeover_alone_divert() {
    use cratonvm_gc_core::gc_quiescence::TakeoverVerdict;
    let hw = TakeoverVerdict {
        helper_windows: 1,
        ..TakeoverVerdict::NONE
    };
    assert!(pinned_copy_takes_takeover_cycle(
        true, false, false, false, hw
    ));
    assert!(
        !pinned_copy_takes_takeover_cycle(false, false, false, false, hw),
        "moving young off (term 0)"
    );
    assert!(
        !pinned_copy_takes_takeover_cycle(true, true, false, false, hw),
        "forced non-moving JIT roots"
    );
    assert!(
        !pinned_copy_takes_takeover_cycle(true, false, true, false, hw),
        "another coverage obligation is open"
    );
    assert!(
        !pinned_copy_takes_takeover_cycle(true, false, false, true, hw),
        "another divert term holds"
    );
    let frozen = TakeoverVerdict { frozen: 1, ..hw };
    assert!(!pinned_copy_takes_takeover_cycle(
        true, false, false, false, frozen
    ));
    let unpinned = TakeoverVerdict {
        pins_complete: false,
        ..hw
    };
    assert!(!pinned_copy_takes_takeover_cycle(
        true, false, false, false, unpinned
    ));
    assert!(!pinned_copy_takes_takeover_cycle(
        true,
        false,
        false,
        false,
        TakeoverVerdict::NONE
    ));
}

/// With `CRATONVM_GEN_PINNED_YOUNG_COPY_TAKEOVER` unset (every unit-test
/// run), the arm is off and the pinned copy's own census never moves.
#[test]
fn the_takeover_arm_is_opt_in() {
    if cratonvm_gc_core::gc_quiescence::pinned_young_copy_takeover_enabled() {
        return;
    }
    assert!(!cratonvm_gc_core::gc_quiescence::generational_takeover_pins_honoured());
    let heap = GenerationalHeap::with_sizes(4 * 1024 * 1024, 4 * 1024 * 1024);
    let y = heap.alloc_object(ClassId::new(7), 2);
    let mut roots = vec![y];
    let _ = moving_cycle(&heap, &mut roots);
    assert_eq!(heap.pinned_takeover_census(), [0, 0, 0, 0]);
}
