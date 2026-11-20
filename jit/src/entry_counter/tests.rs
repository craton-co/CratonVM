use super::*;

/// The emitter bakes `&counter.count` and addresses it at
/// `COUNT_OFFSET`. If a field is ever added ahead of `count`, the
/// prologue would increment the wrong word — silently, since the value it
/// corrupts is only read by a heuristic. This is that guard, and it is the
/// same shape as `test_jit_pic_slot_offsets`.
#[test]
fn entry_counter_count_offset_is_zero() {
    let c = JitEntryCounter::new();
    let base = &c as *const JitEntryCounter as usize;
    assert_eq!(
        c.count_addr() - base,
        JitEntryCounter::COUNT_OFFSET as usize,
        "count_addr must be base + COUNT_OFFSET"
    );
    assert_eq!(
        JitEntryCounter::COUNT_OFFSET,
        0,
        "the emitter encodes COUNT_OFFSET as a disp8; a non-zero value \
         needs the emitter updated in the same commit"
    );
}

/// The word the emitter increments is 8 bytes wide and 8-byte aligned.
/// Both halves matter: the instruction is a 64-bit `add`, and the
/// no-tearing argument in the module comment is an argument about ALIGNED
/// accesses.
#[test]
fn entry_counter_word_is_an_aligned_u64() {
    // `count` is the first field (offset pinned by
    // `entry_counter_count_offset_is_zero`) and is a u64 wide.
    assert_eq!(std::mem::size_of::<AtomicU64>(), 8);
    assert!(std::mem::align_of::<JitEntryCounter>() >= 8);
    let c = JitEntryCounter::new();
    assert_eq!(c.count_addr() % 8, 0, "counter word must be 8-byte aligned");
}

/// One counter per cache line, so two hot methods' entry stores never
/// share a line (see the struct doc). A boxed counter must honour it too.
#[test]
fn entry_counter_owns_its_cache_line() {
    assert_eq!(std::mem::align_of::<JitEntryCounter>(), 64);
    assert_eq!(std::mem::size_of::<JitEntryCounter>(), 64);
    let a = Box::new(JitEntryCounter::new());
    let b = Box::new(JitEntryCounter::new());
    assert_eq!(a.count_addr() % 64, 0);
    assert_eq!(b.count_addr() % 64, 0);
    assert_ne!(a.count_addr() / 64, b.count_addr() / 64);
}

/// Default-ON with an explicit kill switch (round 9 wave 3). Unset — the
/// default configuration — must arm the counter, or the hotness door into
/// the optimizing tier is dead again for every method the structural
/// door refuses; the opt-out words must disarm it, or the kill switch is
/// not one.
#[test]
fn the_entry_counter_is_default_on_with_a_kill_switch() {
    assert!(entry_counter_armed_by(None), "unset must arm the counter");
    for on in ["1", "true", "on", "yes", ""] {
        assert!(entry_counter_armed_by(Some(on)), "{on:?} must arm it");
    }
    for off in ["0", "false", "off", "no", " OFF ", "No"] {
        assert!(!entry_counter_armed_by(Some(off)), "{off:?} must disarm it");
    }
}

#[test]
fn a_fresh_counter_reads_zero() {
    assert_eq!(JitEntryCounter::new().get(), 0);
    assert_eq!(JitEntryCounter::default().get(), 0);
}

#[test]
fn bumping_is_visible_to_the_reader() {
    let c = JitEntryCounter::new();
    c.bump_for_test();
    c.bump_for_test();
    assert_eq!(c.get(), 2);
    c.set_for_test(19_999);
    assert_eq!(c.get(), 19_999);
}

/// The drain reports a row only when its count moved since the previous
/// drain (r11-tier-entry-count-drain-rereports-every-entered-body).
#[test]
fn take_if_moved_reports_only_changes() {
    let c = JitEntryCounter::new();
    assert_eq!(c.take_if_moved(), None, "a never-entered body is not a row");
    c.bump_for_test();
    assert_eq!(c.take_if_moved(), Some(1));
    assert_eq!(
        c.take_if_moved(),
        None,
        "an unmoved count is not re-reported"
    );
    c.bump_for_test();
    assert_eq!(c.take_if_moved(), Some(2));
    assert_eq!(c.get(), 2, "draining does not consume the count itself");
}

/// The back-edge word sits where the emitter addresses it (base +
/// `BACK_EDGE_OFFSET`, a disp8), is an aligned `u64`, and adding it moved
/// neither older field nor the one-line size.
#[test]
fn back_edge_offset_matches_the_field() {
    let c = JitEntryCounter::new();
    let base = c.base_addr();
    let field = &c.back_edges as *const AtomicU64 as usize;
    assert_eq!(field - base, JitEntryCounter::BACK_EDGE_OFFSET as usize);
    assert!(i8::try_from(JitEntryCounter::BACK_EDGE_OFFSET).is_ok());
    assert_eq!(field % 8, 0);
    assert_eq!(c.count_addr() - base, 0, "the entry word did not move");
    assert_eq!(std::mem::size_of::<JitEntryCounter>(), 64);
}


