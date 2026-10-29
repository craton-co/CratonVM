// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! gcj j2/og2 (2026-10-19) — prefetch in front of the stop-the-world old-gen
//! mark's LIFO worklist (`CRATONVM_GC_OLD_MARK_PREFETCH`, opt-in;
//! `docs/feature-designs/generative-gc-architecture.md`).
//!
//! `GenerationalHeap::old_gen_gc_inner`'s BFS pops a `*mut u8` from its
//! worklist and reads the object's header at once. On an old generation larger
//! than the last-level cache nearly every pop is a miss on an address that was
//! known when it was pushed. [`MarkPrefetchFifo`] sits between the worklist and
//! the scan: a pop first moves entries off the top of the worklist into a ring
//! of [`MARK_PREFETCH_DEPTH`] slots, prefetching each header as it enters, and
//! then scans the OLDEST ring entry, whose line has had `DEPTH` scans' worth of
//! time to arrive (prefetch-on-grey, Boehm 2000; Cher, Hosking and
//! Vijaykumar 2004).
//!
//! Only the ORDER of the scans changes. Every entry still goes through the
//! loop body exactly once, and the BFS never depended on order: the mark bit
//! is set at the push sites, the finalizer round and the in-mark closures run
//! only when [`MarkPrefetchFifo::pop`] answers `None`, which it does only
//! when the worklist AND the ring are empty (the condition the loop's `else`
//! arm tested on the worklist alone). Pushes go to the worklist unchanged.
//!
//! Off (the default), [`MarkPrefetchFifo::pop`] is `worklist.pop()`.
//!
//! Unlike ZGC's `CRATONVM_ZGC_MARK_PREFETCH_NEXT` (+12 % CPU on a tree in
//! zs s1/mc: it prefetched the entry under the popped one, a sibling popped
//! only after the popped node's whole subtree), every prefetched entry here is
//! scanned within the next `DEPTH` pops, so its line is still cached. What it
//! costs is depth-first locality for the `DEPTH` entries in flight.

/// gcj j2/og2 — `CRATONVM_GC_OLD_MARK_PREFETCH` (opt-in, `=1` arms; read once
/// per major in `old_gen_gc_inner`).
pub(crate) const MARK_PREFETCH_FLAG: &str = "CRATONVM_GC_OLD_MARK_PREFETCH";

/// Ring depth: scans of lead time per prefetched header. The proposal's first
/// value; its rows decide whether another is worth a knob.
pub(crate) const MARK_PREFETCH_DEPTH: usize = 8;

/// The ring between the old mark's worklist and its scan. See the module doc.
pub(crate) struct MarkPrefetchFifo {
    on: bool,
    ring: [*mut u8; MARK_PREFETCH_DEPTH],
    /// Index of the oldest entry.
    head: usize,
    len: usize,
    /// Headers prefetched (entries moved into the ring), for the `[gcphase]`
    /// engagement count.
    prefetched: usize,
}

impl MarkPrefetchFifo {
    pub(crate) fn new(on: bool) -> Self {
        Self {
            on,
            ring: [std::ptr::null_mut(); MARK_PREFETCH_DEPTH],
            head: 0,
            len: 0,
            prefetched: 0,
        }
    }

    /// The next object to scan: `worklist.pop()` when off. On: top the ring up
    /// from the worklist (prefetching each entry it takes), then the oldest
    /// ring entry. `None` only when both are empty.
    #[inline]
    pub(crate) fn pop(&mut self, worklist: &mut Vec<*mut u8>) -> Option<*mut u8> {
        if !self.on {
            return worklist.pop();
        }
        while self.len < MARK_PREFETCH_DEPTH {
            let Some(p) = worklist.pop() else {
                break;
            };
            prefetch_read(p as usize);
            let tail = (self.head + self.len) % MARK_PREFETCH_DEPTH;
            self.ring[tail] = p;
            self.len += 1;
            self.prefetched += 1;
        }
        if self.len == 0 {
            return None;
        }
        let p = self.ring[self.head];
        self.head = (self.head + 1) % MARK_PREFETCH_DEPTH;
        self.len -= 1;
        Some(p)
    }

