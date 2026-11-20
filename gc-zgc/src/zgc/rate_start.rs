// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

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
pub fn zgc_concurrent_rate_start_enabled() -> bool {
    match Err::<String, std::env::VarError>(std::env::VarError::NotPresent) {
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
pub struct ZRateStartState {
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
    pub fn new(enabled: bool) -> Self {
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
mod tests;
