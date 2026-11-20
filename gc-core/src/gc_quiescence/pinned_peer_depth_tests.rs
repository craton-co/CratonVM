use super::*;

/// The ledger sums the depths of peers whose stacks were pinned.
#[test]
fn pinned_depths_accumulate() {
    clear_xt_cycle_pinned_jit_depth();
    add_xt_cycle_pinned_jit_depth(Some(3));
    add_xt_cycle_pinned_jit_depth(Some(4));
    assert_eq!(xt_cycle_pinned_jit_depth(), 7);
}

/// THE safety property: one peer of unknown depth voids the whole credit,
/// rather than being counted as zero.
///
/// Counting it as zero is the unsound direction -- it would claim a peer's
/// frames are accounted for when nothing pinned or proved them, and the
/// collector would then relocate an object that peer's frame still names.
#[test]
fn one_unknown_depth_voids_the_whole_credit() {
    clear_xt_cycle_pinned_jit_depth();
    add_xt_cycle_pinned_jit_depth(Some(5));
    add_xt_cycle_pinned_jit_depth(None);
    add_xt_cycle_pinned_jit_depth(Some(6));
    assert_eq!(
        xt_cycle_pinned_jit_depth(),
        0,
        "an unattributable peer must void the credit, not contribute 0 to it"
    );
}

/// The poison does not outlive the cycle that set it.
#[test]
fn clearing_lifts_the_poison() {
    clear_xt_cycle_pinned_jit_depth();
    add_xt_cycle_pinned_jit_depth(None);
    assert_eq!(xt_cycle_pinned_jit_depth(), 0);
    clear_xt_cycle_pinned_jit_depth();
    add_xt_cycle_pinned_jit_depth(Some(2));
    assert_eq!(xt_cycle_pinned_jit_depth(), 2);
}

/// A registered thread reads back what it published; an unregistered tid is
/// `None`, which is what makes the distinction above expressible.
#[test]
fn depth_slot_round_trips_and_unknown_tid_is_none() {
    let tid = 0xFEED_0001;
    let slot = register_self_jit_depth_slot(tid);
    slot.store(9, Ordering::Release);
    assert_eq!(jit_depth_of_tid(tid), Some(9));
    assert_eq!(jit_depth_of_tid(0xFEED_0002), None);
}

/// Re-registering a recycled OS tid hands back the SAME cell -- one tid's
/// depth must never be split across two of them -- but RESET, because the
/// previous owner may be dead and its leftover depth is held by nobody.
#[test]
fn re_registering_a_tid_returns_the_same_slot_reset_to_zero() {
    let tid = 0xFEED_0003;
    let a = register_self_jit_depth_slot(tid);
    a.store(4, Ordering::Release);
    let b = register_self_jit_depth_slot(tid);
    assert!(std::sync::Arc::ptr_eq(&a, &b), "one tid, one cell");
    assert_eq!(
        b.load(Ordering::Acquire),
        0,
        "a recycled tid must not inherit the dead thread's depth"
    );
}

/// A departed thread leaves nothing readable behind.
#[test]
fn unregistering_removes_the_slot() {
    let tid = 0xFEED_0004;
    register_self_jit_depth_slot(tid).store(7, Ordering::Release);
    assert_eq!(jit_depth_of_tid(tid), Some(7));
    unregister_jit_depth_slot(tid);
    assert_eq!(
        jit_depth_of_tid(tid),
        None,
        "a dead thread's depth must read as UNKNOWN, not as a stale number"
    );
}

/// gci i3tk item 5 / i5tk X1: a POISONED per-tid map must not read as
/// "never published". Since the never-published rule
/// (`CRATONVM_XT_HELPER_WINDOW_UNPUBLISHED_NATIVE`) is default ON, a
/// `None` + `None` answer is taken as "no compiled frame", so every writer
/// and reader recovers the poison instead of degrading.
///
/// Poisoning the process-wide maps is safe for the rest of the suite only
/// because EVERY site recovers now (this test pins that): a poisoned lock
/// stays poisoned for the life of the test binary.
#[test]
fn gci_i13tq_a_poisoned_per_tid_map_still_publishes_and_reads() {
    // Poison both maps: a thread panics while holding each write guard.
    let r = std::thread::spawn(|| {
        let _g = per_tid_jit_depth().write().unwrap_or_else(|p| p.into_inner());
        panic!("gci i13/tq: deliberate poison of the per-tid depth map");
    })
    .join();
    assert!(r.is_err(), "the poisoning thread must have panicked");
    let r = std::thread::spawn(|| {
        let _g = per_tid_shadow_addr().write().unwrap_or_else(|p| p.into_inner());
        panic!("gci i13/tq: deliberate poison of the per-tid shadow map");
    })
    .join();
    assert!(r.is_err(), "the poisoning thread must have panicked");
    assert!(per_tid_jit_depth().is_poisoned());
    assert!(per_tid_shadow_addr().is_poisoned());

    let tid = 0xFEED_1301;
    // A writer under poison still lands in the shared map (not a detached slot).
    let slot = register_self_jit_depth_slot(tid);
    slot.store(5, Ordering::Release);
    assert_eq!(
        jit_depth_of_tid(tid),
        Some(5),
        "a poisoned depth map must not read as never published"
    );
    assert!(
        std::sync::Arc::ptr_eq(&slot, &register_self_jit_depth_slot(tid)),
        "a poisoned map must still hand back the registered cell"
    );
    publish_self_shadow_addr(tid, 0x1000, 0x2000, 0x3000);
    assert_eq!(
        shadow_window_of_tid(tid),
        Some((0x1000, 0x2000, 0x3000)),
        "a poisoned shadow map must not read as never published"
    );
    // And removal still removes.
    unregister_jit_depth_slot(tid);
    assert_eq!(jit_depth_of_tid(tid), None);
    assert_eq!(shadow_window_of_tid(tid), None);
}

/// gci i47w lane oe48: retracting a buffer withdraws exactly the triples
/// published for that `(base, end)`, under whichever tids published it,
/// and leaves every other window alone; `base == 0` is a no-op.
#[test]
fn gci_i47w_oe48_retracting_a_buffer_withdraws_only_its_windows() {
    let (a, b, c) = (0xFEED_4E11u32, 0xFEED_4E12u32, 0xFEED_4E13u32);
    publish_self_shadow_addr(a, 0x7_1000, 0x7_2000, 0x7_3000);
    // The same buffer published under a second tid (a runner that ran it
    // before the first one did): both go.
    publish_self_shadow_addr(b, 0x7_1000, 0x7_2000, 0x7_3000);
    publish_self_shadow_addr(c, 0x8_1000, 0x8_2000, 0x8_3000);
    retract_shadow_window_of_buffer(0, 0);
    let untouched = shadow_window_of_tid(a);
    // Same base, another end: not this buffer.
    retract_shadow_window_of_buffer(0x7_2000, 0x7_4000);
    let other_end = shadow_window_of_tid(a);
    retract_shadow_window_of_buffer(0x7_2000, 0x7_3000);
    let (ra, rb, rc) = (
        shadow_window_of_tid(a),
        shadow_window_of_tid(b),
        shadow_window_of_tid(c),
    );
    for t in [a, b, c] {
        unregister_jit_depth_slot(t);
    }
    assert_eq!(untouched, Some((0x7_1000, 0x7_2000, 0x7_3000)));
    assert_eq!(other_end, Some((0x7_1000, 0x7_2000, 0x7_3000)));
    assert_eq!(ra, None);
    assert_eq!(rb, None);
    assert_eq!(rc, Some((0x8_1000, 0x8_2000, 0x8_3000)));
}