    /// Headers prefetched so far (0 when off).
    pub(crate) fn prefetched(&self) -> usize {
        self.prefetched
    }

    /// t1 G2 (`CRATONVM_GC_OLD_PAR_MARK`): hand the ring's entries back to the
    /// worklist (they are marked and not yet scanned), so a parallel drain
    /// that takes the whole worklist takes them too. Empty afterwards.
    pub(crate) fn spill_into(&mut self, worklist: &mut Vec<*mut u8>) {
        while self.len > 0 {
            worklist.push(self.ring[self.head]);
            self.head = (self.head + 1) % MARK_PREFETCH_DEPTH;
            self.len -= 1;
        }
    }
}

/// Prefetch the cache line at `addr` for reading. A hint: never faults, a
/// no-op off x86-64 (the same shape as `zgc/mark.rs`'s).
#[inline(always)]
fn prefetch_read(addr: usize) {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: `_mm_prefetch` is a hint that cannot fault on any address, and
    // SSE is part of the x86-64 baseline.
    unsafe {
        core::arch::x86_64::_mm_prefetch(addr as *const i8, core::arch::x86_64::_MM_HINT_T0);
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = addr;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(fifo: &mut MarkPrefetchFifo, worklist: &mut Vec<*mut u8>) -> Vec<usize> {
        let mut out = Vec::new();
        while let Some(p) = fifo.pop(worklist) {
            out.push(p as usize);
        }
        out
    }

    /// Off, the ring is `Vec::pop`: the same LIFO order, nothing prefetched.
    #[test]
    fn gcj_j2og2_an_off_ring_pops_exactly_as_the_worklist() {
        let mut fifo = MarkPrefetchFifo::new(false);
        let mut w: Vec<*mut u8> = (1..=20usize).map(|i| (i * 16) as *mut u8).collect();
        let got = drain(&mut fifo, &mut w);
        let want: Vec<usize> = (1..=20usize).rev().map(|i| i * 16).collect();
        assert_eq!(got, want);
        assert_eq!(fifo.prefetched(), 0);
    }

    /// On, every entry is popped exactly once, including entries pushed while
    /// the ring holds others, and `None` comes only when both are empty.
    #[test]
    fn gcj_j2og2_an_armed_ring_pops_every_entry_once() {
        let mut fifo = MarkPrefetchFifo::new(true);
        let mut w: Vec<*mut u8> = (1..=5usize).map(|i| (i * 16) as *mut u8).collect();
        let mut seen = Vec::new();
        let mut next = 6usize;
        while let Some(p) = fifo.pop(&mut w) {
            seen.push(p as usize);
            // Each of the first 30 scans pushes one child, as a mark would.
            if next <= 35 {
                w.push((next * 16) as *mut u8);
                next += 1;
            }
        }
        assert!(w.is_empty());
        let mut sorted = seen.clone();
        sorted.sort_unstable();
        let want: Vec<usize> = (1..=35usize).map(|i| i * 16).collect();
        assert_eq!(sorted, want, "every entry exactly once");
        assert_eq!(fifo.prefetched(), 35);
        // The ring is FIFO over what it took from the LIFO top: the first
        // pops are the top `DEPTH` entries, oldest-taken first.
        let mut fifo = MarkPrefetchFifo::new(true);
        let mut w: Vec<*mut u8> = (1..=20usize).map(|i| (i * 16) as *mut u8).collect();
        assert_eq!(fifo.pop(&mut w), Some((20 * 16) as *mut u8));
        assert_eq!(w.len(), 20 - MARK_PREFETCH_DEPTH);
        assert_eq!(fifo.pop(&mut w), Some((19 * 16) as *mut u8));
    }
}
