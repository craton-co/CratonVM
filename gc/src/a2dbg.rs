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

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

/// Cached `CRATONVM_DBG_A2` gate. When unset, `record` is a cheap early return.
#[inline]
pub fn enabled() -> bool {
    static E: OnceLock<bool> = OnceLock::new();
    *E.get_or_init(|| std::env::var_os("CRATONVM_DBG_A2").is_some())
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
#[inline]
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
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut l) = LOG.lock() {
        // Bound memory on this contended host: the young gen is tiny, so the
        // recent tail always covers every live young object. Clear (not
        // ring-shift) when large — cheaper, and only loses ancient records.
        if l.len() >= 300_000 {
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
