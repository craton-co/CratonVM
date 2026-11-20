use super::*;

fn aligned_buffer(bytes: usize) -> (Vec<u8>, *mut u8, usize) {
    let mut buf = vec![0u8; bytes + 8];
    let raw = buf.as_mut_ptr();
    let base = ((raw as usize + 7) & !7) as *mut u8;
    (buf, base, bytes)
}

/// The counters cost the hot `Tlab` nothing: eight bytes in its trailing
/// padding and four after `tuning`.
#[test]
fn gci_i28tl_the_cycle_counters_do_not_grow_the_tlab() {
    assert_eq!(std::mem::size_of::<TlabCycleCounters>(), 8);
    #[cfg(target_arch = "x86_64")]
    assert_eq!(std::mem::size_of::<Tlab>(), 272, "Tlab grew");
}

/// A buried tail is collection waste; a refill's retire moves it to the
/// refill bucket; the refill site's adopt carries every counter and counts
/// the refill; a keep is a slow allocation.
#[test]
fn gci_i28tl_retire_refill_and_keep_feed_the_counters() {
    let (_owner, base, usable) = aligned_buffer(64 * 1024);
    // SAFETY: `_owner` keeps the zeroed, 8-aligned buffer alive.
    let mut tlab = unsafe { Tlab::new(base, usable) };
    tlab.alloc(usable - 4096, 8).unwrap();
    tlab.retire_for_refill();
    let m = tlab.cycle_mark();
    assert_eq!(m.since_start().refill_waste_bytes, 4096);
    assert_eq!(m.since_start().gc_waste_bytes, 0, "a refill's tail is not gc waste");

    let (_owner2, base2, usable2) = aligned_buffer(64 * 1024);
    // SAFETY: as above.
    let mut next = unsafe { Tlab::new(base2, usable2) };
    next.adopt_cycle_mark_for_refill(m);
    assert_eq!(next.cycle_mark().since_start().refills, 1);
    assert_eq!(next.cycle_mark().since_start().refill_waste_bytes, 4096);

    next.arm_share_sizer(true);
    let mut sized = TlabShareSizer {
        window_epoch: 0,
        ..TlabShareSizer::default()
    };
    sized.set_desired(64 * 1024);
    next.adopt_share_sizer(sized);
    next.alloc(usable2 - 4096, 8).unwrap();
    assert!(next.keep_on_miss(8192));
    assert_eq!(next.cycle_mark().since_start().slow_allocations, 1);

    next.retire();
    let d = next.cycle_mark().since(m);
    assert_eq!(d.refills, 1);
    assert_eq!(d.slow_allocations, 1);
    assert_eq!(d.gc_waste_bytes, 4096, "the pause-time tail is gc waste");
    assert_eq!(d.refill_waste_bytes, 0);
    next.retire();
    assert_eq!(next.cycle_mark().since(m), d, "an idempotent retire adds nothing");
}

/// Differences survive the counters' wrap. (gci i43/tj: the stranger
/// half of this test, a refused difference above `0x8000`, is gone with
/// the plausibility test; see `gci_i43_tj_a_hot_cycle_above_32768_refills_is_counted`.)
#[test]
fn gci_i28tl_marks_difference_across_a_wrap() {
    let before = TlabCycleMark {
        // gcj j2/yg2: the counters are 24 bits wide now.
        refills: TLAB_CYCLE_COUNTER_MASK,
        slow_allocations: 3,
        refill_waste_words: u32::MAX,
        gc_waste_words: 10,
    };
    let after = TlabCycleMark {
        refills: 1,
        slow_allocations: 5,
        refill_waste_words: 1,
        gc_waste_words: 12,
    };
    let d = after.since(before);
    assert_eq!((d.refills, d.slow_allocations), (2, 2));
    assert_eq!((d.refill_waste_bytes, d.gc_waste_bytes), (16, 16));
}

