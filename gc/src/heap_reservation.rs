// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! F-16 — G1's heap: RESERVED at `-Xmx`, COMMITTED as a growing PREFIX.
//!
//! # Why this is a thin policy and not a second implementation
//!
//! It used to carry its own `VirtualAlloc` / `mmap` layer. It does not any
//! more: `crate::reservation::HeapStore` landed on `dev` in parallel with this
//! branch, solving the same problem one level lower and for every collector
//! rather than just G1, and two sets of `extern "system"` declarations for the
//! same symbols do not even compile together. `HeapStore` is now the backing
//! store and this file is only the policy G1 needs on top of it.
//!
//! # The problem
//!
//! `G1Collector::new` allocated its whole arena with `alloc_zeroed`, sized to
//! `-Xmx`, in the constructor. There was no `-Xms` (the flag was parsed and
//! discarded), no expansion and no uncommit, so `-Xmx16g` charged 16 GiB
//! against the process at startup whether or not a byte was ever used — on
//! Windows, immediately, as commit charge against the page file.
//!
//! # The policy: a PREFIX, not an arbitrary set
//!
//! `HeapStore` commits in granules anywhere in the reservation. G1 additionally
//! requires the committed set to be a contiguous prefix, and that is this
//! file's whole reason to exist:
//!
//! * `gen_heap::publish_jit_read_bounds` tells the JIT "a raw load anywhere in
//!   `[base, end)` cannot fault", and compiled `getfield` fast paths test
//!   against it at runtime. That claim is only expressible as a RANGE, so the
//!   set of mapped bytes has to be one.
//! * region `i` starts at `arena_base + i * region_size` and the address→region
//!   lookup is a shift (F-09). A reservation is contiguous address space, so
//!   that arithmetic is unchanged; only the mapping state differs.
//!
//! Committing "through" a region therefore commits everything below it. That
//! costs almost nothing: `claim_free_region` starts at zero and advances a
//! rotating hint, so claims run roughly in index order.
//!
//! # Reading the prefix is lock-free; moving it is not
//!
//! `committed` is an atomic that every allocation path reads and almost never
//! moves — `commit_to` returns on it without touching the store when the range
//! is already covered. Only an actual grow takes the store's mutex, which is
//! why a `HeapStore` needing `&mut` for `commit_range` is not a problem on a
//! path that runs under a shared guard.

use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::Mutex;

use crate::reservation::HeapStore;

/// A contiguous heap reservation with a committed prefix.
pub struct ReservedHeap {
    /// Locked only to move the prefix. Reads go to `committed` instead.
    store: Mutex<HeapStore>,
    base: usize,
    /// Total reserved bytes — the address range the region table spans. Fixed.
    reserved: usize,
    /// Bytes committed from `base`. Only this prefix may be touched.
    committed: AtomicUsize,
    /// Whether the store is a real reservation, cached so the hot paths and the
    /// diagnostics never take the lock to ask.
    reserved_backing: bool,
}

impl ReservedHeap {
    /// Reserve `reserved` bytes and commit the first `initial_commit`.
    ///
    /// `reserved` is `-Xmx` rounded to the region grid by the caller;
    /// `initial_commit` is `-Xms`, clamped into `[0, reserved]`.
    ///
    /// Never fails: `HeapStore::new` falls back to a wholly-committed block
    /// when the OS refuses, when the capacity is below one granule, or under
    /// its own kill switch, and this reports that block's whole length as
    /// committed — which is exactly the pre-F-16 behaviour.
    pub fn new(reserved: usize, initial_commit: usize, region: &str) -> Self {
        let mut store = HeapStore::new(reserved, region);
        let base = store.as_ptr() as usize;
        let reserved_backing = matches!(store, HeapStore::Reserved(_));

        let initial = if reserved_backing {
            initial_commit.min(reserved)
        } else {
            // The fallback is committed in full at construction; saying so is
            // what lets every caller treat the two identically.
            reserved
        };
        if reserved_backing && initial > 0 && !store.commit_range(0, initial) {
            // Could not commit even the initial prefix. Report only what is
            // certainly there; `commit_to` will retry as the heap is used, and
            // a refusal there is an allocation failure the collector already
            // handles.
            return Self {
                store: Mutex::new(store),
                base,
                reserved,
                committed: AtomicUsize::new(0),
                reserved_backing,
            };
        }
        Self {
            store: Mutex::new(store),
            base,
            reserved,
            committed: AtomicUsize::new(initial),
            reserved_backing,
        }
    }

    /// Base address of the reservation.
    #[inline]
    pub fn base(&self) -> usize {
        self.base
    }

