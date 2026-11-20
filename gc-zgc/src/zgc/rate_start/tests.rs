//! zt t26/zc9: the reader, the rule as arithmetic (hand-worked numbers),
//! and the request / drop / reset protocol on a real heap.

use super::*;
use cratonvm_gc_core::collector::GarbageCollector;


/// The rule by hand. `rate_scaled = 2^20` is 1 byte per nanosecond and
/// `mark_nanos = 2_000_000` a 2 ms phase, so the expected allocation is
/// `(2^20 * 2_000_000) >> 20 = 2_000_000` bytes, times 3/2 = 3_000_000.
/// Capacity 100_000_000, reserve 5_000_000 (grace 2_500_000). Headroom
/// needs free > 5_000_000 + 3_000_000 = 8_000_000; the rule's point is free
/// <= 5_000_000 + 2 * 3_000_000 = 11_000_000, i.e. used >= 89_000_000.
#[test]
fn zt26zc9_the_rule_by_hand() {
    let v = |warm, used, since, rate, mark| {
        rate_start_verdict(warm, 100_000_000, used, 5_000_000, since, rate, mark)
    };
    let (r, m) = (1u64 << 20, 2_000_000u64);
    assert_eq!(
        v(true, 89_000_000, 10_000_000, r, m),
        ZRateStartVerdict::Due,
        "at the point"
    );
    assert_eq!(
        v(true, 88_999_999, 10_000_000, r, m),
        ZRateStartVerdict::NotYet,
        "one byte early"
    );
    assert_eq!(
        v(true, 91_999_999, 10_000_000, r, m),
        ZRateStartVerdict::Due,
        "free 8_000_001"
    );
    assert_eq!(
        v(true, 92_000_000, 10_000_000, r, m),
        ZRateStartVerdict::NoHeadroom,
        "free 8_000_000: no room to finish a mark"
    );
    assert_eq!(
        v(false, 89_000_000, 10_000_000, r, m),
        ZRateStartVerdict::NotWarm
    );
    assert_eq!(
        v(true, 89_000_000, 10_000_000, 0, m),
        ZRateStartVerdict::NoSample
    );
    assert_eq!(
        v(true, 89_000_000, 10_000_000, r, 0),
        ZRateStartVerdict::NoSample
    );
    assert_eq!(
        v(true, 89_000_000, 2_499_999, r, m),
        ZRateStartVerdict::TooSoon,
        "less than a grace since the last collection"
    );
    assert_eq!(v(true, 89_000_000, 2_500_000, r, m), ZRateStartVerdict::Due);
}

/// A 64 MiB heap with the every-trigger mode acting, the warm-up clause
/// off (so the rule is warm), the rule seeded `on`, 1 byte per nanosecond
/// (`2^20` scaled) and a phase of `reserve` nanoseconds, so the expected
/// allocation is `E = reserve * 3 / 2` and the rule is due for free space
/// in `(reserve + E, reserve + 2E]`. On 64 MiB the reserve is
/// `max(64 MiB / 20, the 8 MiB headroom margin) = 8_388_608`, `E =
/// 12_582_912`, the grace (half the reserve) `4_194_304 = E / 3`; the
/// numbers below are written in `E` so they hold for any reserve.
fn rate_heap(on: bool) -> std::sync::Arc<ZgcRealHeap> {
    let heap = ZgcRealHeap::new_shared(64 * 1024 * 1024);
    heap.set_relocation_enabled(false);
    heap.dbg_set_conc_start_bytes(heap.gc_threshold * 60 / 100);
    heap.dbg_set_conc_every_trigger(true);
    heap.counters.warmup_enabled.store(false, Ordering::Relaxed);
    heap.counters.rstart.enabled.store(on, Ordering::Relaxed);
    heap.alloc_rate_scaled.store(1 << 20, Ordering::Relaxed);
    let reserve = heap.conc_trigger_reserve() as u64;
    heap.conc_mark_nanos_ewma.store(reserve, Ordering::Relaxed);
    heap.cycle_alloc_watermark.store(0, Ordering::Relaxed);
    heap
}

/// `E`, the expected allocation during a mark on [`rate_heap`].
fn expected(heap: &ZgcRealHeap) -> usize {
    let reserve = heap.conc_trigger_reserve();
    let e = director::conc_expected_mark_allocation(1 << 20, reserve as u64)
        .expect("rate and phase are seeded") as usize;
    assert_eq!(e, reserve * 3 / 2, "E = reserve * 3 / 2");
    e
}