/// gcj j2/yg2 (`gcj-j1yg-tlab-totals-counters-wrap-above-65536-refills-per-cycle`):
/// a mark pair 70 000 refills (and slow allocations) apart on one thread
/// reports 70 000; the refill site's carry crosses the 16-bit boundary
/// into the share sizer's high byte and travels with the sizer; the
/// counters wrap at 2^24.
#[test]
fn gcj_j2yg2_a_cycle_of_70000_refills_is_counted() {
    let before = TlabCycleMark {
        refills: 10,
        slow_allocations: 20,
        ..TlabCycleMark::default()
    };
    let after = TlabCycleMark {
        refills: 70_010,
        slow_allocations: 70_020,
        ..TlabCycleMark::default()
    };
    let d = after.since(before);
    assert_eq!((d.refills, d.slow_allocations), (70_000, 70_000));

    let (_owner, base, usable) = aligned_buffer(16 * 1024);
    // SAFETY: `_owner` keeps the zeroed, 8-aligned buffer alive.
    let mut tlab = unsafe { Tlab::new(base, usable) };
    let at_edge = TlabCycleMark {
        refills: 0xFFFF,
        slow_allocations: 0x1_0005,
        ..TlabCycleMark::default()
    };
    tlab.adopt_cycle_mark_for_refill(at_edge);
    let m = tlab.cycle_mark();
    assert_eq!((m.refills, m.slow_allocations), (0x1_0000, 0x1_0005));
    assert_eq!(m.since(at_edge).refills, 1);

    let (_owner2, base2, usable2) = aligned_buffer(16 * 1024);
    // SAFETY: as above.
    let mut next = unsafe { Tlab::new(base2, usable2) };
    next.adopt_share_sizer(tlab.share_sizer());
    next.adopt_cycle_mark_for_refill(tlab.cycle_mark());
    assert_eq!(next.cycle_mark().refills, 0x1_0001, "the high byte travels");

    let top = TlabCycleMark {
        refills: TLAB_CYCLE_COUNTER_MASK,
        ..TlabCycleMark::default()
    };
    next.adopt_cycle_mark_for_refill(top);
    assert_eq!(next.cycle_mark().refills, 0, "the counter wraps at 2^24");
    assert_eq!(next.cycle_mark().since(top).refills, 1);
}

/// gci i43/tj (`gci-i42ta-tlab-totals-refill-counter-wraps-on-a-hot-thread-RETIRED-UNFIXED-20261018`):
/// one thread whose cycle holds 40 000 refills and 40 000 slow
/// allocations (more than `0x8000`, across the `u16` wrap) reports this
/// cycle's counts, not its whole history.
#[test]
fn gci_i43_tj_a_hot_cycle_above_32768_refills_is_counted() {
    let before = TlabCycleMark {
        refills: 50_000,
        slow_allocations: 30_000,
        refill_waste_words: 7,
        gc_waste_words: 9,
    };
    let after = TlabCycleMark {
        // 50 000 + 40 000 = 90 000 (gcj j2/yg2: no longer wrapped at
        // 65 536; the counters are 24 bits wide).
        refills: 90_000,
        // 30 000 + 40 000 = 70 000.
        slow_allocations: 70_000,
        refill_waste_words: 107,
        gc_waste_words: 19,
    };
    let d = after.since(before);
    assert_eq!((d.refills, d.slow_allocations), (40_000, 40_000));
    assert_eq!((d.refill_waste_bytes, d.gc_waste_bytes), (800, 80));

    // Through the per-VM state, keyed by thread id: the second publish is
    // this cycle alone (40 000 refills, the cycle's allocated bytes).
    let state = TlabTotalsState::default();
    let first = state.publish(vec![(7, before, 1_000)]);
    assert_eq!(first.refills, 50_000, "a new thread counts from its start");
    let second = state.publish(vec![(7, after, 1_000 + 40_000 * 262_144)]);
    assert_eq!((second.threads, second.refills, second.max_refills), (1, 40_000, 40_000));
    assert_eq!(second.slow_allocations, 40_000);
    assert_eq!(second.allocated_bytes, 40_000 * 262_144);
    assert_eq!((second.refill_waste_bytes, second.gc_waste_bytes), (800, 80));
}

