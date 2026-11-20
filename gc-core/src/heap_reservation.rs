// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

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
    /// gci i42/ta: `CRATONVM_G1_UNCOMMIT_TAIL_GRANULE` (default ON, `=0`
    /// restores the pre-i42 range), latched once per heap: [`Self::decommit_to`]
    /// also releases the mapped remainder of the prefix's last granule. See
    /// there.
    uncommit_tail_granule: bool,
    /// zt t29/g1 (G1 carry page item 10a, `-XX:+AlwaysPreTouch`): set by
    /// [`Self::set_pre_touch`]; while set, every [`Self::commit_to`] growth
    /// touches the pages it just committed, as HotSpot G1 pre-touches each
    /// region commit.
    pre_touch: AtomicBool,
    /// zt t29/g1: the startup commit as asked (`initial_commit` clamped to the
    /// reservation), the span [`Self::set_pre_touch`] touches on the
    /// wholly-committed fallback, where the whole reservation is "committed"
    /// only in name.
    startup_commit: usize,
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
        Self::new_with_thp(reserved, initial_commit, region, true)
    }

    /// Reserve `reserved` bytes and commit `initial_commit`, optionally suppressing
    /// transparent huge pages (`MADV_NOHUGEPAGE`) before the initial commit.
    pub fn new_with_thp(
        reserved: usize,
        initial_commit: usize,
        region: &str,
        allow_thp: bool,
    ) -> Self {
        let mut store = HeapStore::new(reserved, region);
        if !allow_thp {
            let _ = store.advise_no_huge_pages();
        }
        let base = store.as_ptr() as usize;
        let reserved_backing = matches!(store, HeapStore::Reserved(_));
        let uncommit_tail_granule =
            true;

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
                uncommit_tail_granule,
                pre_touch: AtomicBool::new(false),
                startup_commit: initial_commit.min(reserved),
            };
        }
        Self {
            store: Mutex::new(store),
            base,
            reserved,
            committed: AtomicUsize::new(initial),
            reserved_backing,
            uncommit_tail_granule,
            pre_touch: AtomicBool::new(false),
            startup_commit: initial_commit.min(reserved),
        }
    }

    /// zt t29/g1 (G1 carry page item 10a, `-XX:+AlwaysPreTouch`): from now on
    /// every growth of the committed prefix touches the pages it commits, and,
    /// when `on`, the prefix committed NOW (the `-Xms` startup commit) is
    /// touched before this returns ([`crate::reservation::pre_touch_range`]:
    /// one `MADV_POPULATE_WRITE`, or one value-preserving write per 4 KiB) --
    /// HotSpot G1 pre-touches the regions it commits at heap initialisation
    /// and on every expansion. On the wholly-committed fallback only the
    /// startup commit's bytes are touched. Returns the bytes touched now.
    pub fn set_pre_touch(&self, on: bool) -> usize {
        // Under the store lock, so a concurrent `commit_to` either sees the bit
        // (and touches its own growth) or finished before the prefix is read.
        let _store = self.store.lock();
        self.pre_touch.store(on, Ordering::Release);
        if !on {
            return 0;
        }
        let len = if self.reserved_backing {
            self.committed.load(Ordering::Acquire)
        } else {
            self.startup_commit
        };
        // SAFETY: `[base, base + len)` is committed (the prefix, or the
        // fallback's wholly-committed block) and stays mapped while the store
        // lock is held (`decommit_to` takes it).
        unsafe { crate::reservation::pre_touch_range(self.base as *mut u8, len) }
    }

    /// zki (page `zgc-zt23hunt4-*` item 5(c)): advise the whole reservation
    /// `MADV_HUGEPAGE` ([`crate::reservation::HeapStore::advise_huge_pages`]),
    /// as HotSpot 25 G1 advises its heap under `-XX:+UseTransparentHugePages`.
    /// `false` on the fallback store, off Linux, or when the kernel refused.
    pub fn advise_huge_pages(&self) -> bool {
        self.store.lock().advise_huge_pages()
    }

    /// zt t29/g1: whether [`Self::set_pre_touch`] is in force.
    pub fn pre_touch_enabled(&self) -> bool {
        self.pre_touch.load(Ordering::Acquire)
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
        if self.pre_touch.load(Ordering::Acquire) {
            // zt t29/g1 (`-XX:+AlwaysPreTouch`): the growth is touched before
            // it is published, as HotSpot G1 pre-touches a region commit.
            // SAFETY: `[current, want)` was committed just above, under the
            // store lock that every decommit takes.
            let _ = unsafe {
                crate::reservation::pre_touch_range(
                    (self.base + current) as *mut u8,
                    want - current,
                )
            };
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
    /// Returns `Some(released_bytes)` if the prefix is now at most `want`. The fallback returns
    /// `None` and changes nothing: it has no way to give pages back.
    pub fn decommit_to(&self, want: usize) -> Option<usize> {
        if !self.reserved_backing {
            return None;
        }
        let current = self.committed.load(Ordering::Acquire);
        if want >= current {
            return Some(0);
        }
        let mut store = self.store.lock();
        let current = self.committed.load(Ordering::Acquire);
        if want >= current {
            return Some(0);
        }
        // Publish the shrink BEFORE unmapping, so no reader can observe a
        // length that covers pages already gone.
        self.committed.store(want, Ordering::Release);
        // `decommit_range` returns the bytes it actually released; it only
        // releases whole granules, so the real committed extent may stay above
        // `want`. Reporting the LOWER figure is the safe direction — every
        // caller treats `committed_len` as "may be touched", and touching less
        // than is mapped is harmless where the reverse is a fault.
        //
        // gci i42/ta (`CRATONVM_G1_UNCOMMIT_TAIL_GRANULE`, default ON): the
        // range runs to `current` ROUNDED UP to a granule, not to `current`. `commit_to` commits
        // whole granules, so a prefix that ends mid-granule (a 1 MiB region
        // grid on 2 MiB granules: any odd number of regions) has the rest of
        // its last granule mapped too -- bytes above the prefix, which nothing
        // may touch. `decommit_range` rounds INWARD, so ending at `current`
        // kept that whole granule resident on every shrink. Everything in
        // `[current, round_up(current))` is above the prefix by definition,
        // so releasing it is as safe as releasing `[want, current)`; the
        // store clips the range to its own (granule-rounded) reservation.
        let end = decommit_end(current, self.uncommit_tail_granule);
        let released = store.decommit_range(want, end - want, "heap-shrink");
        Some(released)
    }
}

/// gci i42/ta: where [`ReservedHeap::decommit_to`]'s range ends for a prefix
/// of `current` bytes: `current` rounded up to a granule when
/// `uncommit_tail_granule` is on (the bytes above the prefix in its last
/// granule are mapped but untouchable), `current` itself when off (the pre-i42
/// range, which the store's inward rounding left one granule short). Pure.
fn decommit_end(current: usize, uncommit_tail_granule: bool) -> usize {
    if !uncommit_tail_granule {
        return current;
    }
    current
        .div_ceil(crate::reservation::GRANULE)
        .saturating_mul(crate::reservation::GRANULE)
        .max(current)
}

#[cfg(test)]
mod tests;
