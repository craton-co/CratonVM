//! zt t26/zc9: the allocation-rate start of an every-trigger cycle
//! (`CRATONVM_ZGC_CONCURRENT_RATE_START`, **opt-in**; on under the concurrent
//! profile `CRATONVM_ZGC_CONCURRENT=1`, whose `=0` on this key kills it
//! there). Stage 1a of
//! `docs/feature-designs/zgc-architecture.md`.
//!
//! # What the rows showed (`t26-zc9-report.md` section 2)
//!
//! Under the profile `MtChurnProbe 8 2500 384 -Xmx1g` spent 17.4 s of its
//! 54.9 s wall in stop-the-world collections (HotSpot's whole run: 20.8 s), and
//! 35 of its 53 collections were stop-the-world MARKED although every trigger
//! was meant to open a concurrent mark: `ct_no_headroom=25` (the clause said yes
//! at a point where the heap could no longer finish a mark, so
//! `conc_every_trigger_early` collected) and `ct_lapsed=10` (the mark start was
//! refused by the take-over cover). The clauses fire late by construction -- the
//! live-bytes threshold sits at 75 % of `-Xmx` -- and every collection a late
//! clause starts is the expensive kind: a stop-the-world mark, and a concurrent
//! close that comes back too full to defer its sweep (`csw_last_refusal=6`,
//! urgent) and so sweeps and slides inside the pause.
//!
//! # The rule
//!
//! HotSpot 25's director starts a cycle when the time until the heap is out
//! at the sampled allocation rate, with `ZAllocationSpikeTolerance` (default
//! 2), falls to the expected duration of a GC (`rule_major_allocation_rate`,
//! recalled from the HotSpot 25 sources, not cited line by line), and only
//! once the warm-up cycles are done. In bytes against this heap's own
//! measurements -- the smoothed allocation rate and concurrent-phase duration
//! the every-trigger mode already keeps (`alloc_rate_scaled`,
//! `conc_mark_nanos_ewma`), whose product with the 3/2 safety factor is
//! [`director::conc_expected_mark_allocation`] -- a cycle is DUE when
//!
//! ```text
//! free <= reserve + SPIKE_TOLERANCE * expected
//! ```
//!
//! while the heap still has the headroom to finish a mark
//! (`free > reserve + expected`, [`director::conc_cycle_has_headroom`]'s own
//! test: past it the clauses decide as before), at least
//! [`director::conc_request_grace`] bytes after the last collection (no cycle
//! over a heap nothing has been allocated into since the last one), and only
//! after the warm-up cycles. A due poll records an every-trigger request, as an
//! early clause yes does; the poll site that sees it opens the cycle.
//!
//! # A request the rule made never becomes a collection by itself
//!
//! A request whose mark start is refused (the take-over cover) or that no
//! site serves within its grace LAPSES, and a lapsed request is a
//! stop-the-world collection (`ConcTriggerGate::RequestLapsed`). That is right
//! for a clause's request -- the clause wanted a collection -- and wrong for
//! this rule's, which fires well before any clause does: a refused early start
//! would become an early stop-the-world collection, the very thing the rule
//! exists to avoid. So the rule's own lapsed request is DROPPED instead
//! (`crs_dropped`), the poll falls through to the clauses, and the rule waits
//! another grace of allocation before it asks again. At the urgent reserve the
//! lapse collects, as for any request.
//!
//! Census at exit (`CRATONVM_GC_STATS=1`): `[GC] zgc-rate-start: crs_enabled=
//! crs_requests= crs_opened= crs_dropped=`.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::{director, ZgcRealHeap};

/// `CRATONVM_ZGC_CONCURRENT_RATE_START` (zt t26/zc9, **opt-in**; on under
/// `CRATONVM_ZGC_CONCURRENT=1`, whose `=0` kills it there): an every-trigger
/// heap asks for its concurrent cycle at HotSpot's allocation-rate point
/// (module doc). Read once per heap, at construction ([`ZRateStartState::new`]).
pub(crate) fn zgc_concurrent_rate_start_enabled() -> bool {
    match cratonvm_types::flags::runtime_var("CRATONVM_ZGC_CONCURRENT_RATE_START") {
        Ok(value) if !value.trim().is_empty() => !cratonvm_types::flags::value_is_off(&value),
        _ => super::zgc_concurrent_profile_enabled(),
    }
}

/// HotSpot's `ZAllocationSpikeTolerance` default: the expected allocation
/// during a mark is doubled before it is compared with the free space.
pub const ZGC_RATE_START_SPIKE_TOLERANCE: u64 = 2;