/// Free space `reserve + 2E - E / 8`: inside the rule's band, and still
/// inside it after a grace (`E / 3`) more allocation (`2E - E/8 - E/3 > E`).
fn due_extra(heap: &ZgcRealHeap) -> usize {
    let e = expected(heap);
    2 * e - e / 8
}

/// `used` at which free space is `reserve + extra`.
fn used_at(heap: &ZgcRealHeap, extra: usize) -> usize {
    heap.heap_capacity() - heap.conc_trigger_reserve() - extra
}

fn requested_at(heap: &ZgcRealHeap) -> usize {
    heap.counters
        .conc_trigger
        .requested_at
        .load(Ordering::Relaxed)
}

/// The request: at the rule's point `needs_gc` answers no and records the
/// rule's request; the kill (seeded off) and a poll above the point (the
/// control) record nothing. A collection's accounting resets the slot and
/// the rule's ownership.
#[test]
fn zt26zc9_a_due_poll_records_the_rules_request() {
    let off = rate_heap(false);
    let due = used_at(&off, due_extra(&off));
    assert!(!off.conc_rate_start_request(due), "the kill");
    assert_eq!(requested_at(&off), 0);
    assert!(!off.rate_start_report().enabled);

    let heap = rate_heap(true);
    assert!(heap.rate_start_report().enabled);
    let e = expected(&heap);
    assert!(
        !heap.conc_rate_start_request(used_at(&heap, 2 * e + e / 2)),
        "control: free = reserve + 2.5E is above the point (reserve + 2E)"
    );
    assert_eq!(requested_at(&heap), 0);
    let due = used_at(&heap, due_extra(&heap));
    heap.allocated.store(due, Ordering::Relaxed);
    assert!(!heap.needs_gc(), "the request holds the collection");
    assert_eq!(requested_at(&heap), due);
    assert!(heap.counters.rstart.pending.load(Ordering::Relaxed));
    assert!(
        heap.conc_start_requested(),
        "a poll site would open the cycle"
    );
    let r = heap.rate_start_report();
    assert_eq!((r.requests, r.opened, r.dropped), (1, 0, 0));
    assert_eq!(heap.conc_trigger_report().requests, 1);
    heap.note_conc_start_outcome(true);
    assert_eq!(
        heap.rate_start_report().opened,
        1,
        "opened on the rule's request"
    );
    heap.conc_every_trigger_account_collection();
    assert_eq!(requested_at(&heap), 0, "the collection resets the slot");
    assert!(!heap.counters.rstart.pending.load(Ordering::Relaxed));
}

/// The drop: the rule's request whose mark start was refused does not
/// collect (the gate falls through) and the rule holds for a grace; a
/// clause's refused request (the control) collects; so does the rule's at
/// the urgent reserve. A collection lifts the hold.
#[test]
fn zt26zc9_a_refused_rate_request_is_dropped_not_collected() {
    let heap = rate_heap(true);
    let due = used_at(&heap, due_extra(&heap));
    assert!(heap.conc_rate_start_request(due));
    heap.note_conc_start_outcome(false);
    assert_eq!(
        heap.conc_every_trigger_gate(due),
        None,
        "the rule's lapsed request is dropped, the clauses decide"
    );
    assert_eq!(requested_at(&heap), 0);
    assert_eq!(heap.rate_start_report().dropped, 1);
    assert!(
        !heap.conc_rate_start_request(due + 1),
        "held for a grace of allocation"
    );
    // Free space reserve + 2E - E/8 - E/3: still in the band.
    let grace = director::conc_request_grace(heap.conc_trigger_reserve());
    assert!(
        heap.conc_rate_start_request(due + grace),
        "asks again after the grace"
    );
    // The urgent reserve: the rule's own lapse collects.
    let full = heap.heap_capacity() - heap.conc_trigger_reserve();
    heap.note_conc_start_outcome(false);
    assert_eq!(heap.conc_every_trigger_gate(full), Some(true));
    heap.conc_every_trigger_account_collection();
    assert_eq!(
        heap.counters.rstart.hold_until.load(Ordering::Relaxed),
        0,
        "the collection lifts the hold"
    );
    // Control: a clause's request (free = reserve + 2E - E/8 > reserve + E,
    // so `conc_every_trigger_early` records it) lapses into its collection
    // as before.
    assert!(!heap.conc_every_trigger_early(due));
    assert_ne!(requested_at(&heap), 0);
    assert!(!heap.counters.rstart.pending.load(Ordering::Relaxed));
    heap.note_conc_start_outcome(false);
    assert_eq!(heap.conc_every_trigger_gate(due), Some(true));
    assert_eq!(heap.rate_start_report().dropped, 1, "only the rule's own");
}
