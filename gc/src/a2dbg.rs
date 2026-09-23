// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! A2 forensic allocation breadcrumb (gated `CRATONVM_DBG_A2`).
//!
//! Records `(addr -> class_id, kind, element_type, array_length, num_slots,
//! real_size)` at every young-gen header write so the non-moving sweep can,
//! at a linear-walk size desync, look up the EXACT object that was allocated
//! at (or covering) the corrupt address and compare its real allocated size
//! to the size the walker computed. This pins whether the desync is an
//! allocator that wrote a wrong/partial header vs a `gen_object_total_size`
//! walker bug. Default-inert (zero cost when the env gate is unset).

use crate::gc_flags;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

/// Cached `CRATONVM_DBG_A2` gate. When unset, `record` is a cheap early return.
#[inline]
pub fn enabled() -> bool {
    gc_flags().dbg_a2
}

/// One recorded allocation.
#[derive(Clone, Copy, Debug)]
pub struct Rec {
    pub addr: usize,
    pub class_id: u32,
    pub kind: u8,
    pub element_type: u8,
    pub array_length: u32,
    pub num_slots: u32,
    pub size: usize,
    pub seq: u64,
}

static LOG: Mutex<Vec<Rec>> = Mutex::new(Vec::new());
static SEQ: AtomicU64 = AtomicU64::new(0);

/// Record a young allocation's header at write time. No-op unless gated on.
///
/// PERF: the gate is a cached bool, but the seven-argument body kept the
/// whole thing out of line, so every young allocation paid a call/return pair
/// to reach a `return` (1.2% of the `CratonBench hashmap` phase as its own
/// profile symbol). The gate test is now inlined at the call site and the
/// recording body is `#[cold]`.
#[inline(always)]
pub fn record(
    addr: usize,
    class_id: u32,
    kind: u8,
    element_type: u8,
    array_length: u32,
    num_slots: u32,
    size: usize,
) {
    if !enabled() {
        return;
    }
    record_armed(
        addr,
        class_id,
        kind,
        element_type,
        array_length,
        num_slots,
        size,
    );
}

#[cold]
#[inline(never)]
fn record_armed(
    addr: usize,
    class_id: u32,
    kind: u8,
    element_type: u8,
    array_length: u32,
    num_slots: u32,
    size: usize,
) {
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut l) = LOG.lock() {
        // Bound memory on this contended host: the young gen is tiny, so the
        // recent tail always covers every live young object. Clear (not
        // ring-shift) when large — cheaper, and only loses ancient records.
        // Family-A investigation (2026-07-03): raised from 300k to 4M —
        // a heavy multi-threaded allocation workload (100 threads x 1000+
        // proxy/reflection calls, each several allocations) wraps a 300k ring
        // in well under a second, so "NO allocation record covers" was often
        // a false negative (ring already cleared), not evidence the address
        // was never allocated. 4M records is a bounded ~200MB Vec<Rec>
        // (Rec is ~48 bytes) — acceptable for a default-inert, opt-in-only
        // diagnostic.
        if l.len() >= 4_000_000 {
            l.clear();
        }
        l.push(Rec {
            addr,
            class_id,
            kind,
            element_type,
            array_length,
            num_slots,
            size,
            seq,
        });
    }
}

/// Drop all breadcrumb records. Called when the young arena is swapped/reset by
/// the moving collector — every recorded absolute address becomes stale (live
/// objects are copied to a new from-space), so cross-epoch lookups would lie.
/// Clearing per swap keeps the breadcrumb reliable WITHIN one non-moving epoch
/// (where A2's desync lives), so a `lookup_at` at the sweep names the right alloc.
#[inline]
pub fn clear() {
    if !enabled() {
        return;
    }
    if let Ok(mut l) = LOG.lock() {
        l.clear();
    }
}

/// Mark an address as freed/zeroed by the sweep (size-0 sentinel, kind=0xFF).
/// `lookup_covering` skips it automatically (size 0 never covers); `lookup_at`
/// surfaces it so the dump can show a slot was freed (disambiguates a stale
/// breadcrumb from a live object after address reuse).
#[inline]
pub fn record_free(addr: usize) {
    record(addr, 0, 0xFF, 0xFF, 0, 0, 0);
}

/// Mark an OLD-GEN block freed by the in-place old sweep
/// (`sweep_old_gen_non_moving`), preserving the victim's pre-free header
/// identity (class_id/num_slots/size) so a later zero-header access at this
/// address can be attributed: "the old sweep freed a live `class_id=X`
/// object here" is the smoking gun for a root-set gap in that sweep's mark
/// phase (kind sentinel 0xFE; size recorded but excluded from
/// `lookup_covering` semantics is NOT needed — covering hits on a freed span
/// are exactly what we want surfaced, so the real size is kept).
#[inline]
pub fn record_old_sweep_free(addr: usize, class_id: u32, num_slots: u32, size: usize) {
    record(addr, class_id, 0xFE, 0xFE, 0, num_slots, size);
}

/// Most-recent allocation whose `[addr, addr+size)` covers `target`.
pub fn lookup_covering(target: usize) -> Option<Rec> {
    let l = LOG.lock().ok()?;
    l.iter()
        .rev()
        .find(|r| r.addr <= target && target < r.addr + r.size)
        .copied()
}

/// Most-recent allocation whose start address is exactly `target`.
pub fn lookup_at(target: usize) -> Option<Rec> {
    let l = LOG.lock().ok()?;
    l.iter().rev().find(|r| r.addr == target).copied()
}

/// Full recorded event history touching `target` (exact-start allocs, the
/// sweep's free sentinels at that address, and any allocation whose span
/// covers it), oldest first, capped at the last `max` events. The lifecycle
/// SEQUENCE is the discriminator the single most-recent record cannot give:
/// `alloc(Entry) → free → alloc(other)` at one address proves the span was
/// reclaimed and reissued while the Entry was still referenced, while
/// `alloc(Entry)` alone followed by a zeroed header proves in-place
/// clobbering, and an empty history means the address was never
/// header-written (or the ring wrapped).
pub fn history_at(target: usize, max: usize) -> Vec<Rec> {
    let Ok(l) = LOG.lock() else {
        return Vec::new();
    };
    let mut hits: Vec<Rec> = l
        .iter()
        .filter(|r| {
            r.addr == target || (r.size > 0 && r.addr <= target && target < r.addr + r.size)
        })
        .copied()
        .collect();
    if hits.len() > max {
        hits.drain(..hits.len() - max);
    }
    hits
}