/// What the rule says at one poll ([`rate_start_verdict`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZRateStartVerdict {
    /// The warm-up cycles are not done (HotSpot's rate rule waits for them;
    /// the warm-up clause starts those cycles).
    NotWarm,
    /// No allocation rate or no concurrent-phase duration measured yet.
    NoSample,
    /// Less than a request's grace allocated since the last collection.
    TooSoon,
    /// Free space still above the rule's point.
    NotYet,
    /// The heap can no longer finish a mark opened now: the clauses decide
    /// (and collect stop-the-world, as before).
    NoHeadroom,
    /// Ask for the cycle.
    Due,
}

/// The rule, as arithmetic. `used` is `allocated`, `since_last` the bytes
/// allocated since the last collection, `rate_scaled` / `mark_nanos` the
/// smoothed rate (bytes per nanosecond, scaled by 2^20) and concurrent-phase
/// duration. Pure, so it is unit-tested without a heap.
pub fn rate_start_verdict(
    warm: bool,
    capacity: usize,
    used: usize,
    reserve: usize,
    since_last: usize,
    rate_scaled: u64,
    mark_nanos: u64,
) -> ZRateStartVerdict {
    if !warm {
        return ZRateStartVerdict::NotWarm;
    }
    let Some(expected) = director::conc_expected_mark_allocation(rate_scaled, mark_nanos) else {
        return ZRateStartVerdict::NoSample;
    };
    if since_last < director::conc_request_grace(reserve) {
        return ZRateStartVerdict::TooSoon;
    }
    let free = capacity.saturating_sub(used) as u64;
    let reserve = reserve as u64;
    if free <= reserve.saturating_add(expected) {
        return ZRateStartVerdict::NoHeadroom;
    }
    if free <= reserve.saturating_add(expected.saturating_mul(ZGC_RATE_START_SPIKE_TOLERANCE)) {
        return ZRateStartVerdict::Due;
    }
    ZRateStartVerdict::NotYet
}

/// The per-heap state (inside the boxed `ZgcCounters`, so `ZgcRealHeap`'s size
/// ratchet is untouched).
pub(crate) struct ZRateStartState {
    /// `CRATONVM_ZGC_CONCURRENT_RATE_START` on this heap (construction seed).
    enabled: AtomicBool,
    /// The pending every-trigger request (`ZConcTriggerState::requested_at`)
    /// is this rule's. Cleared by every collection, by a `System.gc()`
    /// request taking the slot over, and by the drop of a lapsed request.
    pending: AtomicBool,
    /// After a dropped request: `allocated` below which the rule does not ask
    /// again (`0`: no hold). Cleared by every collection.
    hold_until: AtomicUsize,
    /// Engagement, cumulative: requests the rule recorded, cycles that opened
    /// on one, and lapsed requests dropped instead of collected.
    requests: AtomicUsize,
    opened: AtomicUsize,
    dropped: AtomicUsize,
}

/// The `[GC] zgc-rate-start:` summary figures.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ZRateStartReport {
    pub enabled: bool,
    pub requests: usize,
    pub opened: usize,
    pub dropped: usize,
}

impl ZRateStartState {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            enabled: AtomicBool::new(enabled),
            pending: AtomicBool::new(false),
            hold_until: AtomicUsize::new(0),
            requests: AtomicUsize::new(0),
            opened: AtomicUsize::new(0),
            dropped: AtomicUsize::new(0),
        }
    }
}

impl ZgcRealHeap {
    /// `needs_gc`'s rule under the every-trigger mode, asked after the gate
    /// found no cycle open and none requested. `true`: a request is recorded
    /// (or another poll's appeared meanwhile) and `needs_gc` answers no. One
    /// relaxed load with the switch off.
    pub(super) fn conc_rate_start_request(&self, a: usize) -> bool {
        let st = &self.counters.rstart;
        if !st.enabled.load(Ordering::Relaxed) {
            return false;
        }
        let hold = st.hold_until.load(Ordering::Relaxed);
        if hold != 0 && a < hold {
            return false;
        }
        let warm = !self.counters.warmup_enabled.load(Ordering::Relaxed)
            || self.warmup_cycles_done() >= super::ZGC_WARMUP_CYCLES;
        let watermark = self.cycle_alloc_watermark.load(Ordering::Relaxed);
        let verdict = rate_start_verdict(
            warm,
            self.heap_capacity(),
            a,
            self.conc_trigger_reserve(),
            a.saturating_sub(watermark),
            self.alloc_rate_scaled.load(Ordering::Relaxed),
            self.conc_mark_nanos_ewma.load(Ordering::Relaxed),
        );
        if verdict != ZRateStartVerdict::Due {
            return false;
        }
        let ct = &self.counters.conc_trigger;
        if ct
            .requested_at
            .compare_exchange(0, a.max(1), Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            ct.refused.store(false, Ordering::Relaxed);
            ct.requests.fetch_add(1, Ordering::Relaxed);
            st.pending.store(true, Ordering::Relaxed);
            st.requests.fetch_add(1, Ordering::Relaxed);
        }
        true
    }