    /// Total reserved bytes. The span the region table covers, NOT the memory
    /// charged to the process.
    #[inline]
    pub fn reserved_len(&self) -> usize {
        self.reserved
    }

    /// Bytes currently committed: charged to the process, and safe to touch.
    #[inline]
    pub fn committed_len(&self) -> usize {
        self.committed.load(Ordering::Acquire)
    }

    /// Is this a real reservation, or the wholly-committed fallback?
    #[inline]
    pub fn is_reserved(&self) -> bool {
        self.reserved_backing
    }

    /// Grow the committed prefix to at least `want` bytes.
    ///
    /// Returns `true` if `[base, want)` is committed afterwards — including
    /// when it already was, and always for the fallback. `false` only when the
    /// OS refused, which the caller must treat as "no more heap": the address
    /// space is reserved but the pages are not there.
    pub fn commit_to(&self, want: usize) -> bool {
        if want > self.reserved {
            return false;
        }
        // The common case, and the reason the mutex below is affordable on an
        // allocation path: one acquire load and a compare.
        if want <= self.committed.load(Ordering::Acquire) {
            return true;
        }
        if !self.reserved_backing {
            return true;
        }
        let mut store = self.store.lock();
        // Re-read under the lock: another thread may have grown past `want`
        // while this one queued.
        let current = self.committed.load(Ordering::Acquire);
        if want <= current {
            return true;
        }
        if !store.commit_range(current, want - current) {
            return false;
        }
        // Release: the pages must be visible to any thread that observes the
        // new length, which is the whole point of publishing it.
        self.committed.store(want, Ordering::Release);
        true
    }