/// HANDLER7-1 (`CRATONVM_JIT_C1_BACK_EDGE_C2_BAR`): off, the weight is the
/// 8:1 sum exactly; on, a loop-dominated body that took at least 16 entries
/// reports `back_edges / 2`, so the R19Handler6 kernel shape (400 entries x
/// 256 iterations) crosses the default 20 000 bar where the 8:1 sum leaves
/// it at 13 200; a body entered fewer than 16 times keeps the 8:1 sum.
#[test]
fn the_back_edge_bar_reports_loop_dominated_bodies_at_two_to_one() {
    let (entries, back_edges) = (400u64, 400u64 * 256);
    assert_eq!(weighted_count(entries, back_edges, false), 13_200);
    assert_eq!(weighted_count(entries, back_edges, true), 51_200);
    // HotSpot's Tier4BackEdgeThreshold is the door under the default bar.
    assert_eq!(weighted_count(16, 39_998, true), 19_999);
    assert_eq!(weighted_count(16, 40_000, true), 20_000);
    // Below the entry floor the switch changes nothing.
    assert_eq!(
        weighted_count(15, 1_000_000, true),
        weighted_count(15, 1_000_000, false)
    );
    // An entry-dominated body keeps its 8:1 sum (the larger term).
    assert_eq!(weighted_count(30_000, 8_000, true), 31_000);
    // Saturates, never wraps.
    assert_eq!(weighted_count(u64::MAX, u64::MAX, true), u64::MAX);
    // The off arm is the drain's own number.
    let c = JitEntryCounter::new();
    c.set_for_test(entries);
    c.set_back_edges_for_test(back_edges);
    if !c1_back_edge_c2_bar_enabled() {
        assert_eq!(c.take_if_moved(), Some(13_200));
    }
}

/// j27 TIER: the trip word sits where the prologue check addresses it
/// (base + `TRIP_AT_OFFSET`, a disp8), and adding it and the sink kept
/// the box at one cache line with every older offset unchanged.
#[test]
fn j27_trip_at_offset_matches_the_field() {
    let c = JitEntryCounter::new();
    let base = c.base_addr();
    let field = &c.trip_at as *const AtomicU64 as usize;
    assert_eq!(field - base, JitEntryCounter::TRIP_AT_OFFSET as usize);
    assert!(i8::try_from(JitEntryCounter::TRIP_AT_OFFSET).is_ok());
    assert_eq!(field % 8, 0);
    assert_eq!(c.count_addr() - base, 0);
    assert_eq!(
        &c.back_edges as *const AtomicU64 as usize - base,
        JitEntryCounter::BACK_EDGE_OFFSET as usize
    );
    assert_eq!(std::mem::size_of::<JitEntryCounter>(), 64);
}

/// j27 TIER: a fresh (and a `Default`) box is disarmed; an armed box
/// trips once, delivers once, and is disarmed again; a second call (a
/// thread that raced past the bar) delivers nothing; an unarmed box's
/// helper call is a no-op, and so is a null pointer.
#[test]
fn j27_the_entry_trip_is_one_shot_and_disarmed_by_default() {
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;
    assert_eq!(JitEntryCounter::new().trip_at(), u64::MAX);
    assert_eq!(JitEntryCounter::default().trip_at(), u64::MAX);
    let woken = Arc::new(AtomicUsize::new(0));
    let w = Arc::clone(&woken);
    let sink = Arc::new(EntryTripSink::new(Box::new(move || {
        w.fetch_add(1, Ordering::Relaxed);
    })));
    let c = Box::new(JitEntryCounter::new());
    let ptr: *const JitEntryCounter = &*c;
    jit_entry_counter_tripped(ptr);
    assert_eq!(woken.load(Ordering::Relaxed), 0, "unarmed: nothing");
    c.arm_trip(20_000, &sink);
    assert_eq!(c.trip_at(), 20_000);
    jit_entry_counter_tripped(ptr);
    assert_eq!(woken.load(Ordering::Relaxed), 1);
    assert_eq!(sink.trips(), 1);
    assert_eq!(c.trip_at(), u64::MAX, "the trip disarms itself");
    jit_entry_counter_tripped(ptr);
    assert_eq!(woken.load(Ordering::Relaxed), 1, "one-shot");
    // Re-arming re-opens it, with the first sink kept.
    c.arm_trip(30_000, &sink);
    jit_entry_counter_tripped(ptr);
    assert_eq!(woken.load(Ordering::Relaxed), 2);
    jit_entry_counter_tripped(std::ptr::null());
    // A panicking sink does not unwind out of the extern "C" helper.
    let bad = Arc::new(EntryTripSink::new(Box::new(|| panic!("sink"))));
    let d = JitEntryCounter::new();
    d.arm_trip(1, &bad);
    jit_entry_counter_tripped(&d);
    assert_eq!(d.trip_at(), u64::MAX);
}

/// A boxed counter's address is stable across moves of the `Box`, which is
/// the property that makes it legal to bake into an immediate at codegen
/// time and then move the box onto the `CompiledMethod`. If this ever
/// stopped holding, every published body would be incrementing a freed
/// word.
#[test]
fn boxing_keeps_the_address_stable() {
    let boxed = Box::new(JitEntryCounter::new());
    let addr = boxed.count_addr();
    let moved = boxed;
    assert_eq!(moved.count_addr(), addr);
    let in_a_vec = vec![moved];
    assert_eq!(in_a_vec[0].count_addr(), addr);
}