    /// The gate answered `RequestLapsed` for the pending request made at
    /// `requested_at`. When that request is this rule's and free space is
    /// still above the urgent `reserve`, drop it (the poll falls through to
    /// the clauses) and hold the rule for a grace of allocation. `true`:
    /// dropped. A request a clause made, or one at the urgent reserve, lapses
    /// into its collection as before.
    pub(super) fn conc_rate_start_drop_lapsed(
        &self,
        requested_at: usize,
        used: usize,
        capacity: usize,
        reserve: usize,
    ) -> bool {
        let st = &self.counters.rstart;
        if !st.pending.load(Ordering::Relaxed) || capacity.saturating_sub(used) <= reserve {
            return false;
        }
        let ct = &self.counters.conc_trigger;
        if ct
            .requested_at
            .compare_exchange(requested_at, 0, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return false;
        }
        ct.refused.store(false, Ordering::Relaxed);
        st.pending.store(false, Ordering::Relaxed);
        st.hold_until.store(
            used.saturating_add(director::conc_request_grace(reserve))
                .max(1),
            Ordering::Relaxed,
        );
        st.dropped.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// The VM opened a cycle on the pending request (`note_conc_start_outcome`).
    pub(super) fn note_rate_start_opened(&self) {
        let st = &self.counters.rstart;
        if st.pending.load(Ordering::Relaxed) {
            st.opened.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The request slot changed hands (a `System.gc()` request took it, or a
    /// collection reset it): the pending request is no longer this rule's.
    /// `collection`: a collection closed the interval, which also lifts a hold.
    pub(super) fn conc_rate_start_disown(&self, collection: bool) {
        let st = &self.counters.rstart;
        st.pending.store(false, Ordering::Relaxed);
        if collection {
            st.hold_until.store(0, Ordering::Relaxed);
        }
    }

    /// The `[GC] zgc-rate-start:` figures. `enabled` is the switch acting: on,
    /// and the every-trigger mode armed.
    pub fn rate_start_report(&self) -> ZRateStartReport {
        let st = &self.counters.rstart;
        let n = |c: &AtomicUsize| c.load(Ordering::Relaxed);
        ZRateStartReport {
            enabled: st.enabled.load(Ordering::Relaxed) && self.conc_every_trigger_armed(),
            requests: n(&st.requests),
            opened: n(&st.opened),
            dropped: n(&st.dropped),
        }
    }
}

#[cfg(test)]
mod tests {
    //! zt t26/zc9: the reader, the rule as arithmetic (hand-worked numbers),
    //! and the request / drop / reset protocol on a real heap.

    use super::*;
    use crate::collector::GarbageCollector;

    /// The reader: unset off; the profile turns it on; its own `=0` kills it
    /// inside the profile; alone it is on; the profile `=0` is off.
    #[test]
    fn zt26zc9_the_switch_reads_the_profile_and_its_own_kill() {
        let run = |edits: &[(&str, Option<&str>)]| {
            let all: Vec<(&str, Option<&str>)> = [
                "CRATONVM_ZGC_CONCURRENT",
                "CRATONVM_ZGC_CONCURRENT_RATE_START",
            ]
            .iter()
            .map(|&k| {
                let v = edits.iter().find(|(e, _)| *e == k).and_then(|(_, v)| *v);
                (k, v)
            })
            .collect();
            cratonvm_types::flags::with_thread_overrides(&all, zgc_concurrent_rate_start_enabled)
        };
        assert!(!run(&[]), "control: unset is off");
        assert!(run(&[("CRATONVM_ZGC_CONCURRENT", Some("1"))]));
        assert!(!run(&[
            ("CRATONVM_ZGC_CONCURRENT", Some("1")),
            ("CRATONVM_ZGC_CONCURRENT_RATE_START", Some("0")),
        ]));
        assert!(run(&[("CRATONVM_ZGC_CONCURRENT_RATE_START", Some("1"))]));
        assert!(!run(&[("CRATONVM_ZGC_CONCURRENT", Some("0"))]));
    }

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
}