/// gci i43/tj: a key whose allocated total went backwards is a fresh
/// thread history (counted from its start), whatever its counters say.
#[test]
fn gci_i43_tj_an_allocated_total_that_went_backwards_counts_from_the_start() {
    let state = TlabTotalsState::default();
    let mark = |refills: u32| TlabCycleMark {
        refills,
        ..TlabCycleMark::default()
    };
    state.publish(vec![(3, mark(10), 5_000)]);
    let t = state.publish(vec![(3, mark(12), 100)]);
    assert_eq!((t.refills, t.allocated_bytes), (12, 100));
}

/// HotSpot 25's line, field for field (`ThreadLocalAllocStats::publish`):
/// two spaces after the thread count, `max` with no colon after the slow
/// allocations, `%4.1f%%`, byte figures with a `B`.
#[test]
fn gci_i28tl_the_line_is_hotspots_publish_line() {
    let mut t = TlabTotals::default();
    assert_eq!(t.line(), None, "publish prints nothing when nothing was allocated");
    t.add_thread(
        TlabCycleDelta {
            refills: 3,
            slow_allocations: 1,
            gc_waste_bytes: 64,
            refill_waste_bytes: 1024,
        },
        100_000,
    );
    t.add_thread(
        TlabCycleDelta {
            refills: 0,
            slow_allocations: 2,
            gc_waste_bytes: 0,
            refill_waste_bytes: 0,
        },
        5_000,
    );
    assert_eq!(
        t.line().as_deref(),
        Some(
            "TLAB totals: thrds: 1  refills: 3 max: 3 slow allocs: 3 max 2 waste:  1.1% \
             gc: 64B max: 64B slow: 1024B max: 1024B"
        )
    );
}

/// gci i30/ob hand-back 1: any refill prints the line (HotSpot's
/// `_allocated_size` grows at every refill), even when this VM's
/// allocated-byte counter has not moved yet; the waste share is of the
/// handed-out bytes, so it never passes 100 %.
#[test]
fn gci_i30ob_any_refill_prints_the_line_and_waste_stays_a_share() {
    let mut t = TlabTotals::default();
    t.add_thread(
        TlabCycleDelta {
            refills: 1,
            slow_allocations: 0,
            gc_waste_bytes: 262_032,
            refill_waste_bytes: 0,
        },
        0,
    );
    assert_eq!(
        t.line().as_deref(),
        Some(
            "TLAB totals: thrds: 1  refills: 1 max: 1 slow allocs: 0 max 0 waste: 100.0% \
             gc: 262032B max: 262032B slow: 0B max: 0B"
        )
    );
    let mut u = TlabTotals::default();
    u.add_thread(
        TlabCycleDelta {
            refills: 1,
            slow_allocations: 0,
            gc_waste_bytes: 262_032,
            refill_waste_bytes: 0,
        },
        176,
    );
    let line = u.line().expect("a refill prints");
    assert!(line.contains("waste: 99.9%"), "{line}");
}

/// The per-VM state differences each thread against its own previous
/// mark, counts a new thread from its start, and drops dead threads.
#[test]
fn gci_i28tl_the_state_differences_per_thread_and_forgets_the_dead() {
    let state = TlabTotalsState::default();
    let mark = |refills: u32| TlabCycleMark {
        refills,
        ..TlabCycleMark::default()
    };
    let first = state.publish(vec![(0x1000, mark(4), 4000), (0x2000, mark(1), 1000)]);
    assert_eq!((first.threads, first.refills, first.max_refills), (2, 5, 4));
    assert_eq!(first.allocated_bytes, 5000);
    let second = state.publish(vec![(0x2000, mark(3), 3000), (0x3000, mark(1), 500)]);
    assert_eq!((second.threads, second.refills, second.max_refills), (2, 3, 2));
    assert_eq!(second.allocated_bytes, 2500);
    let third = state.publish(vec![(0x1000, mark(1), 100)]);
    assert_eq!(third.refills, 1, "0x1000 was forgotten: counted from its start");
    state.stash(Some("x".to_string()));
    assert_eq!(state.take().as_deref(), Some("x"));
    assert_eq!(state.take(), None);
}