    /// Shrink the committed prefix to `want` bytes, returning the pages above
    /// it to the OS.
    ///
    /// # Caller obligation
    ///
    /// Every byte in `[want, committed)` must be dead and unreachable: no live
    /// object, no region the collector will allocate into, no address any
    /// compiled frame or conservative root holds. Only callable from a
    /// stop-the-world phase, and the caller must additionally have narrowed the
    /// published JIT read bounds FIRST — a compiled `getfield` tests against
    /// those at runtime, and unmapping pages a live bound still describes is a
    /// fault in compiled code.
    ///
    /// Returns `true` if the prefix is now at most `want`. The fallback returns
    /// `false` and changes nothing: it has no way to give pages back.
    pub fn decommit_to(&self, want: usize) -> bool {
        if !self.reserved_backing {
            return false;
        }
        let current = self.committed.load(Ordering::Acquire);
        if want >= current {
            return true;
        }
        let mut store = self.store.lock();
        let current = self.committed.load(Ordering::Acquire);
        if want >= current {
            return true;
        }
        // Publish the shrink BEFORE unmapping, so no reader can observe a
        // length that covers pages already gone.
        self.committed.store(want, Ordering::Release);
        // `decommit_range` returns the bytes it actually released; it only
        // releases whole granules, so the real committed extent may stay above
        // `want`. Reporting the LOWER figure is the safe direction — every
        // caller treats `committed_len` as "may be touched", and touching less
        // than is mapped is harmless where the reverse is a fault.
        let _released = store.decommit_range(want, current - want, "heap-shrink");
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: usize = 1024 * 1024;
    /// `HeapStore` declines to reserve below one granule, so every test that
    /// wants the reserved arm must ask for at least this much.
    const BIG: usize = 64 * MIB;

    #[test]
    fn a_large_heap_commits_only_what_was_asked_for() {
        let heap = ReservedHeap::new(BIG, 4 * MIB, "test");
        assert_eq!(heap.reserved_len(), BIG);
        assert!(heap.base() != 0);
        if heap.is_reserved() {
            assert!(
                heap.committed_len() <= 4 * MIB,
                "-Xmx is address space, -Xms is memory (got {})",
                heap.committed_len()
            );
        } else {
            assert_eq!(
                heap.committed_len(),
                BIG,
                "the fallback is the pre-F-16 behaviour and says so"
            );
        }
    }

    /// The reserved arm must actually be TAKEN where the platform supports it.
    /// Every other test here has a fallback arm, so a silent regression to the
    /// pre-F-16 behaviour would leave them all green.
    #[test]
    #[cfg(any(windows, target_os = "linux"))]
    fn this_platform_really_reserves() {
        let heap = ReservedHeap::new(BIG, MIB, "test");
        assert!(
            heap.is_reserved(),
            "windows and linux both reserve; falling back here means every \
             other test in this module quietly started covering the fallback"
        );
        assert!(heap.committed_len() < heap.reserved_len());
    }

    #[test]
    fn the_committed_prefix_is_writable_and_zeroed() {
        let heap = ReservedHeap::new(BIG, 4 * MIB, "test");
        let n = heap.committed_len().min(4 * MIB);
        assert!(n > 0);
        // SAFETY: `[base, committed)` is committed by construction.
        let slice = unsafe { std::slice::from_raw_parts_mut(heap.base() as *mut u8, n) };
        assert!(slice.iter().all(|&b| b == 0), "committed pages arrive zeroed");
        slice[0] = 0xAB;
        slice[n - 1] = 0xCD;
        assert_eq!((slice[0], slice[n - 1]), (0xAB, 0xCD));
    }

    #[test]
    fn growing_the_prefix_makes_the_new_bytes_usable() {
        let heap = ReservedHeap::new(BIG, 4 * MIB, "test");
        let before = heap.committed_len();
        assert!(heap.commit_to(16 * MIB));
        assert!(heap.committed_len() >= 16 * MIB);

        // SAFETY: just committed.
        let slice = unsafe {
            std::slice::from_raw_parts_mut((heap.base() + before) as *mut u8, 16 * MIB - before)
        };
        assert!(slice.iter().all(|&b| b == 0));
        slice[0] = 0x5A;
        assert_eq!(slice[0], 0x5A);
    }

    #[test]
    fn growing_is_monotone_and_idempotent() {
        let heap = ReservedHeap::new(BIG, 4 * MIB, "test");
        assert!(heap.commit_to(16 * MIB));
        let high = heap.committed_len();
        // A request BELOW the current prefix must not shrink it: the callers
        // are region claims arriving out of order, and a claim of a low index
        // must not un-map a higher one already in use.
        assert!(heap.commit_to(4 * MIB));
        assert_eq!(heap.committed_len(), high);
    }

    #[test]
    fn a_request_past_the_reservation_is_refused_rather_than_clamped() {
        let heap = ReservedHeap::new(BIG, 4 * MIB, "test");
        assert!(
            !heap.commit_to(BIG * 2),
            "the reservation is the hard limit -Xmx means; silently clamping \
             would report success for memory the caller does not have"
        );
    }

    #[test]
    fn decommitting_gives_pages_back_and_they_read_as_zero_again() {
        let heap = ReservedHeap::new(BIG, 32 * MIB, "test");
        if !heap.is_reserved() {
            assert!(!heap.decommit_to(MIB));
            assert_eq!(heap.committed_len(), BIG);
            return;
        }
        let probe = heap.base() + 24 * MIB;
        // SAFETY: committed by construction.
        unsafe { std::ptr::write_bytes(probe as *mut u8, 0xFF, 4096) };
        assert!(heap.decommit_to(8 * MIB));
        assert_eq!(heap.committed_len(), 8 * MIB);
        assert_eq!(heap.reserved_len(), BIG, "the RESERVATION is untouched");

        // Re-commit and confirm the dirty bytes did not survive: a recycled
        // region must not hand an allocator someone else's data.
        assert!(heap.commit_to(32 * MIB));
        let slice = unsafe { std::slice::from_raw_parts(probe as *const u8, 4096) };
        assert!(
            slice.iter().all(|&b| b == 0),
            "recommitted pages must be zero, not the bytes that were there"
        );
    }

    #[test]
    fn a_zero_sized_heap_is_inert_rather_than_a_panic() {
        let heap = ReservedHeap::new(0, 0, "test");
        assert_eq!(heap.reserved_len(), 0);
        assert_eq!(heap.committed_len(), 0);
        assert!(!heap.commit_to(1));
    }

    #[test]
    fn the_initial_commit_is_clamped_into_the_reservation() {
        // -Xms above -Xmx: take the reservation, not a refusal — the two are
        // separately specified and a user who oversizes one should get a heap.
        let heap = ReservedHeap::new(BIG, 4 * BIG, "test");
        assert_eq!(heap.committed_len(), BIG);
    }

    /// A heap too small for the backing store to reserve must still WORK — it
    /// simply gets the wholly-committed block, and every caller must be unable
    /// to tell except through `committed_len`.
    #[test]
    fn a_sub_granule_heap_falls_back_and_is_fully_usable() {
        let heap = ReservedHeap::new(64 * 1024, 8 * 1024, "test");
        assert!(!heap.is_reserved());
        assert_eq!(heap.committed_len(), 64 * 1024);
        assert!(heap.commit_to(64 * 1024));
        // SAFETY: the fallback is committed in full.
        let slice = unsafe { std::slice::from_raw_parts_mut(heap.base() as *mut u8, 64 * 1024) };
        slice[0] = 1;
        slice[64 * 1024 - 1] = 2;
        assert_eq!((slice[0], slice[64 * 1024 - 1]), (1, 2));
    }
}
